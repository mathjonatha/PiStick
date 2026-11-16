//! Compositor Wayland sem tela (só Linux): é uma [`WindowSource`] cujas janelas vêm de
//! clientes Wayland de verdade.
//!
//! Duas threads se falam por dois canais, e a sessão nunca toca em objetos Wayland:
//!
//! - compositor -> sessão: o [`Store`] compartilhado (pixels de cada janela e fila de eventos),
//!   protegido por um `Mutex`, mais o `waker` que acorda a sessão;
//! - sessão -> compositor: um canal do calloop com [`Cmd`] (entrada, quadro enviado).

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

/// Tamanho inicial pedido a um toplevel novo.
pub const INITIAL_SIZE: (u32, u32) = (960, 600);

/// Onde criar o socket Wayland.
pub enum SocketSpec {
    /// `$XDG_RUNTIME_DIR/<base>-N`, com o primeiro N livre.
    Auto(String),
    /// Caminho absoluto (usado pelos testes).
    Path(PathBuf),
}

/// Pixels e metadados de uma janela mapeada.
pub(crate) struct WinBuf {
    pub id: u32,
    pub width: u32,
    pub height: u32,
    pub title: String,
    pub app_id: String,
    /// BGRX, stride = largura × 4. Só a área da geometria da janela (veja `set_window_geometry`).
    pub pixels: Vec<u8>,
    /// Mãe do diálogo ou pai do popup (0 = nenhuma).
    pub parent: u32,
    /// Popup: posição relativa ao conteúdo do pai e se captura a entrada.
    pub popup: Option<(i32, i32, bool)>,
    /// Tamanhos mínimo e máximo declarados pelo app (0 = sem limite).
    pub limits: (u32, u32, u32, u32),
}

/// Estado compartilhado entre o compositor e a sessão.
#[derive(Default)]
pub(crate) struct Store {
    pub windows: Vec<WinBuf>,
    pub events: Vec<SourceEvent>,
    /// `Some` enquanto há uma sessão ativa; os eventos só são enfileirados nesse caso.
    pub waker: Option<Waker>,
    /// Extensões que o cliente da sessão ativa anunciou (bits de `caps`).
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

    /// Enfileira um evento (se há sessão) e a acorda.
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

/// Comandos da sessão para o compositor.
pub(crate) enum Cmd {
    Input(Message),
    FrameSent(u32),
    /// A sessão terminou: os popups abertos são dispensados (ninguém mais os vê).
    SessionEnded,
    Quit,
}

/// O compositor rodando em sua própria thread.
pub struct WaylandCompositor {
    shared: Shared,
    tx: Sender<Cmd>,
    thread: Option<JoinHandle<()>>,
    /// Nome do socket (valor de `WAYLAND_DISPLAY`).
    pub socket_name: String,
    /// Diretório do socket (valor de `XDG_RUNTIME_DIR` para os clientes).
    pub socket_path: PathBuf,
}

impl WaylandCompositor {
    /// Cria o socket e inicia a thread do compositor.
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

    /// Nova fonte de janelas para uma sessão.
    pub fn source(&self) -> Box<dyn WindowSource> {
        Box::new(WaylandSource { shared: self.shared.clone(), tx: self.tx.clone() })
    }

    /// Encerra a thread do compositor e espera por ela.
    pub fn shutdown(mut self) {
        let _ = self.tx.send(Cmd::Quit);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// Lado da sessão: lê o armazenamento compartilhado e manda comandos ao compositor.
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
        // O retrato das janelas atuais substitui qualquer evento antigo.
        st.events.clear();
        let mut created = Vec::new();
        let mut damage = Vec::new();
        // Ordem de criação: o pai sempre vem antes do popup; o `Parent` do diálogo, antes do `Created`.
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
