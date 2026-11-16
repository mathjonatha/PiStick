//! Buffers DMABUF (`zwp_linux_dmabuf_v1`, marco 4.5): aceitação, validação e leitura direta.
//!
//! O `psd` não compõe nada: só precisa dos pixels de cada buffer para o codec. Por isso aceita apenas
//! o que dá para ler com a CPU, sem renderizador: `XRGB8888`, `ARGB8888`, `XBGR8888` e `ABGR8888`
//! (os dois últimos são o que o compositor do Android/Waydroid entrega; R e B trocam de lugar na
//! cópia, veja [`troca_r_b`]), um plano, modificador `LINEAR` (ou `INVALID`, que quer dizer "sem
//! modificador" e é tratado como linear). O Mesa escolhe entre o que o compositor anuncia;
//! anunciando só linear, o V3D do Pi renderiza em buffer linear.
//!
//! Leitura de um quadro (função [`com_pixels`]), sem cache de mapeamento (mapear custa 25 a 60 µs):
//! 1. `poll(POLLIN)` no descritor: espera a GPU terminar de escrever (sincronismo implícito, o
//!    `dma_resv` do kernel). Sem isso o quadro sai incompleto; o teste de prova mostrou 400 de 400
//!    quadros errados sem a espera e nenhum com ela.
//! 2. `mmap` somente leitura do descritor inteiro, a partir do início (o `offset` do plano é somado
//!    por quem lê, como num pool `wl_shm`).
//! 3. `DMA_BUF_IOCTL_SYNC` de início e de fim de leitura em volta da cópia.
//!
//! Risco residual (SIGBUS): se o dono do descritor encolher o arquivo depois de validado, tocar numa
//! página além do novo fim mata o processo com SIGBUS. Com DMABUF de GPU o tamanho é fixo na
//! alocação (não há `ftruncate`), e a validação por `lseek` confere que o descritor cobre
//! `offset + stride * altura`. Um descritor de outro tipo (um `memfd` com selos, por exemplo) pode
//! ser truncado por um cliente malicioso; não há tratador de SIGBUS, por decisão do projeto.
//!
//! Todo o `unsafe` do marco fica neste arquivo.

use std::ffi::c_void;
use std::fs::File;
use std::io::{Seek, SeekFrom};
use std::os::fd::{AsRawFd, BorrowedFd, RawFd};

use smithay::backend::allocator::dmabuf::Dmabuf;
use smithay::backend::allocator::{Buffer as _, Format, Fourcc, Modifier};
use smithay::reexports::wayland_server::protocol::wl_shm;
use smithay::wayland::dmabuf::ImportNotifier;
use smithay::wayland::shm::BufferData;

/// Maior largura ou altura aceita.
pub(crate) const MAX_LADO: i32 = 4096;
/// Nó de renderização do V3D, anunciado como dispositivo principal do *feedback*.
const NO_RENDER: &str = "/dev/dri/renderD128";
/// Espera máxima pelo fim da escrita da GPU antes de ler (um quadro nunca leva isso).
const ESPERA_GPU_MS: i32 = 100;

/// Por que um buffer foi recusado (vira o erro de protocolo correspondente).
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Rejeicao {
    /// Formato ou modificador não anunciado (`invalid_format`).
    Formato,
    /// Mais ou menos de um plano (`incomplete`).
    Planos,
    /// Dimensões fora de 1..=4096, `stride` curto demais ou descritor menor que o buffer (`invalid_dimensions`).
    Dimensoes,
}

/// Formatos aceitos (todos lineares, 32 bits por pixel, um plano).
const FORMATOS: [Fourcc; 4] = [Fourcc::Xrgb8888, Fourcc::Argb8888, Fourcc::Xbgr8888, Fourcc::Abgr8888];

/// Formatos e modificadores anunciados: só o que se lê direto.
pub(crate) fn formatos() -> Vec<Format> {
    FORMATOS.into_iter().map(|code| Format { code, modifier: Modifier::Linear }).collect()
}

/// O formato do `wl_shm` que corresponde ao do DMABUF (`None` se não é um dos aceitos).
fn formato_shm(code: Fourcc) -> Option<wl_shm::Format> {
    match code {
        Fourcc::Argb8888 => Some(wl_shm::Format::Argb8888),
        Fourcc::Xrgb8888 => Some(wl_shm::Format::Xrgb8888),
        Fourcc::Abgr8888 => Some(wl_shm::Format::Abgr8888),
        Fourcc::Xbgr8888 => Some(wl_shm::Format::Xbgr8888),
        _ => None,
    }
}

/// O formato guarda os canais na ordem R, G, B, A na memória (`ABGR8888`/`XBGR8888`)? Quem copia
/// precisa então trocar R e B para chegar ao formato interno (B, G, R, X).
pub(crate) fn precisa_trocar_r_b(f: wl_shm::Format) -> bool {
    matches!(f, wl_shm::Format::Abgr8888 | wl_shm::Format::Xbgr8888)
}

/// Troca o primeiro e o terceiro byte de cada pixel de 4 bytes (R e B), no lugar. O quarto byte (alfa
/// ou sem uso) e o verde ficam como estão. Sobras de menos de 4 bytes no fim não são tocadas.
pub(crate) fn troca_r_b(px: &mut [u8]) {
    for p in px.chunks_exact_mut(4) {
        p.swap(0, 2);
    }
}

/// Número do dispositivo (`dev_t`) do nó de renderização; `None` se a máquina não tem GPU.
pub(crate) fn dispositivo() -> Option<libc::dev_t> {
    Some(std::os::unix::fs::MetadataExt::rdev(&std::fs::metadata(NO_RENDER).ok()?))
}

/// Tamanho do descritor por `lseek` (o `fstat` de um dmabuf devolve 0). O deslocamento é compartilhado
/// com o descritor do cliente (é a mesma descrição de arquivo), por isso volta a 0.
fn tamanho(fd: BorrowedFd<'_>) -> Option<u64> {
    let mut f = File::from(fd.try_clone_to_owned().ok()?);
    let n = f.seek(SeekFrom::End(0)).ok()?;
    f.seek(SeekFrom::Start(0)).ok()?;
    Some(n)
}

/// Confere tudo o que o Smithay não confere: modificador anunciado, um plano, dimensões de 1 a 4096,
/// `stride >= largura * 4` e descritor com pelo menos `offset + stride * altura` bytes.
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

/// Responde ao cliente com o erro de protocolo de `r`.
pub(crate) fn recusar(n: ImportNotifier, r: Rejeicao) {
    match r {
        Rejeicao::Formato => n.invalid_format(),
        Rejeicao::Planos => n.incomplete(),
        Rejeicao::Dimensoes => n.invalid_dimensions(),
    }
}

/// Mapeamento somente leitura de um descritor, desfeito ao sair de escopo.
struct Mapa {
    ptr: *mut c_void,
    len: usize,
}

impl Mapa {
    fn nova(fd: RawFd, len: usize) -> Option<Mapa> {
        if len == 0 {
            return None;
        }
        // SAFETY: `mmap` com endereço nulo, `len > 0`, só leitura e compartilhado; o kernel escolhe o
        // endereço e o resultado é conferido contra MAP_FAILED. Não há referência para a memória.
        let ptr = unsafe { libc::mmap(std::ptr::null_mut(), len, libc::PROT_READ, libc::MAP_SHARED, fd, 0) };
        (ptr != libc::MAP_FAILED).then_some(Mapa { ptr, len })
    }
}

impl Drop for Mapa {
    fn drop(&mut self) {
        // SAFETY: `ptr` e `len` são exatamente os de um `mmap` bem-sucedido feito em `nova`, e este é
        // o único dono do mapeamento (nada o copia nem o devolve).
        unsafe {
            libc::munmap(self.ptr, self.len);
        }
    }
}

/// `DMA_BUF_IOCTL_SYNC` (`_IOW('b', 0, __u64)`): início/fim de acesso da CPU, para invalidar caches.
const DMA_BUF_IOCTL_SYNC: u64 = 0x4008_6200;
const SYNC_READ: u64 = 1;
const SYNC_START: u64 = 0;
const SYNC_END: u64 = 4;

/// Aberta no início da leitura da CPU e fechada ao soltar. Descritores sem o ioctl (arquivo comum nos
/// testes) recusam com ENOTTY e a falha é ignorada.
struct Sincronia(RawFd);

impl Sincronia {
    fn ioctl(fd: RawFd, flags: u64) {
        // SAFETY: `fd` é um descritor aberto emprestado de um `Dmabuf` que vive mais que esta chamada; o
        // argumento é um ponteiro para um `u64` local, que é o que o ioctl lê.
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

/// Espera a GPU terminar de escrever no buffer (`poll(POLLIN)` no dmabuf, fence de escrita).
fn espera_escrita(fd: RawFd) {
    let mut p = libc::pollfd { fd, events: libc::POLLIN, revents: 0 };
    // SAFETY: `p` é um único `pollfd` válido na pilha, `nfds` = 1.
    unsafe {
        libc::poll(&mut p, 1, ESPERA_GPU_MS);
    }
}

/// Chama `f(ponteiro, tamanho do mapeamento, dados)` com o buffer mapeado para leitura, no mesmo formato
/// de um pool `wl_shm` (`ponteiro` é o início do descritor; `dados.offset` é onde o buffer começa).
/// `None` se o descritor não pôde ser lido (o que vira erro do cliente, nunca queda do `psd`).
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

/// O descritor pode ser mapeado? Confirmado na criação do buffer: mapeamento que falha vira erro do cliente.
pub(crate) fn mapeavel(d: &Dmabuf) -> bool {
    com_pixels(d, |_, _, _| ()).is_some()
}

#[cfg(test)]
mod tests {
    use super::*;
    use smithay::backend::allocator::dmabuf::DmabufFlags;
    use std::io::Write;
    use std::os::fd::OwnedFd;

    /// Arquivo comum fazendo as vezes de dmabuf: tem `lseek` e `mmap`, e `poll`/`ioctl` não atrapalham.
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
        // R, G, B, A -> B, G, R, A em cada pixel; sobra de 3 bytes no fim intocada.
        let mut v = vec![1, 2, 3, 4, 10, 20, 30, 40, 9, 9, 9];
        troca_r_b(&mut v);
        assert_eq!(v, vec![3, 2, 1, 4, 30, 20, 10, 40, 9, 9, 9]);
        // Idempotente em dobro.
        troca_r_b(&mut v);
        assert_eq!(v, vec![1, 2, 3, 4, 10, 20, 30, 40, 9, 9, 9]);
        troca_r_b(&mut []);
    }

    #[test]
    fn recusa_formato_e_modificador_nao_anunciados() {
        let d = dmabuf(arquivo(&[0; 4096]), 16, 8, Fourcc::Rgb565, Modifier::Linear, 0, 64);
        assert_eq!(validar(&d), Err(Rejeicao::Formato));
        // BROADCOM_UIF (tiling do V3D): não dá para ler direto.
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
        // stride menor que largura * 4
        assert_eq!(validar(&g(16, 8, 60, 4096)), Err(Rejeicao::Dimensoes));
        // descritor menor que stride * altura
        assert_eq!(validar(&g(16, 8, 64, 64 * 8 - 1)), Err(Rejeicao::Dimensoes));
        assert_eq!(validar(&g(16, 8, 64, 64 * 8)), Ok(()));
        // offset conta
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
            // SAFETY: o teste é dono do mapeamento durante o callback e `len` bytes são legíveis.
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
        // `com_pixels` confere o tamanho de novo antes de mapear: devolve `None` em vez de arriscar SIGBUS.
        assert!(com_pixels(&d, |_, _, _| ()).is_none());
    }
}
