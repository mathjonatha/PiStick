
use std::collections::{HashMap, VecDeque};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpStream, ToSocketAddrs};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use ps_audio::{decode, pcm_to_samples, CODEC_PCM, CODEC_PSA1, SAMPLE_RATE};

use crate::audio::{LogFn, BLOCK_FRAMES, CHANNELS};
use crate::source::Waker;

pub const MIC_NODE: &str = "pistick-mic";
pub const PIPEWIRE_MIC_ADDR: &str = "127.0.0.1:4712";
pub const OFF_DELAY: Duration = Duration::from_millis(500);
pub const QUEUE_TARGET_MS: usize = 100;
pub const QUEUE_MIN_MS: usize = 20;
pub const QUEUE_MAX_MS: usize = 100;
pub const QUEUE_OVERFLOW_SLACK_MS: usize = 50;
pub const WARMUP_US: u64 = 6_000_000;
pub const JITTER_WINDOW_US: u64 = 30_000_000;
const FRAMES_PER_MS: usize = SAMPLE_RATE as usize / 1000;

#[derive(Debug, Default)]
pub struct LinkTracker {
    counts: HashMap<(String, String), u32>,
    header: Option<String>,
}

fn is_mic_out(port: &str) -> bool {
    port.strip_prefix(MIC_NODE).is_some_and(|r| r.starts_with(":capture_"))
}

fn node_of(port: &str) -> &str {
    port.rsplit_once(':').map_or(port, |(n, _)| n)
}

impl LinkTracker {
    pub fn new() -> LinkTracker {
        LinkTracker::default()
    }

    pub fn reset(&mut self) {
        self.counts.clear();
        self.header = None;
    }

    pub fn recording(&self) -> bool {
        !self.counts.is_empty()
    }

    pub fn consumers(&self) -> Vec<String> {
        let mut v: Vec<String> = self.counts.keys().map(|(_, dst)| node_of(dst).to_string()).collect();
        v.sort();
        v.dedup();
        v
    }

    fn add(&mut self, out: &str, dst: &str) {
        *self.counts.entry((out.to_string(), dst.to_string())).or_insert(0) += 1;
    }

    fn remove(&mut self, out: &str, dst: &str) {
        let key = (out.to_string(), dst.to_string());
        if let Some(n) = self.counts.get_mut(&key) {
            if *n > 1 {
                *n -= 1;
            } else {
                self.counts.remove(&key);
            }
        }
    }

    pub fn feed_line(&mut self, line: &str) -> bool {
        let line = line.trim_end_matches(['\r', '\n']);
        let before = (self.recording(), self.consumers());
        if let Some(rest) = line.strip_prefix("= ") {
            if let Some(dst) = rest.trim_start().strip_prefix("|-> ") {
                if let Some(h) = self.header.clone().filter(|h| is_mic_out(h)) {
                    self.add(&h, dst.trim());
                }
            } else if !rest.starts_with(' ') {
                self.header = Some(rest.trim().to_string());
            }
        } else if let Some(rest) = line.strip_prefix("+ ") {
            if let Some((out, dst)) = rest.split_once(" -> ") {
                if is_mic_out(out.trim()) {
                    self.add(out.trim(), dst.trim());
                }
            }
        } else if let Some(rest) = line.strip_prefix("- ") {
            if let Some((out, dst)) = rest.split_once(" -> ") {
                if is_mic_out(out.trim()) {
                    self.remove(out.trim(), dst.trim());
                }
            }
        }
        before != (self.recording(), self.consumers())
    }
}

#[derive(Debug, Default)]
pub struct RequestGate {
    on: bool,
    idle_since: Option<Instant>,
}

impl RequestGate {
    pub fn new() -> RequestGate {
        RequestGate::default()
    }

    pub fn is_on(&self) -> bool {
        self.on
    }

    pub fn update(&mut self, recording: bool, now: Instant) -> Option<bool> {
        if recording {
            self.idle_since = None;
            if !self.on {
                self.on = true;
                return Some(true);
            }
            return None;
        }
        if !self.on {
            return None;
        }
        let since = *self.idle_since.get_or_insert(now);
        if now.duration_since(since) >= OFF_DELAY {
            self.on = false;
            self.idle_since = None;
            return Some(false);
        }
        None
    }

    pub fn next_deadline(&self) -> Option<Instant> {
        self.idle_since.map(|t| t + OFF_DELAY)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pop {
    Full,
    Underrun,
    Silence,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct QueueStats {
    pub packets: u64,
    pub frames_in: u64,
    pub dropped_frames: u64,
    pub skipped_frames: u64,
    pub underruns: u64,
    pub silence_blocks: u64,
}

pub struct MicQueue {
    max_ms: usize,
    buf: VecDeque<i16>,
    primed: bool,
    target_frames: usize,
    stats: QueueStats,
    first_arrival_us: Option<u64>,
    frames_seen: u64,
    buckets: VecDeque<(u64, i64, i64)>,
    last_arrival_us: Option<u64>,
    max_gap_us: u64,
    warm: bool,
}

impl Default for MicQueue {
    fn default() -> Self {
        MicQueue::new()
    }
}

impl MicQueue {
    pub fn new() -> MicQueue {
        MicQueue::with_max_ms(QUEUE_MAX_MS)
    }

    pub fn with_max_ms(max_ms: usize) -> MicQueue {
        let max_ms = max_ms.clamp(QUEUE_MIN_MS, 300);
        MicQueue {
            max_ms,
            buf: VecDeque::new(),
            primed: false,
            target_frames: max_ms * FRAMES_PER_MS,
            stats: QueueStats::default(),
            first_arrival_us: None,
            frames_seen: 0,
            buckets: VecDeque::new(),
            last_arrival_us: None,
            max_gap_us: 0,
            warm: false,
        }
    }

    pub fn take_max_gap_ms(&mut self) -> f64 {
        std::mem::take(&mut self.max_gap_us) as f64 / 1000.0
    }

    pub fn level_frames(&self) -> usize {
        self.buf.len() / CHANNELS
    }

    pub fn level_ms(&self) -> f64 {
        self.level_frames() as f64 / FRAMES_PER_MS as f64
    }

    pub fn target_ms(&self) -> f64 {
        self.target_frames as f64 / FRAMES_PER_MS as f64
    }

    pub fn stats(&self) -> QueueStats {
        self.stats
    }

    pub fn reset(&mut self) {
        *self = MicQueue::with_max_ms(self.max_ms);
    }

    fn jitter_ms(&self) -> f64 {
        let lo = self.buckets.iter().map(|b| b.1).min();
        let hi = self.buckets.iter().map(|b| b.2).max();
        match (lo, hi) {
            (Some(l), Some(h)) => (h - l) as f64 / 1000.0,
            _ => 0.0,
        }
    }

    fn note_arrival(&mut self, frames: usize, now_us: u64) {
        if let Some(last) = self.last_arrival_us {
            self.max_gap_us = self.max_gap_us.max(now_us.saturating_sub(last));
        }
        self.last_arrival_us = Some(now_us);
        let first = *self.first_arrival_us.get_or_insert(now_us);
        let nominal = self.frames_seen * 1_000_000 / u64::from(SAMPLE_RATE);
        let transit = (now_us - first) as i64 - nominal as i64;
        self.frames_seen += frames as u64;
        while self.buckets.front().is_some_and(|b| now_us.saturating_sub(b.0) > JITTER_WINDOW_US) {
            self.buckets.pop_front();
        }
        match self.buckets.back_mut() {
            Some(b) if now_us - b.0 < 1_000_000 => {
                b.1 = b.1.min(transit);
                b.2 = b.2.max(transit);
            }
            _ => self.buckets.push_back((now_us, transit, transit)),
        }
        let want_ms = (self.jitter_ms() + 20.0).ceil() as usize;
        let want_ms = want_ms.div_ceil(10) * 10;
        self.warm = now_us.saturating_sub(first) >= WARMUP_US;
        let want_ms = if self.warm { want_ms } else { want_ms.max(self.max_ms) };
        self.target_frames = want_ms.clamp(QUEUE_MIN_MS, self.max_ms) * FRAMES_PER_MS;
    }

    pub fn push(&mut self, samples: &[i16], now_us: u64) {
        let frames = samples.len() / CHANNELS;
        if frames == 0 {
            return;
        }
        self.stats.packets += 1;
        self.stats.frames_in += frames as u64;
        self.note_arrival(frames, now_us);
        self.buf.extend(&samples[..frames * CHANNELS]);
        let limit = (self.max_ms + QUEUE_OVERFLOW_SLACK_MS) * FRAMES_PER_MS;
        if self.level_frames() > limit {
            let excess = self.level_frames() - self.max_ms * FRAMES_PER_MS;
            self.buf.drain(..excess * CHANNELS);
            self.stats.dropped_frames += excess as u64;
        }
    }

    pub fn pop_block(&mut self, out: &mut [i16]) -> Pop {
        debug_assert_eq!(out.len(), BLOCK_FRAMES * CHANNELS);
        if !self.primed {
            if self.level_frames() >= self.target_frames.max(BLOCK_FRAMES) {
                self.primed = true;
            } else {
                out.fill(0);
                self.stats.silence_blocks += 1;
                return Pop::Silence;
            }
        }
        if self.level_frames() >= BLOCK_FRAMES {
            let excess = self.level_frames().saturating_sub(self.target_frames);
            let skip = if !self.warm {
                0
            } else if excess > 50 * FRAMES_PER_MS {
                10
            } else if excess > 30 * FRAMES_PER_MS {
                5
            } else if excess > 10 * FRAMES_PER_MS {
                1
            } else {
                0
            };
            let skip = skip.min(self.level_frames() - BLOCK_FRAMES);
            if skip == 0 {
                for (o, s) in out.iter_mut().zip(self.buf.drain(..BLOCK_FRAMES * CHANNELS)) {
                    *o = s;
                }
            } else {
                let src: Vec<i16> = self.buf.drain(..(BLOCK_FRAMES + skip) * CHANNELS).collect();
                for i in 0..BLOCK_FRAMES {
                    let j = i + i * skip / BLOCK_FRAMES;
                    out[i * CHANNELS] = src[j * CHANNELS];
                    out[i * CHANNELS + 1] = src[j * CHANNELS + 1];
                }
                self.stats.skipped_frames += skip as u64;
            }
            return Pop::Full;
        }
        let have = self.buf.len();
        for (o, s) in out.iter_mut().zip(self.buf.drain(..)) {
            *o = s;
        }
        out[have..].fill(0);
        self.primed = false;
        self.stats.underruns += 1;
        Pop::Underrun
    }
}

pub struct LinkFeed {
    pub reader: Box<dyn Read + Send>,
    pub kill: Arc<dyn Fn() + Send + Sync>,
}

pub type OpenFeed = Arc<dyn Fn() -> std::io::Result<LinkFeed> + Send + Sync>;

pub fn pw_link_feed() -> OpenFeed {
    Arc::new(|| {
        let mut cmd = Command::new("pw-link");
        cmd.args(["--links", "--monitor"]).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::null()).env_remove("DBUS_SESSION_BUS_ADDRESS");
        let mut child: Child = cmd.spawn().map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                std::io::Error::new(e.kind(), "o programa pw-link não está instalado")
            } else {
                e
            }
        })?;
        #[cfg(target_os = "linux")]
        crate::discovery::register_child(child.id());
        let stdout = child.stdout.take().ok_or_else(|| std::io::Error::other("sem saída do pw-link"))?;
        let child = Arc::new(Mutex::new(child));
        let kill: Arc<dyn Fn() + Send + Sync> = Arc::new(move || {
            let mut c = child.lock().unwrap_or_else(|e| e.into_inner());
            let _ = c.kill();
            let _ = c.wait();
        });
        Ok(LinkFeed { reader: Box::new(stdout), kill })
    })
}

#[derive(Default)]
struct HubState {
    gen: u64,
    next_id: u64,
    owner: Option<u64>,
    waiting: Vec<(u64, Waker)>,
    waker: Option<Waker>,
    recording: bool,
    consumers: Vec<String>,
    kill_monitor: Option<Arc<dyn Fn() + Send + Sync>>,
}

struct Inner {
    open: OpenFeed,
    pcm_addr: String,
    log: LogFn,
    state: Mutex<HubState>,
    queue: Mutex<MicQueue>,
    pw_drops: std::sync::atomic::AtomicU64,
    pw_late: std::sync::atomic::AtomicU64,
}

impl Inner {
    fn lock(&self) -> std::sync::MutexGuard<'_, HubState> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn queue(&self) -> std::sync::MutexGuard<'_, MicQueue> {
        self.queue.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn is_current(&self, gen: u64) -> bool {
        self.lock().gen == gen
    }

    fn set_recording(&self, gen: u64, recording: bool, consumers: Vec<String>) {
        let waker = {
            let mut g = self.lock();
            if g.gen != gen || (g.recording == recording && g.consumers == consumers) {
                return;
            }
            g.recording = recording;
            g.consumers = consumers;
            g.waker.clone()
        };
        if let Some(w) = waker {
            w();
        }
    }
}

pub struct MicHub {
    inner: Arc<Inner>,
}

impl MicHub {
    pub fn new(open: OpenFeed, pcm_addr: &str, log: LogFn) -> Arc<MicHub> {
        MicHub::with_max_ms(open, pcm_addr, log, QUEUE_MAX_MS)
    }

    pub fn with_max_ms(open: OpenFeed, pcm_addr: &str, log: LogFn, max_ms: usize) -> Arc<MicHub> {
        Arc::new(MicHub { inner: Arc::new(Inner { open, pcm_addr: pcm_addr.to_string(), log, state: Mutex::new(HubState::default()), queue: Mutex::new(MicQueue::with_max_ms(max_ms)), pw_drops: Default::default(), pw_late: Default::default() }) })
    }

    pub fn attach(&self, waker: Waker) -> MicSession {
        let (id, gen) = {
            let mut g = self.inner.lock();
            g.next_id += 1;
            let id = g.next_id;
            if g.owner.is_none() {
                g.owner = Some(id);
                g.gen += 1;
                g.waker = Some(waker);
                g.recording = false;
                g.consumers.clear();
                (id, Some(g.gen))
            } else {
                g.waiting.push((id, waker));
                (id, None)
            }
        };
        if let Some(gen) = gen {
            self.inner.queue().reset();
            spawn_monitor(&self.inner, gen);
        } else {
            (self.inner.log)("microfone: outro cliente já tem o microfone; este espera");
        }
        MicSession { inner: self.inner.clone(), id, gate: RequestGate::new(), stream: None, writer: None, discarded: 0, discard_logged: false, seq_gaps: 0, packets: 0, bytes: 0 }
    }
}

fn spawn_monitor(inner: &Arc<Inner>, gen: u64) {
    let inner2 = inner.clone();
    let spawned = std::thread::Builder::new().name("psd-mic-monitor".into()).spawn(move || monitor_loop(&inner2, gen));
    if let Err(e) = spawned {
        (inner.log)(&format!("microfone: não foi possível criar a thread do monitor: {e}"));
    }
}

fn monitor_loop(inner: &Arc<Inner>, gen: u64) {
    let mut warned = false;
    let sleep_while_current = |total: Duration| {
        let t0 = Instant::now();
        while t0.elapsed() < total && inner.is_current(gen) {
            std::thread::sleep(Duration::from_millis(50));
        }
    };
    while inner.is_current(gen) {
        let feed = match (inner.open)() {
            Ok(f) => f,
            Err(e) => {
                if !warned {
                    (inner.log)(&format!("microfone: não foi possível iniciar o monitor de ligações: {e}; tentando de novo a cada 2 s"));
                    warned = true;
                }
                sleep_while_current(Duration::from_secs(2));
                continue;
            }
        };
        warned = false;
        let kill = feed.kill.clone();
        let current = {
            let mut g = inner.lock();
            if g.gen == gen {
                g.kill_monitor = Some(kill.clone());
                true
            } else {
                false
            }
        };
        if !current {
            kill();
            return;
        }
        let mut tracker = LinkTracker::new();
        for line in BufReader::new(feed.reader).lines() {
            let Ok(line) = line else { break };
            if tracker.feed_line(&line) {
                inner.set_recording(gen, tracker.recording(), tracker.consumers());
            }
            if !inner.is_current(gen) {
                break;
            }
        }
        kill();
        inner.set_recording(gen, false, Vec::new());
        if inner.is_current(gen) {
            (inner.log)("microfone: o monitor de ligações terminou; reabrindo");
            sleep_while_current(Duration::from_secs(1));
        }
    }
}

struct StreamState {
    codec: u8,
    channels: u8,
    next_seq: Option<u32>,
}

struct Writer {
    stop: Arc<AtomicBool>,
}

#[derive(Debug, Default, Clone, Copy, PartialEq)]
pub struct MicStats {
    pub packets: u64,
    pub bytes: u64,
    pub level_ms: f64,
    pub target_ms: f64,
    pub gap_ms: f64,
    pub pw_drops: u64,
    pub pw_late: u64,
    pub queue: QueueStats,
}

pub struct MicSession {
    inner: Arc<Inner>,
    id: u64,
    gate: RequestGate,
    stream: Option<StreamState>,
    writer: Option<Writer>,
    discarded: u64,
    discard_logged: bool,
    seq_gaps: u64,
    packets: u64,
    bytes: u64,
}

impl Drop for MicSession {
    fn drop(&mut self) {
        self.stop_writer();
        let (kill, promoted) = {
            let mut g = self.inner.lock();
            if g.owner == Some(self.id) {
                g.gen += 1;
                g.waker = None;
                g.recording = false;
                g.consumers.clear();
                g.owner = None;
                let kill = g.kill_monitor.take();
                let promoted = if g.waiting.is_empty() {
                    None
                } else {
                    let (nid, w) = g.waiting.remove(0);
                    g.owner = Some(nid);
                    g.gen += 1;
                    g.waker = Some(w.clone());
                    Some((g.gen, w))
                };
                (kill, promoted)
            } else {
                let id = self.id;
                g.waiting.retain(|(i, _)| *i != id);
                (None, None)
            }
        };
        if let Some(k) = kill {
            k();
        }
        self.inner.queue().reset();
        if let Some((gen, w)) = promoted {
            (self.inner.log)("microfone: o cliente que tinha o microfone saiu; o próximo da fila assume");
            spawn_monitor(&self.inner, gen);
            w();
        }
    }
}

impl MicSession {
    pub fn recording(&self) -> (bool, Vec<String>) {
        let g = self.inner.lock();
        if g.owner == Some(self.id) {
            (g.recording, g.consumers.clone())
        } else {
            (false, Vec::new())
        }
    }

    pub fn requested(&self) -> bool {
        self.gate.is_on()
    }

    pub fn next_deadline(&self) -> Option<Instant> {
        self.gate.next_deadline()
    }

    pub fn poll(&mut self, now: Instant) -> Option<bool> {
        let (recording, _) = self.recording();
        let change = self.gate.update(recording, now)?;
        if change {
            self.stream = None;
            self.discarded = 0;
            self.discard_logged = false;
            self.inner.queue().reset();
            self.inner.pw_drops.store(0, Ordering::Relaxed);
            self.inner.pw_late.store(0, Ordering::Relaxed);
            self.start_writer();
        } else {
            self.stop_writer();
            self.stream = None;
            self.discard_logged = false;
        }
        Some(change)
    }

    fn start_writer(&mut self) {
        self.stop_writer();
        let stop = Arc::new(AtomicBool::new(false));
        let inner = self.inner.clone();
        let flag = stop.clone();
        let spawned = std::thread::Builder::new().name("psd-mic-pcm".into()).spawn(move || writer_loop(&inner, &flag));
        match spawned {
            Ok(_) => self.writer = Some(Writer { stop }),
            Err(e) => (self.inner.log)(&format!("microfone: não foi possível criar a thread de escrita: {e}")),
        }
    }

    fn stop_writer(&mut self) {
        if let Some(w) = self.writer.take() {
            w.stop.store(true, Ordering::SeqCst);
        }
    }

    pub fn on_start(&mut self, codec: u8, channels: u8, rate: u32, _frames: u16) -> Result<(), String> {
        if !self.gate.is_on() {
            return Err("AUDIO_START do microfone sem pedido em vigor".into());
        }
        if rate != SAMPLE_RATE {
            return Err(format!("AUDIO_START do microfone com taxa {rate} (só {SAMPLE_RATE})"));
        }
        if codec != CODEC_PCM && codec != CODEC_PSA1 {
            return Err(format!("AUDIO_START do microfone com codec {codec} desconhecido"));
        }
        self.stream = Some(StreamState { codec, channels, next_seq: None });
        self.discard_logged = false;
        Ok(())
    }

    pub fn on_stop(&mut self) -> bool {
        self.stream.take().is_some()
    }

    pub fn on_data(&mut self, seq: u32, data: &[u8], now_us: u64) -> Result<(), String> {
        if !self.gate.is_on() || self.stream.is_none() {
            self.discarded += 1;
            if self.discard_logged {
                return Ok(());
            }
            self.discard_logged = true;
            return Err(if self.gate.is_on() { "AUDIO_DATA do microfone antes do AUDIO_START".into() } else { "AUDIO_DATA do microfone sem pedido em vigor; descartado".into() });
        }
        let Some(st) = self.stream.as_mut() else { return Ok(()) };
        if let Some(esperado) = st.next_seq {
            if seq != esperado {
                self.seq_gaps += 1;
            }
        }
        st.next_seq = Some(seq.wrapping_add(1));
        let (codec, channels) = (st.codec, st.channels as usize);
        let mut mono_or_stereo: Vec<i16> = Vec::new();
        if codec == CODEC_PCM {
            pcm_to_samples(data, &mut mono_or_stereo);
            if !mono_or_stereo.len().is_multiple_of(channels) {
                self.discarded += 1;
                return Err("AUDIO_DATA do microfone com PCM de tamanho inválido; descartado".into());
            }
        } else if let Err(e) = decode(data, channels, &mut mono_or_stereo) {
            self.discarded += 1;
            return Err(format!("AUDIO_DATA do microfone não decodifica ({e:?}); descartado"));
        }
        let stereo: Vec<i16> = if channels == 1 { mono_or_stereo.iter().flat_map(|&s| [s, s]).collect() } else { mono_or_stereo };
        self.packets += 1;
        self.bytes += data.len() as u64;
        self.inner.queue().push(&stereo, now_us);
        Ok(())
    }

    pub fn discarded(&self) -> u64 {
        self.discarded
    }

    pub fn seq_gaps(&self) -> u64 {
        self.seq_gaps
    }

    pub fn take_stats(&mut self) -> MicStats {
        let mut q = self.inner.queue();
        let s = MicStats { packets: self.packets, bytes: self.bytes, level_ms: q.level_ms(), target_ms: q.target_ms(), gap_ms: q.take_max_gap_ms(), pw_drops: self.inner.pw_drops.load(Ordering::Relaxed), pw_late: self.inner.pw_late.load(Ordering::Relaxed), queue: q.stats() };
        drop(q);
        self.packets = 0;
        self.bytes = 0;
        s
    }
}

fn writer_loop(inner: &Arc<Inner>, stop: &Arc<AtomicBool>) {
    let mut warned = false;
    let mut block = vec![0i16; BLOCK_FRAMES * CHANNELS];
    while !stop.load(Ordering::SeqCst) {
        let sa: Option<SocketAddr> = inner.pcm_addr.to_socket_addrs().ok().and_then(|mut a| a.next());
        let conn = sa.and_then(|sa| TcpStream::connect_timeout(&sa, Duration::from_millis(500)).ok());
        let Some(mut s) = conn else {
            if !warned {
                (inner.log)(&format!("microfone: sem conexão com o PipeWire em {}; tentando de novo a cada 0,5 s", inner.pcm_addr));
                warned = true;
            }
            std::thread::sleep(Duration::from_millis(500));
            continue;
        };
        if warned {
            (inner.log)(&format!("microfone: conectado ao PipeWire em {}", inner.pcm_addr));
            warned = false;
        }
        let _ = s.set_nodelay(true);
        let _ = s.set_nonblocking(true);
        let period = Duration::from_micros(BLOCK_FRAMES as u64 * 1_000_000 / u64::from(SAMPLE_RATE));
        let mut next = Instant::now();
        let mut pending: VecDeque<Vec<u8>> = VecDeque::new();
        let mut off = 0usize;
        let mut lost = false;
        while !stop.load(Ordering::SeqCst) && !lost {
            let now = Instant::now();
            if now < next {
                std::thread::sleep((next - now).min(Duration::from_millis(5)));
                continue;
            }
            if now.duration_since(next) > Duration::from_millis(100) {
                next = now;
                inner.pw_late.fetch_add(1, Ordering::Relaxed);
            }
            next += period;
            inner.queue().pop_block(&mut block);
            let mut bytes = Vec::with_capacity(block.len() * 2);
            for v in &block {
                bytes.extend_from_slice(&v.to_le_bytes());
            }
            pending.push_back(bytes);
            while pending.len() > 5 {
                inner.pw_drops.fetch_add(1, Ordering::Relaxed);
                if off > 0 {
                    pending.remove(1);
                } else {
                    pending.pop_front();
                }
            }
            while let Some(front) = pending.front() {
                match s.write(&front[off..]) {
                    Ok(0) => {
                        lost = true;
                        break;
                    }
                    Ok(n) => {
                        off += n;
                        if off == front.len() {
                            pending.pop_front();
                            off = 0;
                        }
                    }
                    Err(e) if matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::Interrupted) => break,
                    Err(_) => {
                        lost = true;
                        break;
                    }
                }
            }
        }
        if lost && !stop.load(Ordering::SeqCst) {
            (inner.log)("microfone: a conexão com o PipeWire caiu; reconectando");
            std::thread::sleep(Duration::from_millis(200));
        }
    }
}

#[cfg(test)]
mod tests;
