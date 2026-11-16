
use std::collections::VecDeque;
use std::io::Read;
use std::net::{SocketAddr, TcpStream, ToSocketAddrs};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use ps_audio::{encode, is_silent, samples_to_pcm, synth, CODEC_PCM, CODEC_PSA1, SAMPLE_RATE};
use ps_proto::{AUDIO_FLAG_DISCONTINUITY, AUDIO_FLAG_DROPPED};

use crate::source::Waker;

pub mod pipewire;

pub const BLOCK_FRAMES: usize = 480;
pub const CHANNELS: usize = 2;
pub const SILENCE_HANG_BLOCKS: u32 = 20;
pub const MAX_QUEUED_PACKETS: usize = 20;
pub const PIPEWIRE_PCM_ADDR: &str = "127.0.0.1:4711";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AudioCodec {
    Pcm,
    Psa1,
}

impl AudioCodec {
    pub fn wire(self) -> u8 {
        match self {
            AudioCodec::Pcm => CODEC_PCM,
            AudioCodec::Psa1 => CODEC_PSA1,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Packet {
    pub flags: u8,
    pub pts_us: u64,
    pub data: Vec<u8>,
}

struct SubState {
    queue: VecDeque<Packet>,
    waker: Waker,
}

struct HubState {
    sub: Option<SubState>,
    silent_blocks: u32,
    sending: bool,
    epoch: u64,
    pushed_packets: u64,
    pushed_bytes: u64,
    dropped_packets: u64,
}

pub struct AudioHub {
    start: Instant,
    codec: AudioCodec,
    state: Mutex<HubState>,
    changed: Condvar,
    stopped: AtomicBool,
}

impl AudioHub {
    pub fn new(codec: AudioCodec) -> Arc<AudioHub> {
        Arc::new(AudioHub {
            start: Instant::now(),
            codec,
            state: Mutex::new(HubState { sub: None, silent_blocks: 0, sending: false, epoch: 0, pushed_packets: 0, pushed_bytes: 0, dropped_packets: 0 }),
            changed: Condvar::new(),
            stopped: AtomicBool::new(false),
        })
    }

    pub fn codec(&self) -> AudioCodec {
        self.codec
    }

    pub fn now_us(&self) -> u64 {
        self.start.elapsed().as_micros() as u64
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HubState> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn subscribe(self: &Arc<Self>, waker: Waker) -> Subscription {
        let mut g = self.lock();
        g.sub = Some(SubState { queue: VecDeque::new(), waker });
        g.epoch += 1;
        g.silent_blocks = 0;
        g.sending = false;
        g.pushed_packets = 0;
        g.pushed_bytes = 0;
        g.dropped_packets = 0;
        self.changed.notify_all();
        Subscription { hub: self.clone(), epoch: g.epoch }
    }

    pub fn has_subscriber(&self) -> bool {
        self.lock().sub.is_some()
    }

    pub fn epoch(&self) -> u64 {
        self.lock().epoch
    }

    pub fn wait_for_subscriber(&self, timeout: Duration) -> bool {
        let g = self.lock();
        if g.sub.is_some() {
            return true;
        }
        let (g, _) = self.changed.wait_timeout_while(g, timeout, |s| s.sub.is_none() && !self.stopped.load(Ordering::SeqCst)).unwrap_or_else(|e| e.into_inner());
        g.sub.is_some()
    }

    pub fn stop(&self) {
        self.stopped.store(true, Ordering::SeqCst);
        self.changed.notify_all();
    }

    pub fn is_stopped(&self) -> bool {
        self.stopped.load(Ordering::SeqCst)
    }

    pub fn mark_gap(&self) {
        self.lock().sending = false;
    }

    pub fn push_block(&self, samples: &[i16], pts_us: u64) {
        let mut g = self.lock();
        let silent = is_silent(samples);
        g.silent_blocks = if silent { g.silent_blocks.saturating_add(1) } else { 0 };
        if g.silent_blocks > SILENCE_HANG_BLOCKS {
            g.sending = false;
            return;
        }
        let was_sending = g.sending;
        g.sending = true;
        let HubState { sub, pushed_packets, pushed_bytes, dropped_packets, .. } = &mut *g;
        let Some(sub) = sub.as_mut() else { return };
        let data = match self.codec {
            AudioCodec::Pcm => {
                let mut d = Vec::new();
                samples_to_pcm(samples, &mut d);
                d
            }
            AudioCodec::Psa1 => {
                let mut d = Vec::new();
                if encode(samples, CHANNELS, &mut d).is_err() {
                    return;
                }
                d
            }
        };
        *pushed_packets += 1;
        *pushed_bytes += data.len() as u64;
        sub.queue.push_back(Packet { flags: if was_sending { 0 } else { AUDIO_FLAG_DISCONTINUITY }, pts_us, data });
        let mut dropped = false;
        while sub.queue.len() > MAX_QUEUED_PACKETS {
            sub.queue.pop_front();
            *dropped_packets += 1;
            dropped = true;
        }
        if dropped {
            if let Some(first) = sub.queue.front_mut() {
                first.flags |= AUDIO_FLAG_DISCONTINUITY | AUDIO_FLAG_DROPPED;
            }
        }
        let waker = sub.waker.clone();
        drop(g);
        waker();
    }
}

pub struct Subscription {
    hub: Arc<AudioHub>,
    epoch: u64,
}

impl Subscription {
    pub fn drain(&self, out: &mut Vec<Packet>) {
        let mut g = self.hub.lock();
        if g.epoch != self.epoch {
            return;
        }
        if let Some(sub) = g.sub.as_mut() {
            out.extend(sub.queue.drain(..));
        }
    }

    pub fn dropped(&self) -> u64 {
        self.hub.lock().dropped_packets
    }

    pub fn codec(&self) -> AudioCodec {
        self.hub.codec
    }

    pub fn now_us(&self) -> u64 {
        self.hub.now_us()
    }
}

impl Drop for Subscription {
    fn drop(&mut self) {
        let mut g = self.hub.lock();
        if g.epoch == self.epoch {
            g.sub = None;
        }
    }
}

pub fn spawn_synthetic(hub: Arc<AudioHub>) -> JoinHandle<()> {
    std::thread::Builder::new()
        .name("psd-audio-sintetico".into())
        .spawn(move || {
            while !hub.is_stopped() {
                if !hub.wait_for_subscriber(Duration::from_millis(200)) {
                    continue;
                }
                let epoch = hub.epoch();
                let t0 = Instant::now();
                let base_us = hub.now_us();
                let mut n = 0u64;
                let mut block = vec![0i16; BLOCK_FRAMES * CHANNELS];
                while !hub.is_stopped() && hub.epoch() == epoch && hub.has_subscriber() {
                    for (i, f) in block.chunks_exact_mut(2).enumerate() {
                        let (l, r) = synth::frame(n + i as u64);
                        f[0] = l;
                        f[1] = r;
                    }
                    hub.push_block(&block, base_us + n * 1_000_000 / SAMPLE_RATE as u64);
                    n += BLOCK_FRAMES as u64;
                    let due = t0 + Duration::from_micros(n * 1_000_000 / SAMPLE_RATE as u64);
                    if let Some(wait) = due.checked_duration_since(Instant::now()) {
                        std::thread::sleep(wait);
                    }
                }
            }
        })
        .expect("criar thread do áudio sintético")
}

pub type LogFn = Arc<dyn Fn(&str) + Send + Sync>;

pub fn spawn_pcm_reader(hub: Arc<AudioHub>, addr: String, log: LogFn) -> JoinHandle<()> {
    std::thread::Builder::new()
        .name("psd-audio-pcm".into())
        .spawn(move || {
            let mut warned = false;
            while !hub.is_stopped() {
                if !hub.wait_for_subscriber(Duration::from_millis(200)) {
                    continue;
                }
                let sa: Option<SocketAddr> = addr.to_socket_addrs().ok().and_then(|mut a| a.next());
                let conn = sa.and_then(|sa| TcpStream::connect_timeout(&sa, Duration::from_millis(500)).ok());
                let Some(mut stream) = conn else {
                    if !warned {
                        log(&format!("áudio: sem conexão com o PipeWire em {addr}; tentando de novo a cada 0,5 s"));
                        warned = true;
                    }
                    std::thread::sleep(Duration::from_millis(500));
                    continue;
                };
                if warned {
                    log(&format!("áudio: conectado ao PipeWire em {addr}"));
                    warned = false;
                }
                let _ = stream.set_nodelay(true);
                let _ = stream.set_read_timeout(Some(Duration::from_millis(250)));
                hub.mark_gap();
                let epoch = hub.epoch();
                read_blocks(&hub, &mut stream, epoch);
                if !hub.is_stopped() && hub.has_subscriber() {
                    log("áudio: a conexão com o PipeWire caiu; reconectando");
                    std::thread::sleep(Duration::from_millis(200));
                }
            }
        })
        .expect("criar thread de leitura do PCM")
}

fn read_blocks<R: Read>(hub: &AudioHub, stream: &mut R, epoch: u64) {
    let block_bytes = BLOCK_FRAMES * CHANNELS * 2;
    let mut acc: Vec<u8> = Vec::with_capacity(block_bytes * 4);
    let mut buf = [0u8; 8192];
    let mut samples = vec![0i16; BLOCK_FRAMES * CHANNELS];
    let base_us = hub.now_us();
    let mut blocks = 0u64;
    loop {
        if hub.is_stopped() || hub.epoch() != epoch || !hub.has_subscriber() {
            return;
        }
        match stream.read(&mut buf) {
            Ok(0) => return,
            Ok(n) => acc.extend_from_slice(&buf[..n]),
            Err(e) if matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut | std::io::ErrorKind::Interrupted) => continue,
            Err(_) => return,
        }
        while acc.len() >= block_bytes {
            for (s, c) in samples.iter_mut().zip(acc[..block_bytes].chunks_exact(2)) {
                *s = i16::from_le_bytes([c[0], c[1]]);
            }
            acc.drain(..block_bytes);
            hub.push_block(&samples, base_us + blocks * (BLOCK_FRAMES as u64 * 1_000_000 / SAMPLE_RATE as u64));
            blocks += 1;
        }
    }
}

#[cfg(test)]
mod tests;
