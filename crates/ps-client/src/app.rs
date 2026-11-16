//! Interface: uma janela do sistema por janela remota, desenhada com `softbuffer`.
//!
//! Janelas de nível superior são janelas normais. Popups (menus e dicas) são janelas sem moldura,
//! fora da barra de tarefas, sempre acima da mãe, que não tomam o foco ao aparecer; a posição na
//! tela é a do conteúdo da mãe mais o deslocamento (x, y) que o servidor informa.

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

/// Cinza usado onde a janela do sistema é maior que o quadro remoto (durante um redimensionamento).
const FUNDO: u32 = 0x0030_3030;
/// Pixels equivalentes a uma "linha" de roda de mouse.
const PIXELS_POR_LINHA: f32 = 15.0;
const MIN_JANELA: u32 = 64;
/// Quando uma janela perde o foco, espera isto antes de avisar o servidor: se o foco foi para um
/// popup da mesma família (clique no menu), não houve perda.
const ATRASO_PERDA_FOCO: Duration = Duration::from_millis(150);

/// Por que o programa terminou.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExitReason {
    LastWindowClosed,
    /// O roteiro de teste mandou encerrar.
    ScriptQuit,
    Disconnected(String),
    /// O aparelho se despediu (`GOODBYE`): `reason` 1 = outro cliente assumiu (`text` = nome dele), 2 = serviço parando.
    Goodbye { reason: u8, text: String },
}

/// Acompanha os tamanhos pedidos ao servidor para distinguir a resposta ao nosso pedido de um
/// redimensionamento que partiu do app (esse a janela do sistema tem de acompanhar).
#[derive(Debug, Default)]
pub struct ResizeTracker {
    /// Último tamanho que o servidor confirmou ou que já pedimos.
    last: (u32, u32),
    /// Pedidos enviados e ainda sem resposta, do mais antigo ao mais novo.
    pending: Vec<(u32, u32)>,
}

impl ResizeTracker {
    pub fn new(size: (u32, u32)) -> Self {
        ResizeTracker { last: size, pending: Vec::new() }
    }

    /// A janela do sistema tem `actual`. Devolve o `RESIZE_REQ` a enviar, se o tamanho mudou.
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

    /// Chegou `WIN_RESIZE` com `size`. Devolve o tamanho para o qual a janela do sistema deve ir:
    /// `None` se é a resposta a um pedido nosso (a janela já está ou vai estar lá).
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

/// A repetição de tecla é feita pelo app (Wayland, `repeat_info`); os eventos de repetição que o
/// sistema hospedeiro gera são descartados, senão a tecla repetiria em dobro.
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
    /// Houve evento de tamanho; o tamanho real é lido em `about_to_wait`, já agrupado. Os
    /// valores dos eventos não servem: na criação o Windows emite um `Resized` transitório
    /// com o tamanho padrão da janela, que não é o tamanho final.
    size_dirty: bool,
    /// Popups só ficam visíveis depois do primeiro quadro (sem piscar vazios).
    shown: bool,
    /// Título que o app deu à janela, sem os avisos de estado do aparelho.
    title: String,
}

/// Janela que deve ser anunciada ao servidor como focada, sem depender do foco real do sistema.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ForcedFocus {
    /// A primeira janela criada.
    First,
    /// A janela remota com este id.
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
    /// Pais conhecidos antes da criação da janela (o servidor manda `WIN_PARENT` antes de `WIN_CREATE`).
    pending_parent: HashMap<u32, u32>,
    /// Janelas do sistema que têm o foco agora.
    focused: HashSet<WindowId>,
    /// Raiz para a qual o servidor foi avisado `FOCUS 1`.
    announced_root: Option<u32>,
    /// Instante a partir do qual a perda de foco vale.
    focus_loss_at: Option<Instant>,
    /// Com roteiro de teste, quem anuncia foco é o roteiro: os eventos de foco do sistema não valem.
    /// Último `STATUS` do aparelho (bits `STATUS_*`).
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
        // Se a thread de escrita acabou, a desconexão já foi (ou será) avisada pela rede.
        let _ = self.out.send(m);
    }

    fn win(&self, remote: u32) -> Option<&Win> {
        self.by_remote.get(&remote).and_then(|id| self.windows.get(id))
    }

    fn remote_of(&self, id: WindowId) -> Option<u32> {
        self.windows.get(&id).map(|w| w.remote)
    }

    /// Janela de nível superior da cadeia de `remote` (ele mesmo, se não é popup).
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

    /// Canto do conteúdo da janela `remote` na tela.
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
        // Diálogo: a janela do sistema fica "dona" da mãe (sempre acima dela).
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
            // Dica: não captura o mouse (o ponteiro "atravessa" a janela).
            let _ = window.set_cursor_hittest(false);
        }
        let id = window.id();
        self.by_remote.insert(remote, id);
        self.windows.insert(id, Win { remote, window, surface, shared, kind: Kind::Popup { parent, x, y }, resize: ResizeTracker::new((width, height)), size_dirty: false, shown: false, title: String::new() });
    }

    /// Cria a janela do sistema e a superfície de desenho dela.
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

    /// Torna a janela a ser criada "filha" da janela remota `parent` no sistema (só no Windows).
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

    /// Reposiciona na tela os popups cujo pai é `parent` (e, em cadeia, os filhos deles).
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

    /// Apresenta o quadro atual e confirma os quadros que ele representa.
    ///
    /// `from_frame`: o pedido veio de um `FRAME` (e não de um redesenho do sistema); só essas apresentações entram
    /// na medição da apresentação atômica (`atomic start` / `atomic report`).
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
            // Janela minimizada: não dá para apresentar; confirma para não travar o fluxo.
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

    // ------------------------------------------------------------------ foco

    /// Uma janela do sistema ganhou ou perdeu o foco.
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

    /// Se o foco continua fora de todas as janelas depois do atraso, avisa o servidor.
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

/// Atributos de popup que só existem no Windows: fora da barra de tarefas, sem cantos arredondados.
#[cfg(windows)]
fn popup_platform_attrs(attrs: WindowAttributes) -> WindowAttributes {
    use winit::platform::windows::{CornerPreference, WindowAttributesExtWindows};
    attrs.with_skip_taskbar(true).with_corner_preference(CornerPreference::DoNotRound).with_undecorated_shadow(false)
}

#[cfg(not(windows))]
fn popup_platform_attrs(attrs: WindowAttributes) -> WindowAttributes {
    attrs
}

/// Nome CSS de cursor (como em `cursor-shape-v1`) para o cursor do sistema. Nome desconhecido vira "default".
pub fn css_cursor(nome: &str) -> CursorIcon {
    nome.parse::<CursorIcon>().unwrap_or(CursorIcon::Default)
}

/// BGRA pré-multiplicado (protocolo) para RGBA sem pré-multiplicação (winit).
pub fn bgra_premultiplied_to_rgba(bgra: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(bgra.len());
    for p in bgra.as_chunks::<4>().0 {
        let a = u32::from(p[3]);
        let un = |c: u8| (u32::from(c) * 255 + a / 2).checked_div(a).unwrap_or(0).min(255) as u8;
        out.extend_from_slice(&[un(p[2]), un(p[1]), un(p[0]), p[3]]);
    }
    out
}

/// Copia o quadro decodificado para a superfície e a apresenta. Devolve os ACKs pendentes,
/// ou `None` se a apresentação falhou (o erro já foi impresso).
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
            // Os marcadores da imagem que vai para a tela (a medição da apresentação atômica).
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

/// Copia o quadro BGRX remoto para o buffer do `softbuffer` (0x00RRGGBB em `u32`).
/// Onde o buffer é maior que o quadro, pinta o fundo cinza.
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
                // Só dá para tornar uma janela "filha" no sistema na criação: o servidor manda
                // `WIN_PARENT` antes de `WIN_CREATE`. Depois disso, a mudança é só registrada.
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
                            // O app mudou o tamanho por conta própria: a janela do sistema acompanha.
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
                    // A janela local já não existe: confirma direto.
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
                // Popups não têm botão de fechar; quem os dispensa é o servidor.
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
                // O winit positivo = conteúdo sobe/vai para a direita; o eixo evdev/Wayland é o oposto.
                let (dx, dy) = match delta {
                    MouseScrollDelta::LineDelta(x, y) => (-x * PIXELS_POR_LINHA, -y * PIXELS_POR_LINHA),
                    MouseScrollDelta::PixelDelta(p) => (-p.x as f32, -p.y as f32),
                };
                self.send(Message::PointerAxis { win: remote, dx, dy });
            }
            WindowEvent::KeyboardInput { event, .. } => {
                if !forward_key_event(event.repeat) {
                    return; // a repetição é feita pelo lado que injeta no teclado do app
                }
                if let PhysicalKey::Code(code) = event.physical_key {
                    if let Some(keycode) = keycode_to_evdev(code) {
                        // O teclado sempre vai com o id da janela raiz; o servidor o entrega ao
                        // popup que detém a captura, se houver.
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
        // Quadro 2x1: pixel (B=1,G=2,R=3,X=0xFF) e (B=10,G=20,R=30,X=0)
        let src = [1u8, 2, 3, 0xFF, 10, 20, 30, 0];
        // Destino 3x2, maior que o quadro
        let mut dst = vec![0u32; 6];
        blit(&mut dst, 3, 2, &src, 2, 1);
        assert_eq!(dst[0] & 0x00FF_FFFF, 0x0003_0201); // 0x00RRGGBB
        assert_eq!(dst[1] & 0x00FF_FFFF, 0x001E_140A);
        assert_eq!(dst[2], FUNDO);
        assert_eq!(&dst[3..], &[FUNDO; 3]);
    }

    #[test]
    fn blit_recorta_quando_o_destino_e_menor() {
        let src: Vec<u8> = (0..4 * 3 * 2).map(|i| i as u8).collect(); // 3x2
        let mut dst = vec![0u32; 2];
        blit(&mut dst, 2, 1, &src, 3, 2);
        assert_eq!(dst[0], u32::from_le_bytes([0, 1, 2, 3]));
        assert_eq!(dst[1], u32::from_le_bytes([4, 5, 6, 7]));
    }

    #[test]
    fn redimensionamento_do_app_e_acompanhado() {
        let mut t = ResizeTracker::new((960, 600));
        // O app pede 700x500 por conta própria (ex.: restaurar um tamanho salvo): a janela acompanha.
        assert_eq!(t.server_resized((700, 500)), Some((700, 500)));
        // Depois disso o tamanho do sistema bate com o do servidor: nenhum pedido de volta.
        assert_eq!(t.user_resized((700, 500)), None);
    }

    #[test]
    fn resposta_ao_proprio_pedido_nao_mexe_na_janela() {
        let mut t = ResizeTracker::new((960, 600));
        // O usuário arrasta a borda: 810, 820, 830 (cada um vira um pedido).
        assert_eq!(t.user_resized((810, 600)), Some((810, 600)));
        assert_eq!(t.user_resized((820, 600)), Some((820, 600)));
        assert_eq!(t.user_resized((830, 600)), Some((830, 600)));
        // As respostas chegam atrasadas e na ordem; nenhuma deve desfazer o arrasto.
        assert_eq!(t.server_resized((810, 600)), None);
        assert_eq!(t.server_resized((830, 600)), None); // a de 820 foi pulada pelo servidor
        // Já respondido tudo: um redimensionamento novo do app volta a ser acompanhado.
        assert_eq!(t.server_resized((900, 700)), Some((900, 700)));
    }

    #[test]
    fn app_que_nao_aceita_o_tamanho_pedido() {
        let mut t = ResizeTracker::new((960, 600));
        assert_eq!(t.user_resized((200, 100)), Some((200, 100)));
        // O app tem tamanho mínimo e responde com outro: a janela cresce até ele.
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
        // BGRA pré-multiplicado: vermelho a 50% = (B=0, G=0, R=128, A=128) -> RGBA (255, 0, 0, 128).
        let rgba = bgra_premultiplied_to_rgba(&[0, 0, 128, 128, 9, 9, 9, 0, 10, 20, 30, 255]);
        assert_eq!(&rgba[0..4], &[255, 0, 0, 128]);
        assert_eq!(&rgba[4..8], &[0, 0, 0, 0]);
        assert_eq!(&rgba[8..12], &[30, 20, 10, 255]);
    }
}
