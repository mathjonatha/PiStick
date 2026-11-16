//! Microfone do hospedeiro para os apps do Pi (marco 7B). Veja `docs/24-marco-7b-microfone.md`.
//!
//! Peças:
//! - [`LinkTracker`]: analisador do texto de `pw-link --links --monitor`; diz se algum app do Pi está gravando da
//!   fonte `pistick-mic` (existe ligação saindo de `pistick-mic:capture_*`).
//! - [`RequestGate`]: transforma "há gravador" em pedidos `AUDIO_REQUEST` (liga na hora, desliga depois de uma
//!   espera curta para não piscar o indicador do Windows quando um app reabre o dispositivo).
//! - [`MicQueue`]: fila de jitter pequena e adaptativa entre os pacotes que chegam do cliente e o soquete do PipeWire.
//! - [`MicHub`] e [`MicSession`]: o monitor de ligações (um processo filho de vida longa, só enquanto há cliente com
//!   `CAP_MIC`), a escrita de PCM no soquete do segundo servidor `protocol-simple` (só enquanto há pedido ligado) e a
//!   validação do fluxo que o cliente envia.
//!
//! Nada aqui captura nem guarda áudio em arquivo: os blocos vão da rede para o soquete local do PipeWire.

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

/// Nome do nó da fonte virtual (`tools/pi/pipewire/pipewire.conf`).
pub const MIC_NODE: &str = "pistick-mic";
/// Endereço do segundo servidor `protocol-simple` (reprodução) que alimenta a fonte.
pub const PIPEWIRE_MIC_ADDR: &str = "127.0.0.1:4712";
/// Quanto tempo sem gravador antes de pedir ao cliente que pare (menos de 1 s no total, com a fila do cliente).
pub const OFF_DELAY: Duration = Duration::from_millis(500);
/// Alvo inicial (igual ao máximo; vale também durante o aquecimento), mínimo e máximo da fila, em milissegundos. O começo do fluxo
/// é o pior momento do Wi-Fi (o rádio acorda, o estimador de jitter ainda não viu nada): por isso o alvo começa no
/// máximo e só desce depois de [`WARMUP_US`], pela medição.
pub const QUEUE_TARGET_MS: usize = 100;
pub const QUEUE_MIN_MS: usize = 20;
pub const QUEUE_MAX_MS: usize = 100;
/// Folga acima do máximo a partir da qual o excesso é descartado de uma vez.
pub const QUEUE_OVERFLOW_SLACK_MS: usize = 50;
/// Aquecimento: nos primeiros 6 s do fluxo o alvo não desce de [`QUEUE_TARGET_MS`] e o consumo não é acelerado (µs).
pub const WARMUP_US: u64 = 6_000_000;
/// Janela de memória do jitter (µs): um atraso do Wi-Fi mantém o alvo alto por esse tempo. Com 5 s a fila encolhia entre
/// dois atrasos (que no Wi-Fi do desenvolvimento vêm a cada 5 a 10 s) e esvaziava no seguinte.
pub const JITTER_WINDOW_US: u64 = 30_000_000;
/// Quadros por milissegundo.
const FRAMES_PER_MS: usize = SAMPLE_RATE as usize / 1000;

// ---------------------------------------------------------------------------------------------------------------
// Monitor de ligações
// ---------------------------------------------------------------------------------------------------------------

/// Acompanha as ligações que saem da fonte `pistick-mic` pelo texto de `pw-link --links --monitor`.
///
/// Formato (PipeWire 1.4): ao abrir, um despejo com `= PORTA` e, indentadas, `=   |-> DESTINO` (a porta liga a
/// destino) ou `=   |<- ORIGEM`; depois, um evento por linha: `+ ORIGEM -> DESTINO` (ligação criada), `- ORIGEM ->
/// DESTINO` (desfeita) e `+ PORTA` / `- PORTA` (portas, ignoradas). Cada ligação é guardada como contagem por par
/// (origem, destino): dois apps com o mesmo nome produzem o mesmo par, e o `-` de um não pode apagar o outro.
#[derive(Debug, Default)]
pub struct LinkTracker {
    counts: HashMap<(String, String), u32>,
    /// Porta do último `= PORTA` do despejo inicial (só importa se for da fonte).
    header: Option<String>,
}

fn is_mic_out(port: &str) -> bool {
    port.strip_prefix(MIC_NODE).is_some_and(|r| r.starts_with(":capture_"))
}

/// Nome do nó de uma porta `nó:porta` (o que vem antes do último `:`).
fn node_of(port: &str) -> &str {
    port.rsplit_once(':').map_or(port, |(n, _)| n)
}

impl LinkTracker {
    pub fn new() -> LinkTracker {
        LinkTracker::default()
    }

    /// Esquece tudo (o monitor foi reaberto: o despejo inicial reconstrói o estado).
    pub fn reset(&mut self) {
        self.counts.clear();
        self.header = None;
    }

    /// Alguma ligação sai da fonte.
    pub fn recording(&self) -> bool {
        !self.counts.is_empty()
    }

    /// Nomes (sem repetição, em ordem) dos nós que gravam da fonte.
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

    /// Processa uma linha. Devolve `true` se o estado (gravando, quem grava) pode ter mudado.
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

// ---------------------------------------------------------------------------------------------------------------
// Porta dos pedidos
// ---------------------------------------------------------------------------------------------------------------

/// Decide quando mandar `AUDIO_REQUEST`: liga na hora em que há gravador; desliga depois de [`OFF_DELAY`] sem ele.
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

    /// Devolve `Some(novo estado)` quando um pedido deve sair agora.
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

    /// Quando `update` deve ser chamado de novo mesmo sem evento (prazo do desligamento).
    pub fn next_deadline(&self) -> Option<Instant> {
        self.idle_since.map(|t| t + OFF_DELAY)
    }
}

// ---------------------------------------------------------------------------------------------------------------
// Fila de jitter
// ---------------------------------------------------------------------------------------------------------------

/// Resultado de [`MicQueue::pop_block`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pop {
    /// Bloco inteiro de áudio.
    Full,
    /// Faltaram quadros no fim: completado com silêncio e a fila volta a encher até o alvo.
    Underrun,
    /// Fila ainda enchendo (ou sem dados): bloco de silêncio.
    Silence,
}

/// Contadores da fila desde que ela foi criada (ou zerada).
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct QueueStats {
    pub packets: u64,
    pub frames_in: u64,
    pub dropped_frames: u64,
    /// Quadros consumidos a mais para a fila voltar ao alvo (correção gradual).
    pub skipped_frames: u64,
    pub underruns: u64,
    pub silence_blocks: u64,
}

/// Fila de quadros estéreo entre a rede (chegadas irregulares) e o soquete do PipeWire (um bloco a cada 10 ms).
///
/// - Sem dados (ou ainda abaixo do alvo, depois de esvaziar): entrega silêncio.
/// - Acima do alvo mais uma folga: descarta o excesso mais velho até o alvo (atraso acima do alvo é descartado).
/// - O alvo vai de 20 a 100 ms pelo jitter das chegadas nos últimos 30 s: o pior atraso de um pacote em relação
///   ao mais adiantado da janela, mais 20 ms de folga; sobe na hora e desce devagar (a janela vai expirando).
pub struct MicQueue {
    /// Alvo máximo (e inicial) em ms; `psd --mic-max-ms`.
    max_ms: usize,
    buf: VecDeque<i16>,
    primed: bool,
    target_frames: usize,
    stats: QueueStats,
    // Estimativa de jitter: o "trânsito" de um pacote é a chegada menos o instante nominal (quadros já recebidos).
    first_arrival_us: Option<u64>,
    frames_seen: u64,
    /// Por janela de 1 s: (início em µs, menor trânsito, maior trânsito).
    buckets: VecDeque<(u64, i64, i64)>,
    /// Última chegada e o maior intervalo entre duas chegadas desde a última leitura (diagnóstico, µs).
    last_arrival_us: Option<u64>,
    max_gap_us: u64,
    /// O aquecimento acabou (só então o consumo acelerado é permitido).
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

    /// Fila com alvo máximo (e inicial, no aquecimento) de `max_ms` (entre `QUEUE_MIN_MS` e 300).
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

    /// Maior intervalo (ms) entre duas chegadas desde a última chamada; zera o contador.
    pub fn take_max_gap_ms(&mut self) -> f64 {
        std::mem::take(&mut self.max_gap_us) as f64 / 1000.0
    }

    /// Quadros na fila.
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

    /// Esvazia e recomeça (novo fluxo): o alvo volta ao inicial.
    pub fn reset(&mut self) {
        *self = MicQueue::with_max_ms(self.max_ms);
    }

    /// Estimativa do jitter (ms) na janela atual.
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

    /// Entrega quadros estéreo intercalados recebidos do cliente em `now_us` (relógio qualquer, crescente).
    pub fn push(&mut self, samples: &[i16], now_us: u64) {
        let frames = samples.len() / CHANNELS;
        if frames == 0 {
            return;
        }
        self.stats.packets += 1;
        self.stats.frames_in += frames as u64;
        self.note_arrival(frames, now_us);
        self.buf.extend(&samples[..frames * CHANNELS]);
        // Descarte de uma vez só para o que passa muito do máximo (rajada grande): fica o mais novo, até o máximo. O que
        // fica entre o alvo e esse limite volta ao alvo aos poucos, em `pop_block`, sem estalo.
        let limit = (self.max_ms + QUEUE_OVERFLOW_SLACK_MS) * FRAMES_PER_MS;
        if self.level_frames() > limit {
            let excess = self.level_frames() - self.max_ms * FRAMES_PER_MS;
            self.buf.drain(..excess * CHANNELS);
            self.stats.dropped_frames += excess as u64;
        }
    }

    /// Preenche `out` (`BLOCK_FRAMES` quadros estéreo) com o próximo bloco.
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
            // Nível acima do alvo (depois de uma rajada do Wi-Fi): a fila só encolheria descartando de uma vez, e o atraso
            // ficaria alto para sempre. Em vez disso, passado o aquecimento, consome um pouco mais depressa que o relógio:
            // 10 quadros a mais por bloco (cerca de 2%) se o excesso passa de 50 ms, 5 (1%) se passa de 30 ms, 1 (0,2%) se
            // passa de 10 ms; os quadros pulados são espalhados pelo bloco.
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
        // Esvaziou: entrega o que há, completa com zeros e volta a encher até o alvo.
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

// ---------------------------------------------------------------------------------------------------------------
// Origem do texto do monitor
// ---------------------------------------------------------------------------------------------------------------

/// O texto do monitor de ligações e o jeito de encerrá-lo de outra thread.
pub struct LinkFeed {
    pub reader: Box<dyn Read + Send>,
    /// Encerra a origem (mata o processo; fecha o soquete). Pode ser chamada várias vezes, de qualquer thread.
    pub kill: Arc<dyn Fn() + Send + Sync>,
}

/// Abre uma origem do monitor (de novo, se a anterior terminou).
pub type OpenFeed = Arc<dyn Fn() -> std::io::Result<LinkFeed> + Send + Sync>;

/// Origem de produção: o processo filho `pw-link --links --monitor`, no PipeWire do `XDG_RUNTIME_DIR` do `psd`.
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
        // Encerrado com o `psd` quando ele sai por SIGTERM ou SIGINT.
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

// ---------------------------------------------------------------------------------------------------------------
// Hub e sessão
// ---------------------------------------------------------------------------------------------------------------

#[derive(Default)]
struct HubState {
    /// Geração do monitor em curso (cada troca de dono a incrementa). Threads de uma geração antiga terminam sozinhas.
    gen: u64,
    /// Número da próxima sessão.
    next_id: u64,
    /// Sessão que tem o microfone (a primeira que conectou) e as que esperam, em ordem de chegada (decisão 7: um
    /// microfone por vez; só o dono recebe pedidos e só ele tem o monitor de ligações).
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
    /// Blocos de 10 ms que o soquete do PipeWire não aceitou a tempo e foram jogados fora (a escrita guarda no máximo 50 ms).
    pw_drops: std::sync::atomic::AtomicU64,
    /// Vezes em que a escrita chegou atrasada mais de 100 ms e recomeçou a grade de tempo.
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

/// Ponto de encontro do microfone: um por `psd`. A sessão de um cliente com `CAP_MIC` chama [`MicHub::attach`].
pub struct MicHub {
    inner: Arc<Inner>,
}

impl MicHub {
    /// `open`: origem do monitor de ligações (use [`pw_link_feed`]); `pcm_addr`: soquete de reprodução do PipeWire.
    pub fn new(open: OpenFeed, pcm_addr: &str, log: LogFn) -> Arc<MicHub> {
        MicHub::with_max_ms(open, pcm_addr, log, QUEUE_MAX_MS)
    }

    /// Como [`MicHub::new`], com o alvo máximo da fila (e inicial) em `max_ms` (`psd --mic-max-ms`).
    pub fn with_max_ms(open: OpenFeed, pcm_addr: &str, log: LogFn, max_ms: usize) -> Arc<MicHub> {
        Arc::new(MicHub { inner: Arc::new(Inner { open, pcm_addr: pcm_addr.to_string(), log, state: Mutex::new(HubState::default()), queue: Mutex::new(MicQueue::with_max_ms(max_ms)), pw_drops: Default::default(), pw_late: Default::default() }) })
    }

    /// Liga a sessão do cliente. A primeira sessão vira dona do microfone: sobe o monitor de ligações (o filho só
    /// existe enquanto houver dono). As seguintes esperam e nunca recebem pedido enquanto a dona estiver conectada;
    /// quando ela sai, a primeira da fila assume. `waker` é chamado quando "há gravador" muda (e ao assumir).
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

/// O fluxo do cliente em curso.
struct StreamState {
    codec: u8,
    channels: u8,
    next_seq: Option<u32>,
}

struct Writer {
    stop: Arc<AtomicBool>,
}

/// Contadores do segundo corrente para a linha de estatísticas.
#[derive(Debug, Default, Clone, Copy, PartialEq)]
pub struct MicStats {
    pub packets: u64,
    pub bytes: u64,
    pub level_ms: f64,
    pub target_ms: f64,
    /// Maior intervalo entre dois pacotes no último segundo (ms).
    pub gap_ms: f64,
    /// Blocos jogados fora porque o PipeWire não os aceitou, e atrasos da escrita acima de 100 ms (desde o último pedido de ligar).
    pub pw_drops: u64,
    pub pw_late: u64,
    pub queue: QueueStats,
}

/// A parte da sessão: porta dos pedidos, fluxo do cliente e escrita no PipeWire.
pub struct MicSession {
    inner: Arc<Inner>,
    /// Número da sessão no hub.
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
                // A primeira da fila assume o microfone (com um monitor novo, que reconstrói o estado).
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
    /// Há gravador agora, e quem são (para o log).
    pub fn recording(&self) -> (bool, Vec<String>) {
        let g = self.inner.lock();
        if g.owner == Some(self.id) {
            (g.recording, g.consumers.clone())
        } else {
            (false, Vec::new())
        }
    }

    /// O pedido está ligado.
    pub fn requested(&self) -> bool {
        self.gate.is_on()
    }

    /// Prazo da próxima chamada de [`MicSession::poll`] sem evento.
    pub fn next_deadline(&self) -> Option<Instant> {
        self.gate.next_deadline()
    }

    /// Confere se um pedido deve sair. `Some(on)`: a sessão envia `AUDIO_REQUEST` e já ficou preparada (a escrita
    /// no PipeWire começa ao ligar e termina ao desligar).
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

    /// `AUDIO_START` do fluxo 1. `Err(motivo)`: fluxo recusado (o pacote é descartado e o motivo, registrado).
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

    /// `AUDIO_STOP` do fluxo 1. Devolve `true` se havia fluxo aberto.
    pub fn on_stop(&mut self) -> bool {
        self.stream.take().is_some()
    }

    /// `AUDIO_DATA` do fluxo 1. Sem pedido em vigor (ou sem `AUDIO_START`) o pacote é descartado; `Err` só na
    /// primeira vez de cada episódio (para o log não encher). Pacote que não decodifica também é descartado.
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

    /// Pacotes descartados por falta de pedido ou erro desde o início da sessão.
    pub fn discarded(&self) -> u64 {
        self.discarded
    }

    /// Saltos de `seq` vistos desde o início da sessão.
    pub fn seq_gaps(&self) -> u64 {
        self.seq_gaps
    }

    /// Estatísticas do que chegou (pacotes e bytes desde a última chamada de [`MicSession::take_stats`]) e da fila.
    pub fn take_stats(&mut self) -> MicStats {
        let mut q = self.inner.queue();
        let s = MicStats { packets: self.packets, bytes: self.bytes, level_ms: q.level_ms(), target_ms: q.target_ms(), gap_ms: q.take_max_gap_ms(), pw_drops: self.inner.pw_drops.load(Ordering::Relaxed), pw_late: self.inner.pw_late.load(Ordering::Relaxed), queue: q.stats() };
        drop(q);
        self.packets = 0;
        self.bytes = 0;
        s
    }
}

/// Thread de escrita: enquanto não for parada, mantém conexão com o soquete do PipeWire e escreve um bloco de
/// 10 ms a cada 10 ms, no relógio do sistema (o mesmo que move o PipeWire no Pi). O bloqueio do soquete não serve de
/// ritmo: o laço local do TCP guarda segundos de dados antes de bloquear.
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
        // Blocos que o soquete ainda não aceitou (o primeiro pode estar escrito pela metade, em `off` bytes).
        let mut pending: VecDeque<Vec<u8>> = VecDeque::new();
        let mut off = 0usize;
        let mut lost = false;
        while !stop.load(Ordering::SeqCst) && !lost {
            let now = Instant::now();
            if now < next {
                std::thread::sleep((next - now).min(Duration::from_millis(5)));
                continue;
            }
            // Atrasado demais (o sistema travou): não corre atrás, recomeça a grade daqui.
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
            // O PipeWire não consome (por exemplo, o gravador saiu e o grafo parou): guarda no máximo 50 ms; o
            // excesso mais velho é jogado fora, sempre em blocos inteiros para não desalinhar os canais.
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
