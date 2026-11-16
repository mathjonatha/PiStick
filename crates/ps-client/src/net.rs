//! Rede do cliente: tudo que bloqueia fica fora da thread da interface.
//!
//! - thread de leitura: lê mensagens, decodifica quadros e avisa a interface por eventos;
//! - thread de escrita: serializa tudo que a interface e as estatísticas querem enviar;
//! - thread de estatísticas: PING periódico e linha de estatísticas a cada segundo.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::sync::mpsc::{Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use ps_codec::{Decoder, Rect};
use ps_proto::{read_message, write_message, CursorShape, Message, ProtoError, CODEC_BLOCKS, CODEC_H264, GOODBYE_REPLACED, GOODBYE_SHUTDOWN, STATUS_DISPOSABLE, STATUS_TEMP_UNKNOWN, STATUS_THROTTLED_NOW, STATUS_UNDERVOLT_NOW, STATUS_UNDERVOLT_SINCE_BOOT};

use crate::atomic::{SliceGate, Verdict};
use crate::audio::AudioEngine;
use crate::clipboard::ClipboardSync;
use crate::registry::{Registry, WinInfo};
use crate::stats::Stats;
use crate::video::{open_decoder, VideoDecoder};

/// Quadro decodificado de uma janela remota, compartilhado entre a thread de rede
/// (que decodifica) e a thread da interface (que apresenta).
pub struct SharedWindow {
    pub inner: Mutex<WindowBuf>,
}

pub struct WindowBuf {
    pub decoder: Decoder,
    /// `seq` dos quadros decodificados e ainda não apresentados; viram FRAME_ACK.
    pub acks: Vec<u32>,
    /// Estado do vídeo por região, lido pelo roteiro (`verify-video`, `waitvideostart` e afins).
    pub video: VideoState,
    /// Cópia do último quadro **completo** enquanto um quadro fatiado está em curso e retido (fatias com `MORE`
    /// ainda sem a última): a janela apresenta esta cópia, e não o `decoder`, que já tem fatias do quadro novo.
    partial: Vec<u8>,
    held: bool,
}

impl WindowBuf {
    /// Os pixels que a janela deve mostrar agora (a cópia do último quadro completo, se há quadro fatiado retido).
    pub fn display_pixels(&self) -> &[u8] {
        if self.held {
            &self.partial
        } else {
            self.decoder.pixels()
        }
    }

    /// Começa a reter um quadro fatiado: guarda o quadro completo que está na tela antes de aplicar a fatia.
    fn hold_begin(&mut self) {
        if !self.held {
            self.partial.clear();
            self.partial.extend_from_slice(self.decoder.pixels());
            self.held = true;
        }
    }

    /// O quadro fatiado acabou (ou passou a ser apresentado em progresso, ou a janela mudou de tamanho).
    fn hold_end(&mut self) {
        self.held = false;
    }

    /// Escreve um quadro de vídeo decodificado na região; se há quadro retido, também na cópia que a janela mostra.
    fn write_video_region(&mut self, r: Rect, src: &[u8]) {
        self.decoder.write_region(r, src);
        if self.held {
            let (w, h) = (self.decoder.width() as usize, self.decoder.height() as usize);
            if r.is_empty() || r.x as usize >= w || r.y as usize >= h || src.len() < r.w as usize * 4 * r.h as usize {
                return;
            }
            let row_bytes = r.w as usize * 4;
            let cw = (r.w as usize).min(w - r.x as usize);
            let ch = (r.h as usize).min(h - r.y as usize);
            for row in 0..ch {
                let d = ((r.y as usize + row) * w + r.x as usize) * 4;
                let s = row * row_bytes;
                self.partial[d..d + cw * 4].copy_from_slice(&src[s..s + cw * 4]);
                for px in self.partial[d..d + cw * 4].as_chunks_mut::<4>().0.iter_mut() {
                    px[3] = 0xFF;
                }
            }
        }
    }
}

/// O que o roteiro precisa saber do vídeo de uma janela (instantes na hora do cliente).
#[derive(Debug, Clone, Default)]
pub struct VideoState {
    /// Fluxo e região ativos (`VIDEO_START` sem `VIDEO_STOP`).
    pub active: Option<(u32, Rect)>,
    /// Primeiro `FRAME` da janela: a origem dos tempos impressos no log.
    pub first_frame_at: Option<Instant>,
    pub starts: u32,
    pub stops: u32,
    pub start_at: Option<Instant>,
    pub stop_at: Option<Instant>,
    pub last_video_frame_at: Option<Instant>,
    /// Última vez que os pixels da janela mudaram (`FRAME` ou `VIDEO_FRAME` aplicado).
    pub last_update_at: Option<Instant>,
    /// `capture_us` (relógio da sessão do servidor) do último quadro, de blocos ou de vídeo.
    pub last_capture_us: Option<u64>,
    pub video_frames: u64,
}

impl VideoState {
    /// Segundos de `at` desde o primeiro `FRAME` da janela.
    pub fn rel(&self, at: Instant) -> Option<f64> {
        self.first_frame_at.map(|f| at.saturating_duration_since(f).as_secs_f64())
    }

    /// `rel` formatado para o log.
    pub fn rel_text(&self, at: Instant) -> String {
        self.rel(at).map_or_else(|| "sem-FRAME".to_string(), |t| format!("{t:.2}s"))
    }

    fn frame_arrived(&mut self, now: Instant, capture_us: u64) {
        self.first_frame_at.get_or_insert(now);
        self.last_update_at = Some(now);
        self.last_capture_us = Some(capture_us);
    }

    fn video_frame_arrived(&mut self, now: Instant, capture_us: u64, updated: bool) {
        self.last_video_frame_at = Some(now);
        self.video_frames += 1;
        self.last_capture_us = Some(capture_us);
        if updated {
            self.last_update_at = Some(now);
        }
    }

    /// Registra o fim do fluxo; devolve o texto de tempo para o log.
    fn stopped(&mut self, now: Instant) -> String {
        self.active = None;
        self.stops += 1;
        self.stop_at = Some(now);
        self.rel_text(now)
    }
}

impl SharedWindow {
    pub fn new(width: u32, height: u32) -> Arc<Self> {
        Arc::new(SharedWindow { inner: Mutex::new(WindowBuf { decoder: Decoder::new(width, height), acks: Vec::new(), video: VideoState::default(), partial: Vec::new(), held: false }) })
    }
}

/// Um fluxo de vídeo aberto numa janela: o decodificador e a região que ele preenche.
struct VideoStream {
    stream: u32,
    rect: Rect,
    decoder: Box<dyn VideoDecoder>,
    /// Último quadro decodificado (BGRX, só a região).
    buf: Vec<u8>,
    frames: u64,
}

/// `HashMap<janela, fluxo>` com a remoção condicionada ao identificador do fluxo.
trait StreamTable {
    fn remove_if_stream(&mut self, win: u32, stream: u32) -> Option<VideoStream>;
}

impl StreamTable for HashMap<u32, VideoStream> {
    fn remove_if_stream(&mut self, win: u32, stream: u32) -> Option<VideoStream> {
        if self.get(&win).is_some_and(|v| v.stream == stream) {
            self.remove(&win)
        } else {
            None
        }
    }
}

/// Eventos da rede para a interface.
pub enum UserEvent {
    WinCreate { win: u32, width: u32, height: u32, title: String, shared: Arc<SharedWindow> },
    WinResize { win: u32, width: u32, height: u32 },
    WinTitle { win: u32, title: String },
    WinDestroy { win: u32 },
    /// Há quadro novo (ou ACK pendente) nesta janela.
    Frame { win: u32, shared: Arc<SharedWindow> },
    /// Popup (menu ou dica): posição relativa ao conteúdo do pai.
    PopupCreate { win: u32, parent: u32, x: i32, y: i32, width: u32, height: u32, grab: bool, shared: Arc<SharedWindow> },
    PopupMove { win: u32, x: i32, y: i32, width: u32, height: u32 },
    WinParent { win: u32, parent: u32 },
    WinLimits { win: u32, min_w: u32, min_h: u32, max_w: u32, max_h: u32 },
    Cursor { win: u32, shape: CursorShape },
    /// O roteiro de teste pediu para encerrar.
    Quit,
    Disconnected(String),
    /// O aparelho se despediu (`GOODBYE`, `CAP_SESSION`): `reason` 1 = outro cliente assumiu a sessão (`text` = nome dele; o
    /// cliente **não** reconecta sozinho), 2 = o serviço está parando. A conexão acaba logo depois, sem `Disconnected`.
    Goodbye { reason: u8, text: String },
    /// Estado do aparelho (`STATUS`): bits `STATUS_*` e a temperatura em décimos de grau (`None` = desconhecida).
    Status { flags: u32, temp_dc: Option<i16> },
}

/// Para onde as threads de rede mandam os [`UserEvent`]: a interface do cliente. O `psc` o implementa com o
/// `EventLoopProxy` do `winit`; o cliente Android, com uma fila própria. Chamado de várias threads.
pub trait UiSink: Send + Sync {
    /// Entrega o evento à interface. `false` quando a interface já terminou (a thread de rede deve parar).
    fn send(&self, ev: UserEvent) -> bool;
}

/// Microssegundos desde `t0`, o relógio do PING.
pub fn micros_since(t0: Instant) -> u64 {
    t0.elapsed().as_micros() as u64
}

/// O que a thread de socket entrega à thread de processamento.
enum Inbound {
    Msg(Message),
    End(ProtoError),
}

/// Duas threads: a de socket só lê e trata `PONG` na hora (a medida de latência da conexão não
/// espera decodificação de quadro); a de processamento decodifica os quadros e avisa a interface.
#[allow(clippy::too_many_arguments)]
pub fn spawn_reader<R: Read + Send + 'static>(
    mut reader: R,
    ui: Arc<dyn UiSink>,
    out: Sender<Message>,
    stats: Arc<Stats>,
    t0: Instant,
    registry: Arc<Registry>,
    clip: Arc<ClipboardSync>,
    video: bool,
    atomic: bool,
    audio: Option<Arc<AudioEngine>>,
    mic: Arc<crate::mic::Mic>,
) {
    let (tx, rx) = std::sync::mpsc::channel::<Inbound>();
    let stats_sock = stats.clone();
    std::thread::Builder::new()
        .name("psc-socket".into())
        .spawn(move || loop {
            match read_message(&mut reader) {
                Ok(Message::Pong { echo }) => stats_sock.record_rtt(micros_since(t0).saturating_sub(echo)),
                Ok(m) => {
                    if tx.send(Inbound::Msg(m)).is_err() {
                        return;
                    }
                }
                Err(e) => {
                    let _ = tx.send(Inbound::End(e));
                    return;
                }
            }
        })
        .expect("criar thread de socket");
    std::thread::Builder::new()
        .name("psc-decodificacao".into())
        .spawn(move || {
            let mut despedido = false;
            let motivo = match process_loop(&rx, ui.as_ref(), &out, &stats, &registry, &clip, video, atomic, audio.as_deref(), &mic, &mut despedido) {
                Ok(()) => "a interface foi encerrada".to_string(),
                Err(ProtoError::Closed) => "o servidor fechou a conexão".to_string(),
                Err(e) => e.to_string(),
            };
            // Depois de um `GOODBYE` a interface já sabe por que a conexão acabou.
            if !despedido {
                let _ = ui.send(UserEvent::Disconnected(motivo));
            }
        })
        .expect("criar thread de decodificação");
}

#[allow(clippy::too_many_arguments)]
fn process_loop(
    rx: &Receiver<Inbound>,
    ui: &dyn UiSink,
    out: &Sender<Message>,
    stats: &Stats,
    registry: &Registry,
    clip: &ClipboardSync,
    video: bool,
    atomic: bool,
    audio: Option<&AudioEngine>,
    mic: &crate::mic::Mic,
    despedido: &mut bool,
) -> Result<(), ProtoError> {
    let mut windows: HashMap<u32, Arc<SharedWindow>> = HashMap::new();
    // Quadro fatiado em curso, por janela (a regra dos 250 ms; só com `CAP_ATOMIC`).
    let mut gates: HashMap<u32, SliceGate> = HashMap::new();
    // Fluxos de vídeo abertos, por janela (um por vez na conexão, mas a tabela não depende disso).
    let mut streams: HashMap<u32, VideoStream> = HashMap::new();
    // Falha que derruba a conexão (o transporte é confiável: decodificação com erro é defeito).
    macro_rules! fatal {
        ($($arg:tt)*) => {{
            let _ = ui.send(UserEvent::Disconnected(format!($($arg)*)));
            return Ok(());
        }};
    }
    // `send` falha só quando a interface já terminou.
    macro_rules! notify {
        ($ev:expr) => {
            if !ui.send($ev) {
                return Ok(());
            }
        };
    }
    loop {
        let msg = match rx.recv() {
            Ok(Inbound::Msg(m)) => m,
            Ok(Inbound::End(e)) => return Err(e),
            Err(_) => return Ok(()),
        };
        match msg {
            Message::WinCreate { win, width, height, title, .. } => {
                let shared = SharedWindow::new(width, height);
                windows.insert(win, shared.clone());
                let num = registry.add(WinInfo { num: 0, id: win, parent: 0, popup: false, grab: false, x: 0, y: 0, title: title.clone(), shared: shared.clone(), alive: true });
                println!("[psc] janela {num} (id {win}) criada: {width}x{height} \"{title}\"");
                notify!(UserEvent::WinCreate { win, width, height, title, shared });
            }
            Message::PopupCreate { win, parent, x, y, width, height, grab } => {
                let shared = SharedWindow::new(width, height);
                windows.insert(win, shared.clone());
                let num = registry.add(WinInfo { num: 0, id: win, parent, popup: true, grab, x, y, title: String::new(), shared: shared.clone(), alive: true });
                println!("[psc] janela {num} (id {win}) popup de {parent} em ({x}, {y}) {width}x{height} {}", if grab { "com captura" } else { "sem captura" });
                notify!(UserEvent::PopupCreate { win, parent, x, y, width, height, grab, shared });
            }
            Message::PopupMove { win, x, y, width, height } => {
                registry.set_popup_pos(win, x, y);
                println!("[psc] popup {win} movido para ({x}, {y}) {width}x{height}");
                notify!(UserEvent::PopupMove { win, x, y, width, height });
            }
            Message::WinParent { win, parent } => {
                registry.set_parent(win, parent);
                notify!(UserEvent::WinParent { win, parent });
            }
            Message::WinLimits { win, min_w, min_h, max_w, max_h } => notify!(UserEvent::WinLimits { win, min_w, min_h, max_w, max_h }),
            Message::Cursor { win, shape } => {
                match &shape {
                    CursorShape::Hidden => println!("[psc] cursor da janela {win}: oculto"),
                    CursorShape::Named(n) => println!("[psc] cursor da janela {win}: nomeado \"{n}\""),
                    CursorShape::Image { width, height, hot_x, hot_y, .. } => println!("[psc] cursor da janela {win}: imagem {width}x{height} ponto ({hot_x}, {hot_y})"),
                }
                notify!(UserEvent::Cursor { win, shape });
            }
            Message::Clipboard { mime, data } => {
                match clip.from_server(&mime, &data) {
                    Some(texto) => println!("[psc] área de transferência recebida do Pi ({} bytes): {:?}", data.len(), preview(&texto)),
                    None => println!("[psc] área de transferência do Pi ignorada ({mime}, {} bytes)", data.len()),
                }
            }
            Message::VideoStart { win, stream, x, y, width, height, codec } => {
                let Some(shared) = windows.get(&win).cloned() else { continue };
                let (ww, wh) = {
                    let g = shared.inner.lock().unwrap_or_else(|e| e.into_inner());
                    (g.decoder.width(), g.decoder.height())
                };
                if !video || codec != CODEC_H264 {
                    fatal!("VIDEO_START recebido sem que o cliente tenha anunciado H.264 (codec {codec})");
                }
                if width == 0 || height == 0 || x.saturating_add(width) > ww || y.saturating_add(height) > wh {
                    fatal!("VIDEO_START com região {width}x{height} em ({x}, {y}) fora da janela {ww}x{wh}");
                }
                let decoder = match open_decoder(width, height) {
                    Ok(d) => d,
                    Err(e) => fatal!("não foi possível abrir o decodificador de vídeo: {e}"),
                };
                println!("[psc] janela {win}: vídeo iniciado (fluxo {stream}) na região {width}x{height} em ({x}, {y})");
                {
                    let now = Instant::now();
                    let mut g = shared.inner.lock().unwrap_or_else(|e| e.into_inner());
                    g.video.active = Some((stream, Rect::new(x, y, width, height)));
                    g.video.starts += 1;
                    g.video.start_at = Some(now);
                    println!("[psc] video: START janela={win} regiao={width}x{height}+{x}+{y} t={}", g.video.rel_text(now));
                }
                streams.insert(win, VideoStream { stream, rect: Rect::new(x, y, width, height), decoder, buf: Vec::new(), frames: 0 });
            }
            Message::VideoFrame { win, stream, seq, capture_us, data, .. } => {
                let Some(shared) = windows.get(&win).cloned() else {
                    let _ = out.send(Message::FrameAck { win, seq });
                    continue;
                };
                let Some(vs) = streams.get_mut(&win).filter(|v| v.stream == stream) else {
                    // Fluxo desconhecido: descartado e confirmado.
                    let _ = out.send(Message::FrameAck { win, seq });
                    continue;
                };
                let t = Instant::now();
                let decoded = vs.decoder.decode(&data, &mut vs.buf);
                let us = t.elapsed().as_micros() as u64;
                match decoded {
                    Ok(Some((w, h))) => {
                        if (w, h) != (vs.rect.w, vs.rect.h) {
                            fatal!("o quadro de vídeo decodificado tem {w}x{h} e a região do VIDEO_START tem {}x{}", vs.rect.w, vs.rect.h);
                        }
                        let mut g = shared.inner.lock().unwrap_or_else(|e| e.into_inner());
                        g.write_video_region(vs.rect, &vs.buf);
                        g.acks.push(seq);
                        g.video.video_frame_arrived(Instant::now(), capture_us, true);
                    }
                    // A MFT ainda não devolveu quadro (latência do decodificador): confirma mesmo assim.
                    Ok(None) => {
                        let mut g = shared.inner.lock().unwrap_or_else(|e| e.into_inner());
                        g.acks.push(seq);
                        g.video.video_frame_arrived(Instant::now(), capture_us, false);
                    }
                    Err(e) => fatal!("falha ao decodificar o vídeo da janela {win}: {e}"),
                }
                vs.frames += 1;
                stats.record_video(data.len(), us);
                notify!(UserEvent::Frame { win, shared });
            }
            Message::VideoStop { win, stream } => {
                if let Some(v) = streams.remove_if_stream(win, stream) {
                    println!("[psc] janela {win}: vídeo encerrado (fluxo {stream}, {} quadros)", v.frames);
                    if let Some(shared) = windows.get(&win) {
                        let t = shared.inner.lock().unwrap_or_else(|e| e.into_inner()).video.stopped(Instant::now());
                        println!("[psc] video: STOP janela={win} fluxo={stream} quadros={} t={t}", v.frames);
                    }
                }
            }
            Message::AudioStart { stream, codec, channels, rate, frames } => {
                let Some(a) = audio else { fatal!("AUDIO_START recebido sem que o cliente tenha anunciado áudio") };
                if let Err(e) = a.on_start(stream, codec, channels, rate, frames) {
                    fatal!("{e}");
                }
                println!("[psc] áudio: fluxo {stream} aberto (codec {codec}, {channels} canais, {rate} Hz, {frames} quadros por pacote)");
            }
            Message::AudioData { stream, flags, seq, pts_us, data } => {
                let Some(a) = audio else { fatal!("AUDIO_DATA recebido sem que o cliente tenha anunciado áudio") };
                if let Err(e) = a.on_data(stream, flags, seq, pts_us, &data) {
                    fatal!("{e}");
                }
            }
            // Só o fluxo 1 (microfone) existe; o `Mic` conta o pedido e só captura se foi anunciado.
            Message::AudioRequest { stream: ps_proto::AUDIO_STREAM_MIC, on } => mic.request(on),
            Message::AudioRequest { stream, .. } => println!("[psc] microfone: pedido para o fluxo {stream} desconhecido; ignorado"),
            Message::AudioStop { stream } => {
                if let Some(a) = audio {
                    a.on_stop(stream);
                }
                println!("[psc] áudio: fluxo {stream} encerrado pelo servidor");
            }
            Message::WinResize { win, width, height } => {
                // `WIN_RESIZE` encerra o fluxo de vídeo da janela sem `VIDEO_STOP`.
                let tinha_fluxo = streams.remove(&win).is_some();
                if let Some(shared) = windows.get(&win) {
                    // Zera a referência dos dois lados (o servidor também zerou a dele).
                    let mut g = shared.inner.lock().unwrap_or_else(|e| e.into_inner());
                    g.decoder.reset(width, height);
                    g.hold_end();
                    gates.remove(&win);
                    if tinha_fluxo {
                        let t = g.video.stopped(Instant::now());
                        println!("[psc] video: STOP janela={win} (redimensionamento) t={t}");
                    }
                    drop(g);
                    notify!(UserEvent::WinResize { win, width, height });
                }
            }
            Message::WinTitle { win, title } => {
                registry.set_title(win, &title);
                notify!(UserEvent::WinTitle { win, title })
            }
            Message::WinDestroy { win } => {
                windows.remove(&win);
                streams.remove(&win);
                gates.remove(&win);
                registry.destroy(win);
                notify!(UserEvent::WinDestroy { win });
            }
            Message::Frame { win, seq, capture_us, width, height, codec, more, data } => {
                let Some(shared) = windows.get(&win).cloned() else {
                    // Janela já destruída: confirma mesmo assim para não travar o fluxo.
                    let _ = out.send(Message::FrameAck { win, seq });
                    continue;
                };
                let now = Instant::now();
                let verdict = {
                    let mut g = shared.inner.lock().unwrap_or_else(|e| e.into_inner());
                    let valid = codec == CODEC_BLOCKS && (g.decoder.width(), g.decoder.height()) == (width, height);
                    // Sem `CAP_ATOMIC` anunciada o servidor nunca marca MORE; quadro de outro tamanho ou de codec
                    // desconhecido é descartado e confirmado como sempre, sem entrar na regra das fatias.
                    let verdict = if atomic && valid { gates.entry(win).or_default().on_frame(more, now) } else { Verdict::Present };
                    if verdict == Verdict::Hold {
                        // O quadro que está na tela fica guardado enquanto as fatias do quadro novo chegam.
                        g.hold_begin();
                    } else {
                        g.hold_end();
                    }
                    if valid {
                        let t = Instant::now();
                        match g.decoder.decode(&data) {
                            Ok(_) => stats.record_decode(data.len(), t.elapsed().as_micros() as u64),
                            Err(e) => {
                                let _ = ui.send(UserEvent::Disconnected(format!("quadro inválido do servidor: {e}")));
                                return Ok(());
                            }
                        }
                    }
                    if verdict != Verdict::Hold {
                        // Confirma na apresentação.
                        g.acks.push(seq);
                        g.video.frame_arrived(now, capture_us);
                    }
                    verdict
                };
                if verdict == Verdict::Hold {
                    // Fatia retida: confirma na hora (senão o fluxo trava) e não pede redesenho.
                    stats.acks.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    let _ = out.send(Message::FrameAck { win, seq });
                    continue;
                }
                if verdict == Verdict::PresentProgress {
                    stats.atomic_progressive();
                }
                notify!(UserEvent::Frame { win, shared });
            }
            Message::Goodbye { reason, text } => {
                println!("[psc] o aparelho se despediu: {}", goodbye_text(reason, &text));
                *despedido = true;
                let _ = ui.send(UserEvent::Goodbye { reason, text });
                return Ok(());
            }
            Message::Status { flags, temp_dc } => {
                let temp_dc = (temp_dc != STATUS_TEMP_UNKNOWN).then_some(temp_dc);
                println!("[psc] estado do aparelho: {}", status_text(flags, temp_dc));
                notify!(UserEvent::Status { flags, temp_dc });
            }
            // HELLO repetido ou mensagens do sentido cliente -> servidor: ignoradas.
            _ => {}
        }
    }
}

/// Texto da despedida do aparelho, para o terminal e para a interface.
pub fn goodbye_text(reason: u8, text: &str) -> String {
    match reason {
        GOODBYE_REPLACED if text.is_empty() => "outro cliente assumiu a sessão".to_string(),
        GOODBYE_REPLACED => format!("o cliente \"{text}\" assumiu a sessão"),
        GOODBYE_SHUTDOWN => "o aparelho está desligando ou o serviço está parando".to_string(),
        n => format!("motivo {n}"),
    }
}

/// Estado do aparelho em texto, sempre por palavras (nunca só por cor): o que está ligado e a temperatura.
pub fn status_text(flags: u32, temp_dc: Option<i16>) -> String {
    let mut partes: Vec<String> = Vec::new();
    partes.push(if flags & STATUS_UNDERVOLT_NOW != 0 { "energia fraca agora (subtensão)".into() } else { "energia normal".into() });
    if flags & STATUS_UNDERVOLT_SINCE_BOOT != 0 {
        partes.push("houve subtensão desde o boot".into());
    }
    if flags & STATUS_THROTTLED_NOW != 0 {
        partes.push("temperatura limitando o desempenho".into());
    }
    if flags & STATUS_DISPOSABLE != 0 {
        partes.push("modo descartável (o que os apps gravarem some)".into());
    }
    partes.push(match temp_dc {
        Some(t) => format!("{:.1} °C", f32::from(t) / 10.0),
        None => "temperatura desconhecida".into(),
    });
    partes.join("; ")
}

/// Título de uma janela com os avisos de estado do aparelho (por palavras, não só por cor).
pub fn decorated_title(base: &str, flags: u32) -> String {
    let mut t = base.to_string();
    if flags & STATUS_UNDERVOLT_NOW != 0 {
        t.push_str(" [energia fraca]");
    }
    if flags & STATUS_THROTTLED_NOW != 0 {
        t.push_str(" [temperatura alta]");
    }
    if flags & STATUS_DISPOSABLE != 0 {
        t.push_str(" [descartável]");
    }
    t
}

/// Primeiros caracteres de um texto, para o log.
fn preview(t: &str) -> String {
    let mut p: String = t.chars().take(60).collect();
    if t.chars().count() > 60 {
        p.push('…');
    }
    p
}

/// Thread de escrita: encerra quando todos os `Sender` somem ou a escrita falha.
pub fn spawn_writer<W: Write + Send + 'static>(mut writer: W, rx: Receiver<Message>, ui: Arc<dyn UiSink>) {
    std::thread::Builder::new()
        .name("psc-escrita".into())
        .spawn(move || {
            for msg in rx {
                if let Err(e) = write_message(&mut writer, &msg) {
                    let _ = ui.send(UserEvent::Disconnected(format!("falha ao enviar: {e}")));
                    return;
                }
            }
        })
        .expect("criar thread de escrita");
}

/// PING a cada 250 ms; linha de estatísticas a cada segundo.
pub fn spawn_stats(out: Sender<Message>, stats: Arc<Stats>, t0: Instant, audio: Option<Arc<AudioEngine>>) {
    std::thread::Builder::new()
        .name("psc-estatisticas".into())
        .spawn(move || {
            let mut tick = 0u32;
            let (mut last_packets, mut last_under) = (0u64, 0u64);
            loop {
                std::thread::sleep(Duration::from_millis(250));
                if out.send(Message::Ping { value: micros_since(t0) }).is_err() {
                    return;
                }
                tick += 1;
                if tick.is_multiple_of(4) {
                    println!("[psc] 1 s: {}", stats.take_line());
                    if let Some(a) = &audio {
                        let s = a.stats();
                        if s.open || s.packets > last_packets {
                            println!(
                                "[psc] áudio 1 s: {} pacotes, fila {:.1} ms (média {:.1}, máx {:.1}), alvo {:.0} ms, jitter {:.0} ms, esvaziamentos {} (+{}), estouros {}, retomadas {}, descartes {}",
                                s.packets.saturating_sub(last_packets),
                                s.level_now_ms,
                                s.queue.level_avg_ms(),
                                s.queue.level_max_ms(),
                                s.target_ms,
                                s.jitter_ms,
                                s.queue.underruns,
                                s.queue.underruns.saturating_sub(last_under),
                                s.queue.overflows,
                                s.resumes,
                                s.drops
                            );
                        }
                        last_packets = s.packets;
                        last_under = s.queue.underruns;
                    }
                }
            }
        })
        .expect("criar thread de estatísticas");
}

#[cfg(test)]
mod tests {
    use super::*;
    use ps_codec::Encoder;

    fn quadro(w: u32, h: u32, v: u8) -> Vec<u8> {
        let mut px = vec![v; (w * h * 4) as usize];
        for p in px.as_chunks_mut::<4>().0.iter_mut() {
            p[3] = 0xFF;
        }
        px
    }

    /// A interface de teste: uma fila (é o que o cliente Android faz); `send` devolve `false` quando ela some.
    struct FilaSink(Mutex<std::sync::mpsc::Sender<UserEvent>>);

    impl UiSink for FilaSink {
        fn send(&self, ev: UserEvent) -> bool {
            self.0.lock().unwrap().send(ev).is_ok()
        }
    }

    /// O núcleo de rede fala com qualquer interface pelo `UiSink`, sem `winit`: uma `WIN_CREATE` chega como
    /// `UserEvent::WinCreate` e o fim da conexão como `Disconnected`.
    #[test]
    fn a_rede_entrega_eventos_a_uma_interface_qualquer() {
        let mut bytes = Vec::new();
        write_message(&mut bytes, &Message::WinCreate { win: 7, width: 320, height: 200, title: "teste".into(), app_id: "app".into() }).unwrap();
        let (ev_tx, ev_rx) = std::sync::mpsc::channel();
        let ui: Arc<dyn UiSink> = Arc::new(FilaSink(Mutex::new(ev_tx)));
        let (out_tx, _out_rx) = std::sync::mpsc::channel::<Message>();
        let stats = Arc::new(Stats::default());
        let clip = ClipboardSync::new(Arc::new(crate::clipboard::SystemClipboard), out_tx.clone());
        let mic = crate::mic::Mic::new(None, false, out_tx.clone(), Arc::new(|_: &str| {}));
        spawn_reader(std::io::Cursor::new(bytes), ui, out_tx, stats, Instant::now(), Registry::new(), clip, false, false, None, mic);
        let espera = Duration::from_secs(5);
        match ev_rx.recv_timeout(espera).expect("WinCreate") {
            UserEvent::WinCreate { win, width, height, title, .. } => assert_eq!((win, width, height, title.as_str()), (7, 320, 200, "teste")),
            _ => panic!("o primeiro evento devia ser WinCreate"),
        }
        assert!(matches!(ev_rx.recv_timeout(espera).expect("Disconnected"), UserEvent::Disconnected(_)));
    }

    /// Enquanto um quadro fatiado está retido, a janela mostra o quadro completo anterior, mesmo com fatias do
    /// quadro novo já decodificadas; a última fatia solta o quadro novo, inteiro.
    #[test]
    fn quadro_retido_mostra_o_anterior_ate_a_ultima_fatia() {
        let (w, h) = (64u32, 192u32);
        let (velho, novo) = (quadro(w, h, 10), quadro(w, h, 200));
        let shared = SharedWindow::new(w, h);
        let mut g = shared.inner.lock().unwrap();
        let mut enc = Encoder::new(w, h);
        let mut o = Vec::new();
        enc.encode(&velho, (w * 4) as usize, None, &mut o);
        g.decoder.decode(&o).unwrap();
        assert_eq!(g.display_pixels(), velho.as_slice());
        // Três faixas de 64 linhas, uma fatia por faixa.
        let dano = [Rect::new(0, 0, w, h)];
        for faixa in 0..3usize {
            let mut o = Vec::new();
            enc.encode_range(&novo, (w * 4) as usize, Some(&dano), faixa..faixa + 1, &mut o);
            if faixa < 2 {
                g.hold_begin();
            } else {
                g.hold_end();
            }
            g.decoder.decode(&o).unwrap();
            if faixa < 2 {
                assert_eq!(g.display_pixels(), velho.as_slice(), "fatia {faixa}: a tela continua no quadro anterior");
                assert_ne!(g.decoder.pixels(), velho.as_slice(), "mas o decodificador já tem a fatia");
            }
        }
        assert_eq!(g.display_pixels(), novo.as_slice(), "a última fatia apresenta o quadro novo inteiro");
        // Quadro de vídeo durante a retenção: entra no que a janela mostra.
        g.hold_begin();
        g.write_video_region(Rect::new(0, 0, 2, 2), &[7u8; 2 * 2 * 4]);
        assert_eq!(&g.display_pixels()[..3], &[7, 7, 7]);
        assert_eq!(g.display_pixels()[3], 0xFF);
    }

    /// Marco 8A: `STATUS` e `GOODBYE` viram eventos para a interface; depois do `GOODBYE` não vem `Disconnected`.
    #[test]
    fn status_e_goodbye_chegam_a_interface_e_depois_do_goodbye_nao_ha_desconexao() {
        let mut bytes = Vec::new();
        write_message(&mut bytes, &Message::Status { flags: ps_proto::STATUS_UNDERVOLT_NOW | ps_proto::STATUS_DISPOSABLE, temp_dc: 487 }).unwrap();
        write_message(&mut bytes, &Message::Status { flags: 0, temp_dc: ps_proto::STATUS_TEMP_UNKNOWN }).unwrap();
        write_message(&mut bytes, &Message::Goodbye { reason: ps_proto::GOODBYE_REPLACED, text: "outro".into() }).unwrap();
        let (ev_tx, ev_rx) = std::sync::mpsc::channel();
        let ui: Arc<dyn UiSink> = Arc::new(FilaSink(Mutex::new(ev_tx)));
        let (out_tx, _out_rx) = std::sync::mpsc::channel::<Message>();
        let stats = Arc::new(Stats::default());
        let clip = ClipboardSync::new(Arc::new(crate::clipboard::SystemClipboard), out_tx.clone());
        let mic = crate::mic::Mic::new(None, false, out_tx.clone(), Arc::new(|_: &str| {}));
        spawn_reader(std::io::Cursor::new(bytes), ui, out_tx, stats, Instant::now(), Registry::new(), clip, false, false, None, mic);
        let espera = Duration::from_secs(5);
        assert!(matches!(ev_rx.recv_timeout(espera).unwrap(), UserEvent::Status { flags, temp_dc: Some(487) } if flags == 9));
        assert!(matches!(ev_rx.recv_timeout(espera).unwrap(), UserEvent::Status { flags: 0, temp_dc: None }));
        assert!(matches!(ev_rx.recv_timeout(espera).unwrap(), UserEvent::Goodbye { reason: 1, text } if text == "outro"));
        // A thread termina sem mandar `Disconnected` (o que o fim do fluxo de bytes faria).
        assert!(ev_rx.recv_timeout(Duration::from_millis(500)).is_err());
    }

    #[test]
    fn textos_do_estado_do_aparelho_nao_dependem_so_de_cor() {
        assert_eq!(decorated_title("foot", 0), "foot");
        assert_eq!(decorated_title("foot", ps_proto::STATUS_UNDERVOLT_NOW), "foot [energia fraca]");
        assert_eq!(decorated_title("foot", ps_proto::STATUS_UNDERVOLT_NOW | ps_proto::STATUS_DISPOSABLE), "foot [energia fraca] [descartável]");
        assert!(status_text(ps_proto::STATUS_UNDERVOLT_NOW, Some(487)).contains("energia fraca"));
        assert!(status_text(0, None).contains("energia normal") && status_text(0, None).contains("desconhecida"));
        assert!(status_text(ps_proto::STATUS_DISPOSABLE, Some(-15)).contains("-1.5"));
        assert_eq!(goodbye_text(ps_proto::GOODBYE_REPLACED, "X"), "o cliente \"X\" assumiu a sessão");
    }
}
