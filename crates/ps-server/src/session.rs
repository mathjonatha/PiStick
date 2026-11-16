
use std::sync::mpsc::{self, RecvTimeoutError};
use std::time::{Duration, Instant};

use std::sync::{Arc, Mutex};

use std::ops::Range;

use ps_codec::{Encoder, Rect, BLOCK};
use ps_proto::{
    read_hello, read_message, write_message, Message, ProtoError, AUDIO_STREAM_MIC, AUDIO_STREAM_OUT, CAPS_MARCO3, CAP_ATOMIC, CAP_AUDIO, CAP_BLOCK_CODEC, CAP_CLIPBOARD, CAP_CURSOR, CAP_H264, CAP_MIC, CAP_POPUPS, CAP_SESSION,
    CODEC_BLOCKS, CODEC_H264, VIDEO_FLAG_KEY,
};
use ps_transport::{Split, Transport};

use crate::audio::{Subscription, BLOCK_FRAMES, CHANNELS};
use crate::mic::MicSession;
use crate::flow::{plan_slice, strip_rows, subtract_band, Candidate, Flow};
use crate::source::{SourceEvent, Waker, WindowSource};
use crate::synthetic::clamp_size;
use crate::video::detect::{Action, BlockRect, Detector, StopReason, ENCODER_FAILURE_COOLDOWN_MS};
use crate::video::{EncodedFrame, VideoConfig, VideoEncoder, VideoEncoderFactory, MIN_CAPTURE_INTERVAL_MS, VIDEO_FPS};
use crate::ServerOptions;

const MAX_PENDING_RECTS: usize = 32;
const RECENT_CHANGE_MS: u64 = 100;
const PENDING_CREDIT_MS: u64 = 2000;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionEnd {
    ClientClosed,
    ClientGoodbye,
    ServerGoodbye(u8),
    Error(String),
}

pub struct SessionCtl {
    state: Mutex<CtlState>,
}

#[derive(Default)]
struct CtlState {
    request: Option<(u8, String)>,
    waker: Option<Waker>,
}

impl SessionCtl {
    pub fn new() -> Arc<SessionCtl> {
        Arc::new(SessionCtl { state: Mutex::new(CtlState::default()) })
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, CtlState> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn request_end(&self, reason: u8, text: &str) {
        let waker = {
            let mut g = self.lock();
            if g.request.is_none() {
                g.request = Some((reason, text.to_string()));
            }
            g.waker.clone()
        };
        if let Some(w) = waker {
            w();
        }
    }

    fn attach(&self, waker: Waker) {
        let pendente = {
            let mut g = self.lock();
            g.waker = Some(waker.clone());
            g.request.is_some()
        };
        if pendente {
            waker();
        }
    }

    pub(crate) fn take(&self) -> Option<(u8, String)> {
        self.lock().request.take()
    }
}

#[derive(Default)]
struct Held {
    keys: Vec<(u32, u32)>,
    buttons: Vec<(u32, u32)>,
}

impl Held {
    fn note(&mut self, msg: &Message) {
        let (lista, par, pressed) = match msg {
            Message::Key { win, keycode, pressed } => (&mut self.keys, (*win, *keycode), *pressed),
            Message::PointerButton { win, button, pressed } => (&mut self.buttons, (*win, *button), *pressed),
            _ => return,
        };
        lista.retain(|p| p.1 != par.1);
        if pressed {
            lista.push(par);
        }
    }

    fn release_all(&mut self, source: &mut dyn WindowSource, opts: &ServerOptions) {
        let (teclas, botoes) = (self.keys.len(), self.buttons.len());
        for (win, keycode) in self.keys.drain(..) {
            source.input(&Message::Key { win, keycode, pressed: false });
        }
        for (win, button) in self.buttons.drain(..) {
            source.input(&Message::PointerButton { win, button, pressed: false });
        }
        if teclas + botoes > 0 {
            opts.log(&format!("fim da sessão: soltando {teclas} tecla(s) e {botoes} botão(ões) que o cliente deixou apertados"));
        }
    }
}

fn end_from(e: ProtoError, contexto: &str) -> SessionEnd {
    match e {
        ProtoError::Closed => SessionEnd::ClientClosed,
        ProtoError::Io(ref io)
            if matches!(
                io.kind(),
                std::io::ErrorKind::ConnectionReset | std::io::ErrorKind::ConnectionAborted | std::io::ErrorKind::BrokenPipe
            ) =>
        {
            SessionEnd::ClientClosed
        }
        other => SessionEnd::Error(format!("{contexto}: {other}")),
    }
}

enum Ev {
    Msg(Message),
    End(ProtoError),
    Wake,
}

struct Snapshot {
    pixels: Vec<u8>,
    pending: Vec<Rect>,
    cursor: usize,
    capture_us: u64,
}

struct Win {
    id: u32,
    width: u32,
    height: u32,
    encoder: Encoder,
    seq: u32,
    pending: Vec<Rect>,
    snap: Option<Snapshot>,
    spare: Vec<u8>,
    need_full: bool,
    content_us: u64,
    detector: Detector,
    video: Option<ActiveVideo>,
    video_dirty: bool,
    video_touched: Vec<bool>,
    changed_acc: Vec<bool>,
    last_det_ms: u64,
    last_coded: Vec<bool>,
    last_coded_ms: Vec<u64>,
}

struct ActiveVideo {
    stream: u32,
    rect: Rect,
    block_rect: BlockRect,
    encoder: Box<dyn VideoEncoder>,
    last_capture_ms: u64,
    est_bytes: usize,
    frames: u32,
}

impl Win {
    fn new(id: u32, width: u32, height: u32) -> Self {
        let encoder = Encoder::new(width, height);
        let grid = encoder.block_cols() * encoder.block_rows();
        Win {
            id,
            width,
            height,
            encoder,
            seq: 0,
            pending: Vec::new(),
            snap: None,
            spare: Vec::new(),
            need_full: true,
            content_us: 0,
            detector: {
                let mut d = Detector::new(width, height);
                if std::env::var_os("PSD_DEBUG_DETECT").is_some_and(|v| v != "0") {
                    d.enable_trace();
                }
                d
            },
            video: None,
            video_dirty: false,
            video_touched: vec![false; grid],
            changed_acc: vec![false; grid],
            last_det_ms: 0,
            last_coded: vec![false; grid],
            last_coded_ms: vec![0; grid],
        }
    }

    fn add_damage(&mut self, rects: Vec<Rect>, now_ms: u64) {
        let rects: Vec<Rect> = rects.into_iter().filter(|r| !r.is_empty()).collect();
        let Some(v) = &self.video else {
            self.pending.extend(rects);
            self.merge_pending();
            return;
        };
        let cols = self.encoder.block_cols();
        let mut event = vec![false; self.video_touched.len()];
        for r in &rects {
            if let Some(hit) = r.intersect(v.rect) {
                self.video_dirty = true;
                mark_blocks(hit, cols, &mut event);
            }
            let dentro = r.x >= v.rect.x && r.y >= v.rect.y && r.x + r.w <= v.rect.x + v.rect.w && r.y + r.h <= v.rect.y + v.rect.h;
            if !dentro {
                self.pending.push(*r);
            }
        }
        let covered = event.iter().filter(|&&b| b).count() as u32;
        for (t, e) in self.video_touched.iter_mut().zip(&event) {
            *t |= *e;
        }
        self.detector.on_damage(now_ms, covered, v.block_rect.blocks());
        self.merge_pending();
    }

    fn merge_pending(&mut self) {
        if self.pending.len() > MAX_PENDING_RECTS {
            let all = self.pending.iter().fold(Rect::default(), |a, r| a.union(*r));
            self.pending.clear();
            self.pending.push(all);
        }
    }

    fn reset_video(&mut self) {
        self.video = None;
        self.video_dirty = false;
        self.detector.reset(self.width, self.height);
        let grid = self.encoder.block_cols() * self.encoder.block_rows();
        self.video_touched.clear();
        self.video_touched.resize(grid, false);
        self.changed_acc.clear();
        self.changed_acc.resize(grid, false);
        self.clear_detection_history();
        self.last_coded.resize(grid, false);
        self.last_coded_ms.resize(grid, 0);
    }

    fn clear_detection_history(&mut self) {
        self.changed_acc.fill(false);
        self.last_coded.fill(false);
        self.last_coded_ms.fill(0);
    }
}

fn mark_blocks(r: Rect, cols: usize, flags: &mut [bool]) {
    let b = BLOCK as u32;
    for by in (r.y / b)..=((r.y + r.h - 1) / b) {
        for bx in (r.x / b)..=((r.x + r.w - 1) / b) {
            if let Some(f) = flags.get_mut(by as usize * cols + bx as usize) {
                *f = true;
            }
        }
    }
}

struct VideoCtl {
    factory: Option<Arc<dyn VideoEncoderFactory>>,
    bitrate: u32,
    max_qp: u32,
    next_stream: u32,
    blocked_until_ms: u64,
    busy_other: bool,
}

#[derive(Default)]
struct Stats {
    per_win: Vec<(u32, u32, u64)>,
    enc_total_us: u64,
    enc_max_us: u64,
    enc_count: u32,
    held: u32,
    slices: u32,
    det_frames: u32,
    audio_packets: u32,
    audio_bytes: u64,
}

struct AudioCtl {
    sub: Subscription,
    seq: u32,
    dropped_logged: u64,
}

impl Stats {
    fn record_frame(&mut self, win: u32, bytes: usize, enc_us: u64) {
        match self.per_win.iter_mut().find(|e| e.0 == win) {
            Some(e) => {
                e.1 += 1;
                e.2 += bytes as u64;
            }
            None => self.per_win.push((win, 1, bytes as u64)),
        }
        self.enc_total_us += enc_us;
        self.enc_max_us = self.enc_max_us.max(enc_us);
        self.enc_count += 1;
    }

    fn line(&self, flow: &Flow, show_detection: bool, audio: bool) -> String {
        let mut partes: Vec<String> = Vec::new();
        for &(win, n, bytes) in &self.per_win {
            partes.push(format!("janela {win}: {n} quadros, {:.1} KiB/quadro", bytes as f64 / n as f64 / 1024.0));
        }
        if partes.is_empty() {
            partes.push("nenhum quadro enviado".to_string());
        }
        let avg = if self.enc_count > 0 { self.enc_total_us / u64::from(self.enc_count) } else { 0 };
        let audio = if audio { format!(" | áudio: {} pacotes, {:.0} kbit/s", self.audio_packets, self.audio_bytes as f64 * 8.0 / 1000.0) } else { String::new() };
        let det = if show_detection { format!(" | detecção: {} quadros", self.det_frames) } else { String::new() };
        format!(
            "{} | codificação média {avg} µs, máx {} µs | quadros retidos pelo fluxo: {} | fatias: {} | orçamento {:.0} KiB (vazão {:.2} MiB/s), em voo {:.0} KiB{det}{audio}",
            partes.join(" | "),
            self.enc_max_us,
            self.held,
            self.slices,
            flow.budget() as f64 / 1024.0,
            flow.throughput() / 1048576.0,
            flow.in_flight_bytes() as f64 / 1024.0
        )
    }
}

pub fn serve_session<T: Transport>(transport: T, opts: &ServerOptions, source: &mut dyn WindowSource) -> SessionEnd {
    serve_session_with(transport, opts, source, None)
}

pub fn serve_session_with<T: Transport>(transport: T, opts: &ServerOptions, source: &mut dyn WindowSource, ctl: Option<Arc<SessionCtl>>) -> SessionEnd {
    let peer = transport.peer();
    if let Err(e) = transport.set_send_queue_limit(opts.send_queue_bytes) {
        opts.log(&format!("aviso: não foi possível limitar a fila de envio: {e}"));
    }
    let Split { mut reader, mut writer, closer } = match transport.split() {
        Ok(s) => s,
        Err(e) => return SessionEnd::Error(format!("não foi possível separar o transporte: {e}")),
    };

    let (done_tx, done_rx) = mpsc::channel::<()>();
    let vigia_closer = closer.clone();
    let prazo = opts.hello_timeout;
    let vigia = std::thread::spawn(move || {
        if let Err(RecvTimeoutError::Timeout) = done_rx.recv_timeout(prazo) {
            vigia_closer.close();
            return true;
        }
        false
    });
    let hello = read_hello(&mut reader);
    let _ = done_tx.send(());
    let expirou = vigia.join().unwrap_or(false);
    let client_caps = match hello {
        Ok((caps, name)) if !expirou => {
            opts.log(&format!("cliente {peer} conectado: \"{name}\" (caps {caps:#x})"));
            caps
        }
        _ if expirou => {
            closer.close();
            return SessionEnd::Error(format!("HELLO não chegou em {} s; conexão fechada", prazo.as_secs_f32()));
        }
        Err(e) => return SessionEnd::Error(format!("handshake falhou: {e}")),
        Ok(_) => unreachable!(),
    };
    let ours = CAP_BLOCK_CODEC | CAPS_MARCO3 | CAP_ATOMIC | if opts.video.is_some() { CAP_H264 } else { 0 } | if opts.audio.is_some() { CAP_AUDIO } else { 0 } | if opts.mic.is_some() { CAP_MIC } else { 0 } | CAP_SESSION;
    let caps = client_caps & ours;
    if let Err(e) = write_message(&mut writer, &Message::hello_with_caps(&opts.name, ours)) {
        return SessionEnd::Error(format!("falha ao enviar HELLO: {e}"));
    }
    let (tx, rx) = mpsc::channel::<Ev>();
    let wake_tx = tx.clone();
    let waker: Waker = std::sync::Arc::new(move || {
        let _ = wake_tx.send(Ev::Wake);
    });
    let mut windows: Vec<Win> = Vec::new();
    if let Some(c) = &ctl {
        c.attach(waker.clone());
    }
    source.set_client_caps(caps);
    let audio = match (&opts.audio, caps & CAP_AUDIO != 0) {
        (Some(hub), true) => Some(AudioCtl { sub: hub.subscribe(waker.clone()), seq: 0, dropped_logged: 0 }),
        _ => None,
    };
    let mic = match (&opts.mic, caps & CAP_MIC != 0) {
        (Some(hub), true) => Some(hub.attach(waker.clone())),
        _ => None,
    };
    let initial = source.start(waker);

    std::thread::spawn(move || loop {
        match read_message(&mut reader) {
            Ok(m) => {
                if tx.send(Ev::Msg(m)).is_err() {
                    break;
                }
            }
            Err(e) => {
                let _ = tx.send(Ev::End(e));
                break;
            }
        }
    });

    let mut held = Held::default();
    let end = run_loop(&mut writer, &rx, &mut windows, initial, caps, source, opts, audio, mic, ctl.as_deref(), &mut held);
    held.release_all(source, opts);
    source.stop();
    if matches!(end, SessionEnd::ServerGoodbye(_)) && caps & CAP_SESSION != 0 {
        let _ = std::thread::Builder::new().name("adeus".into()).spawn(move || {
            let fim = Instant::now() + Duration::from_millis(250);
            while let Some(resto) = fim.checked_duration_since(Instant::now()) {
                match rx.recv_timeout(resto) {
                    Ok(Ev::End(_)) | Err(_) => break,
                    Ok(_) => {}
                }
            }
            closer.close();
        });
    } else {
        closer.close();
    }
    end
}

#[allow(clippy::too_many_arguments)]
fn run_loop<W: std::io::Write>(
    writer: &mut W,
    rx: &mpsc::Receiver<Ev>,
    windows: &mut Vec<Win>,
    initial: Vec<SourceEvent>,
    caps: u32,
    source: &mut dyn WindowSource,
    opts: &ServerOptions,
    mut audio: Option<AudioCtl>,
    mut mic: Option<MicSession>,
    ctl: Option<&SessionCtl>,
    held: &mut Held,
) -> SessionEnd {
    let start = Instant::now();
    let mut stats = Stats::default();
    let mut last_stats = start;
    let mut last_move_log = start - Duration::from_secs(1);
    let mut moves_suppressed = 0u32;
    let mut flow = Flow::new(opts.flow_initial_throughput, opts.flow_adaptive);
    let mut pace_until_us = 0u64;
    let atomic = caps & CAP_ATOMIC != 0;
    let mut vctl = VideoCtl {
        factory: if caps & CAP_H264 != 0 { opts.video.clone() } else { None },
        bitrate: opts.video_bitrate,
        max_qp: opts.video_max_qp,
        next_stream: 1,
        blocked_until_ms: 0,
        busy_other: false,
    };
    if let Some(a) = &audio {
        let m = Message::AudioStart { stream: AUDIO_STREAM_OUT, codec: a.sub.codec().wire(), channels: CHANNELS as u8, rate: ps_audio::SAMPLE_RATE, frames: BLOCK_FRAMES as u16 };
        if let Err(e) = write_message(writer, &m) {
            return end_from(e, "falha ao enviar AUDIO_START");
        }
        opts.log(&format!("áudio: fluxo aberto ({:?}, {} Hz, {} canais, {} quadros por pacote)", a.sub.codec(), ps_audio::SAMPLE_RATE, CHANNELS, BLOCK_FRAMES));
    }
    for ev in initial {
        if let Err(end) = apply_event(writer, windows, &mut flow, ev, 0, caps, opts) {
            return end;
        }
    }

    loop {
        let now_ms = start.elapsed().as_millis() as u64;
        let mut wait = source.next_poll_in(now_ms).unwrap_or(Duration::from_millis(250)).min(Duration::from_millis(250));
        for w in windows.iter() {
            if let Some(v) = &w.video {
                wait = wait.min(Duration::from_millis(50));
                let due = v.last_capture_ms + MIN_CAPTURE_INTERVAL_MS;
                if w.video_dirty && due > now_ms {
                    wait = wait.min(Duration::from_millis(due - now_ms));
                }
            }
        }
        if pace_until_us > now_ms * 1000 {
            wait = wait.min(Duration::from_micros(pace_until_us - now_ms * 1000));
        }
        if let Some(due) = mic.as_ref().and_then(|m| m.next_deadline()) {
            wait = wait.min(due.saturating_duration_since(Instant::now()));
        }
        match rx.recv_timeout(wait) {
            Ok(Ev::Msg(msg)) => {
                let now_us = start.elapsed().as_micros() as u64;
                held.note(&msg);
                if let Message::Goodbye { reason, .. } = &msg {
                    if caps & CAP_SESSION != 0 {
                        opts.log(&format!("cliente se despediu (GOODBYE, motivo {reason}); sessão encerrada"));
                        return SessionEnd::ClientGoodbye;
                    }
                    opts.log("GOODBYE de um cliente que não anunciou CAP_SESSION; ignorado");
                } else if handle_mic_message(mic.as_mut(), &msg, now_us, opts) {
                } else if let Err(end) = handle_message(writer, source, &mut flow, now_us, msg, opts, &mut last_move_log, &mut moves_suppressed) {
                    return end;
                }
            }
            Ok(Ev::Wake) => {}
            Ok(Ev::End(e)) => return end_from(e, "leitura"),
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => return SessionEnd::Error("thread de leitura terminou".into()),
        }
        if let Some((reason, text)) = ctl.and_then(|c| c.take()) {
            return say_goodbye(writer, caps, reason, &text, mic.as_ref(), opts);
        }

        if let Err(end) = flush_audio(writer, audio.as_mut(), &mut stats, opts) {
            return end;
        }
        if let Err(end) = poll_mic(writer, mic.as_mut(), opts) {
            return end;
        }
        let now = Instant::now();
        let now_us = now.duration_since(start).as_micros() as u64;
        for ev in source.poll(now_us / 1000) {
            if let Err(end) = apply_event(writer, windows, &mut flow, ev, now_us, caps, opts) {
                return end;
            }
        }
        if vctl.factory.is_some() {
            for w in windows.iter_mut() {
                w.detector.suppress_until(vctl.blocked_until_ms);
                if let Some(Action::Stop(motivo)) = w.detector.poll(now_us / 1000) {
                    if let Err(end) = stop_video(writer, w, StopWhy::Detector(motivo), true, now_us, opts) {
                        return end;
                    }
                }
            }
            let mut ocupadas: Vec<u32> = windows.iter().filter(|o| o.video.is_some()).map(|o| o.id).collect();
            for w in windows.iter_mut() {
                let ms = now_us / 1000;
                if w.video.is_none() && (!w.pending.is_empty() || w.snap.is_some()) && ms >= w.last_det_ms + MIN_CAPTURE_INTERVAL_MS {
                    vctl.busy_other = ocupadas.iter().any(|&id| id != w.id);
                    stats.det_frames += 1;
                    if let Err(end) = deliver_detection(writer, w, &flow, now_us, opts, &mut vctl) {
                        return end;
                    }
                    if w.video.is_some() {
                        ocupadas.push(w.id);
                    }
                }
            }
        }

        loop {
            if let Err(end) = flush_audio(writer, audio.as_mut(), &mut stats, opts) {
                return end;
            }
            let now_ms = start.elapsed().as_millis() as u64;
            if pace_until_us > start.elapsed().as_micros() as u64 {
                break;
            }
            let cands: Vec<Candidate> = windows.iter().filter_map(|w| w.candidate(now_ms, &flow)).collect();
            if cands.is_empty() {
                break;
            }
            let now_us = start.elapsed().as_micros() as u64;
            match flow.decide(&cands) {
                Some(pick) => {
                    vctl.busy_other = windows.iter().any(|o| o.id != pick.win && o.video.is_some());
                    let Some(w) = windows.iter_mut().find(|w| w.id == pick.win) else { break };
                    let r = if w.has_block_work() {
                        let fatia = if w.video.is_some() && w.snap.is_none() { None } else { pick.slice_bytes };
                        send_frame(writer, source, w, fatia, atomic, &mut flow, now_us, opts, &mut stats, &mut vctl)
                    } else {
                        send_video_frame(writer, source, w, &mut flow, now_us, opts, &mut stats, &mut vctl)
                    };
                    match r {
                        Ok(sent) => {
                            if let Some(rate) = opts.max_rate.filter(|r| *r > 0.0) {
                                let agora = start.elapsed().as_micros() as u64;
                                pace_until_us = pace_until_us.max(agora) + (sent as f64 / rate * 1e6) as u64;
                            }
                        }
                        Err(end) => return end,
                    }
                }
                None => {
                    stats.held += cands.len() as u32;
                    break;
                }
            }
        }

        if now.duration_since(last_stats) >= Duration::from_secs(1) {
            opts.log(&format!("1 s: {}", stats.line(&flow, vctl.factory.is_some() && windows.iter().all(|w| w.video.is_none()), audio.is_some())));
            if let Some(m) = mic.as_mut().filter(|m| m.requested()) {
                let ms = m.take_stats();
                opts.log(&format!(
                    "microfone: {} pacotes, {:.0} kbit/s | fila {:.0} ms (alvo {:.0} ms), maior intervalo {:.0} ms, descartados {} quadros, esvaziamentos {}, pacotes recusados {}, saltos de seq {}, blocos que o PipeWire não aceitou {}, escritas atrasadas {}",
                    ms.packets,
                    ms.bytes as f64 * 8.0 / 1000.0,
                    ms.level_ms,
                    ms.target_ms,
                    ms.gap_ms,
                    ms.queue.dropped_frames,
                    ms.queue.underruns,
                    m.discarded(),
                    m.seq_gaps(),
                    ms.pw_drops,
                    ms.pw_late
                ));
            }
            stats = Stats::default();
            last_stats = now;
        }
    }
}

fn say_goodbye<W: std::io::Write>(writer: &mut W, caps: u32, reason: u8, text: &str, mic: Option<&MicSession>, opts: &ServerOptions) -> SessionEnd {
    if caps & CAP_SESSION == 0 {
        opts.log(&format!("sessão encerrada pelo servidor (motivo {reason}); o cliente não anunciou CAP_SESSION, só é desconectado"));
        return SessionEnd::ServerGoodbye(reason);
    }
    if mic.is_some_and(|m| m.requested()) {
        let _ = write_message(writer, &Message::AudioRequest { stream: AUDIO_STREAM_MIC, on: false });
    }
    let enviado = write_message(writer, &Message::Goodbye { reason, text: text.to_string() }).is_ok();
    opts.log(&format!("sessão encerrada pelo servidor: GOODBYE motivo {reason}{}{}", if text.is_empty() { String::new() } else { format!(" (\"{text}\")") }, if enviado { "" } else { "; não foi possível enviá-lo" }));
    SessionEnd::ServerGoodbye(reason)
}

fn poll_mic<W: std::io::Write>(writer: &mut W, mic: Option<&mut MicSession>, opts: &ServerOptions) -> Result<(), SessionEnd> {
    let Some(m) = mic else { return Ok(()) };
    let Some(on) = m.poll(Instant::now()) else { return Ok(()) };
    let (_, quem) = m.recording();
    if on {
        opts.log(&format!("microfone: pedido ligado ({} gravando de pistick-mic)", if quem.is_empty() { "?".to_string() } else { quem.join(", ") }));
    } else {
        opts.log("microfone: pedido desligado (nenhum app grava de pistick-mic)");
    }
    write_message(writer, &Message::AudioRequest { stream: AUDIO_STREAM_MIC, on }).map_err(|e| end_from(e, "falha ao enviar AUDIO_REQUEST"))
}

fn handle_mic_message(mic: Option<&mut MicSession>, msg: &Message, now_us: u64, opts: &ServerOptions) -> bool {
    let stream = match msg {
        Message::AudioStart { stream, .. } | Message::AudioData { stream, .. } | Message::AudioStop { stream } => *stream,
        _ => return false,
    };
    if stream != AUDIO_STREAM_MIC {
        return false;
    }
    let Some(m) = mic else {
        opts.log("microfone: mensagem do fluxo 1 sem CAP_MIC na sessão; descartada");
        return true;
    };
    match msg {
        Message::AudioStart { codec, channels, rate, frames, .. } => match m.on_start(*codec, *channels, *rate, *frames) {
            Ok(()) => opts.log(&format!("microfone: fluxo aberto pelo cliente (codec {codec}, {channels} canais, {rate} Hz)")),
            Err(e) => opts.log(&format!("microfone: {e}; descartado")),
        },
        Message::AudioData { seq, data, .. } => {
            if let Err(e) = m.on_data(*seq, data, now_us) {
                opts.log(&format!("microfone: {e}"));
            }
        }
        Message::AudioStop { .. } => {
            let pedido = m.requested();
            if m.on_stop() {
                opts.log("microfone: o cliente encerrou o fluxo (AUDIO_STOP)");
            } else if pedido {
                opts.log("microfone: o cliente respondeu ao pedido com AUDIO_STOP sem abrir o fluxo (sem dispositivo de gravação?)");
            } else {
                opts.log("microfone: AUDIO_STOP do cliente depois do pedido de desligar (confirmação)");
            }
        }
        _ => {}
    }
    true
}

fn flush_audio<W: std::io::Write>(writer: &mut W, audio: Option<&mut AudioCtl>, stats: &mut Stats, opts: &ServerOptions) -> Result<(), SessionEnd> {
    let Some(a) = audio else { return Ok(()) };
    let mut pkts = Vec::new();
    a.sub.drain(&mut pkts);
    for p in pkts {
        let bytes = p.data.len();
        let msg = Message::AudioData { stream: AUDIO_STREAM_OUT, flags: p.flags, seq: a.seq, pts_us: p.pts_us, data: p.data };
        write_message(writer, &msg).map_err(|e| end_from(e, "falha ao enviar AUDIO_DATA"))?;
        a.seq = a.seq.wrapping_add(1);
        stats.audio_packets += 1;
        stats.audio_bytes += bytes as u64;
    }
    let dropped = a.sub.dropped();
    if dropped > a.dropped_logged {
        opts.log(&format!("áudio: {} pacotes descartados por atraso (mais de 200 ms na fila)", dropped - a.dropped_logged));
        a.dropped_logged = dropped;
    }
    Ok(())
}

impl Win {
    fn block_dirty_blocks(&self) -> usize {
        let damage = if self.need_full { None } else { Some(self.frame_pending()) };
        self.encoder.dirty_per_strip(damage).iter().sum()
    }

    fn frame_pending(&self) -> &[Rect] {
        match &self.snap {
            Some(s) => &s.pending,
            None => &self.pending,
        }
    }

    fn has_block_work(&self) -> bool {
        if self.snap.is_some() {
            true
        } else if self.video.is_none() {
            !self.pending.is_empty()
        } else {
            !self.pending.is_empty() && self.block_dirty_blocks() > 0
        }
    }

    fn video_due(&self, now_ms: u64) -> bool {
        self.video.as_ref().is_some_and(|v| self.video_dirty && now_ms >= v.last_capture_ms + MIN_CAPTURE_INTERVAL_MS)
    }

    fn candidate(&self, now_ms: u64, flow: &Flow) -> Option<Candidate> {
        let block_work = self.has_block_work();
        let video_due = self.video_due(now_ms);
        if !block_work && !video_due {
            return None;
        }
        let mut blocks = if block_work { self.block_dirty_blocks() } else { 0 };
        if let (true, Some(v)) = (video_due, &self.video) {
            blocks += (v.est_bytes as f64 / flow.bytes_per_block(self.id).max(1.0)).ceil() as usize;
        }
        Some(Candidate { win: self.id, pending_blocks: blocks, can_slice: !self.need_full })
    }
}

fn apply_event<W: std::io::Write>(writer: &mut W, windows: &mut Vec<Win>, flow: &mut Flow, ev: SourceEvent, now_us: u64, caps: u32, opts: &ServerOptions) -> Result<(), SessionEnd> {
    let send = |writer: &mut W, m: &Message| write_message(writer, m).map_err(|e| end_from(e, "falha ao enviar"));
    match ev {
        SourceEvent::Created { win, width, height, title, app_id } => {
            if windows.iter().all(|w| w.id != win) {
                opts.log(&format!("janela {win}: criada {width}x{height} \"{title}\""));
                windows.push(Win::new(win, width, height));
                flow.add_window(win, now_us);
                send(writer, &Message::WinCreate { win, width, height, title, app_id })?;
            }
        }
        SourceEvent::Resized { win, width, height } => {
            if let Some(w) = windows.iter_mut().find(|w| w.id == win) {
                if (width, height) != (w.width, w.height) {
                    opts.log(&format!("janela {win}: redimensionada para {width}x{height}"));
                    w.width = width;
                    w.height = height;
                    if let Some(v) = w.video.as_ref() {
                        opts.log(&format!("janela {win}: vídeo (fluxo {}) encerrado pelo redimensionamento", v.stream));
                    }
                    w.encoder.reset(width, height);
                    w.reset_video();
                    w.pending.clear();
                    w.snap = None;
                    w.need_full = true;
                    send(writer, &Message::WinResize { win, width, height })?;
                }
            }
        }
        SourceEvent::Title { win, title } => {
            if windows.iter().any(|w| w.id == win) {
                send(writer, &Message::WinTitle { win, title })?;
            }
        }
        SourceEvent::Destroyed { win } => {
            if let Some(i) = windows.iter().position(|w| w.id == win) {
                opts.log(&format!("janela {win}: destruída"));
                windows.remove(i);
                flow.remove_window(win);
                send(writer, &Message::WinDestroy { win })?;
            }
        }
        SourceEvent::PopupCreated { win, parent, x, y, width, height, grab } => {
            if caps & CAP_POPUPS != 0 && windows.iter().all(|w| w.id != win) {
                opts.log(&format!("janela {win}: popup de {parent} em ({x}, {y}) {width}x{height}{}", if grab { " com captura" } else { "" }));
                windows.push(Win::new(win, width, height));
                flow.add_window(win, now_us);
                flow.set_parent(win, parent);
                send(writer, &Message::PopupCreate { win, parent, x, y, width, height, grab })?;
            }
        }
        SourceEvent::PopupMoved { win, x, y, width, height } => {
            if caps & CAP_POPUPS != 0 && windows.iter().any(|w| w.id == win) {
                send(writer, &Message::PopupMove { win, x, y, width, height })?;
            }
        }
        SourceEvent::Parent { win, parent } => {
            if caps & CAP_POPUPS != 0 {
                send(writer, &Message::WinParent { win, parent })?;
            }
        }
        SourceEvent::Limits { win, min_w, min_h, max_w, max_h } => {
            if caps & CAP_POPUPS != 0 {
                send(writer, &Message::WinLimits { win, min_w, min_h, max_w, max_h })?;
            }
        }
        SourceEvent::Cursor { win, shape } => {
            if caps & CAP_CURSOR != 0 && windows.iter().any(|w| w.id == win) {
                send(writer, &Message::Cursor { win, shape })?;
            }
        }
        SourceEvent::Clipboard { mime, data } => {
            if caps & CAP_CLIPBOARD != 0 {
                opts.log(&format!("área de transferência do Pi: {} bytes", data.len()));
                send(writer, &Message::Clipboard { mime, data })?;
            }
        }
        SourceEvent::Damage { win, rects } => {
            if let Some(w) = windows.iter_mut().find(|w| w.id == win) {
                w.content_us = now_us;
                w.add_damage(rects, now_us / 1000);
            }
        }
    }
    Ok(())
}

struct Encoded {
    coded: usize,
    strips: Range<usize>,
    more: bool,
    slice: bool,
    capture_us: u64,
}

#[allow(clippy::too_many_arguments)]
fn send_frame<W: std::io::Write>(
    writer: &mut W,
    source: &mut dyn WindowSource,
    w: &mut Win,
    slice_bytes: Option<usize>,
    atomic: bool,
    flow: &mut Flow,
    now_us: u64,
    opts: &ServerOptions,
    stats: &mut Stats,
    vctl: &mut VideoCtl,
) -> Result<usize, SessionEnd> {
    let (width, height) = (w.width, w.height);
    let stride = width as usize * 4;
    let n_strips = w.encoder.strip_count();
    let mut data = Vec::new();
    let mut enc_us = 0u64;
    let win_id = w.id;
    let seq = w.seq;
    let mut result: Option<Encoded> = None;
    let continuacao = w.snap.is_some();

    if let Some(mut s) = w.snap.take() {
        let t0 = Instant::now();
        let por_faixa = w.encoder.dirty_per_strip(Some(&s.pending));
        let r = match slice_bytes {
            Some(target) => plan_slice(&por_faixa, s.cursor, flow.bytes_per_block(win_id), target),
            None => 0..n_strips,
        };
        let coded = w.encoder.encode_range(&s.pixels, stride, Some(&s.pending), r.clone(), &mut data);
        enc_us = t0.elapsed().as_micros() as u64;
        if slice_bytes.is_some() && !r.is_empty() {
            let (y0, y1) = strip_rows(&r);
            subtract_band(&mut s.pending, y0, y1);
            s.cursor = if r.end >= n_strips { 0 } else { r.end };
        } else {
            s.pending.clear();
        }
        let more = w.encoder.dirty_per_strip(Some(&s.pending)).iter().sum::<usize>() > 0;
        if let Some(hook) = &opts.frame_hook {
            hook(&crate::SentFrame { win: win_id, seq, width, height, pixels: &s.pixels, bytes: data.len(), strips: r.clone() });
        }
        let capture_us = s.capture_us;
        if more {
            w.snap = Some(s);
        } else {
            w.spare = s.pixels;
        }
        result = Some(Encoded { coded, strips: r, more, slice: true, capture_us });
    } else {
        source.with_pixels(win_id, &mut |pw, ph, pixels| {
            if (pw, ph) != (width, height) {
                return;
            }
            let t0 = Instant::now();
            let mut more = false;
            let mut slice = false;
            let (coded, strips) = if w.need_full {
                w.pending.clear();
                (w.encoder.encode(pixels, stride, None, &mut data), 0..n_strips)
            } else if let Some(target) = slice_bytes {
                let por_faixa = w.encoder.dirty_per_strip(Some(&w.pending));
                let r = plan_slice(&por_faixa, 0, flow.bytes_per_block(win_id), target);
                if r.is_empty() {
                    w.pending.clear();
                    result = Some(Encoded { coded: 0, strips: 0..0, more: false, slice: false, capture_us: w.content_us });
                    return;
                }
                slice = true;
                let coded = w.encoder.encode_range(pixels, stride, Some(&w.pending), r.clone(), &mut data);
                let (y0, y1) = strip_rows(&r);
                let mut resto = std::mem::take(&mut w.pending);
                subtract_band(&mut resto, y0, y1);
                if w.encoder.dirty_per_strip(Some(&resto)).iter().sum::<usize>() > 0 {
                    let mut copia = std::mem::take(&mut w.spare);
                    copia.clear();
                    copia.extend_from_slice(pixels);
                    w.snap = Some(Snapshot { pixels: copia, pending: resto, cursor: if r.end >= n_strips { 0 } else { r.end }, capture_us: w.content_us });
                    more = true;
                }
                (coded, r)
            } else {
                let coded = w.encoder.encode(pixels, stride, Some(&w.pending), &mut data);
                w.pending.clear();
                (coded, 0..n_strips)
            };
            enc_us = t0.elapsed().as_micros() as u64;
            w.need_full = false;
            if let Some(hook) = &opts.frame_hook {
                hook(&crate::SentFrame { win: win_id, seq, width, height, pixels, bytes: data.len(), strips: strips.clone() });
            }
            result = Some(Encoded { coded, strips, more, slice, capture_us: w.content_us });
        });
    }
    let Some(enc) = result else {
        w.pending.clear();
        w.need_full = true;
        return Ok(0);
    };
    if enc.strips.is_empty() && !enc.slice {
        return Ok(0);
    }
    if w.video.is_some() && enc.coded == 0 && !enc.more && !continuacao {
        source.frame_sent(w.id);
        return Ok(0);
    }
    let (strips, sent_slice, more) = (enc.strips, enc.slice, enc.more);
    if sent_slice {
        stats.slices += 1;
    }

    let bytes = data.len();
    flow.record_encoded(w.id, enc.coded, bytes);
    let msg = Message::Frame { win: w.id, seq: w.seq, capture_us: enc.capture_us, width, height, codec: CODEC_BLOCKS, more: more && atomic, data };
    write_message(writer, &msg).map_err(|e| end_from(e, "falha ao enviar FRAME"))?;
    flow.on_sent(w.id, w.seq, bytes, now_us);
    source.frame_sent(w.id);
    w.seq = w.seq.wrapping_add(1);
    stats.record_frame(w.id, bytes, enc_us);

    if vctl.factory.is_some() && w.video.is_none() {
        let now_ms = now_us / 1000;
        let cols = w.encoder.block_cols();
        let coded_now = w.encoder.coded_blocks();
        for (a, &c) in w.changed_acc.iter_mut().zip(coded_now) {
            *a |= c;
        }
        let attempted = w.encoder.attempted_blocks();
        let (lo, hi) = (strips.start * cols, (strips.end * cols).min(coded_now.len()));
        for (((last, ms), &c), &t) in w.last_coded[lo..hi].iter_mut().zip(&mut w.last_coded_ms[lo..hi]).zip(&coded_now[lo..hi]).zip(&attempted[lo..hi]) {
            if t {
                *last = c;
                if c {
                    *ms = now_ms;
                }
            }
        }
        let intervalo_ok = now_ms >= w.last_det_ms + MIN_CAPTURE_INTERVAL_MS;
        if !sent_slice || intervalo_ok || (w.pending.is_empty() && w.snap.is_none()) {
            stats.det_frames += 1;
            deliver_detection(writer, w, flow, now_us, opts, vctl)?;
        }
    }
    Ok(bytes)
}

fn deliver_detection<W: std::io::Write>(writer: &mut W, w: &mut Win, flow: &Flow, now_us: u64, opts: &ServerOptions, vctl: &mut VideoCtl) -> Result<(), SessionEnd> {
    let now_ms = now_us / 1000;
    let (width, height) = (w.width, w.height);
    let cols = w.encoder.block_cols();
    w.last_det_ms = now_ms;
    let changed = std::mem::replace(&mut w.changed_acc, vec![false; w.video_touched.len()]);
    let mut changed = changed;
    let mut pend = vec![false; changed.len()];
    let em_curso = w.snap.as_ref().map_or(&[][..], |s| s.pending.as_slice());
    for r in w.pending.iter().chain(em_curso) {
        if let Some(hit) = r.intersect(Rect::new(0, 0, width, height)) {
            mark_blocks(hit, cols, &mut pend);
        }
    }
    for (((c, &last), &ms), &p) in changed.iter_mut().zip(&w.last_coded).zip(&w.last_coded_ms).zip(&pend) {
        let idade = now_ms.saturating_sub(ms);
        *c |= last && (idade <= RECENT_CHANGE_MS || (p && idade <= PENDING_CREDIT_MS));
    }
    let action = w.detector.on_frame(now_ms, &changed, flow.bytes_per_block(w.id));
    for linha in w.detector.take_trace() {
        opts.log(&format!("janela {}: {linha}", w.id));
    }
    if let Some(Action::Start(rect)) = action {
        if vctl.busy_other {
            w.detector.decline();
        } else {
            start_video(writer, w, rect, now_us, opts, vctl)?;
        }
    }
    Ok(())
}

enum StopWhy {
    Detector(StopReason),
    EncoderError,
}

fn start_video<W: std::io::Write>(writer: &mut W, w: &mut Win, brect: BlockRect, now_us: u64, opts: &ServerOptions, vctl: &mut VideoCtl) -> Result<(), SessionEnd> {
    let now_ms = now_us / 1000;
    let Some(factory) = vctl.factory.clone() else { return Ok(()) };
    let rect = w.detector.to_pixels(brect);
    let cfg = VideoConfig { width: rect.w, height: rect.h, bitrate: vctl.bitrate, fps: VIDEO_FPS, max_qp: vctl.max_qp };
    let encoder = match factory.open(&cfg) {
        Ok(e) => e,
        Err(e) => {
            opts.log(&format!("janela {}: não foi possível abrir o codificador de vídeo: {e}; sem vídeo por {} s", w.id, ENCODER_FAILURE_COOLDOWN_MS / 1000));
            w.detector.encoder_failed(now_ms);
            vctl.blocked_until_ms = now_ms + ENCODER_FAILURE_COOLDOWN_MS;
            return Ok(());
        }
    };
    if let Some(d) = encoder.describe() {
        opts.log(&format!("janela {}: {d}", w.id));
    }
    let stream = vctl.next_stream;
    vctl.next_stream += 1;
    write_message(writer, &Message::VideoStart { win: w.id, stream, x: rect.x, y: rect.y, width: rect.w, height: rect.h, codec: CODEC_H264 })
        .map_err(|e| end_from(e, "falha ao enviar VIDEO_START"))?;
    w.clear_detection_history();
    w.encoder.set_excluded(Some(rect));
    opts.log(&format!("janela {}: vídeo iniciado (fluxo {stream}) na região {}x{} em ({}, {}) t={:.3}s", w.id, rect.w, rect.h, rect.x, rect.y, now_us as f64 / 1e6));
    w.video = Some(ActiveVideo {
        stream,
        rect,
        block_rect: brect,
        encoder,
        last_capture_ms: now_ms.saturating_sub(MIN_CAPTURE_INTERVAL_MS),
        est_bytes: (vctl.bitrate as usize / 8 / VIDEO_FPS as usize).max(1),
        frames: 0,
    });
    w.video_dirty = true;
    w.video_touched.fill(false);
    Ok(())
}

fn stop_video<W: std::io::Write>(writer: &mut W, w: &mut Win, why: StopWhy, send_stop: bool, now_us: u64, opts: &ServerOptions) -> Result<(), SessionEnd> {
    let Some(v) = w.video.take() else { return Ok(()) };
    let motivo = match why {
        StopWhy::Detector(StopReason::NoDamage) => "sem dano na região",
        StopWhy::Detector(StopReason::LowCoverage) => "dano cobre menos da metade da região",
        StopWhy::EncoderError => "erro do codificador",
    };
    opts.log(&format!("janela {}: vídeo encerrado (fluxo {}, {} quadros): {motivo} t={:.3}s", w.id, v.stream, v.frames, now_us as f64 / 1e6));
    if send_stop {
        write_message(writer, &Message::VideoStop { win: w.id, stream: v.stream }).map_err(|e| end_from(e, "falha ao enviar VIDEO_STOP"))?;
    }
    w.video_dirty = false;
    w.clear_detection_history();
    w.encoder.set_excluded(None);
    w.encoder.invalidate_rect(v.rect);
    if let Some(s) = w.snap.take() {
        w.pending.extend(s.pending);
        w.spare = s.pixels;
    }
    w.pending.push(v.rect);
    w.merge_pending();
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn send_video_frame<W: std::io::Write>(
    writer: &mut W,
    source: &mut dyn WindowSource,
    w: &mut Win,
    flow: &mut Flow,
    now_us: u64,
    opts: &ServerOptions,
    stats: &mut Stats,
    vctl: &mut VideoCtl,
) -> Result<usize, SessionEnd> {
    let now_ms = now_us / 1000;
    let (width, height, win_id) = (w.width, w.height, w.id);
    let Some(v) = w.video.as_mut() else { return Ok(0) };
    let stride = width as usize * 4;
    let origin = (v.rect.y as usize * width as usize + v.rect.x as usize) * 4;
    let mut result: Option<std::io::Result<EncodedFrame>> = None;
    let mut enc_us = 0u64;
    source.with_pixels(win_id, &mut |pw, ph, pixels| {
        if (pw, ph) != (width, height) {
            return;
        }
        let t0 = Instant::now();
        result = Some(v.encoder.encode(&pixels[origin..], stride));
        enc_us = t0.elapsed().as_micros() as u64;
    });
    let Some(result) = result else {
        v.last_capture_ms = now_ms;
        w.video_dirty = false;
        return Ok(0);
    };
    let frame = match result {
        Ok(f) if v.frames == 0 && !f.keyframe => Err(std::io::Error::other("o primeiro quadro do fluxo não é quadro-chave")),
        other => other,
    };
    let frame = match frame {
        Ok(f) => f,
        Err(e) => {
            opts.log(&format!("janela {win_id}: erro no codificador de vídeo: {e}; sem vídeo por {} s", ENCODER_FAILURE_COOLDOWN_MS / 1000));
            w.detector.encoder_failed(now_ms);
            vctl.blocked_until_ms = now_ms + ENCODER_FAILURE_COOLDOWN_MS;
            return stop_video(writer, w, StopWhy::EncoderError, true, now_us, opts).map(|()| 0);
        }
    };
    let bytes = frame.data.len();
    let keyframe = frame.keyframe;
    let msg = Message::VideoFrame { win: win_id, stream: v.stream, seq: w.seq, capture_us: w.content_us, flags: if keyframe { VIDEO_FLAG_KEY } else { 0 }, data: frame.data };
    write_message(writer, &msg).map_err(|e| end_from(e, "falha ao enviar VIDEO_FRAME"))?;
    flow.on_sent(win_id, w.seq, bytes, now_us);
    source.frame_sent(win_id);
    w.seq = w.seq.wrapping_add(1);
    stats.record_frame(win_id, bytes, enc_us);
    v.frames += 1;
    v.last_capture_ms = now_ms;
    if !keyframe {
        v.est_bytes = (v.est_bytes + bytes) / 2;
    }
    w.video_dirty = false;
    let _ = w.detector.on_frame(now_ms, &w.video_touched, flow.bytes_per_block(win_id));
    w.video_touched.fill(false);
    Ok(bytes)
}

#[allow(clippy::too_many_arguments)]
fn handle_message<W: std::io::Write>(
    writer: &mut W,
    source: &mut dyn WindowSource,
    flow: &mut Flow,
    now_us: u64,
    msg: Message,
    opts: &ServerOptions,
    last_move_log: &mut Instant,
    moves_suppressed: &mut u32,
) -> Result<(), SessionEnd> {
    if let Some(hook) = &opts.input_hook {
        hook(&msg);
    }
    let send = |writer: &mut W, m: &Message| write_message(writer, m).map_err(|e| end_from(e, "falha ao enviar"));
    match msg {
        Message::Ping { value } => send(writer, &Message::Pong { echo: value })?,
        Message::FrameAck { win, seq } => flow.on_ack(win, seq, now_us),
        Message::ResizeReq { win, width, height } => {
            let (nw, nh) = clamp_size(width, height);
            source.input(&Message::ResizeReq { win, width: nw, height: nh });
        }
        Message::CloseReq { win } => {
            opts.log(&format!("janela {win}: pedido de fechamento do cliente"));
            source.input(&Message::CloseReq { win });
        }
        Message::PointerMove { win, x, y } => {
            if last_move_log.elapsed() >= Duration::from_millis(250) {
                opts.log(&format!("entrada: janela {win} ponteiro em ({x:.1}, {y:.1}) [+{} movimentos omitidos]", *moves_suppressed));
                *last_move_log = Instant::now();
                *moves_suppressed = 0;
            } else {
                *moves_suppressed += 1;
            }
            source.input(&msg);
        }
        Message::PointerButton { win, button, pressed } => {
            opts.log(&format!("entrada: janela {win} botão {button:#x} {}", if pressed { "pressionado" } else { "solto" }));
            source.input(&msg);
        }
        Message::PointerAxis { win, dx, dy } => {
            opts.log(&format!("entrada: janela {win} rolagem ({dx:.1}, {dy:.1})"));
            source.input(&msg);
        }
        Message::Key { win, keycode, pressed } => {
            opts.log(&format!("entrada: janela {win} tecla {keycode} {}", if pressed { "pressionada" } else { "solta" }));
            source.input(&msg);
        }
        Message::Focus { win, focused } => {
            if focused {
                flow.set_focus(win);
            } else {
                flow.clear_focus(win);
            }
            opts.log(&format!("entrada: janela {win} foco {}", if focused { "ganho" } else { "perdido" }));
            source.input(&msg);
        }
        Message::ClipboardUp { ref mime, ref data } => {
            opts.log(&format!("entrada: área de transferência do hospedeiro ({mime}, {} bytes)", data.len()));
            source.input(&msg);
        }
        other => opts.log(&format!("mensagem inesperada do cliente ignorada: kind {}", other.kind())),
    }
    Ok(())
}
