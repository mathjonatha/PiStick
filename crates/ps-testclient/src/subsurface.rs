
use std::io::Write;
use std::os::fd::{AsFd, OwnedFd};
use std::os::unix::fs::FileExt;
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use wayland_client::globals::{registry_queue_init, GlobalList, GlobalListContents};
use wayland_client::protocol::wl_buffer::{self, WlBuffer};
use wayland_client::protocol::wl_compositor::WlCompositor;
use wayland_client::protocol::wl_registry::WlRegistry;
use wayland_client::protocol::wl_shm::{self, WlShm};
use wayland_client::protocol::wl_shm_pool::WlShmPool;
use wayland_client::protocol::wl_subcompositor::WlSubcompositor;
use wayland_client::protocol::wl_subsurface::WlSubsurface;
use wayland_client::protocol::wl_surface::WlSurface;
use wayland_client::{Connection, Dispatch, EventQueue, QueueHandle};
use wayland_protocols::wp::linux_dmabuf::zv1::client::zwp_linux_buffer_params_v1::{self, ZwpLinuxBufferParamsV1};
use wayland_protocols::wp::linux_dmabuf::zv1::client::zwp_linux_dmabuf_v1::{self, ZwpLinuxDmabufV1};
use wayland_protocols::wp::viewporter::client::wp_viewport::WpViewport;
use wayland_protocols::wp::viewporter::client::wp_viewporter::WpViewporter;
use wayland_protocols::xdg::shell::client::xdg_surface::{self, XdgSurface};
use wayland_protocols::xdg::shell::client::xdg_toplevel::{self, XdgToplevel};
use wayland_protocols::xdg::shell::client::xdg_wm_base::{self, XdgWmBase};

use crate::dmabuf::{Dmabuf, FormatoDmabuf};
use crate::pattern;

pub const COR_RAIZ: u32 = 0x2060A0;
pub const COR_SUB_A: u32 = 0xE0C020;
pub const COR_SUB_B: u32 = 0x20C040;
pub const COR_ESQUERDA: u32 = 0xFF0000;
pub const COR_DIREITA: u32 = 0x0000FF;
pub const COR_RAIZ_2: u32 = 0x80A040;

pub const RAIZ: (u32, u32) = (200, 120);
pub const SUB_POS: (i32, i32) = (50, 30);
pub const SUB_TAM: (u32, u32) = (60, 40);
pub const ORDEM_A: ((i32, i32), (u32, u32)) = ((20, 20), (100, 60));
pub const ORDEM_B: ((i32, i32), (u32, u32)) = ((60, 40), (100, 60));
pub const DMABUF_TAM: (u32, u32) = (320, 200);
pub const DMABUF_RAJADA: u32 = 40;
pub const ESTICADA_TAM: (u32, u32) = (100, 50);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Cenario {
    Dessincronizada,
    Sincronizada,
    Ordem,
    Remover,
    Dmabuf1x1,
    RaizEsticada,
    Fantasma,
}

pub struct SubOptions {
    pub socket: PathBuf,
    pub cenario: Cenario,
    pub passo: Duration,
    pub log: Option<PathBuf>,
}

impl Default for SubOptions {
    fn default() -> Self {
        SubOptions { socket: PathBuf::new(), cenario: Cenario::Dessincronizada, passo: Duration::from_millis(700), log: None }
    }
}

struct State {
    configured: bool,
    closed: bool,
}

struct Cliente {
    _conn: Connection,
    queue: EventQueue<State>,
    qh: QueueHandle<State>,
    st: State,
    compositor: WlCompositor,
    shm: WlShm,
    subcomp: WlSubcompositor,
    viewporter: Option<WpViewporter>,
    dmabuf: Option<ZwpLinuxDmabufV1>,
    raiz: WlSurface,
    buffers: Vec<WlBuffer>,
    dmabufs: Vec<(Dmabuf, WlBuffer)>,
    log: Option<std::fs::File>,
}

fn opcional<I: wayland_client::Proxy + 'static>(globals: &GlobalList, qh: &QueueHandle<State>, versoes: std::ops::RangeInclusive<u32>) -> Option<I>
where
    State: Dispatch<I, ()>,
{
    globals.bind::<I, _, _>(qh, versoes, ()).ok()
}

impl Cliente {
    fn novo(o: &SubOptions) -> Result<Cliente, String> {
        let stream = UnixStream::connect(&o.socket).map_err(|e| format!("conectar a {}: {e}", o.socket.display()))?;
        let conn = Connection::from_socket(stream).map_err(|e| e.to_string())?;
        let (globals, queue) = registry_queue_init::<State>(&conn).map_err(|e| e.to_string())?;
        let qh = queue.handle();
        let compositor: WlCompositor = globals.bind(&qh, 4..=6, ()).map_err(|e| format!("wl_compositor: {e}"))?;
        let shm: WlShm = globals.bind(&qh, 1..=1, ()).map_err(|e| format!("wl_shm: {e}"))?;
        let subcomp: WlSubcompositor = globals.bind(&qh, 1..=1, ()).map_err(|e| format!("wl_subcompositor: {e}"))?;
        let wm_base: XdgWmBase = globals.bind(&qh, 1..=6, ()).map_err(|e| format!("xdg_wm_base: {e}"))?;
        let viewporter = opcional::<WpViewporter>(&globals, &qh, 1..=1);
        let dmabuf = opcional::<ZwpLinuxDmabufV1>(&globals, &qh, 3..=3);
        let log = match &o.log {
            Some(p) => Some(std::fs::File::create(p).map_err(|e| format!("criar {}: {e}", p.display()))?),
            None => None,
        };
        let raiz = compositor.create_surface(&qh, ());
        let xs = wm_base.get_xdg_surface(&raiz, &qh, ());
        let top = xs.get_toplevel(&qh, ());
        top.set_title("sub-teste".into());
        raiz.commit();
        let mut c = Cliente { _conn: conn, queue, qh, st: State { configured: false, closed: false }, compositor, shm, subcomp, viewporter, dmabuf, raiz, buffers: Vec::new(), dmabufs: Vec::new(), log };
        let fim = Instant::now() + Duration::from_secs(5);
        while !c.st.configured {
            c.queue.roundtrip(&mut c.st).map_err(|e| e.to_string())?;
            if Instant::now() > fim {
                return Err("sem configure".into());
            }
        }
        Ok(c)
    }

    fn conn_flush(&self) -> Result<(), String> {
        self._conn.flush().map_err(|e| e.to_string())
    }

    fn sincroniza(&mut self) -> Result<(), String> {
        self.queue.roundtrip(&mut self.st).map(|_| ()).map_err(|e| format!("erro do compositor: {e}"))
    }

    fn nota(&mut self, linha: &str) -> Result<(), String> {
        self.sincroniza()?;
        if let Some(f) = &mut self.log {
            let _ = writeln!(f, "{linha}");
            let _ = f.flush();
        }
        Ok(())
    }

    fn dorme(&mut self, d: Duration) -> Result<(), String> {
        let fim = Instant::now() + d;
        while Instant::now() < fim && !self.st.closed {
            self.sincroniza()?;
            std::thread::sleep(Duration::from_millis(20));
        }
        Ok(())
    }

    fn shm_buffer(&mut self, w: u32, h: u32, fmt: wl_shm::Format, cor: impl Fn(u32, u32) -> u32) -> Result<WlBuffer, String> {
        let dir = std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from).unwrap_or_else(std::env::temp_dir);
        let file = tempfile::tempfile_in(dir).map_err(|e| e.to_string())?;
        let mut bytes = Vec::with_capacity((w * h * 4) as usize);
        for y in 0..h {
            for x in 0..w {
                bytes.extend_from_slice(&cor(x, y).to_le_bytes());
            }
        }
        file.write_all_at(&bytes, 0).map_err(|e| e.to_string())?;
        let fd: OwnedFd = file.into();
        let pool: WlShmPool = self.shm.create_pool(fd.as_fd(), bytes.len() as i32, &self.qh, ());
        let buffer = pool.create_buffer(0, w as i32, h as i32, (w * 4) as i32, fmt, &self.qh, ());
        pool.destroy();
        self.buffers.push(buffer.clone());
        Ok(buffer)
    }

    fn cor_solida(&mut self, w: u32, h: u32, rgb: u32) -> Result<WlBuffer, String> {
        self.shm_buffer(w, h, wl_shm::Format::Xrgb8888, move |_, _| 0xFF00_0000 | rgb)
    }

    fn dmabuf_novo(&mut self, w: u32, h: u32, formato: FormatoDmabuf) -> Result<usize, String> {
        let d = Dmabuf::aloca((w * h * 4) as usize)?;
        let dmabuf = self.dmabuf.as_ref().ok_or("sem zwp_linux_dmabuf_v1")?;
        let params = dmabuf.create_params(&self.qh, ());
        params.add(d.fd.as_fd(), 0, 0, w * 4, 0, 0);
        let buffer = params.create_immed(w as i32, h as i32, formato.fourcc(), zwp_linux_buffer_params_v1::Flags::empty(), &self.qh, ());
        params.destroy();
        self.dmabufs.push((d, buffer));
        Ok(self.dmabufs.len() - 1)
    }

    fn dmabuf_pinta(&mut self, i: usize, (w, h): (u32, u32), formato: FormatoDmabuf, n: u32) {
        let mut canvas = vec![0u32; (w * h) as usize];
        pattern::paint_full(&mut canvas, w, h, n);
        let bytes: Vec<u8> = canvas.iter().flat_map(|p| formato.bytes(*p)).collect();
        self.dmabufs[i].0.escreve(&bytes);
    }

    fn sub(&self, pos: (i32, i32)) -> (WlSurface, WlSubsurface) {
        let s = self.compositor.create_surface(&self.qh, ());
        let ss = self.subcomp.get_subsurface(&s, &self.raiz, &self.qh, ());
        ss.set_position(pos.0, pos.1);
        (s, ss)
    }

    fn pinta(s: &WlSurface, b: &WlBuffer, dano: Option<(i32, i32, i32, i32)>, tam: (u32, u32)) {
        s.attach(Some(b), 0, 0);
        let d = dano.unwrap_or((0, 0, tam.0 as i32, tam.1 as i32));
        s.damage_buffer(d.0, d.1, d.2, d.3);
        s.commit();
    }

    fn destino(&self, s: &WlSurface, w: i32, h: i32) -> Result<WpViewport, String> {
        let v = self.viewporter.as_ref().ok_or("sem wp_viewporter")?.get_viewport(s, &self.qh, ());
        v.set_destination(w, h);
        Ok(v)
    }
}

pub fn run_sub(o: SubOptions) -> Result<(), String> {
    let mut c = Cliente::novo(&o)?;
    let passo = o.passo;
    match o.cenario {
        Cenario::Dessincronizada | Cenario::Remover => {
            let a = c.cor_solida(RAIZ.0, RAIZ.1, COR_RAIZ)?;
            let (sub, ss) = c.sub(SUB_POS);
            ss.set_desync();
            let b = c.cor_solida(SUB_TAM.0, SUB_TAM.1, COR_SUB_A)?;
            Cliente::pinta(&sub, &b, None, SUB_TAM);
            let raiz = c.raiz.clone();
            Cliente::pinta(&raiz, &a, None, RAIZ);
            c.nota("pronto")?;
            c.dorme(passo)?;
            if o.cenario == Cenario::Dessincronizada {
                let b2 = c.cor_solida(SUB_TAM.0, SUB_TAM.1, COR_SUB_B)?;
                Cliente::pinta(&sub, &b2, Some((10, 10, 20, 20)), SUB_TAM);
                c.nota("dano")?;
            } else {
                ss.destroy();
                sub.destroy();
                c.raiz.commit();
                c.nota("removida")?;
            }
            c.dorme(passo * 3)?;
        }
        Cenario::Sincronizada => {
            let a = c.cor_solida(RAIZ.0, RAIZ.1, COR_RAIZ)?;
            let raiz = c.raiz.clone();
            Cliente::pinta(&raiz, &a, None, RAIZ);
            c.nota("raiz")?;
            c.dorme(passo)?;
            let (sub, _ss) = c.sub(SUB_POS);
            let b = c.cor_solida(SUB_TAM.0, SUB_TAM.1, COR_SUB_A)?;
            Cliente::pinta(&sub, &b, None, SUB_TAM);
            c.nota("sub-commit")?;
            c.dorme(passo * 2)?;
            c.raiz.commit();
            c.nota("pai-commit")?;
            c.dorme(passo * 3)?;
        }
        Cenario::Ordem => {
            let a = c.cor_solida(RAIZ.0, RAIZ.1, COR_RAIZ)?;
            let (sa, ssa) = c.sub(ORDEM_A.0);
            let ba = c.cor_solida(ORDEM_A.1 .0, ORDEM_A.1 .1, COR_SUB_A)?;
            Cliente::pinta(&sa, &ba, None, ORDEM_A.1);
            let (sb, _ssb) = c.sub(ORDEM_B.0);
            let bb = c.cor_solida(ORDEM_B.1 .0, ORDEM_B.1 .1, COR_SUB_B)?;
            Cliente::pinta(&sb, &bb, None, ORDEM_B.1);
            let raiz = c.raiz.clone();
            Cliente::pinta(&raiz, &a, None, RAIZ);
            c.nota("ordem0")?;
            c.dorme(passo * 2)?;
            ssa.place_above(&sb);
            c.raiz.commit();
            c.nota("ordem1")?;
            c.dorme(passo * 2)?;
            ssa.place_below(&sb);
            c.raiz.commit();
            c.nota("ordem2")?;
            c.dorme(passo * 3)?;
        }
        Cenario::Dmabuf1x1 => {
            let a = c.shm_buffer(1, 1, wl_shm::Format::Argb8888, |_, _| 0xFF00_0000)?;
            let _v = c.destino(&c.raiz.clone(), DMABUF_TAM.0 as i32, DMABUF_TAM.1 as i32)?;
            let raiz = c.raiz.clone();
            Cliente::pinta(&raiz, &a, None, (1, 1));
            let (sub, ss) = c.sub((0, 0));
            ss.set_desync();
            let formato = FormatoDmabuf::Abgr8888;
            let ids = [c.dmabuf_novo(DMABUF_TAM.0, DMABUF_TAM.1, formato)?, c.dmabuf_novo(DMABUF_TAM.0, DMABUF_TAM.1, formato)?, c.dmabuf_novo(DMABUF_TAM.0, DMABUF_TAM.1, formato)?];
            c.dmabuf_pinta(ids[0], DMABUF_TAM, formato, 0);
            let b0 = c.dmabufs[ids[0]].1.clone();
            Cliente::pinta(&sub, &b0, None, DMABUF_TAM);
            c.nota("pronto")?;
            c.dorme(passo * 2)?;
            for n in 1..=DMABUF_RAJADA {
                let i = ids[n as usize % 3];
                c.dmabuf_pinta(i, DMABUF_TAM, formato, n);
                let b = c.dmabufs[i].1.clone();
                Cliente::pinta(&sub, &b, None, DMABUF_TAM);
                c.conn_flush()?;
                std::thread::sleep(Duration::from_millis(3));
            }
            c.nota("rajada")?;
            c.dorme(passo * 3)?;
        }
        Cenario::RaizEsticada => {
            let a = c.shm_buffer(2, 1, wl_shm::Format::Xrgb8888, |x, _| 0xFF00_0000 | if x == 0 { COR_ESQUERDA } else { COR_DIREITA })?;
            let v = c.destino(&c.raiz.clone(), ESTICADA_TAM.0 as i32, ESTICADA_TAM.1 as i32)?;
            let raiz = c.raiz.clone();
            Cliente::pinta(&raiz, &a, None, (2, 1));
            c.nota("pronto")?;
            c.dorme(passo * 2)?;
            v.set_destination(ESTICADA_TAM.0 as i32 / 2, ESTICADA_TAM.1 as i32 / 2);
            c.raiz.commit();
            c.nota("menor")?;
            c.dorme(passo * 3)?;
        }
        Cenario::Fantasma => {
            let raiz = c.raiz.clone();
            let um = c.cor_solida(1, 1, 0x000000)?;
            Cliente::pinta(&raiz, &um, None, (1, 1));
            c.nota("1x1")?;
            c.dorme(passo)?;
            let a = c.cor_solida(RAIZ.0, RAIZ.1, COR_RAIZ)?;
            Cliente::pinta(&raiz, &a, None, RAIZ);
            c.nota("real")?;
            c.dorme(passo)?;
            let um = c.cor_solida(1, 1, 0x000000)?;
            Cliente::pinta(&raiz, &um, None, (1, 1));
            c.nota("1x1-de-novo")?;
            c.dorme(passo)?;
            let b = c.cor_solida(RAIZ.0, RAIZ.1, COR_RAIZ_2)?;
            Cliente::pinta(&raiz, &b, None, RAIZ);
            c.nota("real-2")?;
            c.dorme(passo * 3)?;
        }
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
sem_eventos!(WlCompositor, WlSurface, WlShm, WlShmPool, WlSubcompositor, WlSubsurface, WpViewporter, WpViewport, ZwpLinuxBufferParamsV1);

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
