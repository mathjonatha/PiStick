//! Daemon do PiStick (`psd`). Nesta fase só existe a fonte sintética: duas janelas de
//! teste com conteúdo animado. A lógica fica aqui na biblioteca; o binário é fino.

// Lint novo do clippy do Rust 1.100 (Pi); o Rust 1.95 do notebook não o conhece como erro.
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

/// Um quadro prestes a ser enviado, para observadores (testes, medições).
pub struct SentFrame<'a> {
    pub win: u32,
    pub seq: u32,
    pub width: u32,
    pub height: u32,
    /// Pixels BGRX do quadro inteiro, exatamente o que o codificador recebeu.
    pub pixels: &'a [u8],
    /// Tamanho dos dados codificados.
    pub bytes: usize,
    /// Faixas de blocos que este `FRAME` cobre (o quadro inteiro, ou uma fatia).
    pub strips: std::ops::Range<usize>,
}

pub type FrameHook = Arc<dyn Fn(&SentFrame<'_>) + Send + Sync>;
pub type InputHook = Arc<dyn Fn(&Message) + Send + Sync>;

/// Janela de vídeo sintética (`psd --synthetic-video`, marco 4).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct SyntheticVideo {
    /// A animação para depois deste tempo (para testar a volta ao modo sem perdas).
    pub pause_after: Option<std::time::Duration>,
}

/// Opções do servidor.
#[derive(Clone)]
pub struct ServerOptions {
    /// Nome enviado no HELLO.
    pub name: String,
    /// Taxa de atualização do conteúdo sintético.
    pub fps: u32,
    pub ui_size: (u32, u32),
    pub gradient_size: (u32, u32),
    /// Chamado a cada quadro enviado.
    pub frame_hook: Option<FrameHook>,
    /// Chamado a cada mensagem recebida do cliente (depois do HELLO).
    pub input_hook: Option<InputHook>,
    /// Vazão inicial do controle de fluxo, em bytes por segundo (padrão 4 MiB/s).
    pub flow_initial_throughput: f64,
    /// Se a vazão estimada se adapta às confirmações (padrão sim; testes desligam).
    pub flow_adaptive: bool,
    /// `SO_SNDBUF` do socket do servidor (padrão 256 KiB).
    pub send_queue_bytes: usize,
    /// Prazo para o cliente enviar o HELLO (5 s pela especificação).
    pub hello_timeout: std::time::Duration,
    /// Não imprime log nem estatísticas.
    pub quiet: bool,
    /// Fábrica de codificadores H.264 (marco 4). Sem ela o servidor não anuncia `CAP_H264` e tudo
    /// segue no codec de blocos.
    pub video: Option<Arc<dyn VideoEncoderFactory>>,
    /// Taxa de bits pedida aos codificadores de vídeo.
    pub video_bitrate: u32,
    /// Teto de QP do H.264 (`--video-max-qp`, 1 a 51).
    pub video_max_qp: u32,
    /// Com `Some`, [`serve_loop`] serve só a janela de vídeo sintética (no lugar das duas de sempre).
    pub synthetic_video: Option<SyntheticVideo>,
    /// Ponto de encontro do áudio (marco 7). Sem ele o servidor não anuncia `CAP_AUDIO`.
    pub audio: Option<Arc<audio::AudioHub>>,
    /// Microfone do hospedeiro (marco 7B). Sem ele o servidor não anuncia `CAP_MIC` e não pede nada.
    pub mic: Option<Arc<mic::MicHub>>,
    /// Limite de taxa **só para teste** (`--max-rate`), em bytes por segundo: o servidor espaça os quadros de
    /// imagem como se a conexão tivesse essa taxa. Quem o define também fixa o orçamento do controle de fluxo.
    pub max_rate: Option<f64>,
    /// `psd --synthetic-stripes`: no lugar das janelas de sempre, serve só a janela de ruído com marcadores de quadro
    /// (prova da apresentação atômica, `docs/20`).
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

/// Aceita clientes um de cada vez com a fonte sintética; quando um cai, volta a escutar.
/// Para depois de `max_sessions` sessões (`None` = para sempre).
pub fn serve_loop<L: Listener>(listener: &L, opts: &ServerOptions, max_sessions: Option<usize>) -> std::io::Result<()> {
    serve_loop_with(listener, opts, max_sessions, || match opts.synthetic_video {
        Some(sv) => Box::new(synthetic::SyntheticSource::video(sv.pause_after.map(|d| d.as_millis() as u64))) as Box<dyn WindowSource>,
        None if opts.synthetic_stripes => Box::new(synthetic::SyntheticSource::stripes()) as Box<dyn WindowSource>,
        None => Box::new(synthetic::SyntheticSource::new(opts.fps, opts.ui_size, opts.gradient_size)) as Box<dyn WindowSource>,
    })
}

/// Como [`serve_loop`], com a fonte de janelas escolhida por `make_source` (chamada a cada sessão).
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

/// Registra como uma sessão terminou.
pub(crate) fn log_end(opts: &ServerOptions, peer: &str, end: &SessionEnd) {
    match end {
        SessionEnd::ClientClosed => opts.log(&format!("cliente {peer} desconectou; aguardando nova conexão")),
        SessionEnd::ClientGoodbye => opts.log(&format!("cliente {peer} se despediu e desconectou; aguardando nova conexão")),
        SessionEnd::ServerGoodbye(motivo) => opts.log(&format!("sessão com {peer} encerrada pelo servidor (motivo {motivo})")),
        SessionEnd::Error(e) => opts.log(&format!("sessão com {peer} encerrada com erro: {e}; aguardando nova conexão")),
    }
}
