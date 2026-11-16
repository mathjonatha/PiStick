
use std::collections::{HashMap, HashSet};
use std::num::NonZeroU32;
use std::rc::Rc;
use std::sync::mpsc::Sender;
use std::sync::Arc;
use std::time::{Duration, Instant};

use ps_proto::{CursorShape, Message};
use softbuffer::{Context, Surface};
use winit::application::ApplicationHandler;
use winit::dpi::{PhysicalPosition, PhysicalSize};
use winit::event::{ElementState, MouseScrollDelta, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, OwnedDisplayHandle};
use winit::keyboard::PhysicalKey;
use winit::window::{CursorIcon, CustomCursor, Window, WindowAttributes, WindowId};

use crate::clipboard::ClipboardSync;
use crate::keymap::{button_to_evdev, keycode_to_evdev};
use crate::net::{decorated_title, SharedWindow, UserEvent};
use crate::stats::Stats;

const FUNDO: u32 = 0x0030_3030;
const PIXELS_POR_LINHA: f32 = 15.0;
const MIN_JANELA: u32 = 64;
const ATRASO_PERDA_FOCO: Duration = Duration::from_millis(150);

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExitReason {
    LastWindowClosed,
    ScriptQuit,
    Disconnected(String),
    Goodbye { reason: u8, text: String },
}

#[derive(Debug, Default)]
pub struct ResizeTracker {
    last: (u32, u32),
    pending: Vec<(u32, u32)>,
}

impl ResizeTracker {
    pub fn new(size: (u32, u32)) -> Self {
        ResizeTracker { last: size, pending: Vec::new() }
    }

    pub fn user_resized(&mut self, actual: (u32, u32)) -> Option<(u32, u32)> {
        if actual.0 == 0 || actual.1 == 0 || actual == self.last {
            return None;
        }
        self.last = actual;
        self.pending.push(actual);
        if self.pending.len() > 8 {
            self.pending.remove(0);
        }
        Some(actual)
    }

    pub fn server_resized(&mut self, size: (u32, u32)) -> Option<(u32, u32)> {
        if let Some(i) = self.pending.iter().position(|p| *p == size) {
            self.pending.drain(..=i);
            if self.pending.is_empty() {
                self.last = size;
            }
            return None;
        }
        self.pending.clear();
        self.last = size;
        Some(size)
    }
}

pub fn forward_key_event(repeat: bool) -> bool {
    !repeat
}

type WinSurface = Surface<OwnedDisplayHandle, Rc<Window>>;

enum Kind {
    Top,
    Popup { parent: u32, x: i32, y: i32 },
}

struct Win {
    remote: u32,
    window: Rc<Window>,
    surface: WinSurface,
    shared: Arc<SharedWindow>,
    kind: Kind,
    resize: ResizeTracker,
    size_dirty: bool,
    shown: bool,
    title: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ForcedFocus {
    First,
    Win(u32),
}

pub struct App {
    forced_focus: Option<ForcedFocus>,
    context: Context<OwnedDisplayHandle>,
    out: Sender<Message>,
    stats: Arc<Stats>,
    clip: Arc<ClipboardSync>,
    windows: HashMap<WindowId, Win>,
    by_remote: HashMap<u32, WindowId>,
    had_window: bool,
    pending_parent: HashMap<u32, u32>,
    focused: HashSet<WindowId>,
    announced_root: Option<u32>,
    focus_loss_at: Option<Instant>,
    status_flags: u32,
    pub script_mode: bool,
    pub exit_reason: Option<ExitReason>,
}

impl App {
    pub fn new(context: Context<OwnedDisplayHandle>, out: Sender<Message>, stats: Arc<Stats>, forced_focus: Option<ForcedFocus>, clip: Arc<ClipboardSync>) -> Self {
        App {
            forced_focus,
            context,
            out,
            stats,
            clip,
            windows: HashMap::new(),
            by_remote: HashMap::new(),
            had_window: false,
            pending_parent: HashMap::new(),
            focused: HashSet::new(),
            announced_root: None,
            focus_loss_at: None,
            script_mode: false,
            status_flags: 0,
            exit_reason: None,
        }
    }

    fn send(&self, m: Message) {
        let _ = self.out.send(m);
    }

    fn win(&self, remote: u32) -> Option<&Win> {
        self.by_remote.get(&remote).and_then(|id| self.windows.get(id))
    }

    fn remote_of(&self, id: WindowId) -> Option<u32> {
        self.windows.get(&id).map(|w| w.remote)
    }

    fn root_remote(&self, remote: u32) -> u32 {
        let mut cur = remote;
        for _ in 0..16 {
            match self.win(cur).map(|w| &w.kind) {
                Some(Kind::Popup { parent, .. }) => cur = *parent,
                _ => break,
            }
        }
        cur
    }

    fn content_origin(&self, remote: u32) -> Option<(i32, i32)> {
        let p = self.win(remote)?.window.inner_position().ok()?;
        Some((p.x, p.y))
    }

    fn popup_screen_pos(&self, parent: u32, x: i32, y: i32) -> Option<PhysicalPosition<i32>> {
        let (px, py) = self.content_origin(parent)?;
        Some(PhysicalPosition::new(px + x, py + y))
    }

    fn create_window(&mut self, event_loop: &ActiveEventLoop, remote: u32, width: u32, height: u32, title: String, shared: Arc<SharedWindow>) {
        let title_base = title.clone();
        let title = decorated_title(&title, self.status_flags);
        let mut attrs = Window::default_attributes()
            .with_title(title)
            .with_inner_size(PhysicalSize::new(width, height))
            .with_min_inner_size(PhysicalSize::new(MIN_JANELA, MIN_JANELA));
        if let Some(parent) = self.pending_parent.remove(&remote) {
            attrs = self.with_owner(attrs, parent);
        }
        let Some((window, surface)) = self.open(event_loop, attrs, remote) else { return };
        let id = window.id();
        self.by_remote.insert(remote, id);
        self.windows.insert(id, Win { remote, window, surface, shared, kind: Kind::Top, resize: ResizeTracker::new((width, height)), size_dirty: true, shown: true, title: title_base });
        let primeira = !self.had_window;
        self.had_window = true;
        if match self.forced_focus {
            Some(ForcedFocus::First) => primeira,
            Some(ForcedFocus::Win(id)) => id == remote,
            None => false,
        } {
            self.send(Message::Focus { win: remote, focused: true });
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn create_popup(&mut self, event_loop: &ActiveEventLoop, remote: u32, parent: u32, x: i32, y: i32, width: u32, height: u32, grab: bool, shared: Arc<SharedWindow>) {
        let mut attrs = Window::default_attributes()
            .with_title("")
            .with_decorations(false)
            .with_resizable(false)
            .with_active(false)
            .with_visible(false)
            .with_inner_size(PhysicalSize::new(width, height));
        if let Some(pos) = self.popup_screen_pos(parent, x, y) {
            attrs = attrs.with_position(pos);
        }
        attrs = self.with_owner(attrs, parent);
        attrs = popup_platform_attrs(attrs);
        let Some((window, surface)) = self.open(event_loop, attrs, remote) else { return };
        if !grab {
            let _ = window.set_cursor_hittest(false);
        }
        let id = window.id();
        self.by_remote.insert(remote, id);
        self.windows.insert(id, Win { remote, window, surface, shared, kind: Kind::Popup { parent, x, y }, resize: ResizeTracker::new((width, height)), size_dirty: false, shown: false, title: String::new() });
    }

    fn open(&self, event_loop: &ActiveEventLoop, attrs: WindowAttributes, remote: u32) -> Option<(Rc<Window>, WinSurface)> {
        let window = match event_loop.create_window(attrs) {
            Ok(w) => Rc::new(w),
            Err(e) => {
                eprintln!("[psc] não foi possível criar a janela {remote}: {e}");
                return None;
            }
        };
        match Surface::new(&self.context, window.clone()) {
            Ok(s) => Some((window, s)),
            Err(e) => {
                eprintln!("[psc] não foi possível criar a superfície da janela {remote}: {e}");
                None
            }
        }
    }

    #[cfg(windows)]
    fn with_owner(&self, attrs: WindowAttributes, parent: u32) -> WindowAttributes {
        use winit::platform::windows::WindowAttributesExtWindows;
        use winit::raw_window_handle::{HasWindowHandle, RawWindowHandle};
        let hwnd = self.win(parent).and_then(|w| match w.window.window_handle().ok()?.as_raw() {
            RawWindowHandle::Win32(h) => Some(h.hwnd.get()),
            _ => None,
        });
        match hwnd {
            Some(h) => attrs.with_owner_window(h),
            None => attrs,
        }
    }

    #[cfg(not(windows))]
    fn with_owner(&self, attrs: WindowAttributes, _parent: u32) -> WindowAttributes {
        attrs
    }

    fn reposition_children(&self, parent: u32) {
        for w in self.windows.values() {
            if let Kind::Popup { parent: p, x, y } = w.kind {
                if p == parent {
                    if let Some(pos) = self.popup_screen_pos(parent, x, y) {
                        w.window.set_outer_position(pos);
                    }
                    self.reposition_children(w.remote);
                }
            }
        }
    }

    fn present(&mut self, id: WindowId, from_frame: bool) {
        let probe = from_frame && self.stats.atomic_is_on();
        let Some(w) = self.windows.get_mut(&id) else { return };
        let remote = w.remote;
        let size = w.window.inner_size();
        let t0 = Instant::now();
        let acks = match (NonZeroU32::new(size.width), NonZeroU32::new(size.height)) {
            (Some(nw), Some(nh)) => match draw(w, nw, nh, probe.then_some(&*self.stats)) {
                Some(acks) => acks,
                None => return,
            },
            _ => std::mem::take(&mut w.shared.inner.lock().unwrap_or_else(|e| e.into_inner()).acks),
        };
        if !w.shown {
            w.shown = true;
            w.window.set_visible(true);
        }
        self.stats.record_present(t0.elapsed().as_micros() as u64);
        self.confirm(remote, acks);
    }

    fn confirm(&self, win: u32, acks: Vec<u32>) {
        for seq in acks {
            self.stats.acks.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            self.send(Message::FrameAck { win, seq });
        }
    }

    fn set_cursor(&self, event_loop: &ActiveEventLoop, win: u32, shape: &CursorShape) {
        let Some(w) = self.win(win) else { return };
        match shape {
            CursorShape::Hidden => w.window.set_cursor_visible(false),
            CursorShape::Named(nome) => {
                w.window.set_cursor_visible(true);
                w.window.set_cursor(css_cursor(nome));
            }
            CursorShape::Image { width, height, hot_x, hot_y, bgra } => {
                let rgba = bgra_premultiplied_to_rgba(bgra);
                let hx = (*hot_x).clamp(0, (*width).saturating_sub(1) as i16) as u16;
                let hy = (*hot_y).clamp(0, (*height).saturating_sub(1) as i16) as u16;
                match CustomCursor::from_rgba(rgba, *width, *height, hx, hy) {
                    Ok(src) => {
                        w.window.set_cursor_visible(true);
                        w.window.set_cursor(event_loop.create_custom_cursor(src));
                    }
                    Err(e) => eprintln!("[psc] imagem de cursor inválida: {e}"),
                }
            }
        }
    }

    fn focus_event(&mut self, id: WindowId, focused: bool) {
        let Some(remote) = self.remote_of(id) else { return };
        if self.script_mode || self.forced_focus.is_some() {
            return;
        }
        if focused {
            self.focused.insert(id);
            self.focus_loss_at = None;
            let root = self.root_remote(remote);
            if self.announced_root != Some(root) {
                if let Some(old) = self.announced_root {
                    self.send(Message::Focus { win: old, focused: false });
                }
                self.send(Message::Focus { win: root, focused: true });
                self.announced_root = Some(root);
            }
            self.clip.on_focus();
        } else {
            self.focused.remove(&id);
            self.note_focus_lost();
        }
    }

    fn note_focus_lost(&mut self) {
        if self.focused.is_empty() && self.announced_root.is_some() && self.focus_loss_at.is_none() {
            self.focus_loss_at = Some(Instant::now() + ATRASO_PERDA_FOCO);
        }
    }

    fn settle_focus(&mut self) {
        let Some(at) = self.focus_loss_at else { return };
        if Instant::now() < at {
            return;
        }
        self.focus_loss_at = None;
        if self.focused.is_empty() {
            if let Some(root) = self.announced_root.take() {
                self.send(Message::Focus { win: root, focused: false });
            }
        }
    }
}

#[cfg(windows)]
fn popup_platform_attrs(attrs: WindowAttributes) -> WindowAttributes {
    use winit::platform::windows::{CornerPreference, WindowAttributesExtWindows};
    attrs.with_skip_taskbar(true).with_corner_preference(CornerPreference::DoNotRound).with_undecorated_shadow(false)
}

#[cfg(not(windows))]
fn popup_platform_attrs(attrs: WindowAttributes) -> WindowAttributes {
    attrs
}

pub fn css_cursor(nome: &str) -> CursorIcon {
    nome.parse::<CursorIcon>().unwrap_or(CursorIcon::Default)
}

pub fn bgra_premultiplied_to_rgba(bgra: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(bgra.len());
    for p in bgra.as_chunks::<4>().0 {
        let a = u32::from(p[3]);
        let un = |c: u8| (u32::from(c) * 255 + a / 2).checked_div(a).unwrap_or(0).min(255) as u8;
        out.extend_from_slice(&[un(p[2]), un(p[1]), un(p[0]), p[3]]);
    }
    out
}

fn draw(w: &mut Win, nw: NonZeroU32, nh: NonZeroU32, probe: Option<&Stats>) -> Option<Vec<u32>> {
    if let Err(e) = w.surface.resize(nw, nh) {
        eprintln!("[psc] erro ao ajustar a superfície: {e}");
        return None;
    }
    let mut buffer = match w.surface.buffer_mut() {
        Ok(b) => b,
        Err(e) => {
            eprintln!("[psc] erro ao obter o buffer: {e}");
            return None;
        }
    };
    let acks = {
        let mut g = w.shared.inner.lock().unwrap_or_else(|e| e.into_inner());
        let (sw, sh) = (g.decoder.width() as usize, g.decoder.height() as usize);
        let pixels = g.display_pixels();
        if let Some(stats) = probe {
            stats.atomic_observe(&ps_codec::testutil::stripes::read_markers(pixels, sw as u32, sh as u32));
        }
        blit(&mut buffer, nw.get() as usize, nh.get() as usize, pixels, sw, sh);
        std::mem::take(&mut g.acks)
    };
    if let Err(e) = buffer.present() {
        eprintln!("[psc] erro ao apresentar: {e}");
        return None;
    }
    Some(acks)
}

pub fn blit(dst: &mut [u32], dw: usize, dh: usize, src: &[u8], sw: usize, sh: usize) {
    let copy_w = dw.min(sw);
    for y in 0..dh {
        let row = &mut dst[y * dw..(y + 1) * dw];
        if y < sh {
            let srow = &src[y * sw * 4..y * sw * 4 + copy_w * 4];
            for (d, s) in row.iter_mut().zip(srow.as_chunks::<4>().0) {
                *d = u32::from_le_bytes([s[0], s[1], s[2], s[3]]);
            }
            row[copy_w..].fill(FUNDO);
        } else {
            row.fill(FUNDO);
        }
    }
}

impl ApplicationHandler<UserEvent> for App {
    fn resumed(&mut self, _event_loop: &ActiveEventLoop) {}

    fn user_event(&mut self, event_loop: &ActiveEventLoop, event: UserEvent) {
        match event {
            UserEvent::WinCreate { win, width, height, title, shared } => self.create_window(event_loop, win, width, height, title, shared),
            UserEvent::PopupCreate { win, parent, x, y, width, height, grab, shared } => {
                self.create_popup(event_loop, win, parent, x, y, width, height, grab, shared);
            }
            UserEvent::PopupMove { win, x, y, width, height } => {
                let parent = match self.win(win).map(|w| &w.kind) {
                    Some(Kind::Popup { parent, .. }) => *parent,
                    _ => return,
                };
                if let Some(pos) = self.popup_screen_pos(parent, x, y) {
                    if let Some(w) = self.win(win) {
                        w.window.set_outer_position(pos);
                    }
                }
                if let Some(id) = self.by_remote.get(&win).copied() {
                    if let Some(w) = self.windows.get_mut(&id) {
                        w.kind = Kind::Popup { parent, x, y };
                        let atual = w.window.inner_size();
                        if (atual.width, atual.height) != (width, height) && width > 0 && height > 0 {
                            let _ = w.window.request_inner_size(PhysicalSize::new(width, height));
                        }
                    }
                }
                self.reposition_children(win);
            }
            UserEvent::WinParent { win, parent } => {
                if self.by_remote.contains_key(&win) {
                    println!("[psc] janela {win}: nova mãe {parent} (o sistema só aceita a mãe na criação)");
                } else if parent != 0 {
                    self.pending_parent.insert(win, parent);
                }
            }
            UserEvent::WinLimits { win, min_w, min_h, max_w, max_h } => {
                if let Some(w) = self.win(win) {
                    let min = PhysicalSize::new(min_w.max(MIN_JANELA), min_h.max(MIN_JANELA));
                    w.window.set_min_inner_size(Some(min));
                    w.window.set_max_inner_size((max_w > 0 && max_h > 0).then(|| PhysicalSize::new(max_w, max_h)));
                }
            }
            UserEvent::Cursor { win, shape } => self.set_cursor(event_loop, win, &shape),
            UserEvent::WinResize { win, width, height } => {
                if let Some(id) = self.by_remote.get(&win).copied() {
                    if let Some(w) = self.windows.get_mut(&id) {
                        let acompanhar = match w.kind {
                            Kind::Top => w.resize.server_resized((width, height)),
                            Kind::Popup { .. } => Some((width, height)),
                        };
                        if let Some((nw, nh)) = acompanhar {
                            let atual = w.window.inner_size();
                            if (atual.width, atual.height) != (nw, nh) {
                                let _ = w.window.request_inner_size(PhysicalSize::new(nw, nh));
                            }
                        }
                        w.window.request_redraw();
                    }
                }
            }
            UserEvent::WinTitle { win, title } => {
                let flags = self.status_flags;
                if let Some(id) = self.by_remote.get(&win).copied() {
                    if let Some(w) = self.windows.get_mut(&id) {
                        w.window.set_title(&decorated_title(&title, flags));
                        w.title = title;
                    }
                }
            }
            UserEvent::Status { flags, .. } => {
                self.status_flags = flags;
                for w in self.windows.values().filter(|w| matches!(w.kind, Kind::Top)) {
                    w.window.set_title(&decorated_title(&w.title, flags));
                }
            }
            UserEvent::Goodbye { reason, text } => {
                self.exit_reason = Some(ExitReason::Goodbye { reason, text });
                event_loop.exit();
            }
            UserEvent::WinDestroy { win } => {
                self.pending_parent.remove(&win);
                if let Some(id) = self.by_remote.remove(&win) {
                    self.windows.remove(&id);
                    self.focused.remove(&id);
                    self.note_focus_lost();
                }
                if self.announced_root == Some(win) {
                    self.announced_root = None;
                }
                if self.had_window && self.windows.is_empty() {
                    self.exit_reason = Some(ExitReason::LastWindowClosed);
                    event_loop.exit();
                }
            }
            UserEvent::Frame { win, shared } => match self.by_remote.get(&win).copied() {
                Some(id) => self.present(id, true),
                None => {
                    let acks = std::mem::take(&mut shared.inner.lock().unwrap_or_else(|e| e.into_inner()).acks);
                    self.confirm(win, acks);
                }
            },
            UserEvent::Quit => {
                self.exit_reason = Some(ExitReason::ScriptQuit);
                event_loop.exit();
            }
            UserEvent::Disconnected(motivo) => {
                self.exit_reason = Some(ExitReason::Disconnected(motivo));
                event_loop.exit();
            }
        }
    }

    fn window_event(&mut self, _event_loop: &ActiveEventLoop, id: WindowId, event: WindowEvent) {
        let Some(remote) = self.remote_of(id) else { return };
        match event {
            WindowEvent::CloseRequested => {
                if matches!(self.win(remote).map(|w| &w.kind), Some(Kind::Top)) {
                    self.send(Message::CloseReq { win: remote });
                }
            }
            WindowEvent::Resized(_) => {
                if let Some(w) = self.windows.get_mut(&id) {
                    w.size_dirty = true;
                    w.window.request_redraw();
                }
            }
            WindowEvent::Moved(_) => self.reposition_children(remote),
            WindowEvent::RedrawRequested => self.present(id, false),
            WindowEvent::CursorMoved { position, .. } => {
                self.send(Message::PointerMove { win: remote, x: position.x as f32, y: position.y as f32 });
            }
            WindowEvent::MouseInput { state, button, .. } => {
                if let Some(code) = button_to_evdev(button) {
                    self.send(Message::PointerButton { win: remote, button: code, pressed: state == ElementState::Pressed });
                }
            }
            WindowEvent::MouseWheel { delta, .. } => {
                let (dx, dy) = match delta {
                    MouseScrollDelta::LineDelta(x, y) => (-x * PIXELS_POR_LINHA, -y * PIXELS_POR_LINHA),
                    MouseScrollDelta::PixelDelta(p) => (-p.x as f32, -p.y as f32),
                };
                self.send(Message::PointerAxis { win: remote, dx, dy });
            }
            WindowEvent::KeyboardInput { event, .. } => {
                if !forward_key_event(event.repeat) {
                    return;
                }
                if let PhysicalKey::Code(code) = event.physical_key {
                    if let Some(keycode) = keycode_to_evdev(code) {
                        let win = self.root_remote(remote);
                        self.send(Message::Key { win, keycode, pressed: event.state == ElementState::Pressed });
                    }
                }
            }
            WindowEvent::Focused(focused) => self.focus_event(id, focused),
            _ => {}
        }
    }

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        let mut pedidos = Vec::new();
        for w in self.windows.values_mut() {
            if !std::mem::take(&mut w.size_dirty) {
                continue;
            }
            if !matches!(w.kind, Kind::Top) {
                continue;
            }
            let size = w.window.inner_size();
            if let Some((rw, rh)) = w.resize.user_resized((size.width, size.height)) {
                pedidos.push(Message::ResizeReq { win: w.remote, width: rw, height: rh });
            }
        }
        for m in pedidos {
            self.send(m);
        }
        self.settle_focus();
        match self.focus_loss_at {
            Some(at) => event_loop.set_control_flow(ControlFlow::WaitUntil(at)),
            None => event_loop.set_control_flow(ControlFlow::Wait),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blit_converte_bgrx_e_pinta_o_fundo() {
        let src = [1u8, 2, 3, 0xFF, 10, 20, 30, 0];
        let mut dst = vec![0u32; 6];
        blit(&mut dst, 3, 2, &src, 2, 1);
        assert_eq!(dst[0] & 0x00FF_FFFF, 0x0003_0201);
        assert_eq!(dst[1] & 0x00FF_FFFF, 0x001E_140A);
        assert_eq!(dst[2], FUNDO);
        assert_eq!(&dst[3..], &[FUNDO; 3]);
    }

    #[test]
    fn blit_recorta_quando_o_destino_e_menor() {
        let src: Vec<u8> = (0..4 * 3 * 2).map(|i| i as u8).collect();
        let mut dst = vec![0u32; 2];
        blit(&mut dst, 2, 1, &src, 3, 2);
        assert_eq!(dst[0], u32::from_le_bytes([0, 1, 2, 3]));
        assert_eq!(dst[1], u32::from_le_bytes([4, 5, 6, 7]));
    }

    #[test]
    fn redimensionamento_do_app_e_acompanhado() {
        let mut t = ResizeTracker::new((960, 600));
        assert_eq!(t.server_resized((700, 500)), Some((700, 500)));
        assert_eq!(t.user_resized((700, 500)), None);
    }

    #[test]
    fn resposta_ao_proprio_pedido_nao_mexe_na_janela() {
        let mut t = ResizeTracker::new((960, 600));
        assert_eq!(t.user_resized((810, 600)), Some((810, 600)));
        assert_eq!(t.user_resized((820, 600)), Some((820, 600)));
        assert_eq!(t.user_resized((830, 600)), Some((830, 600)));
        assert_eq!(t.server_resized((810, 600)), None);
        assert_eq!(t.server_resized((830, 600)), None);
        assert_eq!(t.server_resized((900, 700)), Some((900, 700)));
    }

    #[test]
    fn app_que_nao_aceita_o_tamanho_pedido() {
        let mut t = ResizeTracker::new((960, 600));
        assert_eq!(t.user_resized((200, 100)), Some((200, 100)));
        assert_eq!(t.server_resized((300, 200)), Some((300, 200)));
    }

    #[test]
    fn repeticao_do_sistema_e_descartada() {
        assert!(forward_key_event(false));
        assert!(!forward_key_event(true));
    }

    #[test]
    fn cursores_nomeados() {
        assert_eq!(css_cursor("text"), CursorIcon::Text);
        assert_eq!(css_cursor("pointer"), CursorIcon::Pointer);
        assert_eq!(css_cursor("ew-resize"), CursorIcon::EwResize);
        assert_eq!(css_cursor("default"), CursorIcon::Default);
        assert_eq!(css_cursor("nome-que-nao-existe"), CursorIcon::Default);
    }

    #[test]
    fn cursor_de_imagem_sai_do_pre_multiplicado() {
        let rgba = bgra_premultiplied_to_rgba(&[0, 0, 128, 128, 9, 9, 9, 0, 10, 20, 30, 255]);
        assert_eq!(&rgba[0..4], &[255, 0, 0, 128]);
        assert_eq!(&rgba[4..8], &[0, 0, 0, 0]);
        assert_eq!(&rgba[8..12], &[30, 20, 10, 255]);
    }
}
