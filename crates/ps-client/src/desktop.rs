
use std::path::PathBuf;
use std::sync::mpsc;
use std::sync::Arc;
use std::time::Instant;

use ps_proto::{read_hello, write_message, Message, CAP_ATOMIC, CAP_AUDIO, CAP_H264, CAP_MIC, CAP_SESSION, GOODBYE_CLIENT};
use ps_transport::{Split, TcpTransport, Transport};
use winit::event_loop::{EventLoop, EventLoopProxy};

use crate::app::{self, ExitReason, ForcedFocus};
use crate::net::{self, UiSink, UserEvent};
use crate::{audio, clipboard, mic, registry, script, secure, stats, video, CLIENT_CAPS};

struct WinitSink(EventLoopProxy<UserEvent>);

impl UiSink for WinitSink {
    fn send(&self, ev: UserEvent) -> bool {
        self.0.send_event(ev).is_ok()
    }
}

pub struct RunOptions {
    pub forced_focus: Option<ForcedFocus>,
    pub script: Vec<script::Cmd>,
    pub shot_dir: PathBuf,
    pub video: bool,
    pub audio: audio::AudioOptions,
    pub atomic: bool,
    pub mic: mic::MicOptions,
    pub mic_link: mic::LinkTrust,
}

impl Default for RunOptions {
    fn default() -> Self {
        RunOptions { forced_focus: None, script: Vec::new(), shot_dir: PathBuf::from(".cache").join("shot"), video: true, audio: audio::AudioOptions::default(), atomic: true, mic: mic::MicOptions::default(), mic_link: mic::LinkTrust::Untrusted }
    }
}

pub fn run(addr: &str, mut opts: RunOptions) -> Result<ExitReason, String> {
    let transport = TcpTransport::connect(addr).map_err(|e| format!("não foi possível conectar a {addr}: {e}"))?;
    opts.mic_link = if transport.peer_ip().is_some_and(|i| i.is_loopback()) { mic::LinkTrust::Loopback } else { mic::LinkTrust::Untrusted };
    run_with_transport(transport, opts)
}

#[derive(Debug)]
pub enum RunError {
    Connect(secure::ConnectError),
    Other(String),
}

impl std::fmt::Display for RunError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RunError::Connect(e) => write!(f, "{e}"),
            RunError::Other(e) => f.write_str(e),
        }
    }
}

pub fn run_connected(tcp: TcpTransport, address: &str, security: &secure::SecurityOptions, mut opts: RunOptions) -> Result<ExitReason, RunError> {
    let loopback = tcp.peer_ip().is_some_and(|i| i.is_loopback());
    match secure::open(tcp, address, security).map_err(RunError::Connect)? {
        secure::Connected::Plain(t) => {
            opts.mic_link = if loopback { mic::LinkTrust::Loopback } else { mic::LinkTrust::Untrusted };
            run_with_transport(t, opts).map_err(RunError::Other)
        }
        secure::Connected::Secure(c) => {
            opts.mic_link = mic::LinkTrust::Paired;
            run_with_transport(*c, opts).map_err(RunError::Other)
        }
    }
}

pub fn run_with_transport<T: Transport>(transport: T, opts: RunOptions) -> Result<ExitReason, String> {
    let video = if opts.video {
        match video::probe() {
            Ok(()) => true,
            Err(e) => {
                println!("[psc] vídeo H.264 desativado: {e}");
                false
            }
        }
    } else {
        println!("[psc] vídeo H.264 desativado (--no-video)");
        false
    };
    let audio = audio::start(&opts.audio)?;
    let mic_log = mic::stdout_log();
    let mic_plan = mic::plan(&opts.mic, opts.mic_link, &mic_log)?;
    let peer = transport.peer();
    let Split { mut reader, mut writer, closer: _closer } = transport.split().map_err(|e| format!("transporte: {e}"))?;
    write_message(&mut writer, &Message::hello_with_caps(&format!("psc {}", env!("CARGO_PKG_VERSION")), CLIENT_CAPS | if opts.atomic { CAP_ATOMIC } else { 0 } | if video { CAP_H264 } else { 0 } | if audio.is_some() { CAP_AUDIO } else { 0 } | if mic_plan.announce { CAP_MIC } else { 0 }))
        .map_err(|e| format!("falha ao enviar HELLO: {e}"))?;
    let (caps, nome) = read_hello(&mut reader).map_err(|e| format!("handshake com {peer} falhou: {e}"))?;
    println!("[psc] conectado a {peer}: \"{nome}\"");

    let event_loop = EventLoop::<net::UserEvent>::with_user_event().build().map_err(|e| format!("não foi possível iniciar a interface: {e}"))?;
    let context = softbuffer::Context::new(event_loop.owned_display_handle()).map_err(|e| format!("softbuffer: {e}"))?;
    let ui: Arc<dyn UiSink> = Arc::new(WinitSink(event_loop.create_proxy()));
    let stats = Arc::new(stats::Stats::default());
    let t0 = Instant::now();
    let (out_tx, out_rx) = mpsc::channel::<Message>();
    let mic_on = mic_plan.announce && caps & CAP_MIC != 0;
    if mic_plan.announce && !mic_on {
        println!("[psc] microfone: o aparelho não aceitou a capacidade CAP_MIC; nada será capturado");
    }
    let mic = mic::Mic::new(if mic_on { mic_plan.run } else { None }, mic_on, out_tx.clone(), mic_log);
    let registry = registry::Registry::new();
    let clip = clipboard::ClipboardSync::new(Arc::new(clipboard::SystemClipboard), out_tx.clone());

    net::spawn_writer(writer, out_rx, ui.clone());
    net::spawn_reader(reader, ui.clone(), out_tx.clone(), stats.clone(), t0, registry.clone(), clip.clone(), video, opts.atomic, audio.clone(), mic.clone());
    net::spawn_stats(out_tx.clone(), stats.clone(), t0, audio.clone());

    let scripted = !opts.script.is_empty();
    if scripted {
        script::spawn(opts.script, script::Ctx { out: out_tx.clone(), registry: registry.clone(), ui, shot_dir: opts.shot_dir, clip: clip.clone(), stats: stats.clone(), audio: audio.clone(), mic: mic.clone() });
    }
    let out_goodbye = out_tx.clone();
    let mut app = app::App::new(context, out_tx, stats, opts.forced_focus, clip);
    app.script_mode = scripted;
    let ran = event_loop.run_app(&mut app).map_err(|e| format!("erro no laço de eventos: {e}"));
    mic.shutdown();
    if let Some(a) = &audio {
        a.shutdown();
        a.flush_dump();
    }
    let reason = app.exit_reason.take().unwrap_or(ExitReason::Disconnected("interface encerrada".into()));
    if caps & CAP_SESSION != 0 && matches!(reason, ExitReason::LastWindowClosed | ExitReason::ScriptQuit) {
        let _ = out_goodbye.send(Message::Goodbye { reason: GOODBYE_CLIENT, text: String::new() });
        std::thread::sleep(std::time::Duration::from_millis(60));
    }
    ran?;
    Ok(reason)
}
