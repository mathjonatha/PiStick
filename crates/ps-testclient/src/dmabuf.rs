
use std::fs::OpenOptions;
use std::os::fd::{AsFd, FromRawFd, OwnedFd};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use wayland_client::globals::{registry_queue_init, GlobalListContents};
use wayland_client::protocol::wl_buffer::{self, WlBuffer};
use wayland_client::protocol::wl_compositor::WlCompositor;
use wayland_client::protocol::wl_registry::WlRegistry;
use wayland_client::protocol::wl_surface::WlSurface;
use wayland_client::{Connection, Dispatch, QueueHandle};
use wayland_protocols::wp::linux_dmabuf::zv1::client::zwp_linux_buffer_params_v1::{self, ZwpLinuxBufferParamsV1};
use wayland_protocols::wp::linux_dmabuf::zv1::client::zwp_linux_dmabuf_v1::{self, ZwpLinuxDmabufV1};
use wayland_protocols::xdg::shell::client::xdg_surface::{self, XdgSurface};
use wayland_protocols::xdg::shell::client::xdg_toplevel::{self, XdgToplevel};
use wayland_protocols::xdg::shell::client::xdg_wm_base::{self, XdgWmBase};

use crate::pattern;

const XRGB8888: u32 = 0x3432_5258;
const ABGR8888: u32 = 0x3432_4241;
const XBGR8888: u32 = 0x3432_4258;
const RGB565: u32 = 0x3631_4752;
const MOD_UIF: u64 = 0x0700_0000_0000_0006;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Defeito {
    Nenhum,
    FormatoNaoAnunciado,
    ModificadorTiling,
    DoisPlanos,
    LarguraZero,
    LarguraAcimaDoLimite,
    DescritorCurto,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum FormatoDmabuf {
    #[default]
    Xrgb8888,
    Abgr8888,
    Xbgr8888,
}

impl FormatoDmabuf {
    pub(crate) fn fourcc(self) -> u32 {
        match self {
            FormatoDmabuf::Xrgb8888 => XRGB8888,
            FormatoDmabuf::Abgr8888 => ABGR8888,
            FormatoDmabuf::Xbgr8888 => XBGR8888,
        }
    }

    pub fn bytes(self, rgb: u32) -> [u8; 4] {
        let (r, g, b) = ((rgb >> 16) as u8, (rgb >> 8) as u8, rgb as u8);
        match self {
            FormatoDmabuf::Xrgb8888 => [b, g, r, 0],
            FormatoDmabuf::Abgr8888 => [r, g, b, 0xFF],
            FormatoDmabuf::Xbgr8888 => [r, g, b, 0],
        }
    }
}

pub struct DmabufOptions {
    pub socket: PathBuf,
    pub formato: FormatoDmabuf,
    pub width: u32,
    pub height: u32,
    pub defeito: Defeito,
    pub burst: u32,
    pub hold: Duration,
}

impl Default for DmabufOptions {
    fn default() -> Self {
        DmabufOptions { socket: PathBuf::new(), formato: FormatoDmabuf::Xrgb8888, width: 320, height: 200, defeito: Defeito::Nenhum, burst: 1, hold: Duration::from_secs(3) }
    }
}

pub(crate) struct Dmabuf {
    pub(crate) fd: OwnedFd,
    len: usize,
    map: *mut u8,
}

impl Dmabuf {
    pub(crate) fn aloca(len: usize) -> Result<Dmabuf, String> {
        #[repr(C)]
        struct Alloc {
            len: u64,
            fd: u32,
            fd_flags: u32,
            heap_flags: u64,
        }
        const DMA_HEAP_IOCTL_ALLOC: u64 = 0xC018_4800;
        let heap = OpenOptions::new().read(true).write(true).open("/dev/dma_heap/system").map_err(|e| format!("abrir /dev/dma_heap/system: {e}"))?;
        let mut a = Alloc { len: len as u64, fd: 0, fd_flags: (libc::O_RDWR | libc::O_CLOEXEC) as u32, heap_flags: 0 };
        let r = unsafe { libc::ioctl(std::os::fd::AsRawFd::as_raw_fd(&heap), DMA_HEAP_IOCTL_ALLOC as _, &mut a as *mut Alloc) };
        if r != 0 {
            return Err(format!("DMA_HEAP_IOCTL_ALLOC: {}", std::io::Error::last_os_error()));
        }
        let fd = unsafe { OwnedFd::from_raw_fd(a.fd as i32) };
        let map = unsafe { libc::mmap(std::ptr::null_mut(), len, libc::PROT_READ | libc::PROT_WRITE, libc::MAP_SHARED, std::os::fd::AsRawFd::as_raw_fd(&fd), 0) };
        if map == libc::MAP_FAILED {
            return Err(format!("mmap do dmabuf: {}", std::io::Error::last_os_error()));
        }
        Ok(Dmabuf { fd, len, map: map as *mut u8 })
    }

    pub(crate) fn escreve(&mut self, src: &[u8]) {
        assert!(src.len() <= self.len);
        unsafe { std::ptr::copy_nonoverlapping(src.as_ptr(), self.map, src.len()) };
    }
}

impl Drop for Dmabuf {
    fn drop(&mut self) {
        unsafe { libc::munmap(self.map as *mut _, self.len) };
    }
}

struct State {
    configured: bool,
    closed: bool,
}

pub fn run_dmabuf(o: DmabufOptions) -> Result<(), String> {
    let stream = UnixStream::connect(&o.socket).map_err(|e| format!("conectar a {}: {e}", o.socket.display()))?;
    let conn = Connection::from_socket(stream).map_err(|e| e.to_string())?;
    let (globals, mut queue) = registry_queue_init::<State>(&conn).map_err(|e| e.to_string())?;
    let qh = queue.handle();
    let compositor: WlCompositor = globals.bind(&qh, 4..=6, ()).map_err(|e| format!("wl_compositor: {e}"))?;
    let wm_base: XdgWmBase = globals.bind(&qh, 1..=6, ()).map_err(|e| format!("xdg_wm_base: {e}"))?;
    let dmabuf: ZwpLinuxDmabufV1 = globals.bind(&qh, 3..=3, ()).map_err(|e| format!("zwp_linux_dmabuf_v1: {e}"))?;
    let mut st = State { configured: false, closed: false };

    let surface = compositor.create_surface(&qh, ());
    let xs = wm_base.get_xdg_surface(&surface, &qh, ());
    let top = xs.get_toplevel(&qh, ());
    top.set_title("dmabuf-teste".into());
    surface.commit();
    let fim = Instant::now() + Duration::from_secs(5);
    while !st.configured {
        queue.roundtrip(&mut st).map_err(|e| e.to_string())?;
        if Instant::now() > fim {
            return Err("sem configure".into());
        }
    }

    let (w, h) = (o.width, o.height);
    let (stride, mut largura) = (w * 4, w as i32);
    let mut formato = o.formato.fourcc();
    let mut modificador = 0u64;
    let mut bytes = (stride * h) as usize;
    match o.defeito {
        Defeito::FormatoNaoAnunciado => formato = RGB565,
        Defeito::ModificadorTiling => modificador = MOD_UIF,
        Defeito::LarguraZero => largura = 0,
        Defeito::LarguraAcimaDoLimite => {
            largura = 4097;
        }
        Defeito::DescritorCurto => bytes = 4096,
        Defeito::Nenhum | Defeito::DoisPlanos => {}
    }
    let stride_envio = if o.defeito == Defeito::LarguraAcimaDoLimite { 4097 * 4 } else { stride };
    let nbuf = if o.defeito == Defeito::Nenhum { 3 } else { 1 };
    let mut bufs = Vec::new();
    for _ in 0..nbuf {
        let b = Dmabuf::aloca(bytes.max(4096))?;
        let params = dmabuf.create_params(&qh, ());
        params.add(b.fd.as_fd(), 0, 0, stride_envio, (modificador >> 32) as u32, modificador as u32);
        if o.defeito == Defeito::DoisPlanos {
            params.add(b.fd.as_fd(), 1, 0, stride_envio, 0, 0);
        }
        let buffer = params.create_immed(largura, h as i32, formato, zwp_linux_buffer_params_v1::Flags::empty(), &qh, ());
        params.destroy();
        bufs.push((b, buffer));
    }
    queue.roundtrip(&mut st).map_err(|e| format!("erro do compositor: {e}"))?;
    if o.defeito != Defeito::Nenhum {
        surface.attach(Some(&bufs[0].1), 0, 0);
        surface.commit();
        queue.roundtrip(&mut st).map_err(|e| format!("erro do compositor: {e}"))?;
        return Ok(());
    }
    let mut canvas = vec![0u32; (w * h) as usize];
    for n in 0..o.burst.max(1) {
        pattern::paint_full(&mut canvas, w, h, n);
        let i = n as usize % bufs.len();
        let bytes: Vec<u8> = canvas.iter().flat_map(|p| o.formato.bytes(*p)).collect();
        bufs[i].0.escreve(&bytes);
        surface.attach(Some(&bufs[i].1), 0, 0);
        surface.damage_buffer(0, 0, w as i32, h as i32);
        surface.commit();
        conn.flush().map_err(|e| e.to_string())?;
        std::thread::sleep(Duration::from_millis(3));
    }
    let fim = Instant::now() + o.hold;
    while Instant::now() < fim && !st.closed {
        queue.roundtrip(&mut st).map_err(|e| format!("erro do compositor: {e}"))?;
        std::thread::sleep(Duration::from_millis(20));
    }
    Ok(())
}

impl Dispatch<WlRegistry, GlobalListContents> for State {
    fn event(_: &mut Self, _: &WlRegistry, _: <WlRegistry as wayland_client::Proxy>::Event, _: &GlobalListContents, _: &Connection, _: &QueueHandle<Self>) {}
}

macro_rules! sem_eventos {
    ($($t:ty),*) => {$(
        impl Dispatch<$t, ()> for State {
            fn event(_: &mut Self, _: &$t, _: <$t as wayland_client::Proxy>::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
        }
    )*};
}
sem_eventos!(WlCompositor, WlSurface, ZwpLinuxBufferParamsV1);

impl Dispatch<ZwpLinuxDmabufV1, ()> for State {
    fn event(_: &mut Self, _: &ZwpLinuxDmabufV1, _: zwp_linux_dmabuf_v1::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
}

impl Dispatch<WlBuffer, ()> for State {
    fn event(_: &mut Self, _: &WlBuffer, _: wl_buffer::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
}

impl Dispatch<XdgWmBase, ()> for State {
    fn event(_: &mut Self, base: &XdgWmBase, ev: xdg_wm_base::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
        if let xdg_wm_base::Event::Ping { serial } = ev {
            base.pong(serial);
        }
    }
}

impl Dispatch<XdgSurface, ()> for State {
    fn event(st: &mut Self, xs: &XdgSurface, ev: xdg_surface::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
        if let xdg_surface::Event::Configure { serial } = ev {
            xs.ack_configure(serial);
            st.configured = true;
        }
    }
}

impl Dispatch<XdgToplevel, ()> for State {
    fn event(st: &mut Self, _: &XdgToplevel, ev: xdg_toplevel::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
        if let xdg_toplevel::Event::Close = ev {
            st.closed = true;
        }
    }
}
