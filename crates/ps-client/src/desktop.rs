//! Interface de desktop do cliente (`psc`): `winit` e `softbuffer`. Só existe com a feature `desktop` (padrão).
//!
//! A rede roda em threads próprias; a thread principal só trata eventos da interface.

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

/// A [`UiSink`] do `psc`: entrega os eventos ao laço do `winit` por um `EventLoopProxy`.
struct WinitSink(EventLoopProxy<UserEvent>);

impl UiSink for WinitSink {
    fn send(&self, ev: UserEvent) -> bool {
        // `send_event` falha só quando o laço da interface já terminou.
        self.0.send_event(ev).is_ok()
    }
}

/// Opções de uma execução do cliente.
pub struct RunOptions {
    /// Janela anunciada como focada sem depender do foco real (medições).
    pub forced_focus: Option<ForcedFocus>,
    /// Roteiro de entrada (`--script`, `--type`, `--resize`); executado depois da primeira janela.
    pub script: Vec<script::Cmd>,
    /// Pasta onde o comando `shot` grava os PNG.
    pub shot_dir: PathBuf,
    /// Anuncia `CAP_H264` se o decodificador de vídeo abrir (`false` = `--no-video`).
    pub video: bool,
    /// Áudio (marco 7): `--no-audio`, `--audio-dump`, `--audio-null`, `--audio-volume`.
    pub audio: audio::AudioOptions,
    /// Anuncia `CAP_ATOMIC` (apresentação atômica dos quadros fatiados); `false` = `--no-atomic`.
    pub atomic: bool,
    /// Microfone (marco 7B): `--mic`, `--mic-file`. Sem eles nada é anunciado nem aberto.
    pub mic: mic::MicOptions,
    /// Confiança da conexão para o microfone; [`run_connected`] e [`run`] o preenchem. O padrão (`Untrusted`) nunca
    /// oferece o microfone.
    pub mic_link: mic::LinkTrust,
}

impl Default for RunOptions {
    fn default() -> Self {
        RunOptions { forced_focus: None, script: Vec::new(), shot_dir: PathBuf::from(".cache").join("shot"), video: true, audio: audio::AudioOptions::default(), atomic: true, mic: mic::MicOptions::default(), mic_link: mic::LinkTrust::Untrusted }
    }
}

/// Conecta a `addr`, faz o handshake e roda a interface até a última janela fechar,
/// o roteiro mandar encerrar ou a conexão cair.
pub fn run(addr: &str, mut opts: RunOptions) -> Result<ExitReason, String> {
    let transport = TcpTransport::connect(addr).map_err(|e| format!("não foi possível conectar a {addr}: {e}"))?;
    // Texto claro puro: só o laço local merece o microfone.
    opts.mic_link = if transport.peer_ip().is_some_and(|i| i.is_loopback()) { mic::LinkTrust::Loopback } else { mic::LinkTrust::Untrusted };
    run_with_transport(transport, opts)
}

/// Falha de [`run_connected`]: da abertura do canal (pareamento, chave) ou da sessão.
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

/// Como [`run`], com a conexão TCP já aberta (por exemplo, depois da descoberta por mDNS) e passando pela
/// segurança do marco 9A: texto claro no laço local (ou com `--insecure`), canal cifrado e pareamento nos
/// demais casos. `address` é o endereço usado, só para a lista de aparelhos conhecidos.
pub fn run_connected(tcp: TcpTransport, address: &str, security: &secure::SecurityOptions, mut opts: RunOptions) -> Result<ExitReason, RunError> {
    let loopback = tcp.peer_ip().is_some_and(|i| i.is_loopback());
    match secure::open(tcp, address, security).map_err(RunError::Connect)? {
        secure::Connected::Plain(t) => {
            // Texto claro: laço local (nada sai do computador) ou `--insecure` fora dele (não serve para o microfone).
            opts.mic_link = if loopback { mic::LinkTrust::Loopback } else { mic::LinkTrust::Untrusted };
            run_with_transport(t, opts).map_err(RunError::Other)
        }
        secure::Connected::Secure(c) => {
            // `secure::open` só devolve `Secure` com o aparelho pareado.
            opts.mic_link = mic::LinkTrust::Paired;
            run_with_transport(*c, opts).map_err(RunError::Other)
        }
    }
}

/// Como [`run`], com a conexão já aberta (TCP puro ou canal cifrado, já autenticado).
pub fn run_with_transport<T: Transport>(transport: T, opts: RunOptions) -> Result<ExitReason, String> {
    // 1. Conexão e abertura, antes de abrir qualquer janela.
    // H.264 só se o sistema decodifica: tenta abrir um decodificador agora.
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
    // Áudio: o dispositivo (ou o consumidor que descarta) abre antes do HELLO; só anuncia `CAP_AUDIO` se abriu.
    let audio = audio::start(&opts.audio)?;
    // Microfone: decide agora se anuncia `CAP_MIC` (só com `--mic`/`--mic-file` e conexão cifrada e pareada ou laço
    // local). Nada de dispositivo aqui: a captura só abre depois de um `AUDIO_REQUEST`.
    let mic_log = mic::stdout_log();
    let mic_plan = mic::plan(&opts.mic, opts.mic_link, &mic_log)?;
    let peer = transport.peer();
    let Split { mut reader, mut writer, closer: _closer } = transport.split().map_err(|e| format!("transporte: {e}"))?;
    write_message(&mut writer, &Message::hello_with_caps(&format!("psc {}", env!("CARGO_PKG_VERSION")), CLIENT_CAPS | if opts.atomic { CAP_ATOMIC } else { 0 } | if video { CAP_H264 } else { 0 } | if audio.is_some() { CAP_AUDIO } else { 0 } | if mic_plan.announce { CAP_MIC } else { 0 }))
        .map_err(|e| format!("falha ao enviar HELLO: {e}"))?;
    let (caps, nome) = read_hello(&mut reader).map_err(|e| format!("handshake com {peer} falhou: {e}"))?;
    println!("[psc] conectado a {peer}: \"{nome}\"");

    // 2. Interface e threads de rede.
    let event_loop = EventLoop::<net::UserEvent>::with_user_event().build().map_err(|e| format!("não foi possível iniciar a interface: {e}"))?;
    let context = softbuffer::Context::new(event_loop.owned_display_handle()).map_err(|e| format!("softbuffer: {e}"))?;
    let ui: Arc<dyn UiSink> = Arc::new(WinitSink(event_loop.create_proxy()));
    let stats = Arc::new(stats::Stats::default());
    let t0 = Instant::now();
    let (out_tx, out_rx) = mpsc::channel::<Message>();
    // O servidor só pode pedir o microfone se a capacidade foi anunciada e aceita por ele.
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
    // Com roteiro, quem anuncia o foco é o roteiro; os eventos de foco reais do sistema não valem.
    app.script_mode = scripted;
    let ran = event_loop.run_app(&mut app).map_err(|e| format!("erro no laço de eventos: {e}"));
    // Libera o dispositivo de captura (e para o indicador de microfone do Windows) antes de qualquer outra coisa.
    mic.shutdown();
    if let Some(a) = &audio {
        a.shutdown();
        a.flush_dump();
    }
    let reason = app.exit_reason.take().unwrap_or(ExitReason::Disconnected("interface encerrada".into()));
    // Fechou por software (última janela, roteiro): se despede do aparelho, que encerra a sessão na hora. Com a conexão já
    // perdida ou depois da despedida do aparelho não há a quem dizer.
    if caps & CAP_SESSION != 0 && matches!(reason, ExitReason::LastWindowClosed | ExitReason::ScriptQuit) {
        let _ = out_goodbye.send(Message::Goodbye { reason: GOODBYE_CLIENT, text: String::new() });
        // A thread de escrita leva a mensagem ao soquete; o processo só acaba depois.
        std::thread::sleep(std::time::Duration::from_millis(60));
    }
    ran?;
    Ok(reason)
}
