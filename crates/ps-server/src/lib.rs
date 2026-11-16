
#![allow(unknown_lints, clippy::chunks_exact_to_as_chunks)]

use std::sync::Arc;

use ps_proto::Message;
use ps_transport::{Listener, Transport};
use video::VideoEncoderFactory;

pub mod audio;
pub mod mic;
pub mod flow;
pub mod secure;
#[cfg(unix)]
pub mod ctl;
#[cfg(target_os = "linux")]
pub mod discovery;
pub mod session;
pub mod source;
pub mod synthetic;
pub mod video;
#[cfg(target_os = "linux")]
pub mod wayland;

pub use flow::MAX_FRAMES_PER_WINDOW;
pub use session::{serve_session, serve_session_with, SessionCtl, SessionEnd};
pub use source::{SourceEvent, Waker, WindowSource};

pub struct SentFrame<'a> {
    pub win: u32,
    pub seq: u32,
    pub width: u32,
    pub height: u32,
    pub pixels: &'a [u8],
    pub bytes: usize,
    pub strips: std::ops::Range<usize>,
}

pub type FrameHook = Arc<dyn Fn(&SentFrame<'_>) + Send + Sync>;
pub type InputHook = Arc<dyn Fn(&Message) + Send + Sync>;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct SyntheticVideo {
    pub pause_after: Option<std::time::Duration>,
}

#[derive(Clone)]
pub struct ServerOptions {
    pub name: String,
    pub fps: u32,
    pub ui_size: (u32, u32),
    pub gradient_size: (u32, u32),
    pub frame_hook: Option<FrameHook>,
    pub input_hook: Option<InputHook>,
    pub flow_initial_throughput: f64,
    pub flow_adaptive: bool,
    pub send_queue_bytes: usize,
    pub hello_timeout: std::time::Duration,
    pub quiet: bool,
    pub video: Option<Arc<dyn VideoEncoderFactory>>,
    pub video_bitrate: u32,
    pub video_max_qp: u32,
    pub synthetic_video: Option<SyntheticVideo>,
    pub audio: Option<Arc<audio::AudioHub>>,
    pub mic: Option<Arc<mic::MicHub>>,
    pub max_rate: Option<f64>,
    pub synthetic_stripes: bool,
}

impl Default for ServerOptions {
    fn default() -> Self {
        ServerOptions {
            name: format!("psd {} (sintético)", env!("CARGO_PKG_VERSION")),
            fps: 30,
            ui_size: (640, 420),
            gradient_size: (640, 400),
            frame_hook: None,
            input_hook: None,
            flow_initial_throughput: flow::INITIAL_THROUGHPUT,
            flow_adaptive: true,
            send_queue_bytes: 256 * 1024,
            hello_timeout: std::time::Duration::from_secs(5),
            quiet: false,
            video: None,
            video_bitrate: video::DEFAULT_BITRATE,
            video_max_qp: video::DEFAULT_MAX_QP,
            synthetic_video: None,
            audio: None,
            mic: None,
            max_rate: None,
            synthetic_stripes: false,
        }
    }
}

impl ServerOptions {
    pub(crate) fn log(&self, msg: &str) {
        if !self.quiet {
            println!("[psd] {msg}");
        }
    }
}

pub fn serve_loop<L: Listener>(listener: &L, opts: &ServerOptions, max_sessions: Option<usize>) -> std::io::Result<()> {
    serve_loop_with(listener, opts, max_sessions, || match opts.synthetic_video {
        Some(sv) => Box::new(synthetic::SyntheticSource::video(sv.pause_after.map(|d| d.as_millis() as u64))) as Box<dyn WindowSource>,
        None if opts.synthetic_stripes => Box::new(synthetic::SyntheticSource::stripes()) as Box<dyn WindowSource>,
        None => Box::new(synthetic::SyntheticSource::new(opts.fps, opts.ui_size, opts.gradient_size)) as Box<dyn WindowSource>,
    })
}

pub fn serve_loop_with<L: Listener>(
    listener: &L,
    opts: &ServerOptions,
    max_sessions: Option<usize>,
    mut make_source: impl FnMut() -> Box<dyn WindowSource>,
) -> std::io::Result<()> {
    opts.log(&format!("escutando em {}", listener.local()));
    let mut count = 0usize;
    while max_sessions.is_none_or(|m| count < m) {
        let conn = listener.accept()?;
        let peer = conn.peer();
        let mut source = make_source();
        let end = serve_session(conn, opts, source.as_mut());
        log_end(opts, &peer, &end);
        count += 1;
    }
    Ok(())
}

pub(crate) fn log_end(opts: &ServerOptions, peer: &str, end: &SessionEnd) {
    match end {
        SessionEnd::ClientClosed => opts.log(&format!("cliente {peer} desconectou; aguardando nova conexão")),
        SessionEnd::ClientGoodbye => opts.log(&format!("cliente {peer} se despediu e desconectou; aguardando nova conexão")),
        SessionEnd::ServerGoodbye(motivo) => opts.log(&format!("sessão com {peer} encerrada pelo servidor (motivo {motivo})")),
        SessionEnd::Error(e) => opts.log(&format!("sessão com {peer} encerrada com erro: {e}; aguardando nova conexão")),
    }
}
