
mod dmabuf;
mod state;
mod tree;

use std::io;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::JoinHandle;
use std::time::Duration;

use ps_codec::Rect;
use ps_proto::Message;
use smithay::reexports::calloop::channel::{self, Sender};

use crate::source::{SourceEvent, Waker, WindowSource};

pub const INITIAL_SIZE: (u32, u32) = (960, 600);

pub enum SocketSpec {
    Auto(String),
    Path(PathBuf),
}

pub(crate) struct WinBuf {
    pub id: u32,
    pub width: u32,
    pub height: u32,
    pub title: String,
    pub app_id: String,
    pub pixels: Vec<u8>,
    pub parent: u32,
    pub popup: Option<(i32, i32, bool)>,
    pub limits: (u32, u32, u32, u32),
}

#[derive(Default)]
pub(crate) struct Store {
    pub windows: Vec<WinBuf>,
    pub events: Vec<SourceEvent>,
    pub waker: Option<Waker>,
    pub caps: u32,
    next_id: u32,
}

impl Store {
    pub fn session_active(&self) -> bool {
        self.waker.is_some()
    }

    pub fn has_cap(&self, cap: u32) -> bool {
        self.session_active() && self.caps & cap != 0
    }

    pub fn alloc_id(&mut self) -> u32 {
        self.next_id += 1;
        self.next_id
    }

    pub fn push(&mut self, ev: SourceEvent) {
        if let Some(w) = &self.waker {
            self.events.push(ev);
            w();
        }
    }
}

pub(crate) type Shared = Arc<Mutex<Store>>;

pub(crate) fn lock(shared: &Shared) -> MutexGuard<'_, Store> {
    shared.lock().unwrap_or_else(|e| e.into_inner())
}

pub(crate) enum Cmd {
    Input(Message),
    FrameSent(u32),
    SessionEnded,
    Quit,
}

pub struct WaylandCompositor {
    shared: Shared,
    tx: Sender<Cmd>,
    thread: Option<JoinHandle<()>>,
    pub socket_name: String,
    pub socket_path: PathBuf,
}

impl WaylandCompositor {
    pub fn start(spec: SocketSpec) -> io::Result<WaylandCompositor> {
        let shared: Shared = Arc::default();
        let (tx, rx) = channel::channel::<Cmd>();
        let (listener, name, path) = state::bind(spec)?;
        let sh = shared.clone();
        let thread = std::thread::Builder::new()
            .name("compositor".into())
            .spawn(move || {
                if let Err(e) = state::run(listener, sh, rx) {
                    eprintln!("[psd] compositor Wayland terminou com erro: {e}");
                }
            })?;
        Ok(WaylandCompositor { shared, tx, thread: Some(thread), socket_name: name, socket_path: path })
    }

    pub fn source(&self) -> Box<dyn WindowSource> {
        Box::new(WaylandSource { shared: self.shared.clone(), tx: self.tx.clone() })
    }

    pub fn shutdown(mut self) {
        let _ = self.tx.send(Cmd::Quit);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

pub struct WaylandSource {
    shared: Shared,
    tx: Sender<Cmd>,
}

impl WindowSource for WaylandSource {
    fn set_client_caps(&mut self, caps: u32) {
        lock(&self.shared).caps = caps;
    }

    fn start(&mut self, waker: Waker) -> Vec<SourceEvent> {
        let mut st = lock(&self.shared);
        st.waker = Some(waker);
        st.events.clear();
        let mut created = Vec::new();
        let mut damage = Vec::new();
        for w in &st.windows {
            match w.popup {
                Some((x, y, grab)) => {
                    created.push(SourceEvent::PopupCreated { win: w.id, parent: w.parent, x, y, width: w.width, height: w.height, grab });
                }
                None => {
                    if w.parent != 0 {
                        created.push(SourceEvent::Parent { win: w.id, parent: w.parent });
                    }
                    created.push(SourceEvent::Created { win: w.id, width: w.width, height: w.height, title: w.title.clone(), app_id: w.app_id.clone() });
                    if w.limits != (0, 0, 0, 0) {
                        let (min_w, min_h, max_w, max_h) = w.limits;
                        created.push(SourceEvent::Limits { win: w.id, min_w, min_h, max_w, max_h });
                    }
                }
            }
            damage.push(SourceEvent::Damage { win: w.id, rects: vec![Rect::new(0, 0, w.width, w.height)] });
        }
        st.events.extend(damage);
        drop(st);
        created
    }

    fn poll(&mut self, _now_ms: u64) -> Vec<SourceEvent> {
        std::mem::take(&mut lock(&self.shared).events)
    }

    fn next_poll_in(&self, _now_ms: u64) -> Option<Duration> {
        None
    }

    fn with_pixels(&mut self, win: u32, f: &mut dyn FnMut(u32, u32, &[u8])) {
        let st = lock(&self.shared);
        if let Some(w) = st.windows.iter().find(|w| w.id == win) {
            f(w.width, w.height, &w.pixels);
        }
    }

    fn input(&mut self, msg: &Message) {
        let _ = self.tx.send(Cmd::Input(msg.clone()));
    }

    fn frame_sent(&mut self, win: u32) {
        let _ = self.tx.send(Cmd::FrameSent(win));
    }

    fn stop(&mut self) {
        let _ = self.tx.send(Cmd::SessionEnded);
        let mut st = lock(&self.shared);
        st.waker = None;
        st.caps = 0;
        st.events.clear();
    }
}
