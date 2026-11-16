//! Áudio do servidor (marco 7): do PCM do PipeWire (ou do sintético) até os pacotes `AUDIO_DATA`.
//!
//! Estrutura:
//! - [`AudioHub`]: recebe blocos de 10 ms (480 quadros estéreo), detecta silêncio, codifica e guarda
//!   uma fila curta para a sessão atual. Sem sessão, o bloco é jogado fora.
//! - [`Subscription`]: a sessão se inscreve, é acordada quando há pacote e o retira com
//!   [`Subscription::drain`] (a numeração `seq` é da sessão, depois de qualquer descarte).
//! - Fontes de blocos: [`spawn_synthetic`] (senoides, sem PipeWire) e [`spawn_pcm_reader`] (soquete de PCM
//!   cru do módulo `protocol-simple` do PipeWire, com reconexão).
//! - [`pipewire::PipewireProcs`]: sobe `pipewire`, `wireplumber` e `pipewire-pulse` com a configuração do projeto.

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

/// Quadros por canal em cada pacote (10 ms a 48 kHz).
pub const BLOCK_FRAMES: usize = 480;
/// Canais do fluxo de saída.
pub const CHANNELS: usize = 2;
/// Blocos de silêncio ainda enviados antes de parar o fluxo (200 ms).
pub const SILENCE_HANG_BLOCKS: u32 = 20;
/// Máximo de pacotes esperando a sessão (200 ms); acima disso descarta os mais velhos.
pub const MAX_QUEUED_PACKETS: usize = 20;
/// Endereço padrão do soquete de PCM cru do PipeWire do projeto (`tools/pi/pipewire/pipewire.conf`).
pub const PIPEWIRE_PCM_ADDR: &str = "127.0.0.1:4711";

/// Codec dos pacotes enviados.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AudioCodec {
    Pcm,
    Psa1,
}

impl AudioCodec {
    /// Valor do campo `codec` de `AUDIO_START`.
    pub fn wire(self) -> u8 {
        match self {
            AudioCodec::Pcm => CODEC_PCM,
            AudioCodec::Psa1 => CODEC_PSA1,
        }
    }
}

/// Um pacote pronto para virar `AUDIO_DATA` (sem `seq`, que a sessão atribui).
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
    /// Blocos de silêncio seguidos.
    silent_blocks: u32,
    /// O bloco anterior foi enviado (ou posto na fila).
    sending: bool,
    /// Número da inscrição atual; as fontes o usam para recomeçar o sinal.
    epoch: u64,
    /// Pacotes e bytes colocados na fila desde a inscrição.
    pushed_packets: u64,
    pushed_bytes: u64,
    dropped_packets: u64,
}

/// Ponto de encontro entre as fontes de PCM e a sessão do cliente.
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

    /// Relógio do emissor: microssegundos desde a criação do hub.
    pub fn now_us(&self) -> u64 {
        self.start.elapsed().as_micros() as u64
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HubState> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Inscreve a sessão atual (a anterior, se houver, perde a fila). `waker` é chamado a cada pacote novo.
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

    /// Há sessão inscrita.
    pub fn has_subscriber(&self) -> bool {
        self.lock().sub.is_some()
    }

    /// Número da inscrição atual (0 = nunca houve).
    pub fn epoch(&self) -> u64 {
        self.lock().epoch
    }

    /// Espera até `timeout` por uma sessão inscrita; devolve `true` se há.
    pub fn wait_for_subscriber(&self, timeout: Duration) -> bool {
        let g = self.lock();
        if g.sub.is_some() {
            return true;
        }
        let (g, _) = self.changed.wait_timeout_while(g, timeout, |s| s.sub.is_none() && !self.stopped.load(Ordering::SeqCst)).unwrap_or_else(|e| e.into_inner());
        g.sub.is_some()
    }

    /// Pede às fontes que terminem.
    pub fn stop(&self) {
        self.stopped.store(true, Ordering::SeqCst);
        self.changed.notify_all();
    }

    pub fn is_stopped(&self) -> bool {
        self.stopped.load(Ordering::SeqCst)
    }

    /// A fonte perdeu a continuidade (reconexão do PipeWire): o próximo pacote leva a marca de descontinuidade.
    pub fn mark_gap(&self) {
        self.lock().sending = false;
    }

    /// Entrega um bloco de PCM estéreo intercalado (normalmente 480 quadros). `pts_us` é o instante do
    /// primeiro quadro no relógio do hub ([`AudioHub::now_us`]).
    pub fn push_block(&self, samples: &[i16], pts_us: u64) {
        let mut g = self.lock();
        let silent = is_silent(samples);
        g.silent_blocks = if silent { g.silent_blocks.saturating_add(1) } else { 0 };
        if g.silent_blocks > SILENCE_HANG_BLOCKS {
            // Silêncio com mais de 200 ms: o fluxo para; o próximo som leva a marca de descontinuidade.
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

/// A inscrição da sessão. Ao ser largada, o hub volta a jogar fora o que recebe.
pub struct Subscription {
    hub: Arc<AudioHub>,
    epoch: u64,
}

impl Subscription {
    /// Retira os pacotes que esperam, do mais velho ao mais novo.
    pub fn drain(&self, out: &mut Vec<Packet>) {
        let mut g = self.hub.lock();
        if g.epoch != self.epoch {
            return;
        }
        if let Some(sub) = g.sub.as_mut() {
            out.extend(sub.queue.drain(..));
        }
    }

    /// Pacotes descartados por atraso desde a inscrição.
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

/// Fonte sintética: 440 Hz à esquerda, 880 Hz à direita, 1 s de som e 1 s de silêncio, em tempo real,
/// recomeçando do zero a cada inscrição (os testes dependem disso).
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

/// Log das fontes de áudio.
pub type LogFn = Arc<dyn Fn(&str) + Send + Sync>;

/// Lê PCM cru S16LE estéreo de 48 kHz do soquete `addr` e o entrega ao hub em blocos de 10 ms. Só mantém a
/// conexão enquanto há sessão inscrita (sem ouvinte o PipeWire não gasta CPU com a captura) e reconecta
/// se o PipeWire cair ou ainda não tiver subido.
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

/// Lê blocos até a conexão cair, o hub parar ou a inscrição mudar.
fn read_blocks<R: Read>(hub: &AudioHub, stream: &mut R, epoch: u64) {
    let block_bytes = BLOCK_FRAMES * CHANNELS * 2;
    let mut acc: Vec<u8> = Vec::with_capacity(block_bytes * 4);
    let mut buf = [0u8; 8192];
    let mut samples = vec![0i16; BLOCK_FRAMES * CHANNELS];
    // Os instantes dos blocos seguem a contagem de amostras desde a conexão (grade de 10 ms): o PipeWire dá o
    // ritmo, e o relógio do sistema na hora de ler tem jitter de alguns milissegundos.
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
        // O que sobra de um bloco fica em `acc` (os `recv` chegam com tamanho qualquer).
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
