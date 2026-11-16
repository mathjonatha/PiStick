
use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use ps_audio::{decode, pcm_to_samples, CODEC_PCM, CODEC_PSA1, SAMPLE_RATE};
use ps_proto::{AUDIO_FLAG_DISCONTINUITY, AUDIO_FLAG_DROPPED, AUDIO_STREAM_OUT};

pub mod dump;
#[cfg(windows)]
pub mod wasapi;
#[cfg(windows)]
pub mod wasapi_capture;

pub const MIN_TARGET_FRAMES: usize = 40 * 48;
pub const MAX_TARGET_FRAMES: usize = 300 * 48;
pub const OVERFLOW_MARGIN_FRAMES: usize = 100 * 48;
const AVG_WINDOW_FRAMES: f64 = 2.0 * SAMPLE_RATE as f64;
const SLOW_PERIOD_FRAMES: usize = 480;
const FAST_PERIOD_FRAMES: usize = 96;
const ADJUST_ON_FRAMES: f64 = 10.0 * 48.0;
const ADJUST_FAST_FRAMES: f64 = 30.0 * 48.0;
const ADJUST_OFF_FRAMES: f64 = 5.0 * 48.0;
const BASELINE_SECS: u64 = 60;
const JITTER_SECS: u64 = 30;
const TARGET_DECAY_FRAMES_PER_S: f64 = 5.0 * 48.0;
const UNDERRUN_FLOOR_US: u64 = 30_000_000;

#[derive(Debug, Clone, Copy)]
pub struct QueueConfig {
    pub adaptive: bool,
    pub initial: usize,
    pub max_target: usize,
    pub margin: usize,
}

impl Default for QueueConfig {
    fn default() -> Self {
        QueueConfig { adaptive: true, initial: MIN_TARGET_FRAMES, max_target: MAX_TARGET_FRAMES, margin: OVERFLOW_MARGIN_FRAMES }
    }
}

impl QueueConfig {
    pub fn fixed_ms(ms: usize) -> Self {
        QueueConfig { adaptive: false, initial: ms * 48, max_target: ms * 48, margin: OVERFLOW_MARGIN_FRAMES }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum QState {
    Buffering,
    Playing,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Adjust {
    None,
    Remove,
    Repeat,
}

#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct QueueStats {
    pub underruns: u64,
    pub overflows: u64,
    pub overflow_frames: u64,
    pub removed: u64,
    pub repeated: u64,
    pub restarts: u64,
    pub level_sum: f64,
    pub level_max: usize,
    pub level_samples: u64,
}

impl QueueStats {
    pub fn level_avg_ms(&self) -> f64 {
        if self.level_samples == 0 {
            0.0
        } else {
            self.level_sum / self.level_samples as f64 / 48.0
        }
    }
    pub fn level_max_ms(&self) -> f64 {
        self.level_max as f64 / 48.0
    }
}

pub struct JitterQueue {
    cfg: QueueConfig,
    buf: VecDeque<i16>,
    state: QState,
    tail_silent: bool,
    avg: f64,
    adjust: Adjust,
    fast: bool,
    since_adjust: usize,
    last_frame: [i16; 2],
    stats: QueueStats,
    target: f64,
    floor: f64,
    floor_until_us: u64,
    last_update_us: u64,
    base_buckets: VecDeque<(u64, i64)>,
    jitter_buckets: VecDeque<(u64, u64)>,
    jitter_us: u64,
    last_now_us: u64,
}

impl JitterQueue {
    pub fn new(cfg: QueueConfig) -> Self {
        JitterQueue {
            cfg,
            buf: VecDeque::with_capacity((cfg.initial.max(cfg.max_target) + cfg.margin + ps_audio::MAX_FRAMES) * 2),
            state: QState::Buffering,
            tail_silent: true,
            avg: cfg.initial as f64,
            adjust: Adjust::None,
            fast: false,
            since_adjust: 0,
            last_frame: [0, 0],
            stats: QueueStats::default(),
            target: cfg.initial as f64,
            floor: 0.0,
            floor_until_us: 0,
            last_update_us: 0,
            base_buckets: VecDeque::with_capacity(BASELINE_SECS as usize + 2),
            jitter_buckets: VecDeque::with_capacity(JITTER_SECS as usize + 2),
            jitter_us: 0,
            last_now_us: 0,
        }
    }

    pub fn level_frames(&self) -> usize {
        self.buf.len() / 2
    }

    pub fn is_playing(&self) -> bool {
        self.state == QState::Playing
    }

    pub fn stats(&self) -> QueueStats {
        self.stats
    }

    pub fn reset_stats(&mut self) {
        self.stats = QueueStats::default();
    }

    pub fn target_frames(&self) -> usize {
        self.effective_target(self.last_now_us) as usize
    }

    pub fn target_ms(&self) -> f64 {
        self.target_frames() as f64 / 48.0
    }

    pub fn jitter_ms(&self) -> f64 {
        self.jitter_us as f64 / 1000.0
    }

    fn effective_target(&self, now_us: u64) -> f64 {
        if self.cfg.adaptive && now_us < self.floor_until_us {
            self.target.max(self.floor)
        } else {
            self.target
        }
    }

    fn update_target(&mut self, now_us: u64) {
        if !self.cfg.adaptive {
            return;
        }
        let dt = now_us.saturating_sub(self.last_update_us) as f64 / 1e6;
        self.last_update_us = now_us;
        let raw = (self.jitter_us as f64 * 48.0 / 1000.0 * 1.15 + 15.0 * 48.0).clamp(MIN_TARGET_FRAMES as f64, self.cfg.max_target as f64);
        if raw >= self.target {
            self.target = raw;
        } else {
            self.target = raw.max(self.target - TARGET_DECAY_FRAMES_PER_S * dt);
        }
    }

    fn observe_arrival(&mut self, pts_us: u64, now_us: u64) {
        let sec = now_us / 1_000_000;
        let diff = now_us as i64 - pts_us as i64;
        match self.base_buckets.back_mut() {
            Some((s, m)) if *s == sec => *m = (*m).min(diff),
            _ => self.base_buckets.push_back((sec, diff)),
        }
        while self.base_buckets.front().is_some_and(|(s, _)| *s + BASELINE_SECS <= sec) {
            self.base_buckets.pop_front();
        }
        let baseline = self.base_buckets.iter().map(|(_, m)| *m).min().unwrap_or(diff);
        let delay = (diff - baseline).max(0) as u64;
        match self.jitter_buckets.back_mut() {
            Some((s, m)) if *s == sec => *m = (*m).max(delay),
            _ => self.jitter_buckets.push_back((sec, delay)),
        }
        while self.jitter_buckets.front().is_some_and(|(s, _)| *s + JITTER_SECS <= sec) {
            self.jitter_buckets.pop_front();
        }
        self.jitter_us = self.jitter_buckets.iter().map(|(_, m)| *m).max().unwrap_or(0);
    }

    pub fn push(&mut self, samples: &[i16], restart: bool) {
        let now = self.last_now_us;
        self.push_at(samples, restart, None, now);
    }

    pub fn pull(&mut self, out: &mut [i16]) {
        let now = self.last_now_us;
        self.pull_at(out, now);
    }

    pub fn push_at(&mut self, samples: &[i16], restart: bool, pts_us: Option<u64>, now_us: u64) {
        self.last_now_us = now_us;
        if restart {
            self.buf.clear();
            self.state = QState::Buffering;
            self.adjust = Adjust::None;
            self.base_buckets.clear();
            self.stats.restarts += 1;
        }
        if let (true, Some(pts), false) = (self.cfg.adaptive, pts_us, samples.is_empty()) {
            self.observe_arrival(pts, now_us);
        }
        self.update_target(now_us);
        if restart {
            self.avg = self.effective_target(now_us);
        }
        self.buf.extend(samples.iter().copied());
        self.tail_silent = samples.iter().all(|&s| s == 0);
        let target = self.effective_target(now_us);
        if self.state == QState::Buffering && self.level_frames() as f64 >= target {
            self.state = QState::Playing;
            self.since_adjust = 0;
            self.adjust = Adjust::None;
            self.avg = self.level_frames() as f64;
        }
        if self.level_frames() as f64 > target + self.cfg.margin as f64 {
            let drop = self.level_frames() - target as usize;
            self.buf.drain(..drop * 2);
            self.stats.overflows += 1;
            self.stats.overflow_frames += drop as u64;
            self.avg = target;
            self.adjust = Adjust::None;
        }
    }

    pub fn pull_at(&mut self, out: &mut [i16], now_us: u64) {
        self.last_now_us = now_us;
        self.update_target(now_us);
        let frames = out.len() / 2;
        if self.state == QState::Buffering {
            out.fill(0);
            return;
        }
        let target = self.effective_target(now_us);
        let level = self.level_frames();
        self.stats.level_sum += level as f64;
        self.stats.level_max = self.stats.level_max.max(level);
        self.stats.level_samples += 1;
        let alpha = (frames as f64 / AVG_WINDOW_FRAMES).min(1.0);
        self.avg += (level as f64 - self.avg) * alpha;
        let err = self.avg - target;
        let mag = err.abs();
        let want = if err > 0.0 { Adjust::Remove } else { Adjust::Repeat };
        if mag > ADJUST_FAST_FRAMES {
            self.adjust = want;
            self.fast = true;
        } else if mag > ADJUST_ON_FRAMES || (self.adjust != Adjust::None && mag > ADJUST_OFF_FRAMES) {
            self.adjust = want;
            self.fast = false;
        } else {
            self.adjust = Adjust::None;
        }
        let period = if self.fast { FAST_PERIOD_FRAMES } else { SLOW_PERIOD_FRAMES };
        for i in 0..frames {
            if self.adjust != Adjust::None {
                self.since_adjust += 1;
                if self.since_adjust >= period {
                    self.since_adjust = 0;
                    let now_level = self.buf.len() / 2;
                    match self.adjust {
                        Adjust::Remove if (now_level as f64) < target => {}
                        Adjust::Repeat if now_level as f64 > target => {}
                        Adjust::Remove if self.buf.len() >= 4 => {
                            let (a0, a1) = (self.buf.pop_front().unwrap_or(0), self.buf.pop_front().unwrap_or(0));
                            let (b0, b1) = (self.buf.pop_front().unwrap_or(0), self.buf.pop_front().unwrap_or(0));
                            let m = [((a0 as i32 + b0 as i32) / 2) as i16, ((a1 as i32 + b1 as i32) / 2) as i16];
                            out[i * 2] = m[0];
                            out[i * 2 + 1] = m[1];
                            self.last_frame = m;
                            self.stats.removed += 1;
                            continue;
                        }
                        Adjust::Repeat => {
                            let (n0, n1) = (self.buf.front().copied().unwrap_or(self.last_frame[0]), self.buf.get(1).copied().unwrap_or(self.last_frame[1]));
                            let m = [((self.last_frame[0] as i32 + n0 as i32) / 2) as i16, ((self.last_frame[1] as i32 + n1 as i32) / 2) as i16];
                            out[i * 2] = m[0];
                            out[i * 2 + 1] = m[1];
                            self.last_frame = m;
                            self.stats.repeated += 1;
                            continue;
                        }
                        _ => {}
                    }
                }
            }
            match (self.buf.pop_front(), self.buf.pop_front()) {
                (Some(l), Some(r)) => {
                    out[i * 2] = l;
                    out[i * 2 + 1] = r;
                    self.last_frame = [l, r];
                }
                _ => {
                    out[i * 2..].fill(0);
                    if !self.tail_silent {
                        self.stats.underruns += 1;
                        if self.cfg.adaptive {
                            self.target = (self.target * 1.5).min(self.cfg.max_target as f64);
                            self.floor = self.target;
                            self.floor_until_us = now_us + UNDERRUN_FLOOR_US;
                        }
                    }
                    self.state = QState::Buffering;
                    self.adjust = Adjust::None;
                    self.avg = self.effective_target(now_us);
                    return;
                }
            }
        }
    }
}

#[derive(Debug, Clone)]
pub struct AudioOptions {
    pub enabled: bool,
    pub dump: Option<PathBuf>,
    pub null_output: bool,
    pub volume: f32,
    pub target_ms: Option<u32>,
}

impl Default for AudioOptions {
    fn default() -> Self {
        AudioOptions { enabled: true, dump: None, null_output: false, volume: 1.0, target_ms: None }
    }
}

#[derive(Debug, Clone, Default)]
pub struct AudioStats {
    pub packets: u64,
    pub bytes: u64,
    pub frames: u64,
    pub resumes: u64,
    pub drops: u64,
    pub queue: QueueStats,
    pub level_now_ms: f64,
    pub open: bool,
    pub codec: u8,
    pub secs: f64,
    pub target_ms: f64,
    pub jitter_ms: f64,
    pub first_packet_ms: Option<f64>,
}

impl AudioStats {
    pub fn audiostats_line(&self) -> String {
        let q = &self.queue;
        format!(
            "AUDIOSTATS pacotes={} quadros={} kbit_s={:.0} retomadas={} descartes={} esvaziamentos={} estouros={} removidos={} repetidos={} recomecos={} nivel_medio={:.1}ms nivel_max={:.1}ms nivel_agora={:.1}ms alvo={:.1}ms jitter={:.1}ms janela={:.1}s",
            self.packets,
            self.frames,
            self.kbit_s(),
            self.resumes,
            self.drops,
            q.underruns,
            q.overflows,
            q.removed,
            q.repeated,
            q.restarts,
            q.level_avg_ms(),
            q.level_max_ms(),
            self.level_now_ms,
            self.target_ms,
            self.jitter_ms,
            self.secs
        )
    }

    pub fn kbit_s(&self) -> f64 {
        if self.secs > 0.0 {
            self.bytes as f64 * 8.0 / 1000.0 / self.secs
        } else {
            0.0
        }
    }
}

struct Stream {
    codec: u8,
    channels: usize,
    next_seq: u32,
}

struct Inner {
    stream: Option<Stream>,
    packets: u64,
    bytes: u64,
    frames: u64,
    resumes: u64,
    drops: u64,
    codec: u8,
    dump: Option<dump::WavDump>,
    sound_tail: VecDeque<(u64, Vec<i16>)>,
    since: Instant,
    first_packet_ms: Option<f64>,
}

pub trait AudioSink: Send {
    fn name(&self) -> &'static str;
    fn stop(&mut self);
}

pub struct AudioEngine {
    inner: Mutex<Inner>,
    queue: Mutex<JitterQueue>,
    created: Instant,
    stop: AtomicBool,
    sink: Mutex<Option<Box<dyn AudioSink>>>,
}

const SOUND_TAIL_PACKETS: usize = 100;

const PULL_SPIN_TRIES: u32 = 4000;

impl AudioEngine {
    pub fn new(dump: Option<dump::WavDump>) -> Arc<AudioEngine> {
        Self::with_queue(QueueConfig::default(), dump)
    }

    pub fn with_queue(cfg: QueueConfig, dump: Option<dump::WavDump>) -> Arc<AudioEngine> {
        Arc::new(AudioEngine {
            queue: Mutex::new(JitterQueue::new(cfg)),
            created: Instant::now(),
            inner: Mutex::new(Inner {
                stream: None,
                packets: 0,
                bytes: 0,
                frames: 0,
                resumes: 0,
                drops: 0,
                codec: 0,
                dump,
                sound_tail: VecDeque::new(),
                since: Instant::now(),
                first_packet_ms: None,
            }),
            stop: AtomicBool::new(false),
            sink: Mutex::new(None),
        })
    }

    pub fn attach_sink(&self, s: Box<dyn AudioSink>) {
        let velho = self.sink.lock().unwrap_or_else(|e| e.into_inner()).replace(s);
        if let Some(mut v) = velho {
            v.stop();
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn lock_queue(&self) -> std::sync::MutexGuard<'_, JitterQueue> {
        self.queue.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn now_us(&self) -> u64 {
        self.created.elapsed().as_micros() as u64
    }

    pub fn shutdown(&self) {
        self.stop.store(true, Ordering::SeqCst);
        let s = self.sink.lock().unwrap_or_else(|e| e.into_inner()).take();
        if let Some(mut s) = s {
            s.stop();
        }
    }

    pub fn is_shutdown(&self) -> bool {
        self.stop.load(Ordering::SeqCst)
    }

    pub fn on_start(&self, stream: u8, codec: u8, channels: u8, rate: u32, frames: u16) -> Result<(), String> {
        if stream != AUDIO_STREAM_OUT {
            return Err(format!("AUDIO_START do fluxo {stream}: o servidor só abre o fluxo 0 (saída)"));
        }
        if rate != SAMPLE_RATE {
            return Err(format!("AUDIO_START com taxa {rate} Hz (só {SAMPLE_RATE} Hz é aceito)"));
        }
        if codec != CODEC_PCM && codec != CODEC_PSA1 {
            return Err(format!("AUDIO_START com codec {codec} desconhecido"));
        }
        if channels != 1 && channels != 2 {
            return Err(format!("AUDIO_START com {channels} canais"));
        }
        if frames == 0 || frames as usize > ps_audio::MAX_FRAMES {
            return Err(format!("AUDIO_START com {frames} quadros por pacote"));
        }
        {
            let mut g = self.lock();
            g.stream = Some(Stream { codec, channels: channels as usize, next_seq: 0 });
            g.codec = codec;
        }
        let now = self.now_us();
        self.lock_queue().push_at(&[], true, None, now);
        Ok(())
    }

    pub fn on_data(&self, stream: u8, flags: u8, seq: u32, pts_us: u64, data: &[u8]) -> Result<(), String> {
        let mut g = self.lock();
        let (codec, channels) = match &mut g.stream {
            Some(s) if stream == AUDIO_STREAM_OUT => {
                if seq != s.next_seq {
                    return Err(format!("AUDIO_DATA fora de ordem: seq {seq}, esperado {}", s.next_seq));
                }
                s.next_seq = s.next_seq.wrapping_add(1);
                (s.codec, s.channels)
            }
            _ => return Err(format!("AUDIO_DATA do fluxo {stream} sem AUDIO_START")),
        };
        let mut samples: Vec<i16> = Vec::new();
        match codec {
            CODEC_PCM => {
                if data.is_empty() || !data.len().is_multiple_of(2 * channels) {
                    return Err(format!("AUDIO_DATA PCM com {} bytes", data.len()));
                }
                pcm_to_samples(data, &mut samples);
            }
            _ => {
                decode(data, channels, &mut samples).map_err(|e| format!("AUDIO_DATA PSA1 inválido: {e}"))?;
            }
        }
        if channels == 1 {
            samples = samples.iter().flat_map(|&s| [s, s]).collect();
        }
        let frames = samples.len() / 2;
        if g.first_packet_ms.is_none() {
            g.first_packet_ms = Some(self.created.elapsed().as_secs_f64() * 1000.0);
        }
        g.packets += 1;
        g.bytes += data.len() as u64;
        g.frames += frames as u64;
        let discontinuity = flags & AUDIO_FLAG_DISCONTINUITY != 0;
        if flags & AUDIO_FLAG_DROPPED != 0 {
            g.drops += 1;
        } else if discontinuity {
            g.resumes += 1;
        }
        if let Some(d) = g.dump.as_mut() {
            d.write(pts_us, &samples);
        }
        if !ps_audio::is_silent(&samples) {
            if discontinuity {
                g.sound_tail.clear();
            }
            g.sound_tail.push_back((pts_us, samples.clone()));
            while g.sound_tail.len() > SOUND_TAIL_PACKETS {
                g.sound_tail.pop_front();
            }
        }
        drop(g);
        let now = self.now_us();
        self.lock_queue().push_at(&samples, discontinuity, Some(pts_us), now);
        Ok(())
    }

    pub fn on_stop(&self, stream: u8) {
        let mut g = self.lock();
        if stream == AUDIO_STREAM_OUT {
            g.stream = None;
        }
    }

    pub fn pull(&self, out: &mut [i16]) {
        let now = self.now_us();
        self.lock_queue().pull_at(out, now);
    }

    pub fn pull_nowait(&self, out: &mut [i16]) -> bool {
        let now = self.now_us();
        for _ in 0..PULL_SPIN_TRIES {
            match self.queue.try_lock() {
                Ok(mut g) => {
                    g.pull_at(out, now);
                    return true;
                }
                Err(std::sync::TryLockError::Poisoned(e)) => {
                    e.into_inner().pull_at(out, now);
                    return true;
                }
                Err(std::sync::TryLockError::WouldBlock) => std::hint::spin_loop(),
            }
        }
        out.fill(0);
        false
    }

    #[doc(hidden)]
    pub fn hold_for_test<R>(&self, f: impl FnOnce() -> R) -> R {
        let _g = self.lock_queue();
        f()
    }

    #[doc(hidden)]
    pub fn hold_net_for_test<R>(&self, f: impl FnOnce() -> R) -> R {
        let _g = self.lock();
        f()
    }

    pub fn stats(&self) -> AudioStats {
        let (queue, level_frames, target_ms, jitter_ms) = {
            let q = self.lock_queue();
            (q.stats(), q.level_frames(), q.target_ms(), q.jitter_ms())
        };
        let g = self.lock();
        AudioStats {
            packets: g.packets,
            bytes: g.bytes,
            frames: g.frames,
            resumes: g.resumes,
            drops: g.drops,
            queue,
            level_now_ms: level_frames as f64 / 48.0,
            open: g.stream.is_some(),
            codec: g.codec,
            secs: g.since.elapsed().as_secs_f64(),
            target_ms,
            jitter_ms,
            first_packet_ms: g.first_packet_ms,
        }
    }

    pub fn reset_stats(&self) {
        self.lock_queue().reset_stats();
        let mut g = self.lock();
        g.packets = 0;
        g.bytes = 0;
        g.frames = 0;
        g.resumes = 0;
        g.drops = 0;
        g.since = Instant::now();
    }

    pub fn recent_sound(&self) -> (usize, Vec<i16>) {
        let g = self.lock();
        let mut start = g.sound_tail.len();
        let mut prev: Option<u64> = None;
        for (i, (pts, s)) in g.sound_tail.iter().enumerate().rev() {
            if let Some(p) = prev {
                let dur = (s.len() / 2) as u64 * 1_000_000 / SAMPLE_RATE as u64;
                if p.abs_diff(pts + dur) > 3000 {
                    break;
                }
            }
            prev = Some(*pts);
            start = i;
        }
        let pkts = g.sound_tail.len() - start;
        let samples = g.sound_tail.iter().skip(start).flat_map(|(_, s)| s.iter().copied()).collect();
        (pkts, samples)
    }

    pub fn flush_dump(&self) {
        if let Some(d) = self.lock().dump.as_mut() {
            d.flush();
        }
    }
}

pub fn spawn_null_consumer(engine: Arc<AudioEngine>) {
    std::thread::Builder::new()
        .name("psc-audio-nulo".into())
        .spawn(move || {
            let t0 = Instant::now();
            let mut produced: u64 = 0;
            let mut out = vec![0i16; 480 * 2];
            let mut last_flush = Instant::now();
            while !engine.is_shutdown() {
                let due = (t0.elapsed().as_micros() as u64 * SAMPLE_RATE as u64 / 1_000_000).saturating_sub(produced);
                if due >= 480 {
                    engine.pull(&mut out);
                    produced += 480;
                } else {
                    std::thread::sleep(Duration::from_millis(2));
                }
                if last_flush.elapsed() >= Duration::from_secs(1) {
                    engine.flush_dump();
                    last_flush = Instant::now();
                }
            }
            engine.flush_dump();
        })
        .expect("criar thread do consumidor nulo");
}

pub fn start(opts: &AudioOptions) -> Result<Option<Arc<AudioEngine>>, String> {
    if !opts.enabled {
        println!("[psc] áudio desativado (--no-audio)");
        return Ok(None);
    }
    let dump = match &opts.dump {
        Some(p) => Some(dump::WavDump::create(p).map_err(|e| format!("não foi possível criar {}: {e}", p.display()))?),
        None => None,
    };
    let cfg = match opts.target_ms {
        Some(ms) => QueueConfig::fixed_ms(ms as usize),
        None => QueueConfig::default(),
    };
    let engine = AudioEngine::with_queue(cfg, dump);
    if opts.dump.is_some() || opts.null_output {
        println!("[psc] áudio: saída descartada ({}); nada toca neste computador", if opts.dump.is_some() { "--audio-dump" } else { "--audio-null" });
        spawn_null_consumer(engine.clone());
        return Ok(Some(engine));
    }
    #[cfg(windows)]
    {
        match wasapi::spawn(engine.clone(), opts.volume) {
            Ok(()) => println!("[psc] áudio: WASAPI (modo compartilhado, volume do fluxo {:.0}%)", opts.volume * 100.0),
            Err(e) => {
                println!("[psc] áudio: não foi possível abrir o dispositivo ({e}); sem som, a fila é descartada");
                spawn_null_consumer(engine.clone());
            }
        }
    }
    #[cfg(not(windows))]
    {
        println!("[psc] áudio: sem saída neste sistema; a fila é descartada");
        spawn_null_consumer(engine.clone());
    }
    Ok(Some(engine))
}

#[cfg(test)]
mod tests;
