//! Microfone do hospedeiro para os apps do Pi (marco 7B). Projeto em `docs/24-marco-7b-microfone.md`; mensagens em
//! `docs/05-protocolo.md`, seção "Extensão do marco 7B: microfone".
//!
//! Regras de privacidade (cada uma tem teste):
//!
//! - Sem `--mic` (ou `--mic-file`) o cliente não anuncia `CAP_MIC` e **nunca** abre um dispositivo de captura:
//!   nem a thread de captura existe ([`Mic::new`] sem fonte).
//! - Com `--mic`, só anuncia `CAP_MIC` se a sessão é cifrada e pareada ou é o laço local ([`decide`]). Com `--insecure`
//!   fora do laço (texto claro) não anuncia, e registra por quê.
//! - O dispositivo só é aberto depois de um `AUDIO_REQUEST on=1` do servidor e é **liberado** (todos os objetos do
//!   WASAPI soltos, para o indicador de microfone do Windows apagar) em `on=0`, no fim da sessão ([`Mic::shutdown`]) e
//!   em qualquer erro. Cada ligar e desligar vira uma linha de registro com a hora (UTC).
//! - Pedido recebido sem `CAP_MIC` anunciada é ignorado e registrado.
//!
//! A captura em si é uma função ([`RunFn`]): o WASAPI (`audio/wasapi_capture.rs`, único `unsafe` do microfone) ou o
//! WAV em laço de `--mic-file`, que não toca em dispositivo algum. Tudo o mais, inclusive a decisão e a máquina de
//! pedidos, é independente de sistema e tem teste de unidade.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use ps_audio::wavcheck::parse_wav;
use ps_audio::{encode, CODEC_PSA1, SAMPLE_RATE};
use ps_proto::{Message, AUDIO_FLAG_DISCONTINUITY, AUDIO_STREAM_MIC};

/// Quadros por pacote (10 ms a 48 kHz).
pub const BLOCK_FRAMES: usize = 480;
const BLOCK_SAMPLES: usize = BLOCK_FRAMES * 2;
/// Duração de um pacote em microssegundos.
const BLOCK_US: u64 = 10_000;

/// Onde as linhas de registro vão (a produção imprime; os testes guardam).
pub type LogFn = Arc<dyn Fn(&str) + Send + Sync>;

/// Registro de produção: `[psc] AAAA:MM:SS.mmmZ microfone: ...` com a hora UTC, para casar com o relógio de quem
/// roda o roteiro de teste.
pub fn stdout_log() -> LogFn {
    Arc::new(|m: &str| println!("[psc] {} {m}", utc_clock()))
}

/// Hora UTC do dia, `HH:MM:SS.mmmZ`.
fn utc_clock() -> String {
    let d = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default();
    let s = d.as_secs() % 86_400;
    format!("{:02}:{:02}:{:02}.{:03}Z", s / 3600, (s / 60) % 60, s % 60, d.subsec_millis())
}

// ---------------------------------------------------------------------------------------------------------------
// Opções e decisão de privacidade
// ---------------------------------------------------------------------------------------------------------------

/// Opções do microfone no `psc`.
#[derive(Debug, Clone, Default)]
pub struct MicOptions {
    /// `--mic`: o usuário ligou o recurso.
    pub enabled: bool,
    /// `--mic-file ARQ.wav` (implica `--mic`): manda o WAV em laço no lugar do dispositivo. Nenhum dispositivo é aberto.
    pub file: Option<PathBuf>,
}

impl MicOptions {
    /// O usuário pediu o microfone (por `--mic` ou `--mic-file`).
    pub fn wanted(&self) -> bool {
        self.enabled || self.file.is_some()
    }
}

/// Que confiança a conexão merece para oferecer o microfone.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum LinkTrust {
    /// Texto claro fora do laço local (`--insecure`), ou desconhecido. É o padrão: na dúvida, não oferece.
    #[default]
    Untrusted,
    /// Laço local (`127.0.0.1`): nada sai do computador.
    Loopback,
    /// Canal cifrado (Noise) e aparelho pareado.
    Paired,
}

/// Resultado de [`decide`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    /// O usuário não pediu o microfone: nada é anunciado e nada é aberto.
    Off,
    /// Anuncia `CAP_MIC`.
    Announce,
    /// O usuário pediu, mas a conexão não serve: não anuncia (o texto explica o motivo).
    Blocked(&'static str),
}

/// Decide se o `psc` anuncia `CAP_MIC`: pediu o recurso **e** a sessão é cifrada e pareada ou laço local.
pub fn decide(opts: &MicOptions, link: LinkTrust) -> Decision {
    if !opts.wanted() {
        return Decision::Off;
    }
    match link {
        LinkTrust::Paired | LinkTrust::Loopback => Decision::Announce,
        LinkTrust::Untrusted => Decision::Blocked("a conexão não é cifrada e pareada nem é o laço local (--insecure fora do laço): o microfone não será oferecido ao aparelho"),
    }
}

// ---------------------------------------------------------------------------------------------------------------
// WAV em laço (--mic-file)
// ---------------------------------------------------------------------------------------------------------------

/// Um WAV estéreo de 48 kHz tocado em laço.
#[derive(Debug, Clone)]
pub struct WavLoop {
    /// Amostras estéreo intercaladas; nunca vazio.
    samples: Vec<i16>,
}

impl WavLoop {
    /// `samples`: estéreo intercalado, pelo menos um pacote.
    pub fn from_stereo(samples: Vec<i16>) -> Result<WavLoop, String> {
        if samples.len() < BLOCK_SAMPLES {
            return Err(format!("o arquivo é curto demais ({} quadros; precisa de pelo menos {BLOCK_FRAMES})", samples.len() / 2));
        }
        Ok(WavLoop { samples })
    }

    /// Quadros no arquivo.
    pub fn frames(&self) -> usize {
        self.samples.len() / 2
    }

    /// Preenche `out` (um pacote) a partir de `cursor` (em amostras), voltando ao começo ao chegar ao fim.
    pub fn fill(&self, cursor: &mut usize, out: &mut [i16]) {
        for o in out.iter_mut() {
            *o = self.samples[*cursor];
            *cursor += 1;
            if *cursor >= self.samples.len() {
                *cursor = 0;
            }
        }
    }
}

/// Lê o WAV de `--mic-file`: PCM de 16 bits, 48 kHz, mono ou estéreo (mono vira estéreo duplicado).
pub fn load_wav(path: &Path) -> Result<WavLoop, String> {
    let bytes = std::fs::read(path).map_err(|e| format!("não foi possível ler {}: {e}", path.display()))?;
    let wav = parse_wav(&bytes).map_err(|e| format!("{}: {e}", path.display()))?;
    if wav.rate != SAMPLE_RATE {
        return Err(format!("{}: a taxa é {} Hz (só {SAMPLE_RATE} Hz é aceito)", path.display(), wav.rate));
    }
    let stereo: Vec<i16> = match wav.channels {
        1 => wav.samples.iter().flat_map(|&s| [s, s]).collect(),
        2 => wav.samples,
        n => return Err(format!("{}: {n} canais (só 1 ou 2)", path.display())),
    };
    WavLoop::from_stereo(stereo).map_err(|e| format!("{}: {e}", path.display()))
}

// ---------------------------------------------------------------------------------------------------------------
// Fonte de captura
// ---------------------------------------------------------------------------------------------------------------

/// Uma captura: roda até `stop` ficar verdadeiro. Chama `on_open` uma vez, quando o dispositivo abriu, e `sink` com
/// cada pacote de 480 quadros estéreo (960 amostras intercaladas, 48 kHz). Ao **retornar**, tudo o que a captura abriu já
/// foi liberado. `Err` se o dispositivo não abriu (ou se perdeu e não voltou).
pub type RunFn = Arc<dyn Fn(&AtomicBool, &mut dyn FnMut(), &mut dyn FnMut(&[i16])) -> Result<(), String> + Send + Sync>;

/// Captura por `--mic-file`: o WAV em laço, em tempo real (um pacote a cada 10 ms no relógio do sistema).
pub fn file_run(wav: WavLoop) -> RunFn {
    Arc::new(move |stop, on_open, sink| {
        on_open();
        let mut next = Instant::now();
        let mut cursor = 0usize;
        let mut block = [0i16; BLOCK_SAMPLES];
        while !stop.load(Ordering::SeqCst) {
            let now = Instant::now();
            if now < next {
                std::thread::sleep((next - now).min(Duration::from_millis(5)));
                continue;
            }
            if now.duration_since(next) > Duration::from_millis(100) {
                next = now;
            }
            next += Duration::from_micros(BLOCK_US);
            wav.fill(&mut cursor, &mut block);
            sink(&block);
        }
        Ok(())
    })
}

/// Captura do microfone padrão do Windows (WASAPI).
#[cfg(windows)]
pub fn device_run() -> RunFn {
    Arc::new(crate::audio::wasapi_capture::run)
}

/// Fora do Windows não há captura.
#[cfg(not(windows))]
pub fn device_run() -> RunFn {
    Arc::new(|_, _, _| Err("este sistema não tem captura de microfone".to_string()))
}

/// O que [`plan`] decidiu.
pub struct Plan {
    /// Anuncia `CAP_MIC` no `HELLO`.
    pub announce: bool,
    /// A captura (só existe se anuncia).
    pub run: Option<RunFn>,
}

/// Aplica [`decide`] e prepara a fonte. Registra uma linha quando o usuário pediu e a conexão não deixa. Erro só se o
/// arquivo de `--mic-file` não serve. **Não abre nenhum dispositivo.**
pub fn plan(opts: &MicOptions, link: LinkTrust, log: &LogFn) -> Result<Plan, String> {
    match decide(opts, link) {
        Decision::Off => Ok(Plan { announce: false, run: None }),
        Decision::Blocked(motivo) => {
            log(&format!("microfone: NÃO anunciado: {motivo}"));
            Ok(Plan { announce: false, run: None })
        }
        Decision::Announce => {
            let run = match &opts.file {
                Some(f) => {
                    let wav = load_wav(f)?;
                    log(&format!("microfone: anunciado (CAP_MIC), fonte: arquivo {} em laço ({} quadros); nenhum dispositivo de captura será aberto", f.display(), wav.frames()));
                    file_run(wav)
                }
                None => {
                    log("microfone: anunciado (CAP_MIC); o dispositivo de gravação só abre quando um app do Pi gravar da fonte pistick-mic");
                    device_run()
                }
            };
            Ok(Plan { announce: true, run: Some(run) })
        }
    }
}

// ---------------------------------------------------------------------------------------------------------------
// Estatísticas
// ---------------------------------------------------------------------------------------------------------------

#[derive(Debug)]
struct Counters {
    since: Instant,
    packets: u64,
    bytes: u64,
    requests: u64,
    offs: u64,
    peak: i32,
}

impl Counters {
    fn new() -> Counters {
        Counters { since: Instant::now(), packets: 0, bytes: 0, requests: 0, offs: 0, peak: 0 }
    }
}

/// Retrato do que `micstats report` imprime.
#[derive(Debug, Clone, PartialEq)]
pub struct MicReport {
    pub packets: u64,
    pub kbit_s: f64,
    /// `AUDIO_REQUEST on=1` recebidos.
    pub requests: u64,
    /// `AUDIO_REQUEST on=0` recebidos.
    pub offs: u64,
    /// Maior amplitude absoluta capturada (0 a 32768; 0 = silêncio total).
    pub peak: i32,
    pub announced: bool,
    pub open: bool,
    pub secs: f64,
}

impl MicReport {
    /// A linha do roteiro.
    pub fn line(&self) -> String {
        format!(
            "MICSTATS pacotes={} kbit_s={:.1} pedidos={} desligados={} pico={} anunciou={} captura={} janela={:.1}s",
            self.packets,
            self.kbit_s,
            self.requests,
            self.offs,
            self.peak,
            u8::from(self.announced),
            if self.open { "aberta" } else { "fechada" },
            self.secs
        )
    }
}

// ---------------------------------------------------------------------------------------------------------------
// O microfone do cliente
// ---------------------------------------------------------------------------------------------------------------

struct Shared {
    stats: Mutex<Counters>,
    announced: bool,
    /// O dispositivo (ou o arquivo) está aberto neste instante.
    open: AtomicBool,
    log: LogFn,
}

impl Shared {
    fn stats(&self) -> std::sync::MutexGuard<'_, Counters> {
        self.stats.lock().unwrap_or_else(|e| e.into_inner())
    }
}

enum Cmd {
    Request(bool),
    Shutdown,
}

/// O microfone de uma sessão: conta os pedidos, e (só se anunciado) liga e desliga a captura.
pub struct Mic {
    shared: Arc<Shared>,
    cmd: Mutex<Option<Sender<Cmd>>>,
    worker: Mutex<Option<JoinHandle<()>>>,
}

impl Mic {
    /// `run`: a captura, se o microfone foi anunciado (`None`: não há thread nem dispositivo, e os pedidos que
    /// chegarem só são contados e ignorados). `out`: a fila de saída da conexão.
    pub fn new(run: Option<RunFn>, announced: bool, out: Sender<Message>, log: LogFn) -> Arc<Mic> {
        let shared = Arc::new(Shared { stats: Mutex::new(Counters::new()), announced, open: AtomicBool::new(false), log });
        let (cmd, worker) = match run {
            Some(run) if announced => {
                let (tx, rx) = mpsc::channel();
                let sh = shared.clone();
                let handle = std::thread::Builder::new().name("psc-microfone".into()).spawn(move || worker_loop(&sh, &run, &out, &rx)).ok();
                if handle.is_none() {
                    (shared.log)("microfone: não foi possível criar a thread do microfone; sem microfone");
                }
                (Some(tx), handle)
            }
            _ => (None, None),
        };
        Arc::new(Mic { shared, cmd: Mutex::new(cmd), worker: Mutex::new(worker) })
    }

    /// `AUDIO_REQUEST` do servidor (fluxo 1).
    pub fn request(&self, on: bool) {
        {
            let mut s = self.shared.stats();
            if on {
                s.requests += 1;
            } else {
                s.offs += 1;
            }
        }
        let tx = self.cmd.lock().unwrap_or_else(|e| e.into_inner());
        match tx.as_ref() {
            Some(tx) => {
                let _ = tx.send(Cmd::Request(on));
            }
            None => (self.shared.log)(&format!("microfone: pedido do servidor ({}) sem o microfone anunciado: ignorado, nada é capturado", if on { "ligar" } else { "desligar" })),
        }
    }

    /// Encerra: para a captura (liberando o dispositivo) e espera a thread. Pode ser chamado várias vezes.
    pub fn shutdown(&self) {
        let tx = self.cmd.lock().unwrap_or_else(|e| e.into_inner()).take();
        if let Some(tx) = tx {
            let _ = tx.send(Cmd::Shutdown);
        }
        if let Some(h) = self.worker.lock().unwrap_or_else(|e| e.into_inner()).take() {
            let _ = h.join();
        }
    }

    /// `micstats start`: zera os contadores e a janela.
    pub fn reset_stats(&self) {
        *self.shared.stats() = Counters::new();
    }

    /// `micstats report`.
    pub fn report(&self) -> MicReport {
        let s = self.shared.stats();
        let secs = s.since.elapsed().as_secs_f64();
        MicReport {
            packets: s.packets,
            kbit_s: if secs > 0.0 { s.bytes as f64 * 8.0 / 1000.0 / secs } else { 0.0 },
            requests: s.requests,
            offs: s.offs,
            peak: s.peak,
            announced: self.shared.announced,
            open: self.shared.open.load(Ordering::SeqCst),
            secs,
        }
    }

    /// A captura está aberta agora.
    pub fn is_open(&self) -> bool {
        self.shared.open.load(Ordering::SeqCst)
    }
}

impl Drop for Mic {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// O que um pedido de ligar tem em andamento.
struct Capture {
    stop: Arc<AtomicBool>,
    handle: JoinHandle<()>,
}

fn stop_capture(sh: &Shared, c: Capture, why: &str) {
    c.stop.store(true, Ordering::SeqCst);
    let _ = c.handle.join();
    (sh.log)(&format!("microfone: captura DESLIGADA ({why}); dispositivo liberado"));
}

fn worker_loop(sh: &Arc<Shared>, run: &RunFn, out: &Sender<Message>, rx: &Receiver<Cmd>) {
    let mut active: Option<Capture> = None;
    while let Ok(cmd) = rx.recv() {
        match cmd {
            Cmd::Request(true) => {
                // Uma captura que terminou sozinha (erro) não conta como ativa: o pedido novo tenta de novo.
                if active.as_ref().is_some_and(|c| c.handle.is_finished()) {
                    if let Some(c) = active.take() {
                        let _ = c.handle.join();
                    }
                }
                if active.is_some() {
                    continue;
                }
                (sh.log)("microfone: captura LIGADA (pedido do servidor: um app do Pi grava de pistick-mic)");
                let stop = Arc::new(AtomicBool::new(false));
                let (sh2, run2, out2, stop2) = (sh.clone(), run.clone(), out.clone(), stop.clone());
                match std::thread::Builder::new().name("psc-captura".into()).spawn(move || capture_main(&sh2, &run2, &out2, &stop2)) {
                    Ok(handle) => active = Some(Capture { stop, handle }),
                    Err(e) => (sh.log)(&format!("microfone: não foi possível criar a thread de captura: {e}")),
                }
            }
            Cmd::Request(false) => {
                if let Some(c) = active.take() {
                    stop_capture(sh, c, "pedido do servidor: nenhum app grava mais");
                }
            }
            Cmd::Shutdown => break,
        }
    }
    if let Some(c) = active.take() {
        stop_capture(sh, c, "fim da sessão");
    }
}

/// Corpo da thread de captura: abre, manda `AUDIO_START`, os pacotes e, ao terminar (depois de liberar o
/// dispositivo), `AUDIO_STOP`. Se o dispositivo não abre, responde só com `AUDIO_STOP`.
fn capture_main(sh: &Arc<Shared>, run: &RunFn, out: &Sender<Message>, stop: &Arc<AtomicBool>) {
    let t0 = Instant::now();
    let mut seq: u32 = 0;
    let mut payload: Vec<u8> = Vec::new();
    // Só para medir o atraso (docs/24, aceite 5): com `PSC_MIC_TRACE=1` registra o instante em que um pacote de som
    // segue a um de silêncio. Não grava nem mostra áudio, só a hora.
    let trace = std::env::var_os("PSC_MIC_TRACE").is_some_and(|v| v == "1");
    let mut prev_peak = 0i32;
    let mut on_open = || {
        sh.open.store(true, Ordering::SeqCst);
        (sh.log)("microfone: dispositivo de captura ABERTO");
        let start = Message::AudioStart { stream: AUDIO_STREAM_MIC, codec: CODEC_PSA1, channels: 2, rate: SAMPLE_RATE, frames: BLOCK_FRAMES as u16 };
        if out.send(start).is_err() {
            stop.store(true, Ordering::SeqCst);
        }
    };
    let mut sink = |block: &[i16]| {
        if block.len() != BLOCK_SAMPLES {
            return;
        }
        let peak = block.iter().map(|&s| i32::from(s).abs()).max().unwrap_or(0);
        payload.clear();
        if encode(block, 2, &mut payload).is_err() {
            return;
        }
        if trace && peak > 4000 && prev_peak <= 4000 {
            (sh.log)(&format!("microfone: [trace] inicio de som no pacote seq={seq}"));
        }
        prev_peak = peak;
        let pts_us = (t0.elapsed().as_micros() as u64).saturating_sub(BLOCK_US);
        let flags = if seq == 0 { AUDIO_FLAG_DISCONTINUITY } else { 0 };
        let msg = Message::AudioData { stream: AUDIO_STREAM_MIC, flags, seq, pts_us, data: payload.clone() };
        seq = seq.wrapping_add(1);
        {
            let mut s = sh.stats();
            s.packets += 1;
            s.bytes += payload.len() as u64;
            s.peak = s.peak.max(peak);
        }
        if out.send(msg).is_err() {
            stop.store(true, Ordering::SeqCst);
        }
    };
    let result = run(stop, &mut on_open, &mut sink);
    // `run` já devolveu: o dispositivo foi liberado. Só agora a conexão fica sabendo que o fluxo acabou.
    sh.open.store(false, Ordering::SeqCst);
    if let Err(e) = &result {
        (sh.log)(&format!("microfone: sem captura: {e}; respondendo ao pedido com AUDIO_STOP"));
    }
    let _ = out.send(Message::AudioStop { stream: AUDIO_STREAM_MIC });
}

#[cfg(test)]
mod tests;
