
use std::ffi::c_void;
use std::fs::File;
use std::io::{Seek, SeekFrom};
use std::os::fd::{AsRawFd, BorrowedFd, RawFd};

use smithay::backend::allocator::dmabuf::Dmabuf;
use smithay::backend::allocator::{Buffer as _, Format, Fourcc, Modifier};
use smithay::reexports::wayland_server::protocol::wl_shm;
use smithay::wayland::dmabuf::ImportNotifier;
use smithay::wayland::shm::BufferData;

pub(crate) const MAX_LADO: i32 = 4096;
const NO_RENDER: &str = "/dev/dri/renderD128";
const ESPERA_GPU_MS: i32 = 100;

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Rejeicao {
    Formato,
    Planos,
    Dimensoes,
}

const FORMATOS: [Fourcc; 4] = [Fourcc::Xrgb8888, Fourcc::Argb8888, Fourcc::Xbgr8888, Fourcc::Abgr8888];

pub(crate) fn formatos() -> Vec<Format> {
    FORMATOS.into_iter().map(|code| Format { code, modifier: Modifier::Linear }).collect()
}

fn formato_shm(code: Fourcc) -> Option<wl_shm::Format> {
    match code {
        Fourcc::Argb8888 => Some(wl_shm::Format::Argb8888),
        Fourcc::Xrgb8888 => Some(wl_shm::Format::Xrgb8888),
        Fourcc::Abgr8888 => Some(wl_shm::Format::Abgr8888),
        Fourcc::Xbgr8888 => Some(wl_shm::Format::Xbgr8888),
        _ => None,
    }
}

pub(crate) fn precisa_trocar_r_b(f: wl_shm::Format) -> bool {
    matches!(f, wl_shm::Format::Abgr8888 | wl_shm::Format::Xbgr8888)
}

pub(crate) fn troca_r_b(px: &mut [u8]) {
    for p in px.chunks_exact_mut(4) {
        p.swap(0, 2);
    }
}

pub(crate) fn dispositivo() -> Option<libc::dev_t> {
    Some(std::os::unix::fs::MetadataExt::rdev(&std::fs::metadata(NO_RENDER).ok()?))
}

fn tamanho(fd: BorrowedFd<'_>) -> Option<u64> {
    let mut f = File::from(fd.try_clone_to_owned().ok()?);
    let n = f.seek(SeekFrom::End(0)).ok()?;
    f.seek(SeekFrom::Start(0)).ok()?;
    Some(n)
}

pub(crate) fn validar(d: &Dmabuf) -> Result<(), Rejeicao> {
    if d.num_planes() != 1 {
        return Err(Rejeicao::Planos);
    }
    let fmt = d.format();
    if !FORMATOS.contains(&fmt.code) || !matches!(fmt.modifier, Modifier::Linear | Modifier::Invalid) {
        return Err(Rejeicao::Formato);
    }
    let size = d.size();
    if size.w < 1 || size.h < 1 || size.w > MAX_LADO || size.h > MAX_LADO {
        return Err(Rejeicao::Dimensoes);
    }
    let (stride, offset) = (u64::from(d.strides().next().unwrap_or(0)), u64::from(d.offsets().next().unwrap_or(0)));
    if stride < size.w as u64 * 4 {
        return Err(Rejeicao::Dimensoes);
    }
    let fd = d.handles().next().ok_or(Rejeicao::Planos)?;
    let precisa = offset + stride * size.h as u64;
    match tamanho(fd) {
        Some(t) if t >= precisa => Ok(()),
        _ => Err(Rejeicao::Dimensoes),
    }
}

pub(crate) fn recusar(n: ImportNotifier, r: Rejeicao) {
    match r {
        Rejeicao::Formato => n.invalid_format(),
        Rejeicao::Planos => n.incomplete(),
        Rejeicao::Dimensoes => n.invalid_dimensions(),
    }
}

struct Mapa {
    ptr: *mut c_void,
    len: usize,
}

impl Mapa {
    fn nova(fd: RawFd, len: usize) -> Option<Mapa> {
        if len == 0 {
            return None;
        }
        let ptr = unsafe { libc::mmap(std::ptr::null_mut(), len, libc::PROT_READ, libc::MAP_SHARED, fd, 0) };
        (ptr != libc::MAP_FAILED).then_some(Mapa { ptr, len })
    }
}

impl Drop for Mapa {
    fn drop(&mut self) {
        unsafe {
            libc::munmap(self.ptr, self.len);
        }
    }
}

const DMA_BUF_IOCTL_SYNC: u64 = 0x4008_6200;
const SYNC_READ: u64 = 1;
const SYNC_START: u64 = 0;
const SYNC_END: u64 = 4;

struct Sincronia(RawFd);

impl Sincronia {
    fn ioctl(fd: RawFd, flags: u64) {
        unsafe {
            libc::ioctl(fd, DMA_BUF_IOCTL_SYNC as _, &flags as *const u64);
        }
    }

    fn inicia(fd: RawFd) -> Sincronia {
        Self::ioctl(fd, SYNC_START | SYNC_READ);
        Sincronia(fd)
    }
}

impl Drop for Sincronia {
    fn drop(&mut self) {
        Self::ioctl(self.0, SYNC_END | SYNC_READ);
    }
}

fn espera_escrita(fd: RawFd) {
    let mut p = libc::pollfd { fd, events: libc::POLLIN, revents: 0 };
    unsafe {
        libc::poll(&mut p, 1, ESPERA_GPU_MS);
    }
}

pub(crate) fn com_pixels<T>(d: &Dmabuf, f: impl FnOnce(*const u8, usize, BufferData) -> T) -> Option<T> {
    let fd = d.handles().next()?;
    let raw = fd.as_raw_fd();
    let size = d.size();
    let (stride, offset) = (d.strides().next()?, d.offsets().next()?);
    let format = formato_shm(d.format().code)?;
    let len = usize::try_from(tamanho(fd)?).ok()?;
    if (offset as usize).checked_add((stride as usize).checked_mul(size.h.max(0) as usize)?)? > len {
        return None;
    }
    espera_escrita(raw);
    let mapa = Mapa::nova(raw, len)?;
    let _sync = Sincronia::inicia(raw);
    let dados = BufferData { offset: offset as i32, width: size.w, height: size.h, stride: stride as i32, format };
    Some(f(mapa.ptr as *const u8, mapa.len, dados))
}

pub(crate) fn mapeavel(d: &Dmabuf) -> bool {
    com_pixels(d, |_, _, _| ()).is_some()
}

#[cfg(test)]
mod tests {
    use super::*;
    use smithay::backend::allocator::dmabuf::DmabufFlags;
    use std::io::Write;
    use std::os::fd::OwnedFd;

    fn arquivo(bytes: &[u8]) -> OwnedFd {
        let mut f = tempfile_na_pasta_temp();
        f.write_all(bytes).unwrap();
        f.into()
    }

    fn tempfile_na_pasta_temp() -> File {
        use std::sync::atomic::{AtomicU32, Ordering};
        static N: AtomicU32 = AtomicU32::new(0);
        let p = std::env::temp_dir().join(format!("pistick-dmabuf-{}-{}", std::process::id(), N.fetch_add(1, Ordering::Relaxed)));
        let f = std::fs::OpenOptions::new().read(true).write(true).create_new(true).open(&p).unwrap();
        let _ = std::fs::remove_file(&p);
        f
    }

    fn dmabuf(fd: OwnedFd, w: i32, h: i32, fmt: Fourcc, m: Modifier, offset: u32, stride: u32) -> Dmabuf {
        let mut b = Dmabuf::builder((w, h), fmt, m, DmabufFlags::empty());
        assert!(b.add_plane(fd, 0, offset, stride));
        b.build().unwrap()
    }

    #[test]
    fn aceita_xrgb_e_argb_lineares() {
        for fmt in [Fourcc::Xrgb8888, Fourcc::Argb8888] {
            for m in [Modifier::Linear, Modifier::Invalid] {
                let d = dmabuf(arquivo(&vec![0; 16 * 4 * 8]), 16, 8, fmt, m, 0, 64);
                assert_eq!(validar(&d), Ok(()), "{fmt:?} {m:?}");
            }
        }
    }

    #[test]
    fn aceita_abgr_e_xbgr_lineares_e_anuncia_os_quatro_formatos() {
        for fmt in [Fourcc::Abgr8888, Fourcc::Xbgr8888] {
            for m in [Modifier::Linear, Modifier::Invalid] {
                let d = dmabuf(arquivo(&vec![0; 16 * 4 * 8]), 16, 8, fmt, m, 0, 64);
                assert_eq!(validar(&d), Ok(()), "{fmt:?} {m:?}");
            }
        }
        let anunciados: Vec<Fourcc> = formatos().iter().map(|f| f.code).collect();
        for f in [Fourcc::Xrgb8888, Fourcc::Argb8888, Fourcc::Xbgr8888, Fourcc::Abgr8888] {
            assert!(anunciados.contains(&f), "{f:?} não anunciado");
        }
        assert!(formatos().iter().all(|f| f.modifier == Modifier::Linear));
    }

    #[test]
    fn mapeia_os_formatos_para_o_shm_e_diz_quem_troca_r_b() {
        assert_eq!(formato_shm(Fourcc::Abgr8888), Some(wl_shm::Format::Abgr8888));
        assert_eq!(formato_shm(Fourcc::Xbgr8888), Some(wl_shm::Format::Xbgr8888));
        assert_eq!(formato_shm(Fourcc::Rgb565), None);
        assert!(precisa_trocar_r_b(wl_shm::Format::Abgr8888) && precisa_trocar_r_b(wl_shm::Format::Xbgr8888));
        assert!(!precisa_trocar_r_b(wl_shm::Format::Argb8888) && !precisa_trocar_r_b(wl_shm::Format::Xrgb8888));
    }

    #[test]
    fn troca_r_b_inverte_o_primeiro_e_o_terceiro_byte() {
        let mut v = vec![1, 2, 3, 4, 10, 20, 30, 40, 9, 9, 9];
        troca_r_b(&mut v);
        assert_eq!(v, vec![3, 2, 1, 4, 30, 20, 10, 40, 9, 9, 9]);
        troca_r_b(&mut v);
        assert_eq!(v, vec![1, 2, 3, 4, 10, 20, 30, 40, 9, 9, 9]);
        troca_r_b(&mut []);
    }

    #[test]
    fn recusa_formato_e_modificador_nao_anunciados() {
        let d = dmabuf(arquivo(&[0; 4096]), 16, 8, Fourcc::Rgb565, Modifier::Linear, 0, 64);
        assert_eq!(validar(&d), Err(Rejeicao::Formato));
        let uif = Modifier::from(0x0700_0000_0000_0006u64);
        let d = dmabuf(arquivo(&[0; 4096]), 16, 8, Fourcc::Xrgb8888, uif, 0, 64);
        assert_eq!(validar(&d), Err(Rejeicao::Formato));
    }

    #[test]
    fn recusa_dimensoes_invalidas() {
        let g = |w, h, stride, bytes: usize| dmabuf(arquivo(&vec![0; bytes]), w, h, Fourcc::Xrgb8888, Modifier::Linear, 0, stride);
        assert_eq!(validar(&g(0, 8, 64, 4096)), Err(Rejeicao::Dimensoes));
        assert_eq!(validar(&g(16, 0, 64, 4096)), Err(Rejeicao::Dimensoes));
        assert_eq!(validar(&g(4097, 1, 4097 * 4, 4097 * 4)), Err(Rejeicao::Dimensoes));
        assert_eq!(validar(&g(16, 4097, 64, 64)), Err(Rejeicao::Dimensoes));
        assert_eq!(validar(&g(16, 8, 60, 4096)), Err(Rejeicao::Dimensoes));
        assert_eq!(validar(&g(16, 8, 64, 64 * 8 - 1)), Err(Rejeicao::Dimensoes));
        assert_eq!(validar(&g(16, 8, 64, 64 * 8)), Ok(()));
        let d = dmabuf(arquivo(&vec![0; 64 * 8 + 3]), 16, 8, Fourcc::Xrgb8888, Modifier::Linear, 4, 64);
        assert_eq!(validar(&d), Err(Rejeicao::Dimensoes));
    }

    #[test]
    fn recusa_mais_de_um_plano() {
        let mut b = Dmabuf::builder((16, 8), Fourcc::Xrgb8888, Modifier::Linear, DmabufFlags::empty());
        assert!(b.add_plane(arquivo(&[0; 4096]), 0, 0, 64));
        assert!(b.add_plane(arquivo(&[0; 4096]), 1, 0, 64));
        assert_eq!(validar(&b.build().unwrap()), Err(Rejeicao::Planos));
    }

    #[test]
    fn le_os_pixels_pelo_mapeamento() {
        let (w, h, stride, offset) = (8usize, 4usize, 40usize, 16usize);
        let mut bytes = vec![0u8; offset + stride * h];
        for y in 0..h {
            for x in 0..w * 4 {
                bytes[offset + y * stride + x] = (y * 31 + x) as u8;
            }
        }
        let d = dmabuf(arquivo(&bytes), w as i32, h as i32, Fourcc::Argb8888, Modifier::Linear, offset as u32, stride as u32);
        assert_eq!(validar(&d), Ok(()));
        assert!(mapeavel(&d));
        let lido = com_pixels(&d, |ptr, len, dados| {
            assert_eq!((dados.width, dados.height, dados.stride, dados.offset, dados.format), (8, 4, 40, 16, wl_shm::Format::Argb8888));
            assert_eq!(len, bytes.len());
            unsafe { std::slice::from_raw_parts(ptr, len) }.to_vec()
        })
        .unwrap();
        assert_eq!(lido, bytes);
    }

    #[test]
    fn descritor_truncado_depois_da_validacao_nao_le() {
        let fd = arquivo(&vec![0; 64 * 8]);
        let d = dmabuf(fd.try_clone().unwrap(), 16, 8, Fourcc::Xrgb8888, Modifier::Linear, 0, 64);
        assert_eq!(validar(&d), Ok(()));
        File::from(fd).set_len(10).unwrap();
        assert!(com_pixels(&d, |_, _, _| ()).is_none());
    }
}
