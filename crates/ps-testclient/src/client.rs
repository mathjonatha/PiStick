//! Implementação do cliente: registro, shm, xdg-shell, teclado e ponteiro.

use std::io::Write;
use std::os::fd::OwnedFd;
use std::os::unix::fs::FileExt;
use std::os::unix::net::UnixStream;
use std::path::PathBuf;

use wayland_client::globals::{registry_queue_init, GlobalListContents};
use wayland_client::protocol::wl_buffer::{self, WlBuffer};
use wayland_client::protocol::wl_callback::{self, WlCallback};
use wayland_client::protocol::wl_compositor::WlCompositor;
use wayland_client::protocol::wl_data_device_manager::WlDataDeviceManager;
use wayland_client::protocol::wl_keyboard::{self, WlKeyboard};
use wayland_client::protocol::wl_pointer::{self, WlPointer};
use wayland_client::protocol::wl_registry::WlRegistry;
use wayland_protocols::wp::cursor_shape::v1::client::wp_cursor_shape_manager_v1::WpCursorShapeManagerV1;
use wayland_client::protocol::wl_seat::{self, WlSeat};
use wayland_client::protocol::wl_shm::{self, WlShm};
use wayland_client::protocol::wl_shm_pool::WlShmPool;
use wayland_client::protocol::wl_surface::WlSurface;
use wayland_client::{Connection, Dispatch, QueueHandle, WEnum};
use wayland_protocols::xdg::shell::client::xdg_surface::{self, XdgSurface};
use wayland_protocols::xdg::shell::client::xdg_toplevel::{self, XdgToplevel};
use wayland_protocols::xdg::shell::client::xdg_wm_base::{self, XdgWmBase};
use xkbcommon::xkb;

use crate::pattern;

mod clipboard;
mod kde;
mod cursor;
mod popup;

pub use cursor::{CursorMode, IMAGE_HOTSPOT, IMAGE_PIXEL, IMAGE_SIDE};

/// Opções do cliente de teste.
pub struct Options {
    /// Caminho do socket do compositor; `None` usa `$XDG_RUNTIME_DIR/$WAYLAND_DISPLAY`.
    pub socket: Option<PathBuf>,
    pub title: String,
    pub app_id: String,
    /// Arquivo onde registrar os eventos recebidos (um por linha).
    pub log: Option<PathBuf>,
    /// Tamanho usado se o compositor não sugerir nenhum.
    pub default_size: (u32, u32),
    /// Preenche com lixo tudo fora do dano declarado: quem copia mais que o dano é pego.
    pub poison: bool,
    /// Depois de tantos quadros de faixa desde a última pintura-base, para de desenhar
    /// (o conteúdo "assenta" e pode ser conferido exatamente). `u32::MAX` = sem limite.
    pub band_frames: u32,
    /// Incrementado a cada quadro desenhado (incluindo as pinturas-base).
    pub counter: Option<std::sync::Arc<std::sync::atomic::AtomicU32>>,
    /// Margem (em pixels) em volta da área útil, como a sombra de um app GTK: o buffer é maior que a
    /// geometria declarada com `set_window_geometry`, e a margem é pintada de magenta.
    pub margin: u32,
    /// `xdg_toplevel.set_min_size` / `set_max_size` (largura, altura), antes do primeiro commit.
    pub min_size: Option<(i32, i32)>,
    pub max_size: Option<(i32, i32)>,
    /// Ao receber a tecla "r", o app se redimensiona por conta própria para este tamanho
    /// (sem pedido do compositor), como um app que restaura o tamanho salvo.
    pub self_resize: Option<(u32, u32)>,
    /// Habilita as teclas de comando dos popups (veja `client/popup.rs`).
    pub popups: bool,
    /// Cursor definido quando o ponteiro entra na janela.
    pub cursor: Option<CursorMode>,
    /// Quantas vezes além da primeira o app repete o mesmo `set_cursor` ao entrar (como o Android faz).
    pub cursor_repeat: u32,
    /// Texto oferecido como seleção quando o app recebe a tecla "y". Toda seleção nova que o
    /// compositor apresenta é registrada (`selection-received`).
    pub clipboard: Option<String>,
    /// Pede a decoração pelo protocolo do KDE (o que o GTK 3 usa) e registra o modo recebido.
    pub kde_decoration: bool,
}

impl Default for Options {
    fn default() -> Self {
        Options { socket: None, title: "ps-testclient".into(), app_id: "ps-testclient".into(), log: None, default_size: (640, 400), poison: true, band_frames: u32::MAX, counter: None, margin: 0, min_size: None, max_size: None, self_resize: None, popups: false, cursor: None, cursor_repeat: 0, clipboard: None, kde_decoration: false }
    }
}

struct Buf {
    buffer: WlBuffer,
    busy: bool,
}

struct Surfaces {
    surface: WlSurface,
    xdg_surface: XdgSurface,
    toplevel: XdgToplevel,
}

struct State {
    opts_poison: bool,
    band_frames: u32,
    counter: Option<std::sync::Arc<std::sync::atomic::AtomicU32>>,
    margin: u32,
    self_resize: Option<(u32, u32)>,
    default_size: (u32, u32),
    log: Option<std::fs::File>,
    shm: WlShm,
    compositor: WlCompositor,
    wm_base: XdgWmBase,
    seat: WlSeat,
    popups: Vec<popup::PopupWin>,
    popups_enabled: bool,
    pointer: Option<WlPointer>,
    cursor_mode: Option<CursorMode>,
    cursor_repeat: u32,
    cursor_shape_mgr: Option<WpCursorShapeManagerV1>,
    cursor_objs: cursor::CursorObjs,
    clip: clipboard::ClipObjs,
    clipboard_text: Option<String>,
    /// Último serial recebido (entrada), usado no `xdg_popup.grab`.
    last_serial: u32,
    /// Superfície que tem o foco de teclado, para o registro.
    kbd_focus: String,
    surfaces: Option<Surfaces>,

    // tamanho atual e o sugerido pelo próximo configure
    size: (u32, u32),
    suggested: (u32, u32),
    configured: bool,
    closed: bool,

    // buffers
    pool: Option<WlShmPool>,
    file: Option<std::fs::File>,
    bufs: Vec<Buf>,
    canvas: Vec<u32>,
    /// quadro atual e quadro-base
    n: u32,
    m: u32,
    /// Há um quadro a desenhar mas nenhum buffer livre.
    want_draw: bool,
    /// Já existe um retorno de `frame` pedido.
    frame_pending: bool,

    // entrada
    xkb_ctx: xkb::Context,
    xkb_state: Option<xkb::State>,
}

impl State {
    fn log(&mut self, line: String) {
        if let Some(f) = &mut self.log {
            let _ = writeln!(f, "{line}");
            let _ = f.flush();
        }
    }

    /// Recria pool e buffers para o tamanho atual.
    fn make_buffers(&mut self, qh: &QueueHandle<State>) -> std::io::Result<()> {
        for b in self.bufs.drain(..) {
            b.buffer.destroy();
        }
        if let Some(p) = self.pool.take() {
            p.destroy();
        }
        let (w, h) = self.buffer_size();
        let one = (w * h * 4) as usize;
        let dir = std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from).unwrap_or_else(std::env::temp_dir);
        let file = tempfile::tempfile_in(dir)?;
        file.set_len(2 * one as u64)?;
        let fd: OwnedFd = file.try_clone()?.into();
        let pool = self.shm.create_pool(std::os::fd::AsFd::as_fd(&fd), (2 * one) as i32, qh, ());
        for i in 0..2usize {
            let buffer = pool.create_buffer((i * one) as i32, w as i32, h as i32, (w * 4) as i32, wl_shm::Format::Xrgb8888, qh, i);
            self.bufs.push(Buf { buffer, busy: false });
        }
        self.pool = Some(pool);
        self.file = Some(file);
        Ok(())
    }

    /// O app muda o próprio tamanho: buffers novos e uma pintura-base no tamanho novo.
    fn resize_by_app(&mut self, novo: (u32, u32), qh: &QueueHandle<State>) {
        if novo == self.size || !self.configured {
            return;
        }
        self.size = novo;
        self.suggested = novo;
        self.log(format!("resize-pelo-app {}x{}", novo.0, novo.1));
        self.canvas = vec![0; (novo.0 * novo.1) as usize];
        if self.make_buffers(qh).is_err() {
            self.closed = true;
            return;
        }
        self.n += 1;
        self.draw(qh, true);
    }

    /// Tamanho do buffer: a área útil mais a margem dos dois lados.
    fn buffer_size(&self) -> (u32, u32) {
        (self.size.0 + 2 * self.margin, self.size.1 + 2 * self.margin)
    }

    /// Desenha o próximo quadro e o envia. `full`: pintura-base inteira (começo ou redimensionamento).
    fn draw(&mut self, qh: &QueueHandle<State>, full: bool) {
        if self.closed || self.surfaces.is_none() || !self.configured {
            return;
        }
        let Some(idx) = self.bufs.iter().position(|b| !b.busy) else {
            self.want_draw = true;
            return;
        };
        self.want_draw = false;
        if !full && self.n - self.m >= self.band_frames {
            return;
        }
        if let Some(c) = &self.counter {
            c.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
        let (w, h) = self.size;
        let damage: Vec<(u32, u32, u32, u32)> = if full {
            self.m = self.n;
            pattern::paint_full(&mut self.canvas, w, h, self.m);
            vec![(0, 0, w, h)]
        } else {
            self.n += 1;
            pattern::paint_band(&mut self.canvas, w, h, self.n, self.m).to_vec()
        };

        // Conteúdo do buffer: o quadro inteiro; com `poison`, lixo fora do dano declarado. A margem,
        // quando há, é magenta e a área útil fica deslocada por ela.
        let m = self.margin;
        let (bw, bh) = self.buffer_size();
        let mut bytes = if self.opts_poison && !full { vec![0xA5u8; (bw * bh * 4) as usize] } else { vec![0u8; (bw * bh * 4) as usize] };
        if m > 0 && (full || !self.opts_poison) {
            for px in bytes.chunks_exact_mut(4) {
                px.copy_from_slice(&[0xFF, 0x00, 0xFF, 0xFF]);
            }
        }
        let copy_rect = |bytes: &mut [u8], canvas: &[u32], r: (u32, u32, u32, u32)| {
            for y in r.1..r.1 + r.3 {
                for x in r.0..r.0 + r.2 {
                    let c = canvas[(y * w + x) as usize];
                    let i = (((y + m) * bw + x + m) * 4) as usize;
                    bytes[i..i + 4].copy_from_slice(&[c as u8, (c >> 8) as u8, (c >> 16) as u8, 0xFF]);
                }
            }
        };
        if self.opts_poison && !full {
            for r in &damage {
                copy_rect(&mut bytes, &self.canvas, *r);
            }
        } else {
            copy_rect(&mut bytes, &self.canvas, (0, 0, w, h));
        }
        let Some(file) = &self.file else { return };
        if file.write_all_at(&bytes, (idx as u64) * u64::from(bw * bh * 4)).is_err() {
            return;
        }

        let s = self.surfaces.as_ref().expect("superfície");
        if m > 0 {
            s.xdg_surface.set_window_geometry(m as i32, m as i32, w as i32, h as i32);
        }
        s.surface.attach(Some(&self.bufs[idx].buffer), 0, 0);
        for r in &damage {
            s.surface.damage_buffer((r.0 + m) as i32, (r.1 + m) as i32, r.2 as i32, r.3 as i32);
        }
        if !self.frame_pending {
            s.surface.frame(qh, ());
            self.frame_pending = true;
        }
        s.surface.commit();
        self.bufs[idx].busy = true;
    }
}

pub fn run(opts: Options) -> Result<(), String> {
    let stream = match &opts.socket {
        Some(p) => UnixStream::connect(p).map_err(|e| format!("conectar a {}: {e}", p.display()))?,
        None => {
            let dir = std::env::var_os("XDG_RUNTIME_DIR").ok_or("XDG_RUNTIME_DIR não definido")?;
            let name = std::env::var_os("WAYLAND_DISPLAY").ok_or("WAYLAND_DISPLAY não definido")?;
            let p = PathBuf::from(dir).join(name);
            UnixStream::connect(&p).map_err(|e| format!("conectar a {}: {e}", p.display()))?
        }
    };
    let conn = Connection::from_socket(stream).map_err(|e| e.to_string())?;
    let (globals, mut queue) = registry_queue_init::<State>(&conn).map_err(|e| e.to_string())?;
    let qh = queue.handle();

    let compositor: WlCompositor = globals.bind(&qh, 4..=6, ()).map_err(|e| format!("wl_compositor: {e}"))?;
    let shm: WlShm = globals.bind(&qh, 1..=1, ()).map_err(|e| format!("wl_shm: {e}"))?;
    let wm_base: XdgWmBase = globals.bind(&qh, 1..=6, ()).map_err(|e| format!("xdg_wm_base: {e}"))?;
    let seat: WlSeat = globals.bind(&qh, 1..=7, ()).map_err(|e| format!("wl_seat: {e}"))?;

    let log = match &opts.log {
        Some(p) => Some(std::fs::File::create(p).map_err(|e| format!("criar {}: {e}", p.display()))?),
        None => None,
    };
    let mut st = State {
        opts_poison: opts.poison,
        band_frames: opts.band_frames,
        counter: opts.counter.clone(),
        margin: opts.margin,
        self_resize: opts.self_resize,
        default_size: opts.default_size,
        log,
        shm,
        compositor: compositor.clone(),
        wm_base: wm_base.clone(),
        seat,
        popups: Vec::new(),
        popups_enabled: opts.popups,
        pointer: None,
        cursor_mode: opts.cursor.clone(),
        cursor_repeat: opts.cursor_repeat,
        cursor_shape_mgr: globals.bind::<WpCursorShapeManagerV1, _, _>(&qh, 1..=2, ()).ok(),
        cursor_objs: cursor::CursorObjs::new(),
        clip: clipboard::ClipObjs::new(globals.bind::<WlDataDeviceManager, _, _>(&qh, 1..=3, ()).ok()),
        clipboard_text: opts.clipboard.clone(),
        last_serial: 0,
        kbd_focus: String::new(),
        surfaces: None,
        size: opts.default_size,
        suggested: (0, 0),
        configured: false,
        closed: false,
        pool: None,
        file: None,
        bufs: Vec::new(),
        canvas: Vec::new(),
        n: 0,
        m: 0,
        want_draw: false,
        frame_pending: false,
        xkb_ctx: xkb::Context::new(xkb::CONTEXT_NO_FLAGS),
        xkb_state: None,
    };

    let surface = compositor.create_surface(&qh, ());
    let xdg_surface = wm_base.get_xdg_surface(&surface, &qh, 0usize);
    let toplevel = xdg_surface.get_toplevel(&qh, ());
    toplevel.set_title(opts.title.clone());
    toplevel.set_app_id(opts.app_id.clone());
    if let Some((w, h)) = opts.min_size {
        toplevel.set_min_size(w, h);
    }
    if let Some((w, h)) = opts.max_size {
        toplevel.set_max_size(w, h);
    }
    if opts.kde_decoration {
        kde::request_server_mode(&globals, &qh, &surface);
    }
    surface.commit(); // pedido do configure inicial
    st.surfaces = Some(Surfaces { surface, xdg_surface, toplevel });

    while !st.closed {
        queue.blocking_dispatch(&mut st).map_err(|e| format!("despacho Wayland: {e}"))?;
    }
    st.destroy_all_popups();
    if let Some(s) = st.surfaces.take() {
        s.toplevel.destroy();
        s.xdg_surface.destroy();
        s.surface.destroy();
    }
    for b in st.bufs.drain(..) {
        b.buffer.destroy();
    }
    let _ = conn.flush();
    // Garante que o compositor viu a destruição antes de sairmos.
    let _ = queue.roundtrip(&mut st);
    Ok(())
}

// ---------------------------------------------------------------- tratadores

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
sem_eventos!(WlCompositor, WlShm, WlShmPool, WlSurface);

impl Dispatch<WlBuffer, usize> for State {
    fn event(st: &mut Self, _: &WlBuffer, ev: wl_buffer::Event, idx: &usize, _: &Connection, qh: &QueueHandle<Self>) {
        if let wl_buffer::Event::Release = ev {
            if let Some(b) = st.bufs.get_mut(*idx) {
                b.busy = false;
            }
            if st.want_draw {
                st.draw(qh, false);
            }
        }
    }
}

impl Dispatch<WlCallback, ()> for State {
    fn event(st: &mut Self, _: &WlCallback, ev: wl_callback::Event, _: &(), _: &Connection, qh: &QueueHandle<Self>) {
        if let wl_callback::Event::Done { .. } = ev {
            st.frame_pending = false;
            st.draw(qh, false);
        }
    }
}

impl Dispatch<XdgWmBase, ()> for State {
    fn event(_: &mut Self, base: &XdgWmBase, ev: xdg_wm_base::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
        if let xdg_wm_base::Event::Ping { serial } = ev {
            base.pong(serial);
        }
    }
}

/// Dados do `xdg_surface`: 0 = a janela principal; N > 0 = popup de índice N - 1.
impl Dispatch<XdgSurface, usize> for State {
    fn event(st: &mut Self, xs: &XdgSurface, ev: xdg_surface::Event, quem: &usize, _: &Connection, qh: &QueueHandle<Self>) {
        if let xdg_surface::Event::Configure { serial } = ev {
            xs.ack_configure(serial);
            if *quem > 0 {
                st.popup_configure(*quem - 1, qh);
                return;
            }
            let want = if st.suggested.0 > 0 && st.suggested.1 > 0 { st.suggested } else if st.configured { st.size } else { st.default_size };
            let changed = want != st.size || !st.configured;
            if changed {
                st.size = want;
                st.log(format!("configure {}x{}", want.0, want.1));
                st.canvas = vec![0; (want.0 * want.1) as usize];
                if st.make_buffers(qh).is_err() {
                    st.closed = true;
                    return;
                }
                st.configured = true;
                st.n += 1; // nova base a cada (re)configuração: o quadro-base é o próximo número
                st.draw(qh, true);
            }
        }
    }
}

impl Dispatch<XdgToplevel, ()> for State {
    fn event(st: &mut Self, _: &XdgToplevel, ev: xdg_toplevel::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
        match ev {
            xdg_toplevel::Event::Configure { width, height, states } => {
                st.suggested = (width.max(0) as u32, height.max(0) as u32);
                // `states` é uma lista de u32 em little-endian (enum xdg_toplevel.state).
                let nomes: Vec<&str> = states
                    .chunks_exact(4)
                    .map(|c| match u32::from_le_bytes([c[0], c[1], c[2], c[3]]) {
                        1 => "Maximized",
                        2 => "Fullscreen",
                        3 => "Resizing",
                        4 => "Activated",
                        5 => "TiledLeft",
                        6 => "TiledRight",
                        7 => "TiledTop",
                        8 => "TiledBottom",
                        _ => "Outro",
                    })
                    .collect();
                st.log(format!("toplevel-states {}", nomes.join(",")));
            }
            xdg_toplevel::Event::Close => {
                st.log("close".into());
                st.closed = true;
            }
            _ => {}
        }
    }
}

impl Dispatch<WlSeat, ()> for State {
    fn event(st: &mut Self, seat: &WlSeat, ev: wl_seat::Event, _: &(), _: &Connection, qh: &QueueHandle<Self>) {
        if let wl_seat::Event::Capabilities { capabilities: WEnum::Value(caps) } = ev {
            if caps.contains(wl_seat::Capability::Keyboard) {
                seat.get_keyboard(qh, ());
            }
            st.create_data_device(qh);
            if caps.contains(wl_seat::Capability::Pointer) && st.pointer.is_none() {
                st.pointer = Some(seat.get_pointer(qh, ()));
            }
        }
    }
}

impl Dispatch<WlKeyboard, ()> for State {
    fn event(st: &mut Self, _: &WlKeyboard, ev: wl_keyboard::Event, _: &(), _: &Connection, qh: &QueueHandle<Self>) {
        match ev {
            wl_keyboard::Event::Keymap { fd, size, .. } => {
                // `read_exact_at` e não `read`: o arquivo é o mesmo para todos os clientes (descrição de arquivo
                // compartilhada) e o deslocamento dele não é nosso.
                let mut text = vec![0u8; size as usize];
                if std::fs::File::from(fd).read_exact_at(&mut text, 0).is_ok() {
                    while text.last() == Some(&0) {
                        text.pop();
                    }
                    let text = String::from_utf8_lossy(&text).into_owned();
                    let km = xkb::Keymap::new_from_string(&st.xkb_ctx, text, xkb::KEYMAP_FORMAT_TEXT_V1, xkb::KEYMAP_COMPILE_NO_FLAGS);
                    st.xkb_state = km.as_ref().map(xkb::State::new);
                    st.log(format!("keymap {}", if km.is_some() { "ok" } else { "falhou" }));
                    if let Some(k) = &km {
                        st.log(format!("keymap layout \"{}\"", k.layout_get_name(0)));
                    }
                }
            }
            wl_keyboard::Event::Enter { serial, surface, .. } => {
                st.last_serial = serial;
                st.kbd_focus = st.surface_name(&surface);
                st.log(format!("kbd-enter {}", st.kbd_focus));
            }
            wl_keyboard::Event::Leave { surface, .. } => {
                let nome = st.surface_name(&surface);
                st.log(format!("kbd-leave {nome}"));
            }
            wl_keyboard::Event::Modifiers { mods_depressed, mods_latched, mods_locked, group, .. } => {
                if let Some(x) = &mut st.xkb_state {
                    x.update_mask(mods_depressed, mods_latched, mods_locked, 0, 0, group);
                }
            }
            wl_keyboard::Event::Key { serial, key, state, .. } => {
                st.last_serial = serial;
                let pressed = matches!(state, WEnum::Value(wl_keyboard::KeyState::Pressed));
                let desc = match &st.xkb_state {
                    Some(x) => {
                        let code = xkb::Keycode::new(key + 8);
                        let sym = x.key_get_one_sym(code);
                        let utf8 = if pressed { x.key_get_utf8(code) } else { String::new() };
                        format!("sym={} utf8={:?}", xkb::keysym_get_name(sym), utf8)
                    }
                    None => "sem-keymap".to_string(),
                };
                st.log(format!("key {key} {} {desc} focus={}", if pressed { "pressed" } else { "released" }, st.kbd_focus));
                if pressed && st.popups_enabled {
                    st.popup_key(key, qh);
                }
                if pressed && key == 21 {
                    // KEY_Y: o app define a seleção de texto.
                    st.set_selection(qh);
                }
                if pressed && key == 19 {
                    // KEY_R: o app decide mudar de tamanho sozinho.
                    if let Some(novo) = st.self_resize {
                        st.resize_by_app(novo, qh);
                    }
                }
            }
            _ => {}
        }
    }
}

impl Dispatch<WlPointer, ()> for State {
    fn event(st: &mut Self, _: &WlPointer, ev: wl_pointer::Event, _: &(), _: &Connection, qh: &QueueHandle<Self>) {
        match ev {
            wl_pointer::Event::Enter { serial, surface, surface_x, surface_y } => {
                st.last_serial = serial;
                let nome = st.surface_name(&surface);
                st.log(format!("ptr-enter {surface_x:.1} {surface_y:.1} surface={nome}"));
                st.apply_cursor(serial, qh);
            }
            wl_pointer::Event::Leave { .. } => st.log("ptr-leave".into()),
            wl_pointer::Event::Motion { surface_x, surface_y, .. } => st.log(format!("motion {surface_x:.1} {surface_y:.1}")),
            wl_pointer::Event::Button { serial, button, state, .. } => {
                st.last_serial = serial;
                let pressed = matches!(state, WEnum::Value(wl_pointer::ButtonState::Pressed));
                st.log(format!("button {button} {}", if pressed { "pressed" } else { "released" }));
            }
            wl_pointer::Event::Axis { axis, value, .. } => {
                let nome = match axis {
                    WEnum::Value(wl_pointer::Axis::HorizontalScroll) => "horizontal",
                    _ => "vertical",
                };
                st.log(format!("axis {nome} {value:.2}"));
            }
            _ => {}
        }
    }
}
