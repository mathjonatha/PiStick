//! Codificador H.264 do Raspberry Pi 4 pela API V4L2 de memória para memória com plano múltiplo
//! (`/dev/video11`, driver `bcm2835-codec`). Só Linux.
//!
//! Entrada: `AB24` (o BGRX do protocolo, sem conversão de cor na CPU; veja `PIX_FMT_INPUT`) na fila `OUTPUT_MPLANE`. Saída:
//! H.264 Annex B na fila `CAPTURE_MPLANE`. Os buffers são mapeados com `mmap`.
//!
//! **Todo o `unsafe` do servidor para V4L2 está neste arquivo**: as chamadas `ioctl`, `mmap`,
//! `munmap` e `poll`, cada uma com `// SAFETY:`. As estruturas do kernel são declaradas aqui (sem
//! `bindgen`) e os testes `size_of` e de números de `ioctl` as conferem com o cabeçalho do Pi
//! (`/usr/include/linux/videodev2.h`, aarch64).

use std::fs::OpenOptions;
use std::io;
use std::os::fd::{AsRawFd, OwnedFd, RawFd};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use super::{EncodedFrame, VideoConfig, VideoEncoder, VideoEncoderFactory};

/// Dispositivo do codificador H.264 do Pi 4.
pub const DEFAULT_DEVICE: &str = "/dev/video11";
/// Prazo de um quadro (entrada até a saída), como manda o trait.
pub const ENCODE_DEADLINE: Duration = Duration::from_millis(200);
/// Buffers em cada fila.
const BUFFERS: u32 = 3;

// ---------------------------------------------------------------- constantes do V4L2

const BUF_TYPE_CAPTURE_MPLANE: u32 = 9;
const BUF_TYPE_OUTPUT_MPLANE: u32 = 10;
const MEMORY_MMAP: u32 = 1;
const FIELD_NONE: u32 = 1;
const CAP_VIDEO_M2M_MPLANE: u32 = 0x0000_4000;
const CAP_STREAMING: u32 = 0x0400_0000;
const BUF_FLAG_KEYFRAME: u32 = 0x8;
const BUF_FLAG_LAST: u32 = 0x0010_0000;

const fn fourcc(a: u8, b: u8, c: u8, d: u8) -> u32 {
    a as u32 | (b as u32) << 8 | (c as u32) << 16 | (d as u32) << 24
}
/// Formato de entrada. O driver lista `BGR4` ("32-bit BGRA/X") e `AB24` ("32-bit RGBA"), mas o
/// firmware do Pi 4 interpreta os dois com os nomes trocados em relação à memória do BGRX do
/// protocolo (B, G, R, X): com `BGR4` as cores saem com vermelho e azul trocados (medido nas amostras
/// de barras de cor), então a entrada usa `AB24`.
const PIX_FMT_INPUT: u32 = fourcc(b'A', b'B', b'2', b'4');
const PIX_FMT_H264: u32 = fourcc(b'H', b'2', b'6', b'4');

pub const COLORSPACE_REC709: u32 = 3;
pub const YCBCR_ENC_709: u32 = 2;
pub const QUANTIZATION_LIM_RANGE: u32 = 2;
pub const XFER_FUNC_709: u32 = 1;

const CID_BITRATE_MODE: u32 = 0x0099_09ce;
const CID_BITRATE: u32 = 0x0099_09cf;
const CID_H264_I_PERIOD: u32 = 0x0099_0a66;
const CID_H264_LEVEL: u32 = 0x0099_0a67;
const CID_H264_PROFILE: u32 = 0x0099_0a6b;
const CID_H264_MIN_QP: u32 = 0x0099_0a61;
const CID_H264_MAX_QP: u32 = 0x0099_0a62;
const BITRATE_MODE_VBR: i32 = 0;
const H264_PROFILE_HIGH: i32 = 4;
const H264_LEVEL_4_0: i32 = 11;

// ---------------------------------------------------------------- estruturas do kernel (aarch64)

#[repr(C)]
struct Capability {
    driver: [u8; 16],
    card: [u8; 32],
    bus_info: [u8; 32],
    version: u32,
    capabilities: u32,
    device_caps: u32,
    reserved: [u32; 3],
}

/// `struct v4l2_format`. A união do kernel (200 bytes, alinhada em 8 por causa de ponteiros em
/// `v4l2_window`) é tratada como bytes; os campos de `v4l2_pix_format_mplane` são lidos e escritos por
/// deslocamento (veja `PIX_*`), o que dispensa `unsafe` para acessar a união.
#[repr(C)]
struct Format {
    type_: u32,
    _pad: u32,
    fmt: [u8; 200],
}

// Deslocamentos dentro de `Format::fmt` para `struct v4l2_pix_format_mplane` (empacotada).
const PIX_WIDTH: usize = 0;
const PIX_HEIGHT: usize = 4;
const PIX_PIXELFORMAT: usize = 8;
const PIX_FIELD: usize = 12;
const PIX_COLORSPACE: usize = 16;
const PIX_PLANE0_SIZEIMAGE: usize = 20;
const PIX_PLANE0_BYTESPERLINE: usize = 24;
const PIX_NUM_PLANES: usize = 180;
const PIX_YCBCR_ENC: usize = 182;
const PIX_QUANTIZATION: usize = 183;
const PIX_XFER_FUNC: usize = 184;

impl Format {
    fn new(type_: u32) -> Self {
        Format { type_, _pad: 0, fmt: [0; 200] }
    }
    fn u32_at(&self, off: usize) -> u32 {
        u32::from_ne_bytes([self.fmt[off], self.fmt[off + 1], self.fmt[off + 2], self.fmt[off + 3]])
    }
    fn set_u32(&mut self, off: usize, v: u32) {
        self.fmt[off..off + 4].copy_from_slice(&v.to_ne_bytes());
    }
}

#[repr(C)]
struct RequestBuffers {
    count: u32,
    type_: u32,
    memory: u32,
    capabilities: u32,
    flags: u8,
    reserved: [u8; 3],
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct Plane {
    bytesused: u32,
    length: u32,
    /// `union { mem_offset; userptr; fd }`: 8 bytes em 64 bits.
    m: u64,
    data_offset: u32,
    reserved: [u32; 11],
}

#[repr(C)]
struct Buffer {
    index: u32,
    type_: u32,
    bytesused: u32,
    flags: u32,
    field: u32,
    _pad: u32,
    tv_sec: i64,
    tv_usec: i64,
    timecode: [u8; 16],
    sequence: u32,
    memory: u32,
    /// `union { offset; userptr; planes; fd }`: aqui, o ponteiro para o vetor de `Plane`.
    m: u64,
    length: u32,
    reserved2: u32,
    request_fd: u32,
    _pad2: u32,
}

#[repr(C)]
struct Control {
    id: u32,
    value: i32,
}

#[repr(C)]
struct StreamParm {
    type_: u32,
    parm: [u8; 200],
}

// ---------------------------------------------------------------- ioctl

const IOC_WRITE: u64 = 1;
const IOC_READ: u64 = 2;

/// `_IOC(dir, 'V', nr, size)` da convenção genérica (arm, aarch64, x86).
const fn ioc(dir: u64, nr: u64, size: usize) -> u64 {
    (dir << 30) | ((size as u64) << 16) | ((b'V' as u64) << 8) | nr
}

const VIDIOC_QUERYCAP: u64 = ioc(IOC_READ, 0, std::mem::size_of::<Capability>());
const VIDIOC_S_FMT: u64 = ioc(IOC_READ | IOC_WRITE, 5, std::mem::size_of::<Format>());
const VIDIOC_REQBUFS: u64 = ioc(IOC_READ | IOC_WRITE, 8, std::mem::size_of::<RequestBuffers>());
const VIDIOC_QUERYBUF: u64 = ioc(IOC_READ | IOC_WRITE, 9, std::mem::size_of::<Buffer>());
const VIDIOC_QBUF: u64 = ioc(IOC_READ | IOC_WRITE, 15, std::mem::size_of::<Buffer>());
const VIDIOC_DQBUF: u64 = ioc(IOC_READ | IOC_WRITE, 17, std::mem::size_of::<Buffer>());
const VIDIOC_STREAMON: u64 = ioc(IOC_WRITE, 18, std::mem::size_of::<i32>());
const VIDIOC_STREAMOFF: u64 = ioc(IOC_WRITE, 19, std::mem::size_of::<i32>());
const VIDIOC_S_PARM: u64 = ioc(IOC_READ | IOC_WRITE, 22, std::mem::size_of::<StreamParm>());
const VIDIOC_S_CTRL: u64 = ioc(IOC_READ | IOC_WRITE, 28, std::mem::size_of::<Control>());
const VIDIOC_G_CTRL: u64 = ioc(IOC_READ | IOC_WRITE, 27, std::mem::size_of::<Control>());

/// Chama `ioctl(fd, req, arg)`. O tamanho de `T` já está codificado em `req` (as constantes acima o
/// derivam de `size_of`), e os testes comparam cada número com o do cabeçalho do Pi.
fn ioctl<T>(fd: RawFd, req: u64, arg: &mut T) -> io::Result<()> {
    loop {
        // SAFETY: `fd` é um descritor aberto de que o chamador é dono; `arg` é uma referência exclusiva
        // a um `T` `repr(C)` que o kernel lê e escreve no máximo `size_of::<T>()` bytes (o `req` leva
        // esse tamanho); os ponteiros dentro de `T` (`Buffer::m` para `Plane`) apontam para memória
        // viva do chamador durante a chamada.
        let r = unsafe { libc::ioctl(fd, req as libc::Ioctl, arg as *mut T) };
        if r >= 0 {
            return Ok(());
        }
        let e = io::Error::last_os_error();
        if e.kind() != io::ErrorKind::Interrupted {
            return Err(e);
        }
    }
}

/// Espera `fd` ficar legível (um quadro codificado pronto) por até `timeout`. `false` = prazo.
fn poll_readable(fd: RawFd, timeout: Duration) -> io::Result<bool> {
    let mut p = libc::pollfd { fd, events: libc::POLLIN, revents: 0 };
    let ms = timeout.as_millis().min(i32::MAX as u128) as libc::c_int;
    // SAFETY: `p` é um único `pollfd` válido na pilha e `nfds` é 1.
    let r = unsafe { libc::poll(&mut p, 1, ms) };
    if r < 0 {
        let e = io::Error::last_os_error();
        return if e.kind() == io::ErrorKind::Interrupted { Ok(false) } else { Err(e) };
    }
    Ok(r > 0 && p.revents & (libc::POLLIN | libc::POLLERR) != 0)
}

/// Um buffer do driver mapeado na memória do processo.
struct Mapping {
    ptr: usize,
    len: usize,
}

impl Mapping {
    fn new(fd: RawFd, len: usize, offset: u32) -> io::Result<Mapping> {
        // SAFETY: `mmap` com endereço nulo (o kernel escolhe) numa região de `len` bytes do buffer do
        // driver que ele mesmo informou em `QUERYBUF` (`length` e `mem_offset`); o resultado é conferido.
        let p = unsafe { libc::mmap(std::ptr::null_mut(), len, libc::PROT_READ | libc::PROT_WRITE, libc::MAP_SHARED, fd, offset as libc::off_t) };
        if p == libc::MAP_FAILED {
            return Err(io::Error::last_os_error());
        }
        Ok(Mapping { ptr: p as usize, len })
    }

    fn as_slice(&self) -> &[u8] {
        // SAFETY: `ptr` e `len` vêm de um `mmap` bem-sucedido que só é desfeito no `Drop`. O buffer só
        // é lido depois do `DQBUF`, quando o driver já o devolveu ao programa.
        unsafe { std::slice::from_raw_parts(self.ptr as *const u8, self.len) }
    }

    fn as_mut_slice(&mut self) -> &mut [u8] {
        // SAFETY: como em `as_slice`; `&mut self` garante acesso exclusivo deste lado, e o buffer
        // mapeado de saída está com o programa (não enfileirado) quando é preenchido.
        unsafe { std::slice::from_raw_parts_mut(self.ptr as *mut u8, self.len) }
    }
}

impl Drop for Mapping {
    fn drop(&mut self) {
        // SAFETY: `ptr` e `len` são exatamente os de um `mmap` que ainda não foi desfeito; nenhuma
        // referência a ele sobrevive, pois `as_slice` e `as_mut_slice` emprestam de `self`.
        unsafe {
            libc::munmap(self.ptr as *mut libc::c_void, self.len);
        }
    }
}

// ---------------------------------------------------------------- o codificador

/// O que o driver aceitou na fila de entrada (para relatórios e testes).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Negotiated {
    pub width: u32,
    pub height: u32,
    /// Bytes por linha do buffer de entrada.
    pub bytes_per_line: u32,
    /// Tamanho do buffer de entrada.
    pub size_image: u32,
    pub colorspace: u32,
    pub ycbcr_enc: u32,
    pub quantization: u32,
    pub xfer_func: u32,
    /// Tamanho do buffer de saída (H.264).
    pub capture_size: u32,
}

/// Contadores para medir o atraso do driver.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Counters {
    pub frames_in: u64,
    pub frames_out: u64,
}

pub struct V4l2Encoder {
    width: u32,
    height: u32,
    stride: usize,
    size_image: u32,
    out_bufs: Vec<Mapping>,
    cap_bufs: Vec<Mapping>,
    /// Buffers de entrada que o programa pode preencher.
    out_free: Vec<u32>,
    first: bool,
    negotiated: Negotiated,
    counters: Counters,
    /// Controles aplicados e lidos de volta na abertura (linha de log).
    description: String,
    /// Tem de ser o último campo: os campos são soltos na ordem de declaração, e os `Mapping` fazem `munmap` antes de o descritor fechar.
    fd: OwnedFd,
}

/// Tamanho mínimo, em bytes, que um buffer de entrada mapeado precisa ter para receber `height` linhas
/// de `width` pixels BGRX com `stride` bytes por linha (a última linha não precisa do preenchimento).
fn min_input_len(width: u32, height: u32, stride: u32) -> usize {
    (stride as usize) * (height as usize).saturating_sub(1) + width as usize * 4
}

fn io_err(msg: &str, e: io::Error) -> io::Error {
    io::Error::new(e.kind(), format!("{msg}: {e}"))
}

impl V4l2Encoder {
    pub fn open(path: &Path, cfg: &VideoConfig) -> io::Result<V4l2Encoder> {
        if cfg.width < 32 || cfg.height < 32 || !cfg.width.is_multiple_of(2) || !cfg.height.is_multiple_of(2) {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "o tamanho da região tem de ser par e de pelo menos 32x32"));
        }
        let file = OpenOptions::new().read(true).write(true).custom_flags(libc::O_NONBLOCK).open(path).map_err(|e| io_err(&format!("abrir {}", path.display()), e))?;
        let fd = OwnedFd::from(file);
        let raw = fd.as_raw_fd();

        let mut cap = Capability { driver: [0; 16], card: [0; 32], bus_info: [0; 32], version: 0, capabilities: 0, device_caps: 0, reserved: [0; 3] };
        ioctl(raw, VIDIOC_QUERYCAP, &mut cap).map_err(|e| io_err("QUERYCAP", e))?;
        let caps = if cap.device_caps != 0 { cap.device_caps } else { cap.capabilities };
        if caps & CAP_VIDEO_M2M_MPLANE == 0 || caps & CAP_STREAMING == 0 {
            return Err(io::Error::new(io::ErrorKind::Unsupported, "o dispositivo não é um codec M2M de plano múltiplo com streaming"));
        }

        // Entrada: BGR4, BT.709 em faixa limitada (o que o driver aceitar fica em `negotiated`).
        let mut f = Format::new(BUF_TYPE_OUTPUT_MPLANE);
        f.set_u32(PIX_WIDTH, cfg.width);
        f.set_u32(PIX_HEIGHT, cfg.height);
        f.set_u32(PIX_PIXELFORMAT, PIX_FMT_INPUT);
        f.set_u32(PIX_FIELD, FIELD_NONE);
        f.set_u32(PIX_COLORSPACE, COLORSPACE_REC709);
        f.fmt[PIX_NUM_PLANES] = 1;
        f.fmt[PIX_YCBCR_ENC] = YCBCR_ENC_709 as u8;
        f.fmt[PIX_QUANTIZATION] = QUANTIZATION_LIM_RANGE as u8;
        f.fmt[PIX_XFER_FUNC] = XFER_FUNC_709 as u8;
        f.set_u32(PIX_PLANE0_BYTESPERLINE, cfg.width * 4);
        ioctl(raw, VIDIOC_S_FMT, &mut f).map_err(|e| io_err("S_FMT da entrada", e))?;
        if f.u32_at(PIX_PIXELFORMAT) != PIX_FMT_INPUT {
            return Err(io::Error::new(io::ErrorKind::Unsupported, "o driver não aceitou AB24 na entrada"));
        }
        let (width, height) = (f.u32_at(PIX_WIDTH), f.u32_at(PIX_HEIGHT));
        let stride = f.u32_at(PIX_PLANE0_BYTESPERLINE);
        let size_image = f.u32_at(PIX_PLANE0_SIZEIMAGE);
        if width != cfg.width || height != cfg.height || (stride as usize) < cfg.width as usize * 4 {
            return Err(io::Error::new(io::ErrorKind::Unsupported, format!("o driver mudou o tamanho da entrada para {width}x{height}, {stride} bytes por linha")));
        }
        let mut negotiated = Negotiated {
            width,
            height,
            bytes_per_line: stride,
            size_image,
            colorspace: f.u32_at(PIX_COLORSPACE),
            ycbcr_enc: u32::from(f.fmt[PIX_YCBCR_ENC]),
            quantization: u32::from(f.fmt[PIX_QUANTIZATION]),
            xfer_func: u32::from(f.fmt[PIX_XFER_FUNC]),
            capture_size: 0,
        };

        // Saída: H.264.
        let mut c = Format::new(BUF_TYPE_CAPTURE_MPLANE);
        c.set_u32(PIX_WIDTH, cfg.width);
        c.set_u32(PIX_HEIGHT, cfg.height);
        c.set_u32(PIX_PIXELFORMAT, PIX_FMT_H264);
        c.set_u32(PIX_FIELD, FIELD_NONE);
        c.fmt[PIX_NUM_PLANES] = 1;
        c.set_u32(PIX_PLANE0_SIZEIMAGE, 1 << 20);
        ioctl(raw, VIDIOC_S_FMT, &mut c).map_err(|e| io_err("S_FMT da saída", e))?;
        if c.u32_at(PIX_PIXELFORMAT) != PIX_FMT_H264 {
            return Err(io::Error::new(io::ErrorKind::Unsupported, "o driver não aceitou H.264 na saída"));
        }
        negotiated.capture_size = c.u32_at(PIX_PLANE0_SIZEIMAGE);

        // Controles. A taxa de bits é obrigatória; os outros só afinam o fluxo e o driver pode recusar.
        let bitrate = cfg.bitrate.clamp(25_000, 25_000_000) as i32;
        set_ctrl(raw, CID_BITRATE, bitrate).map_err(|e| io_err("taxa de bits", e))?;
        let mut applied = vec![Applied { name: "taxa", cid: CID_BITRATE, wanted: bitrate, result: Ok(()) }];
        for (name, cid, wanted) in [("modo", CID_BITRATE_MODE, BITRATE_MODE_VBR), ("perfil", CID_H264_PROFILE, H264_PROFILE_HIGH), ("nível", CID_H264_LEVEL, H264_LEVEL_4_0)] {
            applied.push(Applied { name, cid, wanted, result: set_ctrl(raw, cid, wanted) });
        }
        // Sem quadros-chave periódicos: o período de quadros I vai ao máximo que o controle aceitar.
        let mut i_period = Applied { name: "i_period", cid: CID_H264_I_PERIOD, wanted: i32::MAX, result: Err(io::Error::other("nenhum valor aceito")) };
        for p in [i32::MAX, 1 << 24, 1 << 20, 60_000] {
            i_period.wanted = p;
            i_period.result = set_ctrl(raw, CID_H264_I_PERIOD, p);
            if i_period.result.is_ok() {
                break;
            }
        }
        applied.push(i_period);
        let max_qp = cfg.max_qp.clamp(1, 51) as i32;
        applied.push(Applied { name: "qp_max", cid: CID_H264_MAX_QP, wanted: max_qp, result: set_ctrl(raw, CID_H264_MAX_QP, max_qp) });
        // Quadros por segundo (afina o controle de taxa); opcional.
        let mut parm = StreamParm { type_: BUF_TYPE_OUTPUT_MPLANE, parm: [0; 200] };
        parm.parm[8..12].copy_from_slice(&1u32.to_ne_bytes()); // timeperframe.numerator
        parm.parm[12..16].copy_from_slice(&cfg.fps.max(1).to_ne_bytes()); // timeperframe.denominator
        let _ = ioctl(raw, VIDIOC_S_PARM, &mut parm);

        let out_bufs = request_and_map(raw, BUF_TYPE_OUTPUT_MPLANE)?;
        let cap_bufs = request_and_map(raw, BUF_TYPE_CAPTURE_MPLANE)?;
        if out_bufs.is_empty() || cap_bufs.is_empty() {
            return Err(io::Error::new(io::ErrorKind::Unsupported, "o driver não concedeu buffers em uma das filas"));
        }
        // O `encode` copia linha a linha e informa `size_image` no `QBUF`: o buffer mapeado precisa comportar os dois.
        let need = min_input_len(width, height, stride).max(size_image as usize);
        if let Some(m) = out_bufs.iter().find(|m| m.len < need) {
            return Err(io::Error::new(io::ErrorKind::InvalidData, format!("buffer de entrada mapeado com {} bytes; o necessário é {need} ({width}x{height}, {stride} bytes por linha, size_image {size_image})", m.len)));
        }
        // Os buffers de saída começam com o driver, prontos para receber quadros codificados.
        for i in 0..cap_bufs.len() as u32 {
            queue(raw, BUF_TYPE_CAPTURE_MPLANE, i, 0)?;
        }
        for t in [BUF_TYPE_OUTPUT_MPLANE, BUF_TYPE_CAPTURE_MPLANE] {
            let mut ty = t as i32;
            ioctl(raw, VIDIOC_STREAMON, &mut ty).map_err(|e| io_err("STREAMON", e))?;
        }
        let out_free = (0..out_bufs.len() as u32).rev().collect();
        let description = describe_controls(raw, cfg, &applied);
        Ok(V4l2Encoder { width, height, stride: stride as usize, size_image, out_bufs, cap_bufs, out_free, first: true, negotiated, counters: Counters::default(), description, fd })
    }

    /// O que o driver aceitou na entrada.
    pub fn negotiated(&self) -> Negotiated {
        self.negotiated
    }

    pub fn counters(&self) -> Counters {
        self.counters
    }

    /// Recolhe os buffers de entrada que o driver já consumiu (sem bloquear).
    fn reclaim_inputs(&mut self) -> io::Result<()> {
        while let Some(index) = dequeue(self.fd.as_raw_fd(), BUF_TYPE_OUTPUT_MPLANE)? {
            self.out_free.push(index.0);
        }
        Ok(())
    }
}

/// Um controle aplicado na abertura: o que se pediu e o que o driver respondeu.
struct Applied {
    name: &'static str,
    cid: u32,
    wanted: i32,
    result: io::Result<()>,
}

fn get_ctrl(fd: RawFd, id: u32) -> io::Result<i32> {
    let mut c = Control { id, value: 0 };
    ioctl(fd, VIDIOC_G_CTRL, &mut c)?;
    Ok(c.value)
}

/// Texto legível do valor de um controle lido de volta.
fn ctrl_text(name: &str, v: i32) -> String {
    match name {
        "modo" => match v {
            0 => "VBR".into(),
            1 => "CBR".into(),
            o => o.to_string(),
        },
        "perfil" => match v {
            0 => "Baseline".into(),
            1 => "ConstrainedBaseline".into(),
            2 => "Main".into(),
            4 => "High".into(),
            o => o.to_string(),
        },
        "nível" => match v {
            0 => "1.0".into(),
            1 => "1b".into(),
            2..=4 => format!("1.{}", v - 1),
            5..=7 => format!("2.{}", v - 5),
            8..=10 => format!("3.{}", v - 8),
            11..=13 => format!("4.{}", v - 11),
            14 | 15 => format!("5.{}", v - 14),
            o => o.to_string(),
        },
        _ => v.to_string(),
    }
}

/// Linha de log com os controles lidos de volta (`VIDIOC_G_CTRL`) depois de aplicados. Marca com `!` o
/// que o driver recusou ou ajustou (valor lido diferente do pedido).
fn describe_controls(fd: RawFd, cfg: &VideoConfig, applied: &[Applied]) -> String {
    let mut out = format!("codificador: {}x{}", cfg.width, cfg.height);
    for a in applied {
        let lido = get_ctrl(fd, a.cid);
        let valor = lido.as_ref().map_or("?".to_string(), |v| ctrl_text(a.name, *v));
        // O QP máximo aparece junto do mínimo: `qp=MIN..MAX`.
        if a.name == "qp_max" {
            let min = get_ctrl(fd, CID_H264_MIN_QP).map_or("?".to_string(), |v| v.to_string());
            out += &format!(" qp={min}..{valor}");
        } else {
            out += &format!(" {}={valor}", a.name);
        }
        if let Err(e) = &a.result {
            out += &format!("!recusado({e})");
        } else if lido.as_ref().is_ok_and(|v| *v != a.wanted) {
            out += &format!("!pedido={}", ctrl_text(a.name, a.wanted));
        }
    }
    out
}

fn set_ctrl(fd: RawFd, id: u32, value: i32) -> io::Result<()> {
    let mut c = Control { id, value };
    ioctl(fd, VIDIOC_S_CTRL, &mut c)
}

/// `REQBUFS` + `QUERYBUF` + `mmap` de uma fila.
fn request_and_map(fd: RawFd, type_: u32) -> io::Result<Vec<Mapping>> {
    let mut rb = RequestBuffers { count: BUFFERS, type_, memory: MEMORY_MMAP, capabilities: 0, flags: 0, reserved: [0; 3] };
    ioctl(fd, VIDIOC_REQBUFS, &mut rb).map_err(|e| io_err("REQBUFS", e))?;
    if rb.count == 0 {
        return Err(io::Error::other("o driver não concedeu buffers"));
    }
    let mut maps = Vec::new();
    for index in 0..rb.count {
        let mut plane = Plane::default();
        let mut b = new_buffer(type_, index, &mut plane);
        ioctl(fd, VIDIOC_QUERYBUF, &mut b).map_err(|e| io_err("QUERYBUF", e))?;
        // `m` do plano é a união `mem_offset` (32 bits) para `V4L2_MEMORY_MMAP`.
        maps.push(Mapping::new(fd, plane.length as usize, plane.m as u32).map_err(|e| io_err("mmap", e))?);
    }
    Ok(maps)
}

/// `v4l2_buffer` de um plano. `plane` tem de viver até o fim do `ioctl` (o buffer guarda o endereço).
fn new_buffer(type_: u32, index: u32, plane: &mut Plane) -> Buffer {
    Buffer {
        index,
        type_,
        bytesused: 0,
        flags: 0,
        field: 0,
        _pad: 0,
        tv_sec: 0,
        tv_usec: 0,
        timecode: [0; 16],
        sequence: 0,
        memory: MEMORY_MMAP,
        m: plane as *mut Plane as u64,
        length: 1,
        reserved2: 0,
        request_fd: 0,
        _pad2: 0,
    }
}

/// `QBUF` com `bytesused` no plano 0.
fn queue(fd: RawFd, type_: u32, index: u32, bytesused: u32) -> io::Result<()> {
    let mut plane = Plane { bytesused, ..Plane::default() };
    let mut b = new_buffer(type_, index, &mut plane);
    ioctl(fd, VIDIOC_QBUF, &mut b).map_err(|e| io_err("QBUF", e))
}

/// Resultado de um `DQBUF`: índice, bytes usados, deslocamento dos dados e flags.
type Dequeued = (u32, u32, u32, u32);

/// `DQBUF` sem bloquear: `None` se não há buffer pronto.
fn dequeue(fd: RawFd, type_: u32) -> io::Result<Option<Dequeued>> {
    let mut plane = Plane::default();
    let mut b = new_buffer(type_, 0, &mut plane);
    match ioctl(fd, VIDIOC_DQBUF, &mut b) {
        Ok(()) => Ok(Some((b.index, plane.bytesused, plane.data_offset, b.flags))),
        Err(e) if e.kind() == io::ErrorKind::WouldBlock => Ok(None),
        Err(e) => Err(io_err("DQBUF", e)),
    }
}

impl VideoEncoder for V4l2Encoder {
    fn describe(&self) -> Option<String> {
        Some(self.description.clone())
    }

    fn encode(&mut self, pixels: &[u8], stride: usize) -> io::Result<EncodedFrame> {
        let started = Instant::now();
        let (w, h) = (self.width as usize, self.height as usize);
        if stride < w * 4 || pixels.len() < stride * (h - 1) + w * 4 {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "região menor que o declarado"));
        }
        let raw = self.fd.as_raw_fd();
        self.reclaim_inputs()?;
        let index = match self.out_free.pop() {
            Some(i) => i,
            None => {
                // Todos os buffers de entrada estão com o driver: espera algum voltar.
                let fim = started + ENCODE_DEADLINE;
                loop {
                    std::thread::sleep(Duration::from_millis(1));
                    self.reclaim_inputs()?;
                    if let Some(i) = self.out_free.pop() {
                        break i;
                    }
                    if Instant::now() >= fim {
                        return Err(io::Error::new(io::ErrorKind::TimedOut, "nenhum buffer de entrada livre"));
                    }
                }
            }
        };
        // Copia a região linha a linha para o buffer, respeitando os bytes por linha do driver.
        let dst = self.out_bufs[index as usize].as_mut_slice();
        let line = w * 4;
        for row in 0..h {
            dst[row * self.stride..row * self.stride + line].copy_from_slice(&pixels[row * stride..row * stride + line]);
        }
        queue(raw, BUF_TYPE_OUTPUT_MPLANE, index, self.size_image)?;
        self.counters.frames_in += 1;

        // Espera o quadro codificado.
        loop {
            let restante = ENCODE_DEADLINE.saturating_sub(started.elapsed());
            if let Some((i, used, offset, flags)) = dequeue(raw, BUF_TYPE_CAPTURE_MPLANE)? {
                let data = {
                    let buf = self.cap_bufs[i as usize].as_slice();
                    let (a, b) = (offset as usize, offset as usize + used as usize);
                    if b > buf.len() {
                        return Err(io::Error::other("o driver devolveu mais bytes do que o buffer tem"));
                    }
                    buf[a..b].to_vec()
                };
                queue(raw, BUF_TYPE_CAPTURE_MPLANE, i, 0)?;
                if used == 0 || flags & BUF_FLAG_LAST != 0 && data.is_empty() {
                    continue;
                }
                self.counters.frames_out += 1;
                let keyframe = flags & BUF_FLAG_KEYFRAME != 0;
                if self.first && !keyframe {
                    return Err(io::Error::other("o primeiro quadro codificado não é quadro-chave"));
                }
                self.first = false;
                return Ok(EncodedFrame { data, keyframe });
            }
            if restante.is_zero() {
                return Err(io::Error::new(io::ErrorKind::TimedOut, "o codificador não devolveu o quadro em 200 ms"));
            }
            poll_readable(raw, restante)?;
        }
    }
}

impl Drop for V4l2Encoder {
    fn drop(&mut self) {
        let raw = self.fd.as_raw_fd();
        for t in [BUF_TYPE_OUTPUT_MPLANE, BUF_TYPE_CAPTURE_MPLANE] {
            let mut ty = t as i32;
            let _ = ioctl(raw, VIDIOC_STREAMOFF, &mut ty);
        }
        // Depois deste `drop`, os campos são soltos na ordem de declaração: os `Mapping` fazem `munmap` e,
        // por último (`fd` é o último campo), o `OwnedFd` fecha o descritor.
    }
}

/// Fábrica do codificador do Pi.
pub struct V4l2Factory {
    device: PathBuf,
}

impl V4l2Factory {
    pub fn new(device: impl Into<PathBuf>) -> Self {
        V4l2Factory { device: device.into() }
    }

    /// O dispositivo padrão existe?
    pub fn default_available() -> bool {
        Path::new(DEFAULT_DEVICE).exists()
    }
}

impl Default for V4l2Factory {
    fn default() -> Self {
        V4l2Factory::new(DEFAULT_DEVICE)
    }
}

impl VideoEncoderFactory for V4l2Factory {
    fn open(&self, cfg: &VideoConfig) -> io::Result<Box<dyn VideoEncoder>> {
        Ok(Box::new(V4l2Encoder::open(&self.device, cfg)?))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::mem::size_of;

    /// Valores impressos por um programa em C compilado no Pi contra `/usr/include/linux/videodev2.h`
    /// (aarch64, Linux 6.18).
    #[test]
    fn tamanhos_das_estruturas_batem_com_o_cabecalho_do_pi() {
        assert_eq!(size_of::<Capability>(), 104);
        assert_eq!(size_of::<Format>(), 208);
        assert_eq!(size_of::<RequestBuffers>(), 20);
        assert_eq!(size_of::<Plane>(), 64);
        assert_eq!(size_of::<Buffer>(), 88);
        assert_eq!(size_of::<Control>(), 8);
        assert_eq!(size_of::<StreamParm>(), 204);
        // `v4l2_pix_format_mplane` (192) e `v4l2_plane_pix_format` (20) são acessadas por deslocamento:
        // 20 + 8 * 20 = 180 bytes até `num_planes`, e 192 no total.
        assert_eq!(PIX_NUM_PLANES, 20 + 8 * 20);
        assert_eq!(PIX_XFER_FUNC + 1 + 7, 192);
        assert_eq!(size_of::<Format>() - 8, 200);
    }

    #[test]
    fn numeros_dos_ioctl_batem_com_o_cabecalho_do_pi() {
        assert_eq!(VIDIOC_QUERYCAP, 0x8068_5600);
        assert_eq!(VIDIOC_S_FMT, 0xc0d0_5605);
        assert_eq!(VIDIOC_REQBUFS, 0xc014_5608);
        assert_eq!(VIDIOC_QUERYBUF, 0xc058_5609);
        assert_eq!(VIDIOC_QBUF, 0xc058_560f);
        assert_eq!(VIDIOC_DQBUF, 0xc058_5611);
        assert_eq!(VIDIOC_STREAMON, 0x4004_5612);
        assert_eq!(VIDIOC_STREAMOFF, 0x4004_5613);
        assert_eq!(VIDIOC_S_CTRL, 0xc008_561c);
        assert_eq!(VIDIOC_G_CTRL, 0xc008_561b);
        assert_eq!(VIDIOC_S_PARM, 0xc0cc_5616);
    }

    #[test]
    fn tamanho_minimo_do_buffer_de_entrada() {
        // A última linha dispensa o preenchimento do passo.
        assert_eq!(min_input_len(640, 384, 2560), 2560 * 383 + 640 * 4);
        assert_eq!(min_input_len(640, 384, 2688), 2688 * 383 + 2560);
        assert_eq!(min_input_len(32, 1, 4096), 128);
        assert_eq!(min_input_len(32, 0, 128), 128);
    }

    #[test]
    fn constantes_batem_com_o_cabecalho_do_pi() {
        assert_eq!(PIX_FMT_INPUT, 0x3432_4241);
        assert_eq!(PIX_FMT_H264, 0x3436_3248);
        assert_eq!((BUF_TYPE_OUTPUT_MPLANE, BUF_TYPE_CAPTURE_MPLANE, MEMORY_MMAP), (0xa, 0x9, 1));
        assert_eq!((CID_BITRATE, CID_BITRATE_MODE, CID_H264_PROFILE, CID_H264_LEVEL, CID_H264_I_PERIOD), (0x9909cf, 0x9909ce, 0x990a6b, 0x990a67, 0x990a66));
        assert_eq!((CID_H264_MIN_QP, CID_H264_MAX_QP), (0x990a61, 0x990a62));
        assert_eq!((ctrl_text("nível", 11), ctrl_text("perfil", 4), ctrl_text("modo", 0), ctrl_text("nível", 15)), ("4.0".into(), "High".into(), "VBR".into(), "5.1".into()));
        assert_eq!((H264_PROFILE_HIGH, H264_LEVEL_4_0, BITRATE_MODE_VBR), (4, 0xb, 0));
        assert_eq!((BUF_FLAG_KEYFRAME, BUF_FLAG_LAST), (0x8, 0x0010_0000));
        assert_eq!((CAP_VIDEO_M2M_MPLANE, CAP_STREAMING), (0x4000, 0x0400_0000));
        assert_eq!((COLORSPACE_REC709, YCBCR_ENC_709, QUANTIZATION_LIM_RANGE, XFER_FUNC_709, FIELD_NONE), (3, 2, 2, 1, 1));
    }
}
