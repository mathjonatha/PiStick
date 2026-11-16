
use std::collections::HashMap;
use std::io::{self, Read, Write};
use std::marker::PhantomData;
use std::os::fd::AsFd;
use std::os::unix::io::OwnedFd;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use ps_codec::Rect;
use ps_proto::{CursorShape, Message, CAP_CLIPBOARD, CAP_CURSOR, CAP_POPUPS, MAX_CLIPBOARD, MAX_CURSOR_SIDE, MIME_TEXT};
use smithay::backend::input::{Axis, AxisSource, ButtonState, KeyState};
use smithay::input::keyboard::{FilterResult, Keycode, XkbConfig};
use smithay::input::pointer::{AxisFrame, ButtonEvent, CursorImageAttributes, CursorImageStatus, MotionEvent};
use smithay::input::{Seat, SeatHandler, SeatState};
use smithay::output::{Mode as OutMode, Output, PhysicalProperties, Scale, Subpixel};
use smithay::reexports::calloop::generic::Generic;
use smithay::reexports::calloop::timer::{TimeoutAction, Timer};
use smithay::reexports::calloop::{channel, EventLoop, Interest, LoopHandle, Mode, PostAction};
use smithay::reexports::wayland_protocols::xdg::decoration::zv1::server::zxdg_toplevel_decoration_v1::Mode as DecoMode;
use smithay::reexports::wayland_protocols::xdg::shell::server::xdg_toplevel;
use smithay::reexports::wayland_server::backend::{ClientData, ClientId, DisconnectReason};
use smithay::reexports::wayland_server::protocol::wl_buffer::WlBuffer;
use smithay::reexports::wayland_server::protocol::wl_callback::WlCallback;
use smithay::reexports::wayland_server::protocol::wl_seat::WlSeat;
use smithay::reexports::wayland_server::protocol::wl_shm::Format;
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::reexports::wayland_server::backend::ObjectId;
use smithay::reexports::wayland_server::{Client, Display, DisplayHandle, ListeningSocket, Resource};
use smithay::utils::{Logical, Rectangle, Serial, SERIAL_COUNTER};
use smithay::wayland::buffer::BufferHandler;
use smithay::wayland::compositor::{
    get_children, get_parent, get_role, with_states, with_surface_tree_upward, BufferAssignment, CompositorClientState, CompositorHandler,
    CompositorState, Damage, SubsurfaceCachedState, SurfaceAttributes, SurfaceData, TraversalAction,
};
use smithay::wayland::output::{OutputHandler, OutputManagerState};
use smithay::wayland::selection::data_device::{
    request_data_device_client_selection, set_data_device_focus, set_data_device_selection, ClientDndGrabHandler,
    DataDeviceHandler, DataDeviceState, ServerDndGrabHandler,
};
use smithay::wayland::selection::primary_selection::{set_primary_focus, PrimarySelectionHandler, PrimarySelectionState};
use smithay::wayland::selection::{SelectionHandler, SelectionSource, SelectionTarget};
use smithay::reexports::wayland_protocols_misc::server_decoration::server::org_kde_kwin_server_decoration::{
    Mode as KdeMode, OrgKdeKwinServerDecoration,
};
use smithay::reexports::wayland_protocols_misc::server_decoration::server::org_kde_kwin_server_decoration_manager::Mode as DefaultMode;
use smithay::reexports::wayland_server::WEnum;
use smithay::wayland::shell::kde::decoration::{KdeDecorationHandler, KdeDecorationState};
use smithay::wayland::shell::xdg::decoration::{XdgDecorationHandler, XdgDecorationState};
use smithay::wayland::shell::xdg::{
    PopupSurface, PositionerState, SurfaceCachedState, ToplevelSurface, XdgShellHandler, XdgShellState,
    XdgToplevelSurfaceData,
};
use smithay::backend::allocator::dmabuf::Dmabuf;
use smithay::backend::allocator::{Buffer as _, Fourcc};
use smithay::wayland::cursor_shape::CursorShapeManagerState;
use smithay::wayland::dmabuf::{get_dmabuf, DmabufFeedbackBuilder, DmabufGlobal, DmabufHandler, DmabufState, ImportNotifier};
use smithay::wayland::viewporter::{ViewportCachedState, ViewporterState};
use smithay::wayland::shm::{with_buffer_contents, BufferData, ShmHandler, ShmState};
use smithay::wayland::tablet_manager::TabletSeatHandler;
use smithay::{
    delegate_compositor, delegate_cursor_shape, delegate_dmabuf, delegate_data_device, delegate_output, delegate_seat, delegate_shm,
    delegate_kde_decoration, delegate_primary_selection, delegate_viewporter, delegate_xdg_decoration, delegate_xdg_shell,
};

use super::tree::{estica, intersecta as intersect, recompoe, Dirty, Item, Layer, Ret, Tree};
use super::{dmabuf, lock, Cmd, Shared, SocketSpec, WinBuf, INITIAL_SIZE};
use crate::source::SourceEvent;
use crate::synthetic::{MAX_SIZE, MIN_SIZE};

const IDLE_FRAME_PERIOD: Duration = Duration::from_millis(33);
const FRAME_WATCHDOG: Duration = Duration::from_secs(1);
const DMABUF_MIN_INTERVAL: Duration = Duration::from_millis(30);
pub(crate) const MONITOR: (i32, i32) = (1920, 1080);

pub(crate) fn bind(spec: SocketSpec) -> io::Result<(ListeningSocket, String, PathBuf)> {
    fn to_io<E: std::fmt::Display>(e: E) -> io::Error {
        io::Error::other(e.to_string())
    }
    match spec {
        SocketSpec::Auto(base) => {
            let l = ListeningSocket::bind_auto(&base, 0..64).map_err(to_io)?;
            let name = l.socket_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
            let dir = std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from).unwrap_or_default();
            Ok((l, name, dir))
        }
        SocketSpec::Path(p) => {
            let name = p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
            let dir = p.parent().map(PathBuf::from).unwrap_or_default();
            let l = ListeningSocket::bind_absolute(p).map_err(to_io)?;
            Ok((l, name, dir))
        }
    }
}

#[derive(Default)]
struct ClientState {
    compositor: CompositorClientState,
}

impl ClientData for ClientState {
    fn initialized(&self, _id: ClientId) {}
    fn disconnected(&self, _id: ClientId, _reason: DisconnectReason) {}
}

enum Role {
    Toplevel(ToplevelSurface),
    Popup(PopupData),
}

struct PopupData {
    popup: PopupSurface,
    parent: Option<WlSurface>,
    rel: (i32, i32),
    grab: bool,
}

struct WinEntry {
    surface: WlSurface,
    role: Role,
    id: Option<u32>,
    callbacks: Vec<WlCallback>,
    waiting_send: bool,
    waiting_since: Instant,
    origin: (i32, i32),
    limits: (u32, u32, u32, u32),
    dismissed: bool,
    pending_dmabuf: Option<PendingDmabuf>,
    pending_subs: Vec<PendingSub>,
    last_dmabuf_read: Instant,
    tree: Option<Tree<ObjectId>>,
    dirty: Dirty<ObjectId>,
    needs_flush: bool,
    root_alpha: bool,
    root_logical: (u32, u32),
}

struct PendingDmabuf {
    buffer: WlBuffer,
    geom: Option<(i32, i32, i32, i32)>,
    scale: i32,
    dst: Option<(i32, i32)>,
}

struct PendingSub {
    surface: WlSurface,
    buffer: WlBuffer,
    scale: i32,
    dst: Option<(i32, i32)>,
}

impl WinEntry {
    fn new(surface: WlSurface, role: Role) -> Self {
        WinEntry { surface, role, id: None, callbacks: Vec::new(), waiting_send: false, waiting_since: Instant::now(), origin: (0, 0), limits: (0, 0, 0, 0), dismissed: false, pending_dmabuf: None, pending_subs: Vec::new(), last_dmabuf_read: Instant::now() - DMABUF_MIN_INTERVAL, tree: None, dirty: Dirty::default(), needs_flush: false, root_alpha: false, root_logical: (0, 0) }
    }

    fn has_pending(&self) -> bool {
        self.pending_dmabuf.is_some() || !self.pending_subs.is_empty()
    }

    fn toplevel(&self) -> Option<&ToplevelSurface> {
        match &self.role {
            Role::Toplevel(t) => Some(t),
            Role::Popup(_) => None,
        }
    }

    fn popup(&self) -> Option<&PopupData> {
        match &self.role {
            Role::Popup(p) => Some(p),
            Role::Toplevel(_) => None,
        }
    }

    fn is_grab_popup(&self) -> bool {
        self.popup().is_some_and(|p| p.grab)
    }

    fn is_live_popup(&self) -> bool {
        self.popup().is_some() && self.id.is_some() && !self.dismissed
    }
}

pub(crate) struct App {
    dh: DisplayHandle,
    loop_handle: LoopHandle<'static, App>,
    shared: Shared,
    start: Instant,
    quit: bool,
    timer_armed: bool,

    compositor_state: CompositorState,
    xdg_shell_state: XdgShellState,
    shm_state: ShmState,
    dmabuf_state: DmabufState,
    #[allow(dead_code)]
    dmabuf_global: Option<DmabufGlobal>,
    seat_state: SeatState<App>,
    data_device_state: DataDeviceState,
    primary_selection_state: PrimarySelectionState,
    #[allow(dead_code)]
    cursor_shape_state: CursorShapeManagerState,
    #[allow(dead_code)]
    viewporter_state: ViewporterState,
    #[allow(dead_code)]
    output_state: OutputManagerState,
    #[allow(dead_code)]
    decoration_state: XdgDecorationState,
    #[allow(dead_code)]
    kde_decoration_state: KdeDecorationState,
    seat: Seat<App>,
    output: Output,
    windows: Vec<WinEntry>,
    layers: HashMap<ObjectId, Layer>,
    pointer_pos: HashMap<u32, (f64, f64)>,
    pointer_win: Option<u32>,
    focus_root: Option<WlSurface>,
    swallowed: Vec<u32>,
    cursor_images: HashMap<ObjectId, CursorImage>,
    cursor_surface: Option<WlSurface>,
    last_cursor: Option<(u32, CursorShape)>,
    last_cursor_req: Option<(u32, PedidoCursor)>,
    clip_last: Option<String>,
    host_text: Option<String>,
    pending_reads: Vec<(Seat<App>, String)>,
}

const TEXT_MIMES: [&str; 3] = ["text/plain;charset=utf-8", "UTF8_STRING", "text/plain"];
const SELECTION_READ_TIMEOUT: Duration = Duration::from_secs(2);

struct CursorImage {
    width: u16,
    height: u16,
    bgra: Vec<u8>,
}

const MAX_CURSOR_IMAGES: usize = 64;
const CURSOR_ROLE: &str = "cursor_image";

impl App {
    fn now_ms(&self) -> u32 {
        self.start.elapsed().as_millis() as u32
    }

    fn idx_by_surface(&self, surface: &WlSurface) -> Option<usize> {
        self.windows.iter().position(|e| e.surface == *surface)
    }

    fn idx_by_id(&self, id: u32) -> Option<usize> {
        self.windows.iter().position(|e| e.id == Some(id))
    }

    fn fire_callbacks(entry: &mut WinEntry, time: u32) {
        for cb in entry.callbacks.drain(..) {
            cb.done(time);
        }
        entry.waiting_send = false;
    }

    fn arm_timer(&mut self) {
        if self.timer_armed {
            return;
        }
        self.timer_armed = true;
        let r = self.loop_handle.insert_source(Timer::from_duration(IDLE_FRAME_PERIOD), |_, _, app| app.timer_tick());
        if r.is_err() {
            self.timer_armed = false;
        }
    }

    fn timer_tick(&mut self) -> TimeoutAction {
        let session = lock(&self.shared).session_active();
        let time = self.now_ms();
        let due: Vec<usize> = (0..self.windows.len()).filter(|&i| self.windows[i].has_pending() && self.windows[i].last_dmabuf_read.elapsed() >= DMABUF_MIN_INTERVAL).collect();
        for i in due {
            self.flush_pending_dmabuf(i);
        }
        let mut pending = false;
        for e in &mut self.windows {
            if e.callbacks.is_empty() {
                continue;
            }
            if !session || !e.waiting_send || e.waiting_since.elapsed() >= FRAME_WATCHDOG {
                Self::fire_callbacks(e, time);
            } else {
                pending = true;
            }
        }
        if self.windows.iter().any(WinEntry::has_pending) {
            TimeoutAction::ToDuration(IDLE_FRAME_PERIOD)
        } else if pending {
            TimeoutAction::ToDuration(Duration::from_millis(100))
        } else {
            self.timer_armed = false;
            TimeoutAction::Drop
        }
    }

    fn flush_pending_dmabuf(&mut self, idx: usize) {
        let subs = std::mem::take(&mut self.windows[idx].pending_subs);
        if !subs.is_empty() {
            for p in subs {
                self.apply_sub_buffer(idx, &p.surface, p.buffer, &[], p.scale, p.dst);
            }
            self.flush_tree(idx);
        }
        let Some(p) = self.windows[idx].pending_dmabuf.take() else { return };
        let Some(id) = self.windows[idx].id else {
            p.buffer.release();
            return;
        };
        let surface = self.windows[idx].surface.clone();
        let info = buffer_info(&p.buffer);
        let arvore = precisa_arvore(&self.layers, &self.windows[idx], &surface, info, p.dst);
        let mut st = lock(&self.shared);
        let entry = &mut self.windows[idx];
        let Some(wb) = st.windows.iter_mut().find(|w| w.id == id) else {
            drop(st);
            p.buffer.release();
            return;
        };
        let old = (wb.width, wb.height);
        let result = blit_raiz(&self.layers, entry, &surface, Some(&p.buffer), wb, &[], p.scale, p.geom, p.dst, info, arvore);
        let new = (wb.width, wb.height);
        entry.last_dmabuf_read = Instant::now();
        let mut novo_tamanho = None;
        if let Some((rects, origin)) = result {
            entry.origin = origin;
            if old != new {
                st.push(SourceEvent::Resized { win: id, width: new.0, height: new.1 });
                novo_tamanho = Some(new);
            }
            if !rects.is_empty() {
                st.push(SourceEvent::Damage { win: id, rects });
                if !entry.callbacks.is_empty() {
                    if !entry.waiting_send {
                        entry.waiting_since = Instant::now();
                    }
                    entry.waiting_send = true;
                }
            }
        }
        drop(st);
        p.buffer.release();
        if let (Some((w, h)), Some(t)) = (novo_tamanho, self.windows[idx].toplevel()) {
            t.with_pending_state(|s| s.size = Some((w as i32, h as i32).into()));
        }
    }

    fn commit_sub(&mut self, ridx: usize, surface: &WlSurface) {
        let (assign, mut damage, callbacks, scale) = with_states(surface, |states| {
            let mut guard = states.cached_state.get::<SurfaceAttributes>();
            let attrs = guard.current();
            (attrs.buffer.take(), std::mem::take(&mut attrs.damage), std::mem::take(&mut attrs.frame_callbacks), attrs.buffer_scale.max(1))
        });
        let dst = viewport_dst(surface);
        let id = surface.id();
        match assign {
            Some(BufferAssignment::NewBuffer(buffer)) => {
                let entry = &mut self.windows[ridx];
                if let Some(pos) = entry.pending_subs.iter().position(|p| p.surface == *surface) {
                    entry.pending_subs.remove(pos).buffer.release();
                    damage.clear();
                }
                let is_dmabuf = get_dmabuf(&buffer).is_ok();
                if is_dmabuf && entry.id.is_some() && entry.last_dmabuf_read.elapsed() < DMABUF_MIN_INTERVAL {
                    entry.pending_subs.push(PendingSub { surface: surface.clone(), buffer, scale, dst });
                    self.arm_timer();
                } else {
                    self.apply_sub_buffer(ridx, surface, buffer, &damage, scale, dst);
                }
            }
            Some(BufferAssignment::Removed) => {
                let entry = &mut self.windows[ridx];
                if let Some(pos) = entry.pending_subs.iter().position(|p| p.surface == *surface) {
                    entry.pending_subs.remove(pos).buffer.release();
                }
                if self.layers.remove(&id).is_some() {
                    entry.dirty.all = true;
                    entry.needs_flush = true;
                }
            }
            None => {
                if let Some(l) = self.layers.get_mut(&id) {
                    let lg = logical_size((l.w, l.h), dst, scale);
                    if (l.lw, l.lh) != lg {
                        (l.lw, l.lh) = lg;
                        let entry = &mut self.windows[ridx];
                        entry.dirty.all = true;
                        entry.needs_flush = true;
                    }
                }
            }
        }
        if !callbacks.is_empty() {
            self.windows[ridx].callbacks.extend(callbacks);
            self.arm_timer();
        }
    }

    fn apply_sub_buffer(&mut self, ridx: usize, surface: &WlSurface, buffer: WlBuffer, damage: &[Damage], scale: i32, dst: Option<(i32, i32)>) {
        let is_dmabuf = get_dmabuf(&buffer).is_ok();
        let id = surface.id();
        let layer = self.layers.entry(id.clone()).or_default();
        let lido = read_layer(&buffer, layer, damage, scale, dst);
        let vazia = layer.px.is_empty();
        buffer.release();
        if vazia {
            self.layers.remove(&id);
        }
        let entry = &mut self.windows[ridx];
        if is_dmabuf {
            entry.last_dmabuf_read = Instant::now();
        }
        if let Some(partes) = lido {
            entry.dirty.subs.push((id, partes));
            entry.needs_flush = true;
        }
    }

    fn flush_trees(&mut self) {
        for i in 0..self.windows.len() {
            if self.windows[i].needs_flush {
                self.flush_tree(i);
            }
        }
    }

    fn flush_tree(&mut self, idx: usize) {
        self.windows[idx].needs_flush = false;
        let Some(id) = self.windows[idx].id else { return };
        let surface = self.windows[idx].surface.clone();
        let mut st = lock(&self.shared);
        let Some(wb) = st.windows.iter_mut().find(|w| w.id == id) else { return };
        let entry = &mut self.windows[idx];
        let (alpha, logical) = (entry.root_alpha, entry.root_logical);
        let tree = entry.tree.get_or_insert_with(|| Tree::de_janela(&wb.pixels, wb.width, wb.height, alpha, logical));
        if (wb.width, wb.height) != (tree.root_w, tree.root_h) {
            return;
        }
        let items = arrangement(&surface, tree.root_logical, &self.layers);
        let dims = (tree.root_w, tree.root_h);
        let rects = recompoe(tree, items, &self.layers, &mut entry.dirty, entry.origin, dims, &mut wb.pixels, Vec::new(), false);
        if !rects.is_empty() {
            st.push(SourceEvent::Damage { win: id, rects });
            if !entry.callbacks.is_empty() {
                if !entry.waiting_send {
                    entry.waiting_since = Instant::now();
                }
                entry.waiting_send = true;
            }
        }
    }

    fn root_of(surface: &WlSurface) -> WlSurface {
        let mut s = surface.clone();
        while let Some(p) = get_parent(&s) {
            s = p;
        }
        s
    }

    fn parent_surface(&self, idx: usize) -> Option<WlSurface> {
        match &self.windows[idx].role {
            Role::Toplevel(t) => t.parent(),
            Role::Popup(p) => p.parent.clone(),
        }
    }

    fn parent_id(&self, idx: usize) -> u32 {
        self.parent_surface(idx).and_then(|p| self.idx_by_surface(&p)).and_then(|i| self.windows[i].id).unwrap_or(0)
    }

    fn abs_origin(&self, idx: usize) -> (i32, i32) {
        let mut pos = (0, 0);
        let mut cur = idx;
        for _ in 0..16 {
            match &self.windows[cur].role {
                Role::Popup(p) => {
                    pos = (pos.0 + p.rel.0, pos.1 + p.rel.1);
                    match p.parent.as_ref().and_then(|s| self.idx_by_surface(s)) {
                        Some(i) => cur = i,
                        None => break,
                    }
                }
                Role::Toplevel(_) => break,
            }
        }
        pos
    }

    fn root_idx(&self, idx: usize) -> usize {
        let mut cur = idx;
        for _ in 0..16 {
            match self.windows[cur].popup().and_then(|p| p.parent.as_ref()).and_then(|s| self.idx_by_surface(s)) {
                Some(i) => cur = i,
                None => break,
            }
        }
        cur
    }

    fn descendants(&self, surface: &WlSurface) -> Vec<usize> {
        let mut out: Vec<usize> = Vec::new();
        let mut frontier = vec![surface.clone()];
        while let Some(s) = frontier.pop() {
            for (i, e) in self.windows.iter().enumerate() {
                if e.popup().and_then(|p| p.parent.as_ref()) == Some(&s) && !out.contains(&i) {
                    out.push(i);
                    frontier.push(e.surface.clone());
                }
            }
        }
        out
    }

    fn commit_window(&mut self, idx: usize) {
        let surface = self.windows[idx].surface.clone();
        if let Role::Toplevel(t) = &self.windows[idx].role {
            if !t.is_initial_configure_sent() {
                if t.parent().is_some() {
                    t.with_pending_state(|s| s.size = None);
                }
                t.send_configure();
            }
        }
        let (assign, mut damage, callbacks, scale) = with_states(&surface, |states| {
            let mut guard = states.cached_state.get::<SurfaceAttributes>();
            let attrs = guard.current();
            (attrs.buffer.take(), std::mem::take(&mut attrs.damage), std::mem::take(&mut attrs.frame_callbacks), attrs.buffer_scale.max(1))
        });
        let (geom, min, max) = with_states(&surface, |states| {
            let mut guard = states.cached_state.get::<SurfaceCachedState>();
            let c = guard.current();
            (c.geometry, c.min_size, c.max_size)
        });
        let now = self.now_ms();
        if self.windows[idx].dismissed {
            if let Some(BufferAssignment::NewBuffer(b)) = assign {
                b.release();
            }
            for cb in callbacks {
                cb.done(now);
            }
            return;
        }

        let (title, app_id) = toplevel_texts(&surface);
        let parent_id = self.parent_id(idx);
        let shared = self.shared.clone();
        let limits = (min.w.max(0) as u32, min.h.max(0) as u32, max.w.max(0) as u32, max.h.max(0) as u32);
        let geom_rect = geom.map(|g| (g.loc.x * scale, g.loc.y * scale, g.size.w * scale, g.size.h * scale));
        let mut had_damage = false;
        let mut destroy = false;
        let mut newly_mapped = false;
        let mut orphan_popup = false;
        let mut new_size: Option<(u32, u32)> = None;

        let dst = viewport_dst(&surface);
        let mut novo: Option<Option<WlBuffer>> = None;
        match assign {
            Some(BufferAssignment::NewBuffer(b)) => novo = Some(Some(b)),
            Some(BufferAssignment::Removed) => destroy = true,
            None => {
                if self.windows[idx].tree.as_ref().is_some_and(|t| t.root_src.is_some() && t.root_dst != dst) {
                    novo = Some(None);
                }
            }
        }
        if let Some(buffer) = novo {
            let is_popup = self.windows[idx].popup().is_some();
            let info = buffer.as_ref().and_then(buffer_info);
            let lsize = match (&buffer, info) {
                (Some(_), Some((w, h, _))) => Some(dst.unwrap_or((w, h))),
                (None, _) => dst,
                _ => None,
            };
            let minuscula = !is_popup
                && lsize.is_some_and(|l| {
                    let c = geom_rect.and_then(|g| intersect(g, (0, 0, l.0, l.1))).map_or(l, |g| (g.2, g.3));
                    c.0 <= 1 && c.1 <= 1
                });
            if buffer.is_some() {
                if let Some(velho) = self.windows[idx].pending_dmabuf.take() {
                    velho.buffer.release();
                    damage.clear();
                }
            }
            let is_dmabuf = buffer.as_ref().is_some_and(|b| get_dmabuf(b).is_ok());
            if minuscula {
                if let Some(b) = buffer {
                    b.release();
                }
            } else if is_dmabuf && self.windows[idx].id.is_some() && self.windows[idx].last_dmabuf_read.elapsed() < DMABUF_MIN_INTERVAL {
                if let Some(b) = buffer {
                    self.windows[idx].pending_dmabuf = Some(PendingDmabuf { buffer: b, geom: geom_rect, scale, dst });
                    self.arm_timer();
                }
            } else if is_popup && parent_id == 0 {
                orphan_popup = true;
                if let Some(b) = buffer {
                    b.release();
                }
            } else {
                let (grab, rel) = self.windows[idx].popup().map_or((false, (0, 0)), |p| (p.grab, p.rel));
                let arvore = precisa_arvore(&self.layers, &self.windows[idx], &surface, info, dst);
                let entry = &mut self.windows[idx];
                if is_dmabuf {
                    entry.last_dmabuf_read = Instant::now();
                }
                let mut st = lock(&shared);
                let id = match entry.id {
                    Some(id) => id,
                    None => {
                        let id = st.alloc_id();
                        entry.id = Some(id);
                        st.windows.push(WinBuf {
                            id,
                            width: 0,
                            height: 0,
                            title: title.clone(),
                            app_id: app_id.clone(),
                            pixels: Vec::new(),
                            parent: parent_id,
                            popup: is_popup.then_some((rel.0, rel.1, grab)),
                            limits,
                        });
                        id
                    }
                };
                let first = st.windows.iter().find(|w| w.id == id).is_some_and(|w| w.width == 0);
                newly_mapped = first;
                let wb = st.windows.iter_mut().find(|w| w.id == id).expect("janela no armazenamento");
                let old = (wb.width, wb.height);
                let result = blit_raiz(&self.layers, entry, &surface, buffer.as_ref(), wb, &damage, scale, geom_rect, dst, info, arvore);
                let new = (wb.width, wb.height);
                match result {
                    Some((rects, origin)) => {
                        entry.origin = origin;
                        had_damage = !rects.is_empty();
                        if old != new {
                            new_size = Some(new);
                        }
                        if first {
                            if is_popup {
                                st.push(SourceEvent::PopupCreated { win: id, parent: parent_id, x: rel.0, y: rel.1, width: new.0, height: new.1, grab });
                            } else {
                                if parent_id != 0 {
                                    st.push(SourceEvent::Parent { win: id, parent: parent_id });
                                }
                                st.push(SourceEvent::Created { win: id, width: new.0, height: new.1, title, app_id });
                            }
                        } else if old != new {
                            st.push(SourceEvent::Resized { win: id, width: new.0, height: new.1 });
                        }
                        if !is_popup && ((first && limits != (0, 0, 0, 0)) || (!first && limits != entry.limits)) {
                            st.push(SourceEvent::Limits { win: id, min_w: limits.0, min_h: limits.1, max_w: limits.2, max_h: limits.3 });
                        }
                        if !is_popup {
                            entry.limits = limits;
                            if let Some(w) = st.windows.iter_mut().find(|w| w.id == id) {
                                w.limits = limits;
                            }
                        }
                        if had_damage {
                            st.push(SourceEvent::Damage { win: id, rects });
                        }
                    }
                    None => {
                        if first {
                            st.windows.retain(|w| w.id != id);
                            entry.id = None;
                        }
                    }
                }
                drop(st);
                if let Some(b) = buffer {
                    b.release();
                }
            }
        }

        if orphan_popup {
            self.dismiss_popup(idx);
            return;
        }
        if destroy {
            self.unmap(idx);
        }
        if let (Some((w, h)), Some(t)) = (new_size, self.windows[idx].toplevel()) {
            t.with_pending_state(|s| s.size = Some((w as i32, h as i32).into()));
        }
        if newly_mapped && self.windows[idx].id.is_some() {
            if self.windows[idx].popup().is_none() {
                if self.focus_root.is_none() {
                    self.focus_root = Some(surface.clone());
                }
                if let Some(kb) = self.seat.get_keyboard() {
                    if kb.current_focus().is_none() {
                        kb.set_focus(self, Some(surface.clone()), SERIAL_COUNTER.next_serial());
                    }
                }
            } else {
                self.refresh_keyboard_focus();
            }
        }
        let entry = &mut self.windows[idx];
        if entry.tree.is_some() && !had_damage {
            entry.needs_flush = true;
        }
        if !callbacks.is_empty() {
            entry.callbacks.extend(callbacks);
            if had_damage && entry.id.is_some() {
                if !entry.waiting_send {
                    entry.waiting_since = Instant::now();
                }
                entry.waiting_send = true;
            }
            self.arm_timer();
        }
    }

    fn unmap(&mut self, idx: usize) {
        if self.windows[idx].id.is_none() {
            return;
        }
        let surface = self.windows[idx].surface.clone();
        let mut filhos = self.descendants(&surface);
        filhos.sort_unstable_by(|a, b| b.cmp(a));
        for c in filhos {
            self.unmap_one(c);
        }
        self.unmap_one(idx);
        self.refresh_keyboard_focus();
    }

    fn unmap_one(&mut self, idx: usize) {
        if let Some(p) = self.windows[idx].pending_dmabuf.take() {
            p.buffer.release();
        }
        let Some(id) = self.windows[idx].id.take() else { return };
        self.windows[idx].tree = None;
        self.windows[idx].dirty = Dirty::default();
        self.windows[idx].needs_flush = false;
        let mut st = lock(&self.shared);
        st.windows.retain(|w| w.id != id);
        st.push(SourceEvent::Destroyed { win: id });
        drop(st);
        if self.pointer_win == Some(id) {
            self.pointer_win = None;
        }
        self.pointer_pos.remove(&id);
    }

    fn dismiss_popup(&mut self, idx: usize) {
        let surface = self.windows[idx].surface.clone();
        let mut alvo = self.descendants(&surface);
        alvo.push(idx);
        alvo.sort_unstable_by(|a, b| b.cmp(a));
        for i in alvo {
            if self.windows[i].dismissed {
                continue;
            }
            self.windows[i].dismissed = true;
            if let Some(p) = self.windows[i].popup() {
                p.popup.send_popup_done();
            }
            self.unmap_one(i);
        }
        self.refresh_keyboard_focus();
    }

    fn dismiss_all_grabs(&mut self) {
        let tops: Vec<usize> = (0..self.windows.len())
            .filter(|&i| self.windows[i].is_grab_popup() && self.windows[i].is_live_popup())
            .filter(|&i| {
                let pai = self.windows[i].popup().and_then(|p| p.parent.as_ref()).and_then(|s| self.idx_by_surface(s));
                !pai.is_some_and(|p| self.windows[p].is_grab_popup() && self.windows[p].is_live_popup())
            })
            .collect();
        for i in tops {
            self.dismiss_popup(i);
        }
    }

    fn dismiss_all_popups(&mut self) {
        for i in (0..self.windows.len()).rev() {
            if self.windows[i].popup().is_some() && !self.windows[i].dismissed {
                self.dismiss_popup(i);
            }
        }
        self.swallowed.clear();
    }

    fn desired_keyboard_focus(&self) -> Option<WlSurface> {
        let root = self.focus_root.clone()?;
        let root_idx = self.idx_by_surface(&root)?;
        let popup = (0..self.windows.len()).rev().find(|&i| self.windows[i].is_grab_popup() && self.windows[i].is_live_popup() && self.root_idx(i) == root_idx);
        Some(popup.map_or(root, |i| self.windows[i].surface.clone()))
    }

    fn refresh_keyboard_focus(&mut self) {
        let Some(kb) = self.seat.get_keyboard() else { return };
        let want = self.desired_keyboard_focus();
        if kb.current_focus() != want {
            kb.set_focus(self, want, SERIAL_COUNTER.next_serial());
        }
    }

    fn update_activation(&mut self) {
        for e in &self.windows {
            let Some(t) = e.toplevel() else { continue };
            let ativa = self.focus_root.as_ref() == Some(&e.surface);
            let mudou = t.with_pending_state(|s| if ativa { s.states.set(xdg_toplevel::State::Activated) } else { s.states.unset(xdg_toplevel::State::Activated) });
            if mudou {
                t.send_pending_configure();
            }
        }
    }

    fn click_dismisses(&mut self, win: u32) -> bool {
        let Some(idx) = self.idx_by_id(win) else { return false };
        if self.windows[idx].popup().is_some() {
            return false;
        }
        let alguma = self.windows.iter().any(|e| e.is_grab_popup() && e.is_live_popup());
        if alguma {
            self.dismiss_all_grabs();
        }
        alguma
    }

    fn run_pending_reads(&mut self) {
        for (seat, mime) in std::mem::take(&mut self.pending_reads) {
            self.read_selection(&seat, mime);
        }
    }

    fn read_selection(&mut self, seat: &Seat<App>, mime: String) {
        let Ok((leitura, escrita)) = std::io::pipe() else { return };
        if request_data_device_client_selection(seat, mime, OwnedFd::from(escrita)).is_err() {
            return;
        }
        let arquivo = std::fs::File::from(OwnedFd::from(leitura));
        let mut texto: Vec<u8> = Vec::new();
        let fonte = Generic::new(arquivo, Interest::READ, Mode::Level);
        let token = self.loop_handle.insert_source(fonte, move |_, f, app: &mut App| {
            let mut pedaco = [0u8; 16 * 1024];
            match (&**f).read(&mut pedaco) {
                Ok(0) => {
                    app.selection_read(std::mem::take(&mut texto));
                    Ok(PostAction::Remove)
                }
                Ok(n) => {
                    texto.extend_from_slice(&pedaco[..n]);
                    Ok(if texto.len() > MAX_CLIPBOARD { PostAction::Remove } else { PostAction::Continue })
                }
                Err(e) if matches!(e.kind(), io::ErrorKind::Interrupted | io::ErrorKind::WouldBlock) => Ok(PostAction::Continue),
                Err(_) => Ok(PostAction::Remove),
            }
        });
        if let Ok(token) = token {
            let _ = self.loop_handle.insert_source(Timer::from_duration(SELECTION_READ_TIMEOUT), move |_, _, app| {
                app.loop_handle.remove(token);
                TimeoutAction::Drop
            });
        }
    }

    fn selection_read(&mut self, bytes: Vec<u8>) {
        if bytes.is_empty() || bytes.len() > MAX_CLIPBOARD {
            return;
        }
        let texto = String::from_utf8_lossy(&bytes).into_owned();
        if self.clip_last.as_deref() == Some(texto.as_str()) {
            return;
        }
        self.clip_last = Some(texto.clone());
        lock(&self.shared).push(SourceEvent::Clipboard { mime: MIME_TEXT.to_string(), data: texto.into_bytes() });
    }

    fn host_clipboard(&mut self, mime: &str, data: &[u8]) {
        if mime != MIME_TEXT || data.len() > MAX_CLIPBOARD {
            return;
        }
        let texto = String::from_utf8_lossy(data).into_owned();
        if self.clip_last.as_deref() == Some(texto.as_str()) {
            return;
        }
        self.clip_last = Some(texto.clone());
        self.host_text = Some(texto);
        let (dh, seat) = (self.dh.clone(), self.seat.clone());
        set_data_device_selection(&dh, &seat, TEXT_MIMES.iter().map(|m| (*m).to_string()).collect(), ());
    }

    fn push_cursor(&mut self, win: u32, shape: CursorShape) {
        if self.last_cursor.as_ref().is_some_and(|(w, s)| *w == win && *s == shape) {
            return;
        }
        let mut st = lock(&self.shared);
        if !st.has_cap(CAP_CURSOR) {
            return;
        }
        st.push(SourceEvent::Cursor { win, shape: shape.clone() });
        drop(st);
        self.last_cursor = Some((win, shape));
    }

    fn note_cursor_image(&mut self, surface: &WlSurface, img: CursorImage) {
        if self.cursor_images.len() >= MAX_CURSOR_IMAGES && !self.cursor_images.contains_key(&surface.id()) {
            self.cursor_images.clear();
        }
        self.cursor_images.insert(surface.id(), img);
        if self.cursor_surface.as_ref() == Some(surface) {
            self.send_cursor_surface(surface);
        }
    }

    fn send_cursor_surface(&mut self, surface: &WlSurface) {
        let Some(win) = self.pointer_win else { return };
        let Some(img) = self.cursor_images.get(&surface.id()) else { return };
        let (hx, hy) = cursor_hotspot(surface);
        let shape = CursorShape::Image {
            width: img.width,
            height: img.height,
            hot_x: hx.clamp(i32::from(i16::MIN), i32::from(i16::MAX)) as i16,
            hot_y: hy.clamp(i32::from(i16::MIN), i32::from(i16::MAX)) as i16,
            bgra: img.bgra.clone(),
        };
        self.push_cursor(win, shape);
    }

    fn popup_geometry(&self, parent: Option<&WlSurface>, positioner: PositionerState) -> Rectangle<i32, Logical> {
        let (ax, ay) = parent.and_then(|p| self.idx_by_surface(p)).map_or((0, 0), |i| self.abs_origin(i));
        let alvo = Rectangle::new((-ax, -ay).into(), MONITOR.into());
        positioner.get_unconstrained_geometry(alvo)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum PedidoCursor {
    Oculto,
    Nome(String),
    Superficie(ObjectId, (i32, i32)),
}

fn pedido_repetido(anterior: &Option<(u32, PedidoCursor)>, win: u32, pedido: &PedidoCursor) -> bool {
    anterior.as_ref().is_some_and(|(w, p)| *w == win && p == pedido)
}

fn cursor_hotspot(surface: &WlSurface) -> (i32, i32) {
    with_states(surface, |states| {
        states.data_map.get::<Mutex<CursorImageAttributes>>().map_or((0, 0), |m| {
            let h = m.lock().unwrap_or_else(|e| e.into_inner()).hotspot;
            (h.x, h.y)
        })
    })
}

fn toplevel_texts(surface: &WlSurface) -> (String, String) {
    with_states(surface, |states| {
        states
            .data_map
            .get::<XdgToplevelSurfaceData>()
            .map(|d| {
                let d = d.lock().unwrap_or_else(|e| e.into_inner());
                (d.title.clone().unwrap_or_default(), d.app_id.clone().unwrap_or_default())
            })
            .unwrap_or_default()
    })
}

struct PoolMap<'a> {
    ptr: *const u8,
    len: usize,
    _callback: PhantomData<&'a ()>,
}

impl PoolMap<'_> {
    fn copy(&self, src: usize, dst: &mut [u8]) -> bool {
        match src.checked_add(dst.len()) {
            Some(fim) if fim <= self.len => {}
            _ => return false,
        }
        unsafe {
            std::ptr::copy_nonoverlapping(self.ptr.add(src), dst.as_mut_ptr(), dst.len());
        }
        true
    }
}

fn with_pool<T>(buffer: &WlBuffer, f: impl FnOnce(&PoolMap<'_>, BufferData) -> T) -> Option<T> {
    if let Ok(d) = get_dmabuf(buffer) {
        return dmabuf::com_pixels(d, |ptr, len, data| f(&PoolMap { ptr, len, _callback: PhantomData }, data));
    }
    with_buffer_contents(buffer, |ptr, len, data| f(&PoolMap { ptr, len, _callback: PhantomData }, data)).ok()
}

fn blit(buffer: &WlBuffer, wb: &mut WinBuf, damage: &[Damage], scale: i32, geom: Option<Ret>, old_origin: (i32, i32)) -> Option<(Vec<Rect>, (i32, i32))> {
    blit_into(buffer, (&mut wb.width, &mut wb.height, &mut wb.pixels), damage, scale, geom, old_origin)
}

fn blit_into(buffer: &WlBuffer, (width, height, pixels): (&mut u32, &mut u32, &mut Vec<u8>), damage: &[Damage], scale: i32, geom: Option<Ret>, old_origin: (i32, i32)) -> Option<(Vec<Rect>, (i32, i32))> {
    with_pool(buffer, |map, data| {
        if !matches!(data.format, Format::Xrgb8888 | Format::Argb8888 | Format::Abgr8888 | Format::Xbgr8888) || data.width <= 0 || data.height <= 0 || data.stride < data.width * 4 || data.offset < 0 {
            return None;
        }
        let troca = dmabuf::precisa_trocar_r_b(data.format);
        let (bw, bh) = (data.width as usize, data.height as usize);
        let (stride, offset) = (data.stride as usize, data.offset as usize);
        if offset.checked_add(stride.checked_mul(bh)?)? > map.len {
            return None;
        }
        let tudo = (0, 0, bw as i32, bh as i32);
        let (cx, cy, cw, ch) = geom.and_then(|g| intersect(g, tudo)).unwrap_or(tudo);
        let (cw_u, ch_u) = (cw as u32, ch as u32);
        let mut realloc = false;
        if (cw_u, ch_u) != (*width, *height) || (cx, cy) != old_origin {
            if (cw_u, ch_u) != (*width, *height) {
                *width = cw_u;
                *height = ch_u;
                *pixels = vec![0; cw as usize * ch as usize * 4];
            }
            realloc = true;
        }
        let full = Rect::new(0, 0, cw_u, ch_u);
        let mut rects: Vec<Rect> = Vec::new();
        if realloc || damage.is_empty() {
            rects.push(full);
        } else {
            for d in damage {
                let (x, y, w, h) = match d {
                    Damage::Buffer(r) => (r.loc.x, r.loc.y, r.size.w, r.size.h),
                    Damage::Surface(r) => (r.loc.x * scale, r.loc.y * scale, r.size.w * scale, r.size.h * scale),
                };
                if let Some((ix, iy, iw, ih)) = intersect((x, y, w, h), (cx, cy, cw, ch)) {
                    rects.push(Rect::new((ix - cx) as u32, (iy - cy) as u32, iw as u32, ih as u32));
                }
            }
        }
        for r in &rects {
            for row in r.y..r.y + r.h {
                let src = offset + (cy as usize + row as usize) * stride + (cx as usize + r.x as usize) * 4;
                let dst = (row as usize * cw as usize + r.x as usize) * 4;
                if !map.copy(src, &mut pixels[dst..dst + r.w as usize * 4]) {
                    return None;
                }
                if troca {
                    dmabuf::troca_r_b(&mut pixels[dst..dst + r.w as usize * 4]);
                }
            }
        }
        Some((rects, (cx, cy)))
    })
    .flatten()
}

fn buffer_info(buffer: &WlBuffer) -> Option<(i32, i32, bool)> {
    if let Ok(d) = get_dmabuf(buffer) {
        let s = d.size();
        return Some((s.w, s.h, matches!(d.format().code, Fourcc::Argb8888 | Fourcc::Abgr8888)));
    }
    with_buffer_contents(buffer, |_, _, data| (data.width, data.height, matches!(data.format, Format::Argb8888 | Format::Abgr8888))).ok()
}

fn viewport_dst(surface: &WlSurface) -> Option<(i32, i32)> {
    with_states(surface, |states| states.cached_state.get::<ViewportCachedState>().current().dst.map(|d| (d.w, d.h)))
}

fn logical_size(buffer: (u32, u32), dst: Option<(i32, i32)>, scale: i32) -> (u32, u32) {
    match dst {
        Some((w, h)) => (w.max(1) as u32, h.max(1) as u32),
        None => {
            let s = scale.max(1) as u32;
            (buffer.0.div_ceil(s), buffer.1.div_ceil(s))
        }
    }
}

fn read_layer(buffer: &WlBuffer, layer: &mut Layer, damage: &[Damage], scale: i32, dst: Option<(i32, i32)>) -> Option<Option<Vec<Rect>>> {
    with_pool(buffer, |map, data| {
        if !matches!(data.format, Format::Xrgb8888 | Format::Argb8888 | Format::Abgr8888 | Format::Xbgr8888)
            || data.width <= 0
            || data.height <= 0
            || data.width > dmabuf::MAX_LADO
            || data.height > dmabuf::MAX_LADO
            || data.stride < data.width * 4
            || data.offset < 0
        {
            return None;
        }
        let troca = dmabuf::precisa_trocar_r_b(data.format);
        let (bw, bh) = (data.width as usize, data.height as usize);
        let (stride, offset) = (data.stride as usize, data.offset as usize);
        if offset.checked_add(stride.checked_mul(bh)?)? > map.len {
            return None;
        }
        let (lw, lh) = logical_size((bw as u32, bh as u32), dst, scale);
        let mudou = layer.w != bw as u32 || layer.h != bh as u32 || layer.px.len() != bw * bh * 4;
        if mudou {
            layer.w = bw as u32;
            layer.h = bh as u32;
            layer.px = vec![0; bw * bh * 4];
        }
        layer.alpha = matches!(data.format, Format::Argb8888 | Format::Abgr8888);
        (layer.lw, layer.lh) = (lw, lh);
        let escalada = (lw, lh) != (bw as u32, bh as u32);
        let tudo = mudou || escalada || damage.is_empty();
        let rects: Vec<Rect> = if tudo {
            vec![Rect::new(0, 0, bw as u32, bh as u32)]
        } else {
            damage
                .iter()
                .filter_map(|d| {
                    let (x, y, w, h) = match d {
                        Damage::Buffer(r) => (r.loc.x, r.loc.y, r.size.w, r.size.h),
                        Damage::Surface(r) => (r.loc.x * scale, r.loc.y * scale, r.size.w * scale, r.size.h * scale),
                    };
                    intersect((x, y, w, h), (0, 0, bw as i32, bh as i32)).map(|(x, y, w, h)| Rect::new(x as u32, y as u32, w as u32, h as u32))
                })
                .collect()
        };
        for r in &rects {
            for row in r.y..r.y + r.h {
                let src = offset + row as usize * stride + r.x as usize * 4;
                let dst = (row as usize * bw + r.x as usize) * 4;
                if !map.copy(src, &mut layer.px[dst..dst + r.w as usize * 4]) {
                    return None;
                }
                if troca {
                    dmabuf::troca_r_b(&mut layer.px[dst..dst + r.w as usize * 4]);
                }
            }
        }
        Some(if tudo { None } else { Some(rects) })
    })
    .flatten()
}

fn arrangement(root: &WlSurface, logical: (u32, u32), layers: &HashMap<ObjectId, Layer>) -> Vec<Item<ObjectId>> {
    let rid = root.id();
    let mut out = Vec::new();
    let desloc = |s: &WlSurface, data: &SurfaceData| -> (i32, i32) {
        if s.id() == rid {
            return (0, 0);
        }
        let mut g = data.cached_state.get::<SubsurfaceCachedState>();
        let l = g.current().location;
        (l.x, l.y)
    };
    with_surface_tree_upward(
        root,
        (0i32, 0i32),
        |s, data, &(px, py)| {
            let (x, y) = desloc(s, data);
            TraversalAction::DoChildren((px + x, py + y))
        },
        |s, data, &(px, py)| {
            let (x, y) = desloc(s, data);
            if s.id() == rid {
                out.push(Item { key: s.id(), root: true, x: 0, y: 0, w: logical.0, h: logical.1 });
            } else if let Some(l) = layers.get(&s.id()) {
                out.push(Item { key: s.id(), root: false, x: px + x, y: py + y, w: l.lw, h: l.lh });
            }
        },
        |_, _, _| true,
    );
    out
}

fn tem_subs(root: &WlSurface, layers: &HashMap<ObjectId, Layer>) -> bool {
    !layers.is_empty() && !get_children(root).is_empty() && arrangement(root, (0, 0), layers).len() > 1
}

fn precisa_arvore(layers: &HashMap<ObjectId, Layer>, entry: &WinEntry, root: &WlSurface, info: Option<(i32, i32, bool)>, dst: Option<(i32, i32)>) -> bool {
    entry.tree.is_some() || info.is_some_and(|(w, h, _)| dst.is_some_and(|d| d != (w, h))) || tem_subs(root, layers)
}

#[allow(clippy::too_many_arguments)]
fn blit_raiz(
    layers: &HashMap<ObjectId, Layer>,
    entry: &mut WinEntry,
    surface: &WlSurface,
    buffer: Option<&WlBuffer>,
    wb: &mut WinBuf,
    damage: &[Damage],
    scale: i32,
    geom: Option<Ret>,
    dst: Option<(i32, i32)>,
    info: Option<(i32, i32, bool)>,
    arvore: bool,
) -> Option<(Vec<Rect>, (i32, i32))> {
    if arvore {
        return blit_tree(layers, entry, surface, buffer, wb, damage, scale, geom, dst);
    }
    let r = blit(buffer?, wb, damage, scale, geom, entry.origin)?;
    if let Some((w, h, alpha)) = info {
        entry.root_alpha = alpha;
        entry.root_logical = (w as u32, h as u32);
    }
    Some(r)
}

fn renderiza_escalada(t: &mut Tree<ObjectId>, dst: (i32, i32), geom: Option<Ret>) -> Option<(Vec<Rect>, (i32, i32))> {
    let Tree { root, root_w, root_h, root_logical, root_src, .. } = t;
    let src = root_src.as_ref()?;
    let (lw, lh) = (dst.0.max(1), dst.1.max(1));
    *root_logical = (lw as u32, lh as u32);
    let tudo = (0, 0, lw, lh);
    let (cx, cy, cw, ch) = geom.and_then(|g| intersect(g, tudo)).unwrap_or(tudo);
    if (cw as u32, ch as u32) != (*root_w, *root_h) {
        *root_w = cw as u32;
        *root_h = ch as u32;
        *root = vec![0; cw as usize * ch as usize * 4];
    }
    estica(src, (lw as u32, lh as u32), (cx, cy, cw, ch), root);
    Some((vec![Rect::new(0, 0, cw as u32, ch as u32)], (cx, cy)))
}

#[allow(clippy::too_many_arguments)]
fn blit_tree(
    layers: &HashMap<ObjectId, Layer>,
    entry: &mut WinEntry,
    surface: &WlSurface,
    buffer: Option<&WlBuffer>,
    wb: &mut WinBuf,
    damage: &[Damage],
    scale: i32,
    geom: Option<Ret>,
    dst: Option<(i32, i32)>,
) -> Option<(Vec<Rect>, (i32, i32))> {
    let old_origin = entry.origin;
    let tree = entry.tree.get_or_insert_with(Tree::vazia);
    let (rects_raiz, origin) = match buffer {
        Some(b) => {
            let (bw, bh, alpha) = buffer_info(b)?;
            match dst.filter(|d| *d != (bw, bh)) {
                Some(d) => {
                    let src = tree.root_src.get_or_insert_with(Layer::default);
                    read_layer(b, src, &[], scale, dst)?;
                    tree.root_alpha = alpha;
                    renderiza_escalada(tree, d, geom)?
                }
                None => {
                    tree.root_src = None;
                    let r = blit_into(b, (&mut tree.root_w, &mut tree.root_h, &mut tree.root), damage, scale, geom, old_origin)?;
                    tree.root_alpha = alpha;
                    tree.root_logical = (bw as u32, bh as u32);
                    r
                }
            }
        }
        None => renderiza_escalada(tree, dst?, geom)?,
    };
    tree.root_dst = dst;
    let dims = (tree.root_w, tree.root_h);
    let mut tudo = false;
    if (wb.width, wb.height) != dims {
        wb.width = dims.0;
        wb.height = dims.1;
        wb.pixels = vec![0; dims.0 as usize * dims.1 as usize * 4];
        tudo = true;
    }
    let items = arrangement(surface, tree.root_logical, layers);
    let rects = recompoe(tree, items, layers, &mut entry.dirty, origin, dims, &mut wb.pixels, rects_raiz, tudo);
    entry.needs_flush = false;
    entry.root_alpha = tree.root_alpha;
    entry.root_logical = tree.root_logical;
    Some((rects, origin))
}

fn read_cursor(buffer: &WlBuffer) -> Option<CursorImage> {
    with_pool(buffer, |map, data| {
        let side = i32::from(MAX_CURSOR_SIDE);
        if !matches!(data.format, Format::Xrgb8888 | Format::Argb8888) || data.width <= 0 || data.height <= 0 || data.width > side || data.height > side || data.stride < data.width * 4 || data.offset < 0 {
            return None;
        }
        let (w, h) = (data.width as usize, data.height as usize);
        let (stride, offset) = (data.stride as usize, data.offset as usize);
        let mut bgra = vec![0u8; w * h * 4];
        for row in 0..h {
            if !map.copy(offset + row * stride, &mut bgra[row * w * 4..(row + 1) * w * 4]) {
                return None;
            }
        }
        if data.format == Format::Xrgb8888 {
            for px in bgra.chunks_exact_mut(4) {
                px[3] = 0xFF;
            }
        }
        Some(CursorImage { width: w as u16, height: h as u16, bgra })
    })
    .flatten()
}

impl BufferHandler for App {
    fn buffer_destroyed(&mut self, _buffer: &WlBuffer) {}
}

impl CompositorHandler for App {
    fn compositor_state(&mut self) -> &mut CompositorState {
        &mut self.compositor_state
    }

    fn client_compositor_state<'a>(&self, client: &'a Client) -> &'a CompositorClientState {
        &client.get_data::<ClientState>().expect("dados do cliente").compositor
    }

    fn commit(&mut self, surface: &WlSurface) {
        if let Some(idx) = self.idx_by_surface(surface) {
            self.commit_window(idx);
            return;
        }
        let root = Self::root_of(surface);
        if root != *surface {
            if let Some(ridx) = self.idx_by_surface(&root) {
                self.commit_sub(ridx, surface);
                return;
            }
        }
        let (assign, callbacks) = with_states(surface, |states| {
            let mut guard = states.cached_state.get::<SurfaceAttributes>();
            let attrs = guard.current();
            (attrs.buffer.take(), std::mem::take(&mut attrs.frame_callbacks))
        });
        if let Some(BufferAssignment::NewBuffer(b)) = assign {
            if matches!(get_role(surface), None | Some(CURSOR_ROLE)) {
                if let Some(img) = read_cursor(&b) {
                    self.note_cursor_image(surface, img);
                }
            }
            b.release();
        }
        if callbacks.is_empty() {
            return;
        }
        let root = Self::root_of(surface);
        let time = self.now_ms();
        if root != *surface {
            if let Some(i) = self.idx_by_surface(&root) {
                self.windows[i].callbacks.extend(callbacks);
                self.arm_timer();
                return;
            }
        }
        for cb in callbacks {
            cb.done(time);
        }
    }

    fn destroyed(&mut self, surface: &WlSurface) {
        let id = surface.id();
        if self.layers.remove(&id).is_some() {
            for e in &mut self.windows {
                if e.tree.as_ref().is_some_and(|t| t.sig.iter().any(|i| i.key == id)) {
                    e.dirty.all = true;
                    e.needs_flush = true;
                }
            }
        }
        for e in &mut self.windows {
            if let Some(pos) = e.pending_subs.iter().position(|p| p.surface == *surface) {
                e.pending_subs.remove(pos).buffer.release();
            }
        }
        self.cursor_images.remove(&surface.id());
        if self.cursor_surface.as_ref() == Some(surface) {
            self.cursor_surface = None;
        }
        if let Some(idx) = self.idx_by_surface(surface) {
            self.unmap(idx);
        }
    }
}

impl ShmHandler for App {
    fn shm_state(&self) -> &ShmState {
        &self.shm_state
    }
}

impl XdgShellHandler for App {
    fn xdg_shell_state(&mut self) -> &mut XdgShellState {
        &mut self.xdg_shell_state
    }

    fn new_toplevel(&mut self, surface: ToplevelSurface) {
        let ativa = self.focus_root.is_none();
        surface.with_pending_state(|s| {
            s.size = Some((INITIAL_SIZE.0 as i32, INITIAL_SIZE.1 as i32).into());
            if ativa {
                s.states.set(xdg_toplevel::State::Activated);
            }
            s.states.set(xdg_toplevel::State::TiledLeft);
            s.states.set(xdg_toplevel::State::TiledRight);
            s.states.set(xdg_toplevel::State::TiledTop);
            s.states.set(xdg_toplevel::State::TiledBottom);
            s.decoration_mode = Some(DecoMode::ServerSide);
        });
        self.output.enter(surface.wl_surface());
        self.windows.push(WinEntry::new(surface.wl_surface().clone(), Role::Toplevel(surface)));
    }

    fn toplevel_destroyed(&mut self, surface: ToplevelSurface) {
        if let Some(idx) = self.idx_by_surface(surface.wl_surface()) {
            self.unmap(idx);
            if self.focus_root.as_ref() == Some(surface.wl_surface()) {
                self.focus_root = None;
            }
            if let Some(kb) = self.seat.get_keyboard() {
                if kb.current_focus().as_ref() == Some(surface.wl_surface()) {
                    kb.set_focus(self, None, SERIAL_COUNTER.next_serial());
                }
            }
            if let Some(i) = self.idx_by_surface(surface.wl_surface()) {
                let mut e = self.windows.remove(i);
                e.callbacks.clear();
            }
        }
    }

    fn new_popup(&mut self, surface: PopupSurface, positioner: PositionerState) {
        let parent = surface.get_parent_surface();
        let geo = self.popup_geometry(parent.as_ref(), positioner);
        surface.with_pending_state(|s| {
            s.geometry = geo;
            s.positioner = positioner;
        });
        let _ = surface.send_configure();
        let data = PopupData { popup: surface.clone(), parent, rel: (geo.loc.x, geo.loc.y), grab: false };
        let mut entry = WinEntry::new(surface.wl_surface().clone(), Role::Popup(data));
        let aceita = lock(&self.shared).has_cap(CAP_POPUPS) && entry.popup().is_some_and(|p| p.parent.is_some());
        if !aceita {
            entry.dismissed = true;
            surface.send_popup_done();
        }
        self.windows.push(entry);
    }

    fn grab(&mut self, surface: PopupSurface, _seat: WlSeat, _serial: Serial) {
        let Some(idx) = self.idx_by_surface(surface.wl_surface()) else { return };
        if let Role::Popup(p) = &mut self.windows[idx].role {
            p.grab = true;
        }
        if self.windows[idx].is_live_popup() {
            self.refresh_keyboard_focus();
        }
    }

    fn reposition_request(&mut self, surface: PopupSurface, positioner: PositionerState, token: u32) {
        let Some(idx) = self.idx_by_surface(surface.wl_surface()) else { return };
        let parent = self.windows[idx].popup().and_then(|p| p.parent.clone());
        let geo = self.popup_geometry(parent.as_ref(), positioner);
        surface.with_pending_state(|s| {
            s.geometry = geo;
            s.positioner = positioner;
        });
        surface.send_repositioned(token);
        if let Role::Popup(p) = &mut self.windows[idx].role {
            p.rel = (geo.loc.x, geo.loc.y);
        }
        if let Some(id) = self.windows[idx].id {
            let mut st = lock(&self.shared);
            let grab = self.windows[idx].is_grab_popup();
            if let Some(w) = st.windows.iter_mut().find(|w| w.id == id) {
                w.popup = Some((geo.loc.x, geo.loc.y, grab));
            }
            st.push(SourceEvent::PopupMoved { win: id, x: geo.loc.x, y: geo.loc.y, width: geo.size.w.max(0) as u32, height: geo.size.h.max(0) as u32 });
        }
    }

    fn popup_destroyed(&mut self, surface: PopupSurface) {
        if let Some(idx) = self.idx_by_surface(surface.wl_surface()) {
            self.unmap(idx);
            self.windows.remove(idx);
            self.refresh_keyboard_focus();
        }
    }

    fn title_changed(&mut self, surface: ToplevelSurface) {
        let Some(id) = self.idx_by_surface(surface.wl_surface()).and_then(|i| self.windows[i].id) else { return };
        let (title, app_id) = toplevel_texts(surface.wl_surface());
        let mut st = lock(&self.shared);
        if let Some(w) = st.windows.iter_mut().find(|w| w.id == id) {
            w.title = title.clone();
            w.app_id = app_id;
        }
        st.push(SourceEvent::Title { win: id, title });
    }

    fn parent_changed(&mut self, surface: ToplevelSurface) {
        let Some(idx) = self.idx_by_surface(surface.wl_surface()) else { return };
        let Some(id) = self.windows[idx].id else { return };
        let parent = self.parent_id(idx);
        let mut st = lock(&self.shared);
        if let Some(w) = st.windows.iter_mut().find(|w| w.id == id) {
            w.parent = parent;
        }
        st.push(SourceEvent::Parent { win: id, parent });
    }
}

impl XdgDecorationHandler for App {
    fn new_decoration(&mut self, toplevel: ToplevelSurface) {
        toplevel.with_pending_state(|s| s.decoration_mode = Some(DecoMode::ServerSide));
        if toplevel.is_initial_configure_sent() {
            toplevel.send_pending_configure();
        }
    }

    fn request_mode(&mut self, toplevel: ToplevelSurface, _mode: DecoMode) {
        toplevel.with_pending_state(|s| s.decoration_mode = Some(DecoMode::ServerSide));
        toplevel.send_pending_configure();
    }

    fn unset_mode(&mut self, toplevel: ToplevelSurface) {
        toplevel.with_pending_state(|s| s.decoration_mode = Some(DecoMode::ServerSide));
        toplevel.send_pending_configure();
    }
}

impl KdeDecorationHandler for App {
    fn kde_decoration_state(&self) -> &KdeDecorationState {
        &self.kde_decoration_state
    }

    fn request_mode(&mut self, _surface: &WlSurface, decoration: &OrgKdeKwinServerDecoration, _mode: WEnum<KdeMode>) {
        decoration.mode(KdeMode::Server);
    }
}

impl SeatHandler for App {
    type KeyboardFocus = WlSurface;
    type PointerFocus = WlSurface;
    type TouchFocus = WlSurface;

    fn seat_state(&mut self) -> &mut SeatState<Self> {
        &mut self.seat_state
    }

    fn focus_changed(&mut self, seat: &Seat<Self>, focused: Option<&WlSurface>) {
        let dh = self.dh.clone();
        let client = focused.and_then(|s| dh.get_client(s.id()).ok());
        set_data_device_focus(&dh, seat, client.clone());
        set_primary_focus(&dh, seat, client);
    }

    fn cursor_image(&mut self, _seat: &Seat<Self>, image: CursorImageStatus) {
        let Some(win) = self.pointer_win else { return };
        let pedido = match &image {
            CursorImageStatus::Hidden => PedidoCursor::Oculto,
            CursorImageStatus::Named(icon) => PedidoCursor::Nome(icon.name().to_string()),
            CursorImageStatus::Surface(s) => PedidoCursor::Superficie(s.id(), cursor_hotspot(s)),
        };
        if pedido_repetido(&self.last_cursor_req, win, &pedido) {
            return;
        }
        self.last_cursor_req = Some((win, pedido));
        match image {
            CursorImageStatus::Hidden => self.push_cursor(win, CursorShape::Hidden),
            CursorImageStatus::Named(icon) => self.push_cursor(win, CursorShape::Named(icon.name().to_string())),
            CursorImageStatus::Surface(s) => {
                self.cursor_surface = Some(s.clone());
                self.send_cursor_surface(&s);
            }
        }
    }
}

impl TabletSeatHandler for App {}

impl OutputHandler for App {}

impl SelectionHandler for App {
    type SelectionUserData = ();

    fn new_selection(&mut self, ty: SelectionTarget, source: Option<SelectionSource>, seat: Seat<Self>) {
        if ty != SelectionTarget::Clipboard {
            return;
        }
        let Some(source) = source else { return };
        if !lock(&self.shared).has_cap(CAP_CLIPBOARD) {
            return;
        }
        let oferecidos = source.mime_types();
        let Some(mime) = TEXT_MIMES.iter().find(|m| oferecidos.iter().any(|o| o == **m)) else { return };
        self.pending_reads.push((seat, (*mime).to_string()));
    }

    fn send_selection(&mut self, ty: SelectionTarget, mime_type: String, fd: OwnedFd, _seat: Seat<Self>, _user_data: &()) {
        if ty != SelectionTarget::Clipboard || !TEXT_MIMES.contains(&mime_type.as_str()) {
            return;
        }
        let Some(texto) = self.host_text.clone() else { return };
        std::thread::spawn(move || {
            let mut f = std::fs::File::from(fd);
            let _ = f.write_all(texto.as_bytes());
        });
    }
}

impl PrimarySelectionHandler for App {
    fn primary_selection_state(&self) -> &PrimarySelectionState {
        &self.primary_selection_state
    }
}

impl DataDeviceHandler for App {
    fn data_device_state(&self) -> &DataDeviceState {
        &self.data_device_state
    }
}

impl ClientDndGrabHandler for App {}
impl ServerDndGrabHandler for App {
    fn send(&mut self, _mime_type: String, _fd: OwnedFd, _seat: Seat<Self>) {}
}

delegate_compositor!(App);
delegate_shm!(App);
delegate_dmabuf!(App);

impl DmabufHandler for App {
    fn dmabuf_state(&mut self) -> &mut DmabufState {
        &mut self.dmabuf_state
    }

    fn dmabuf_imported(&mut self, _global: &DmabufGlobal, dmabuf: Dmabuf, notifier: ImportNotifier) {
        match dmabuf::validar(&dmabuf) {
            Err(r) => dmabuf::recusar(notifier, r),
            Ok(()) if !dmabuf::mapeavel(&dmabuf) => notifier.failed(),
            Ok(()) => {
                let _ = notifier.successful::<App>();
            }
        }
    }
}
delegate_xdg_shell!(App);
delegate_xdg_decoration!(App);
delegate_kde_decoration!(App);
delegate_seat!(App);
delegate_data_device!(App);
delegate_primary_selection!(App);
delegate_output!(App);
delegate_cursor_shape!(App);
delegate_viewporter!(App);

impl App {
    fn handle_cmd(&mut self, cmd: Cmd) {
        match cmd {
            Cmd::Quit => self.quit = true,
            Cmd::FrameSent(win) => {
                let time = self.now_ms();
                if let Some(e) = self.windows.iter_mut().find(|e| e.id == Some(win)) {
                    Self::fire_callbacks(e, time);
                }
            }
            Cmd::Input(msg) => self.handle_input(msg),
            Cmd::SessionEnded => {
                if let Some(kb) = self.seat.get_keyboard() {
                    let presas: Vec<Keycode> = kb.pressed_keys().into_iter().collect();
                    let time = self.now_ms();
                    for k in presas {
                        kb.input::<(), _>(self, k, KeyState::Released, SERIAL_COUNTER.next_serial(), time, |_, _, _| FilterResult::Forward);
                    }
                }
                self.dismiss_all_popups();
                self.last_cursor = None;
                self.last_cursor_req = None;
            }
        }
    }

    fn target_of(&self, win: u32) -> Option<(WlSurface, (i32, i32))> {
        let e = self.windows.iter().find(|e| e.id == Some(win))?;
        Some((e.surface.clone(), e.origin))
    }

    fn point_at(&mut self, x: f64, y: f64, surface: &WlSurface, origin: (i32, i32)) {
        let Some(ptr) = self.seat.get_pointer() else { return };
        ptr.motion(
            self,
            Some((surface.clone(), (-f64::from(origin.0), -f64::from(origin.1)).into())),
            &MotionEvent { location: (x, y).into(), serial: SERIAL_COUNTER.next_serial(), time: self.now_ms() },
        );
    }

    fn refocus_pointer(&mut self, win: u32, surface: &WlSurface, origin: (i32, i32)) {
        let (x, y) = self.pointer_pos.get(&win).copied().unwrap_or((0.0, 0.0));
        self.pointer_win = Some(win);
        self.point_at(x, y, surface, origin);
        if let Some(ptr) = self.seat.get_pointer() {
            ptr.frame(self);
        }
        self.point_at(x, y, surface, origin);
    }

    fn handle_input(&mut self, msg: Message) {
        match msg {
            Message::PointerMove { win, x, y } => {
                let Some((surface, origin)) = self.target_of(win) else { return };
                self.pointer_pos.insert(win, (f64::from(x), f64::from(y)));
                self.pointer_win = Some(win);
                let entrando = self.seat.get_pointer().is_some_and(|p| p.current_focus().as_ref() != Some(&surface));
                self.point_at(f64::from(x), f64::from(y), &surface, origin);
                if entrando {
                    if let Some(ptr) = self.seat.get_pointer() {
                        ptr.frame(self);
                    }
                    self.point_at(f64::from(x), f64::from(y), &surface, origin);
                }
                if let Some(ptr) = self.seat.get_pointer() {
                    ptr.frame(self);
                }
            }
            Message::PointerButton { win, button, pressed } => {
                let Some((surface, origin)) = self.target_of(win) else { return };
                let Some(ptr) = self.seat.get_pointer() else { return };
                if pressed {
                    if self.click_dismisses(win) {
                        self.swallowed.push(button);
                        return;
                    }
                } else if let Some(i) = self.swallowed.iter().position(|b| *b == button) {
                    self.swallowed.remove(i);
                    return;
                }
                if ptr.current_focus().as_ref() != Some(&surface) {
                    self.refocus_pointer(win, &surface, origin);
                }
                let state = if pressed { ButtonState::Pressed } else { ButtonState::Released };
                ptr.button(self, &ButtonEvent { serial: SERIAL_COUNTER.next_serial(), time: self.now_ms(), button, state });
                ptr.frame(self);
            }
            Message::PointerAxis { win, dx, dy } => {
                let Some((surface, origin)) = self.target_of(win) else { return };
                let Some(ptr) = self.seat.get_pointer() else { return };
                if ptr.current_focus().as_ref() != Some(&surface) {
                    self.refocus_pointer(win, &surface, origin);
                }
                let mut frame = AxisFrame::new(self.now_ms()).source(AxisSource::Wheel);
                for (axis, v) in [(Axis::Horizontal, dx), (Axis::Vertical, dy)] {
                    if v != 0.0 {
                        frame = frame.value(axis, f64::from(v)).v120(axis, (f64::from(v) / 15.0 * 120.0).round() as i32);
                    }
                }
                ptr.axis(self, frame);
                ptr.frame(self);
            }
            Message::Key { win, keycode, pressed } => {
                let Some(kb) = self.seat.get_keyboard() else { return };
                if self.target_of(win).is_none() {
                    return;
                }
                let state = if pressed { KeyState::Pressed } else { KeyState::Released };
                let time = self.now_ms();
                kb.input::<(), _>(self, Keycode::new(keycode + 8), state, SERIAL_COUNTER.next_serial(), time, |_, _, _| FilterResult::Forward);
            }
            Message::Focus { win, focused } => {
                let Some(idx) = self.idx_by_id(win) else { return };
                if self.windows[idx].popup().is_some() {
                    return;
                }
                let surface = self.windows[idx].surface.clone();
                if focused {
                    self.focus_root = Some(surface);
                    self.update_activation();
                    self.refresh_keyboard_focus();
                } else if self.focus_root.as_ref() == Some(&surface) {
                    self.dismiss_all_grabs();
                    self.focus_root = None;
                    self.update_activation();
                    self.refresh_keyboard_focus();
                }
            }
            Message::ResizeReq { win, width, height } => {
                let Some(idx) = self.idx_by_id(win) else { return };
                let Some(t) = self.windows[idx].toplevel() else { return };
                let (w, h) = (width.clamp(MIN_SIZE, MAX_SIZE) as i32, height.clamp(MIN_SIZE, MAX_SIZE) as i32);
                t.with_pending_state(|s| s.size = Some((w, h).into()));
                t.send_pending_configure();
            }
            Message::ClipboardUp { mime, data } => self.host_clipboard(&mime, &data),
            Message::CloseReq { win } => {
                let Some(idx) = self.idx_by_id(win) else { return };
                if self.windows[idx].popup().is_some() {
                    self.dismiss_popup(idx);
                } else if let Some(t) = self.windows[idx].toplevel() {
                    t.send_close();
                }
            }
            _ => {}
        }
    }
}

const NOME_LAYOUT_XKB: &str = "Portuguese (Brazil)";
const NOME_LAYOUT_PSD: &str = "Brazilian (ABNT2)";

pub(crate) fn renomear_layout(keymap: &str) -> String {
    keymap.replace(&format!("\"{NOME_LAYOUT_XKB}\""), &format!("\"{NOME_LAYOUT_PSD}\""))
}

pub(crate) fn keymap_br_texto() -> Option<String> {
    use smithay::input::keyboard::xkb;
    let contexto = xkb::Context::new(xkb::CONTEXT_NO_FLAGS);
    let keymap = xkb::Keymap::new_from_names(&contexto, "", "", "br", "", None, xkb::KEYMAP_COMPILE_NO_FLAGS)?;
    Some(renomear_layout(&keymap.get_as_string(xkb::KEYMAP_FORMAT_TEXT_V1)))
}

pub(crate) fn run(listener: ListeningSocket, shared: Shared, rx: channel::Channel<Cmd>) -> io::Result<()> {
    let mut display: Display<App> = Display::new().map_err(io::Error::other)?;
    let dh = display.handle();
    let mut event_loop: EventLoop<'static, App> = EventLoop::try_new().map_err(io::Error::other)?;
    let loop_handle = event_loop.handle();

    let mut seat_state = SeatState::new();
    let mut seat = seat_state.new_wl_seat(&dh, "pistick");
    seat.add_pointer();
    let xkb = XkbConfig { layout: "br", ..XkbConfig::default() };
    seat.add_keyboard(xkb, 600, 25).map_err(|e| io::Error::other(format!("teclado (xkb layout br): {e:?}")))?;

    let output = Output::new(
        "PISTICK-1".to_string(),
        PhysicalProperties { size: (0, 0).into(), subpixel: Subpixel::Unknown, make: "PiStick".into(), model: "virtual".into() },
    );
    let _ = output.create_global::<App>(&dh);
    let mode = OutMode { size: MONITOR.into(), refresh: 60_000 };
    output.change_current_state(Some(mode), Some(smithay::utils::Transform::Normal), Some(Scale::Integer(1)), Some((0, 0).into()));
    output.set_preferred(mode);

    let mut dmabuf_state = DmabufState::new();
    let dmabuf_global = dmabuf::dispositivo().and_then(|dev| DmabufFeedbackBuilder::new(dev, dmabuf::formatos()).build().ok()).map(|fb| dmabuf_state.create_global_with_default_feedback::<App>(&dh, &fb));
    if dmabuf_global.is_none() {
        eprintln!("[psd] sem nó de renderização: DMABUF desligado (os apps caem em memória comum)");
    }

    let mut app = App {
        dh: dh.clone(),
        loop_handle: loop_handle.clone(),
        shared,
        start: Instant::now(),
        quit: false,
        timer_armed: false,
        compositor_state: CompositorState::new::<App>(&dh),
        xdg_shell_state: XdgShellState::new::<App>(&dh),
        shm_state: ShmState::new::<App>(&dh, vec![]),
        dmabuf_state,
        dmabuf_global,
        seat_state,
        data_device_state: DataDeviceState::new::<App>(&dh),
        primary_selection_state: PrimarySelectionState::new::<App>(&dh),
        cursor_shape_state: CursorShapeManagerState::new::<App>(&dh),
        viewporter_state: ViewporterState::new::<App>(&dh),
        output_state: OutputManagerState::new_with_xdg_output::<App>(&dh),
        decoration_state: XdgDecorationState::new::<App>(&dh),
        kde_decoration_state: KdeDecorationState::new::<App>(&dh, DefaultMode::Server),
        seat,
        output,
        windows: Vec::new(),
        layers: HashMap::new(),
        pointer_pos: HashMap::new(),
        pointer_win: None,
        focus_root: None,
        swallowed: Vec::new(),
        cursor_images: HashMap::new(),
        cursor_surface: None,
        last_cursor: None,
        last_cursor_req: None,
        clip_last: None,
        host_text: None,
        pending_reads: Vec::new(),
    };

    if let (Some(kb), Some(texto)) = (app.seat.get_keyboard(), keymap_br_texto()) {
        if let Err(e) = kb.set_keymap_from_string(&mut app, texto) {
            eprintln!("[psd] keymap com o nome de layout trocado recusado ({e:?}); fica o do xkb");
        }
    }

    loop_handle
        .insert_source(Generic::new(listener, Interest::READ, Mode::Level), |_, listener, app: &mut App| {
            while let Some(stream) = listener.accept()? {
                if let Err(e) = app.dh.insert_client(stream, Arc::new(ClientState::default())) {
                    eprintln!("[psd] cliente Wayland recusado: {e}");
                }
            }
            Ok(PostAction::Continue)
        })
        .map_err(|e| io::Error::other(e.to_string()))?;

    let poll_fd = display.as_fd().try_clone_to_owned()?;
    loop_handle
        .insert_source(Generic::new(poll_fd, Interest::READ, Mode::Level), |_, _, _| Ok(PostAction::Continue))
        .map_err(|e| io::Error::other(e.to_string()))?;

    loop_handle
        .insert_source(rx, |ev, _, app: &mut App| {
            if let channel::Event::Msg(cmd) = ev {
                app.handle_cmd(cmd);
            }
        })
        .map_err(|e| io::Error::other(e.to_string()))?;

    while !app.quit {
        event_loop.dispatch(Some(Duration::from_millis(500)), &mut app)?;
        display.dispatch_clients(&mut app)?;
        app.flush_trees();
        app.run_pending_reads();
        display.flush_clients()?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renomear_layout_troca_so_o_nome_entre_aspas() {
        let km = "xkb_symbols \"pc+br+inet(evdev)\" {
    name[Group1]= \"Portuguese (Brazil)\";
    key <AC10> { [ ccedilla, Ccedilla ] };
};";
        let novo = renomear_layout(km);
        assert!(novo.contains("name[Group1]= \"Brazilian (ABNT2)\";"), "{novo}");
        assert!(!novo.contains("Portuguese"), "{novo}");
        assert!(novo.contains("key <AC10> { [ ccedilla, Ccedilla ] };"));
        let us = "name[Group1]= \"English (US)\";";
        assert_eq!(renomear_layout(us), us);
    }

    #[test]
    fn keymap_br_tem_o_nome_novo_e_continua_abnt2() {
        use smithay::input::keyboard::xkb;
        let texto = keymap_br_texto().expect("keymap br (precisa de xkb-data)");
        let contexto = xkb::Context::new(xkb::CONTEXT_NO_FLAGS);
        let km = xkb::Keymap::new_from_string(&contexto, texto, xkb::KEYMAP_FORMAT_TEXT_V1, xkb::KEYMAP_COMPILE_NO_FLAGS).expect("o keymap com o nome novo compila");
        assert_eq!(km.layout_get_name(0), NOME_LAYOUT_PSD);
        let sym = |evdev: u32| km.key_get_syms_by_level(xkb::Keycode::new(evdev + 8), 0, 0).first().map(|s| s.raw());
        assert_eq!(sym(39), Some(xkb::keysyms::KEY_ccedilla));
        assert_eq!(sym(89), Some(xkb::keysyms::KEY_slash));
        assert_eq!(sym(26), Some(xkb::keysyms::KEY_dead_acute));
    }

    #[test]
    fn set_cursor_igual_ao_anterior_para_a_mesma_janela_e_ignorado() {
        let nome = PedidoCursor::Nome("text".into());
        assert!(!pedido_repetido(&None, 1, &PedidoCursor::Oculto));
        let anterior = Some((1, PedidoCursor::Oculto));
        assert!(pedido_repetido(&anterior, 1, &PedidoCursor::Oculto));
        assert!(!pedido_repetido(&anterior, 1, &nome));
        assert!(!pedido_repetido(&anterior, 2, &PedidoCursor::Oculto));
        let anterior = Some((3, nome.clone()));
        assert!(pedido_repetido(&anterior, 3, &PedidoCursor::Nome("text".into())));
        assert!(!pedido_repetido(&anterior, 3, &PedidoCursor::Nome("pointer".into())));
    }
}
