//! Áudio do cliente (marco 7): decodifica `AUDIO_DATA`, mantém a fila de reprodução e a entrega ao
//! dispositivo (WASAPI no Windows), a um consumidor que descarta (`--audio-null`) ou a um arquivo WAV
//! (`--audio-dump`). Tudo, exceto `wasapi.rs`, é independente de sistema e testável.
//!
//! - [`JitterQueue`]: a fila de reprodução (alvo 40 ms, mínimo 20, máximo 120; correção de desvio de relógio).
//! - [`AudioEngine`]: o que a thread de rede e o dispositivo compartilham: máquina de estados do fluxo
//!   (`AUDIO_START`, `AUDIO_DATA`, `AUDIO_STOP`), decodificação, fila, estatísticas, WAV e amostra recente.

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

/// Alvo mínimo (40 ms), máximo (300 ms) e folga até o descarte (100 ms), em quadros por canal.
pub const MIN_TARGET_FRAMES: usize = 40 * 48;
pub const MAX_TARGET_FRAMES: usize = 300 * 48;
pub const OVERFLOW_MARGIN_FRAMES: usize = 100 * 48;
/// Constante de tempo da média do nível (2 s).
const AVG_WINDOW_FRAMES: f64 = 2.0 * SAMPLE_RATE as f64;
/// Quadros de áudio entre duas correções: lenta (10 ms) e rápida (2 ms, cerca de 2%).
const SLOW_PERIOD_FRAMES: usize = 480;
const FAST_PERIOD_FRAMES: usize = 96;
/// Erro do nível médio (em quadros) que liga a correção, que a torna rápida e que a desliga.
const ADJUST_ON_FRAMES: f64 = 10.0 * 48.0;
const ADJUST_FAST_FRAMES: f64 = 30.0 * 48.0;
const ADJUST_OFF_FRAMES: f64 = 5.0 * 48.0;
/// Janelas do cálculo do atraso de chegada (segundos): linha de base e jitter.
const BASELINE_SECS: u64 = 60;
const JITTER_SECS: u64 = 30;
/// O alvo desce no máximo 5 ms por segundo; depois de um esvaziamento fica no piso por 30 s.
const TARGET_DECAY_FRAMES_PER_S: f64 = 5.0 * 48.0;
const UNDERRUN_FLOOR_US: u64 = 30_000_000;

/// Parâmetros da fila.
#[derive(Debug, Clone, Copy)]
pub struct QueueConfig {
    /// `true`: o alvo acompanha o jitter medido; `false`: fica em `initial` (`--audio-target`).
    pub adaptive: bool,
    /// Alvo inicial (e fixo, sem adaptação), em quadros.
    pub initial: usize,
    /// Teto do alvo adaptativo, em quadros.
    pub max_target: usize,
    /// Folga acima do alvo a partir da qual o excesso é descartado, em quadros.
    pub margin: usize,
}

impl Default for QueueConfig {
    fn default() -> Self {
        QueueConfig { adaptive: true, initial: MIN_TARGET_FRAMES, max_target: MAX_TARGET_FRAMES, margin: OVERFLOW_MARGIN_FRAMES }
    }
}

impl QueueConfig {
    /// Alvo fixo de `ms` milissegundos (sem adaptação).
    pub fn fixed_ms(ms: usize) -> Self {
        QueueConfig { adaptive: false, initial: ms * 48, max_target: ms * 48, margin: OVERFLOW_MARGIN_FRAMES }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum QState {
    /// Esperando a fila chegar ao alvo (saída em silêncio).
    Buffering,
    Playing,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Adjust {
    None,
    /// Nível alto: descarta um quadro de tempos em tempos (emenda por média com o vizinho).
    Remove,
    /// Nível baixo: repete um quadro de tempos em tempos (quadro interpolado).
    Repeat,
}

/// Números da fila desde a criação ou desde o último `reset_stats`.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct QueueStats {
    /// A fila esvaziou tocando, com som ainda por vir (não conta o fim de um trecho de silêncio).
    pub underruns: u64,
    /// A fila passou do máximo e o excesso foi descartado.
    pub overflows: u64,
    pub overflow_frames: u64,
    /// Quadros removidos e repetidos pela correção de desvio.
    pub removed: u64,
    pub repeated: u64,
    /// Recomeços por descontinuidade.
    pub restarts: u64,
    /// Soma e máximo do nível (quadros) amostrado a cada leitura com a fila tocando, e quantas amostras.
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

/// Fila de reprodução: amostras estéreo intercaladas entram em `push_at` e saem em `pull_at`. O relógio vem de
/// fora (microssegundos), o que deixa a fila testável com tempo simulado.
///
/// Alvo adaptativo: o atraso de chegada de cada pacote é (chegada local - `pts_us`) menos o menor valor disso
/// nos últimos 60 s; o jitter é o maior atraso de chegada dos últimos 30 s; o alvo é `jitter * 1,15 + 15 ms`
/// entre 40 e 300 ms. Sobe na hora, desce no máximo 5 ms por segundo, e depois de cada esvaziamento real vale
/// pelo menos 1,5 vez o alvo de então por 30 s.
pub struct JitterQueue {
    cfg: QueueConfig,
    buf: VecDeque<i16>,
    state: QState,
    /// Último `push` terminou em silêncio (todas as amostras zero): a fila vazia depois disso é pausa, não falta.
    tail_silent: bool,
    avg: f64,
    adjust: Adjust,
    fast: bool,
    since_adjust: usize,
    last_frame: [i16; 2],
    stats: QueueStats,
    // --- alvo adaptativo ---
    /// Alvo antes do piso do esvaziamento (quadros).
    target: f64,
    floor: f64,
    floor_until_us: u64,
    last_update_us: u64,
    /// Menor (chegada - pts) por segundo, e maior atraso de chegada por segundo.
    base_buckets: VecDeque<(u64, i64)>,
    jitter_buckets: VecDeque<(u64, u64)>,
    jitter_us: u64,
    /// Último instante recebido (para as chamadas sem relógio).
    last_now_us: u64,
}

impl JitterQueue {
    pub fn new(cfg: QueueConfig) -> Self {
        JitterQueue {
            cfg,
            // Capacidade reservada para o maior nível possível (alvo máximo + folga + um pacote): `push_at` roda com
            // a trava da fila presa e a saída em tempo real a disputa, então não pode realocar ali.
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

    /// Alvo atual, em quadros (já com o piso do último esvaziamento).
    pub fn target_frames(&self) -> usize {
        self.effective_target(self.last_now_us) as usize
    }

    pub fn target_ms(&self) -> f64 {
        self.target_frames() as f64 / 48.0
    }

    /// Jitter dos últimos 30 s (maior atraso de chegada), em ms.
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

    /// Sobe o alvo na hora e o desce no máximo 5 ms por segundo.
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

    /// Registra a chegada de um pacote: atualiza a linha de base, o atraso de chegada e o jitter.
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

    /// Como [`push_at`](Self::push_at) sem relógio nem `pts` (alvo fixo; usado pelos testes simples).
    pub fn push(&mut self, samples: &[i16], restart: bool) {
        let now = self.last_now_us;
        self.push_at(samples, restart, None, now);
    }

    /// Como [`pull_at`](Self::pull_at) sem relógio.
    pub fn pull(&mut self, out: &mut [i16]) {
        let now = self.last_now_us;
        self.pull_at(out, now);
    }

    /// Acrescenta quadros que chegaram em `now_us` (relógio local) com o instante `pts_us` do emissor. Com
    /// `restart`, a fila recomeça antes (pacote perdido, descarte ou retomada) e a linha de base também.
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

    /// Preenche `out` (estéreo intercalado) no instante `now_us`. Fila em formação ou vazia: silêncio.
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
        // Média do nível (2 s) e decisão de correção, com histerese.
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
                    // A média tem atraso (2 s): só corrige para o lado certo se o nível de agora também está desse lado,
                    // senão a correção passa do alvo e a fila esvazia.
                    let now_level = self.buf.len() / 2;
                    match self.adjust {
                        Adjust::Remove if (now_level as f64) < target => {}
                        Adjust::Repeat if now_level as f64 > target => {}
                        // Dois quadros de entrada viram um de saída (a média deles): sem degrau.
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
                        // Um quadro a mais, interpolado entre o último e o próximo, sem consumir entrada.
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
                    // Fila vazia: o resto do bloco é silêncio e a fila volta a se formar.
                    out[i * 2..].fill(0);
                    if !self.tail_silent {
                        self.stats.underruns += 1;
                        if self.cfg.adaptive {
                            // Esvaziamento real: o alvo sobe 50% (até o teto) e fica nisso por 30 s.
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

// ---------------------------------------------------------------------------------------------
// Opções, estatísticas e motor
// ---------------------------------------------------------------------------------------------

/// Opções de áudio do `psc`.
#[derive(Debug, Clone)]
pub struct AudioOptions {
    /// `false` (`--no-audio`): não anuncia `CAP_AUDIO`.
    pub enabled: bool,
    /// `--audio-dump ARQ.wav`: grava o que foi decodificado; a saída vai a um consumidor que descarta.
    pub dump: Option<PathBuf>,
    /// `--audio-null`: toca em lugar nenhum (testes), mas a fila anda em tempo real.
    pub null_output: bool,
    /// `--audio-volume 0..1`: volume do fluxo do `psc` no WASAPI.
    pub volume: f32,
    /// `--audio-target MS`: fixa o alvo da fila em ms e desliga a adaptação (sem a opção, o alvo acompanha o
    /// jitter, entre 40 e 300 ms). O descarte começa 100 ms acima do alvo.
    pub target_ms: Option<u32>,
}

impl Default for AudioOptions {
    fn default() -> Self {
        AudioOptions { enabled: true, dump: None, null_output: false, volume: 1.0, target_ms: None }
    }
}

/// Retrato das estatísticas (`audiostats`).
#[derive(Debug, Clone, Default)]
pub struct AudioStats {
    pub packets: u64,
    pub bytes: u64,
    pub frames: u64,
    /// Pacotes com a marca de descontinuidade por silêncio (bit 0 sozinho).
    pub resumes: u64,
    /// Pacotes com a marca de descarte do servidor (bit 1).
    pub drops: u64,
    pub queue: QueueStats,
    pub level_now_ms: f64,
    /// O fluxo de saída está aberto.
    pub open: bool,
    pub codec: u8,
    /// Segundos desde a criação ou o último `reset_stats`.
    pub secs: f64,
    /// Alvo atual da fila e jitter dos últimos 30 s, em ms.
    pub target_ms: f64,
    pub jitter_ms: f64,
    /// Milissegundos da abertura do áudio (antes do HELLO) até o primeiro `AUDIO_DATA` (nunca é zerado).
    pub first_packet_ms: Option<f64>,
}

impl AudioStats {
    /// A linha `AUDIOSTATS ...` do roteiro do `psc` (e do `logcat` do cliente Android).
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

    /// Taxa de bits recebida (carga dos pacotes), em kbit/s.
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

/// Estado do lado da rede (trava `inner`). A fila de reprodução fica fora, em trava própria.
struct Inner {
    stream: Option<Stream>,
    packets: u64,
    bytes: u64,
    frames: u64,
    resumes: u64,
    drops: u64,
    codec: u8,
    dump: Option<dump::WavDump>,
    /// Últimos pacotes com som (pts, amostras estéreo), para `verify-audio`.
    sound_tail: VecDeque<(u64, Vec<i16>)>,
    since: Instant,
    first_packet_ms: Option<f64>,
}

/// O fim de saída do áudio: quem puxa da fila ([`AudioEngine::pull`]) e entrega a um dispositivo. O WASAPI do `psc` é
/// uma thread própria que não precisa disto; o cliente Android registra o fluxo do AAudio aqui, para o motor o parar
/// no [`AudioEngine::shutdown`].
pub trait AudioSink: Send {
    /// Nome para o registro (`"AAudio"`).
    fn name(&self) -> &'static str;
    /// Para a saída e solta o dispositivo. Chamado uma vez.
    fn stop(&mut self);
}

/// O que a thread de rede e o dispositivo de saída compartilham.
///
/// Duas travas, para o retorno de chamada de tempo real quase nunca esbarrar na thread de rede:
/// - `queue`: só a [`JitterQueue`]. É a única que o dispositivo de saída disputa; fica presa só no `push_at`,
///   no `pull_at` e em leituras curtas de estatística.
/// - `inner`: o resto (fluxo, contadores, `dump`, `sound_tail`). Decodificação, `is_silent`, cópias e escrita do
///   WAV rodam só com esta.
///
/// Ordem fixa se alguma vez for preciso as duas: `inner` primeiro, `queue` depois. O código atual nunca segura as
/// duas ao mesmo tempo.
pub struct AudioEngine {
    inner: Mutex<Inner>,
    queue: Mutex<JitterQueue>,
    /// Origem do relógio da fila (imutável, lida sem trava).
    created: Instant,
    stop: AtomicBool,
    sink: Mutex<Option<Box<dyn AudioSink>>>,
}

/// Quantos pacotes com som `verify-audio` guarda.
const SOUND_TAIL_PACKETS: usize = 100;

/// Voltas de `try_lock` + `spin_loop` do `pull_nowait` antes de desistir: cada volta custa algumas dezenas de
/// nanossegundos, então 4000 voltas são da ordem de algumas dezenas de microssegundos (bem mais que um `push_at`, bem
/// menos que um bloco de 10 ms).
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

    /// Entrega ao motor o fim de saída (ele o para no `shutdown`). Um anterior é parado e trocado.
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

    /// Pede às threads de saída que terminem.
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

    /// `AUDIO_START`. `Err` é erro de protocolo (a conexão deve cair).
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
        // Fluxo novo: a fila recomeça.
        let now = self.now_us();
        self.lock_queue().push_at(&[], true, None, now);
        Ok(())
    }

    /// `AUDIO_DATA`. `Err` é erro de protocolo.
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
                // Um corte de pts: o que veio antes não vale para a verificação de frequência.
                g.sound_tail.clear();
            }
            g.sound_tail.push_back((pts_us, samples.clone()));
            while g.sound_tail.len() > SOUND_TAIL_PACKETS {
                g.sound_tail.pop_front();
            }
        }
        // Termina o trabalho do lado da rede antes de tocar na fila: a trava da fila só vale para o `push_at`.
        drop(g);
        let now = self.now_us();
        self.lock_queue().push_at(&samples, discontinuity, Some(pts_us), now);
        Ok(())
    }

    /// `AUDIO_STOP`: o fluxo fecha; o que já está na fila toca até o fim.
    pub fn on_stop(&self, stream: u8) {
        let mut g = self.lock();
        if stream == AUDIO_STREAM_OUT {
            g.stream = None;
        }
    }

    /// Chamado pelo dispositivo: preenche `out` com o próximo áudio.
    pub fn pull(&self, out: &mut [i16]) {
        let now = self.now_us();
        self.lock_queue().pull_at(out, now);
    }

    /// Como [`pull`](Self::pull) para uma thread de tempo real (o retorno de chamada do AAudio): **nunca dorme nem
    /// bloqueia**. Se a trava da fila está presa (a thread de rede no `push_at`, que dura poucos microssegundos),
    /// gira um pouco com `spin_loop` e tenta de novo; esgotadas as voltas, entrega silêncio e devolve `false` (o
    /// chamador conta a falta; a fila não avança nesse bloco). Sem alocação.
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

    /// Só para testes de outros crates: segura a trava da FILA enquanto `f` roda (simula o `push_at` demorando).
    #[doc(hidden)]
    pub fn hold_for_test<R>(&self, f: impl FnOnce() -> R) -> R {
        let _g = self.lock_queue();
        f()
    }

    /// Só para testes: segura a trava do lado da rede (a decodificação de `on_data` em andamento).
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

    /// O trecho contíguo mais recente de pacotes com som, intercalado (para `verify-audio`).
    pub fn recent_sound(&self) -> (usize, Vec<i16>) {
        let g = self.lock();
        let mut start = g.sound_tail.len();
        let mut prev: Option<u64> = None;
        for (i, (pts, s)) in g.sound_tail.iter().enumerate().rev() {
            if let Some(p) = prev {
                // Pacotes seguidos distam 10 ms (com folga de 3 ms para o arredondamento do relógio).
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

/// Consumidor sem dispositivo: puxa 48 000 quadros por segundo do relógio do sistema, em blocos de 10 ms,
/// e joga fora. Usado com `--audio-dump` e `--audio-null` (nenhum som sai no computador).
pub fn spawn_null_consumer(engine: Arc<AudioEngine>) {
    std::thread::Builder::new()
        .name("psc-audio-nulo".into())
        .spawn(move || {
            let t0 = Instant::now();
            let mut produced: u64 = 0;
            let mut out = vec![0i16; 480 * 2];
            let mut last_flush = Instant::now();
            while !engine.is_shutdown() {
                // Quantos quadros já deviam ter saído.
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

/// Abre a saída escolhida pelas opções e devolve o motor. `None` com `--no-audio`.
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
