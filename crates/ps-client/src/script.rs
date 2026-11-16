//! Roteiro de entrada do `psc` (`--script ARQUIVO`): um comando por linha, executado depois que a
//! primeira janela aparece. Serve para testar de ponta a ponta sem ninguém à frente da tela.
//!
//! ```text
//! wait 500                 espera em milissegundos
//! win 1                    seleciona a janela alvo pela ordem de criação (popups contam);
//! win title Texto          ou a primeira janela viva cujo título contém "Texto"
//! waitwin 2 [ms]           espera a janela de número 2 existir (padrão 10000 ms); também `waitwin title Texto`
//! move 120 40              ponteiro para x,y dentro da janela alvo
//! click left               pressiona e solta (left, right, middle); `click left down` / `click left up`
//! scroll 0 -45             rolagem em pixels
//! type "texto\n"           digita como teclado ABNT2
//! key ctrl+s               combinação: ctrl, shift, alt, altgr, super + tecla
//! hold a 1500              segura a tecla por 1500 ms (quem repete é o app)
//! clip "texto"             define a área de transferência do hospedeiro e força o envio
//! showclip                 escreve no log o texto que está na área de transferência do hospedeiro
//! shot nome                salva PNG de todas as janelas em <pasta>/nome-N.png (N = número da janela)
//! resize 800 500           pede o tamanho da janela alvo
//! echo texto               escreve uma linha no log do psc
//! waitvideostart 5000      espera a região de vídeo da janela alvo ficar ativa (`VIDEO_START`); sem ela no prazo, o roteiro falha
//! waitvideostop 15000      espera o fim do vídeo (`VIDEO_STOP`) depois de ter começado
//! verify-video 30 [2.0]    durante o vídeo: fora da região idêntico à origem bit a bit e PSNR da região >= 30 dB
//!                          (o 2º argumento, opcional, é o limite em s da entrada em vídeo desde o primeiro FRAME)
//! verify-still [1500 [1500]]  parado: janela inteira idêntica à origem; tenta a cada 50 ms por até 1500 ms
//!                          (2º argumento opcional: máximo em ms entre o último VIDEO_FRAME e a janela idêntica)
//! verify-novideo           a janela alvo nunca teve região de vídeo (nenhum VIDEO_START) e não tem agora
//! atomic start             abre a medição da apresentação atômica (docs/20): a cada apresentação causada por `FRAME`,
//!                          lê os marcadores de quadro da imagem que vai para a tela (janela de `psd --synthetic-stripes`)
//! atomic report            fecha a medição e imprime `ATOMIC apresentacoes=... mistas=... quadros_distintos=... fps=...`
//!                          (mistas = marcadores com números de quadro diferentes na mesma imagem)
//! verify-atomic [MIN]      exige que o último `atomic report` tenha `mistas=0` (e ao menos MIN quadros distintos, se dado)
//! videostats start         abre uma janela de medição (quadros de vídeo por s, RTT mediano, decodificação)
//! videostats report [MIN_FPS [MAX_RTT_MS]]   fecha a janela e imprime `VIDEOSTATS ...`; fora dos limites, falha
//! waitaudio 5000           espera chegar o primeiro pacote de áudio; sem ele no prazo, o roteiro falha
//! audiostats               imprime `AUDIOSTATS ...`: pacotes, kbit/s, retomadas, descartes, esvaziamentos da fila,
//!                          estouros, quadros removidos/repetidos, nível médio, máximo e atual da fila, alvo e jitter (ms)
//! audiostats reset         zera os contadores (o aceite conta esvaziamentos a partir daqui)
//! verify-audio 440 880     confere por Goertzel que o som recebido tem FREQ_E no canal esquerdo e FREQ_D no direito
//!                          (amplitude 0,25 do sintético, +-0,02; a outra frequência em cada canal abaixo de 0,02)
//! micstats start           (marco 7B) zera os contadores do microfone e abre a janela de medição
//! micstats report          imprime `MICSTATS pacotes=... kbit_s=... pedidos=... desligados=... pico=... anunciou=0|1 captura=aberta|fechada janela=...s`
//!                          (pedidos = `AUDIO_REQUEST` de ligar recebidos; pico = maior amplitude capturada, 0 a 32768)
//! waitmic on 10000         espera a captura do microfone abrir (um app do Pi gravar); sem ela no prazo, o roteiro falha
//! waitmic off 10000        espera a captura fechar (o gravador sair); imprime quanto demorou
//! quit                     encerra o psc
//! ```
//!
//! `verify-video` e `verify-still` comparam a imagem já composta em memória (a mesma que o `shot` grava)
//! com o gerador da janela de vídeo sintética (`psd --synthetic-video`; veja `verify.rs`) e imprimem uma
//! linha `VERIFY ...`. Se qualquer verificação ou espera falhar, o roteiro continua até o fim e o `psc`
//! sai com código 3 (`SCRIPT_FAILED`).
//!
//! Comentários começam com `#`. Textos entre aspas aceitam `\n`, `\t`, `\\` e `\"`.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::sync::Arc;
use std::time::{Duration, Instant};

use ps_proto::Message;

use crate::audio::AudioEngine;
use crate::autotype::{strokes_for, text_to_strokes};
use crate::clipboard::ClipboardSync;
use crate::keymap::{BTN_LEFT, BTN_MIDDLE, BTN_RIGHT};
use crate::net::{UiSink, UserEvent, VideoState};
use crate::registry::{Registry, WinInfo};
use crate::stats::Stats;
use crate::verify;

/// Vira `true` quando uma verificação ou espera de vídeo do roteiro falha; o `psc` sai com código 3.
pub static SCRIPT_FAILED: AtomicBool = AtomicBool::new(false);

const KEY_LEFTCTRL: u32 = 29;
const KEY_LEFTSHIFT: u32 = 42;
const KEY_LEFTALT: u32 = 56;
const KEY_RIGHTALT: u32 = 100;
const KEY_LEFTMETA: u32 = 125;

/// Como o roteiro escolhe uma janela.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    Num(usize),
    Title(String),
}

/// Combinação de teclas: modificadores (códigos evdev) e a tecla final.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Chord {
    pub mods: Vec<u32>,
    pub key: u32,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Cmd {
    Wait(u64),
    Win(Target),
    WaitWin(Target, u64),
    Move(f32, f32),
    Click(u32, Option<bool>),
    Scroll(f32, f32),
    Type(String),
    Key(Chord),
    Hold(Chord, u64),
    Clip(String),
    ShowClip,
    Shot(String),
    Resize(u32, u32),
    Echo(String),
    /// `verify-video PSNR_MIN_DB [LIMITE_DE_ENTRADA_S]`
    VerifyVideo(f64, Option<f64>),
    /// `verify-still [PRAZO_MS [MAX_DEPOIS_DO_VIDEO_MS]]`
    VerifyStill(u64, Option<u64>),
    WaitVideoStart(u64),
    WaitVideoStop(u64),
    VerifyNoVideo,
    VideoStatsStart,
    /// `videostats report [MIN_FPS [MAX_RTT_MS]]`
    VideoStatsReport(Option<f64>, Option<f64>),
    /// `atomic start`
    AtomicStart,
    /// `atomic report`
    AtomicReport,
    /// `verify-atomic [MIN_QUADROS_DISTINTOS]`
    VerifyAtomic(Option<u64>),
    /// `waitaudio PRAZO_MS`
    WaitAudio(u64),
    /// `audiostats [reset]`; `true` = zerar
    AudioStats(bool),
    /// `verify-audio FREQ_E FREQ_D`
    VerifyAudio(f64, f64),
    /// `micstats start`
    MicStatsStart,
    /// `micstats report`
    MicStatsReport,
    /// `waitmic on|off PRAZO_MS`; `true` = esperar abrir
    WaitMic(bool, u64),
    Quit,
}

/// Divide a linha em palavras; aspas duplas agrupam e aceitam escapes.
fn tokenize(line: &str) -> Result<Vec<String>, String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut tem = false;
    let mut aspas = false;
    let mut chars = line.chars();
    while let Some(c) = chars.next() {
        match c {
            '"' => {
                aspas = !aspas;
                tem = true;
            }
            '\\' if aspas => match chars.next() {
                Some('n') => cur.push('\n'),
                Some('t') => cur.push('\t'),
                Some('r') => cur.push('\r'),
                Some('\\') => cur.push('\\'),
                Some('"') => cur.push('"'),
                Some(o) => {
                    cur.push('\\');
                    cur.push(o);
                }
                None => return Err("barra no fim da linha".into()),
            },
            c if c.is_whitespace() && !aspas => {
                if tem {
                    out.push(std::mem::take(&mut cur));
                    tem = false;
                }
            }
            c => {
                cur.push(c);
                tem = true;
            }
        }
    }
    if aspas {
        return Err("aspas não fechadas".into());
    }
    if tem {
        out.push(cur);
    }
    Ok(out)
}

/// Código evdev de uma tecla por nome (posição física no teclado americano) ou caractere.
fn key_code(nome: &str) -> Option<u32> {
    let n = nome.to_lowercase();
    let fixo = match n.as_str() {
        "enter" | "return" => 28,
        "esc" | "escape" => 1,
        "tab" => 15,
        "space" => 57,
        "backspace" => 14,
        "delete" | "del" => 111,
        "insert" => 110,
        "home" => 102,
        "end" => 107,
        "pageup" | "pgup" => 104,
        "pagedown" | "pgdn" => 109,
        "up" => 103,
        "down" => 108,
        "left" => 105,
        "right" => 106,
        "menu" => 127,
        "f1" => 59,
        "f2" => 60,
        "f3" => 61,
        "f4" => 62,
        "f5" => 63,
        "f6" => 64,
        "f7" => 65,
        "f8" => 66,
        "f9" => 67,
        "f10" => 68,
        "f11" => 87,
        "f12" => 88,
        "minus" => 12,
        "equal" => 13,
        "comma" => 51,
        "period" => 52,
        "slash" => 53,
        _ => 0,
    };
    if fixo != 0 {
        return Some(fixo);
    }
    let mut it = nome.chars();
    let (Some(c), None) = (it.next(), it.next()) else { return None };
    let strokes = strokes_for(c.to_ascii_lowercase())?;
    // Só teclas simples: uma batida, sem Shift.
    match strokes.as_slice() {
        [s] if !s.shift => Some(s.code),
        _ => None,
    }
}

pub fn parse_chord(texto: &str) -> Result<Chord, String> {
    let mut mods = Vec::new();
    let mut partes: Vec<&str> = texto.split('+').collect();
    // "ctrl++" não é suportado; a tecla é sempre o último pedaço.
    let tecla = partes.pop().filter(|t| !t.is_empty()).ok_or_else(|| format!("combinação vazia: {texto:?}"))?;
    for m in partes {
        mods.push(match m.to_lowercase().as_str() {
            "ctrl" | "control" => KEY_LEFTCTRL,
            "shift" => KEY_LEFTSHIFT,
            "alt" => KEY_LEFTALT,
            "altgr" => KEY_RIGHTALT,
            "super" | "win" | "meta" => KEY_LEFTMETA,
            outro => return Err(format!("modificador desconhecido: {outro:?}")),
        });
    }
    let key = key_code(tecla).ok_or_else(|| format!("tecla desconhecida: {tecla:?}"))?;
    Ok(Chord { mods, key })
}

fn parse_target(args: &[String]) -> Result<Target, String> {
    match args {
        [t, rest @ ..] if t == "title" && !rest.is_empty() => Ok(Target::Title(rest.join(" "))),
        [n] => n.parse::<usize>().ok().filter(|n| *n >= 1).map(Target::Num).ok_or_else(|| format!("número de janela inválido: {n:?}")),
        _ => Err("esperava um número de janela ou `title TEXTO`".into()),
    }
}

fn num<T: std::str::FromStr>(s: &str, o_que: &str) -> Result<T, String> {
    s.parse::<T>().map_err(|_| format!("{o_que} inválido: {s:?}"))
}

/// Interpreta uma linha. `Ok(None)` para linha vazia ou comentário.
pub fn parse_line(line: &str) -> Result<Option<Cmd>, String> {
    let l = line.trim();
    if l.is_empty() || l.starts_with('#') {
        return Ok(None);
    }
    let t = tokenize(l)?;
    let (cmd, args) = t.split_first().ok_or("linha vazia")?;
    let a = |n: usize| -> Result<(), String> {
        if args.len() == n {
            Ok(())
        } else {
            Err(format!("`{cmd}` espera {n} argumento(s), recebeu {}", args.len()))
        }
    };
    let c = match cmd.as_str() {
        "wait" => {
            a(1)?;
            Cmd::Wait(num(&args[0], "tempo")?)
        }
        "win" => Cmd::Win(parse_target(args)?),
        "waitwin" => {
            // Prazo opcional no fim: `waitwin 2 3000` ou `waitwin title Salvar 3000`.
            let (alvo, prazo) = match args.split_last() {
                Some((ultimo, resto)) if !resto.is_empty() && (args[0] != "title" || resto.len() >= 2) => match ultimo.parse::<u64>() {
                    Ok(ms) => (resto, ms),
                    Err(_) => (args, 10_000),
                },
                _ => (args, 10_000),
            };
            Cmd::WaitWin(parse_target(alvo)?, prazo)
        }
        "move" => {
            a(2)?;
            Cmd::Move(num(&args[0], "x")?, num(&args[1], "y")?)
        }
        "click" => {
            if args.is_empty() || args.len() > 2 {
                return Err("`click` espera left|right|middle [down|up]".into());
            }
            let b = match args[0].as_str() {
                "left" => BTN_LEFT,
                "right" => BTN_RIGHT,
                "middle" => BTN_MIDDLE,
                o => return Err(format!("botão desconhecido: {o:?}")),
            };
            let fase = match args.get(1).map(String::as_str) {
                None => None,
                Some("down") => Some(true),
                Some("up") => Some(false),
                Some(o) => return Err(format!("esperava down ou up, recebeu {o:?}")),
            };
            Cmd::Click(b, fase)
        }
        "scroll" => {
            a(2)?;
            Cmd::Scroll(num(&args[0], "dx")?, num(&args[1], "dy")?)
        }
        "type" => {
            a(1)?;
            Cmd::Type(args[0].clone())
        }
        "key" => {
            a(1)?;
            Cmd::Key(parse_chord(&args[0])?)
        }
        "hold" => {
            a(2)?;
            Cmd::Hold(parse_chord(&args[0])?, num(&args[1], "tempo")?)
        }
        "clip" => {
            a(1)?;
            Cmd::Clip(args[0].clone())
        }
        "showclip" => {
            a(0)?;
            Cmd::ShowClip
        }
        "shot" => {
            a(1)?;
            Cmd::Shot(args[0].clone())
        }
        "resize" => {
            a(2)?;
            Cmd::Resize(num(&args[0], "largura")?, num(&args[1], "altura")?)
        }
        "echo" => Cmd::Echo(args.join(" ")),
        "waitvideostart" => {
            a(1)?;
            Cmd::WaitVideoStart(num(&args[0], "prazo")?)
        }
        "waitvideostop" => {
            a(1)?;
            Cmd::WaitVideoStop(num(&args[0], "prazo")?)
        }
        "verify-novideo" => {
            a(0)?;
            Cmd::VerifyNoVideo
        }
        "videostats" => match args.first().map(String::as_str) {
            Some("start") if args.len() == 1 => Cmd::VideoStatsStart,
            Some("report") if args.len() <= 3 => {
                let min = args.get(1).map(|v| num::<f64>(v, "fps mínimo")).transpose()?;
                let max = args.get(2).map(|v| num::<f64>(v, "RTT máximo")).transpose()?;
                Cmd::VideoStatsReport(min, max)
            }
            _ => return Err("`videostats` espera `start` ou `report [MIN_FPS [MAX_RTT_MS]]`".into()),
        },
        "verify-video" => {
            if args.is_empty() || args.len() > 2 {
                return Err("`verify-video` espera PSNR_MIN_DB [LIMITE_DE_ENTRADA_S]".into());
            }
            let db: f64 = num(&args[0], "PSNR mínimo")?;
            let lim = args.get(1).map(|v| num::<f64>(v, "limite de entrada")).transpose()?;
            if !db.is_finite() || db < 0.0 || lim.is_some_and(|l| !l.is_finite() || l < 0.0) {
                return Err("`verify-video`: os valores precisam ser números finitos e não negativos".into());
            }
            Cmd::VerifyVideo(db, lim)
        }
        "verify-still" => {
            if args.len() > 2 {
                return Err("`verify-still` espera [PRAZO_MS [MAX_DEPOIS_DO_VIDEO_MS]]".into());
            }
            let prazo = args.first().map(|v| num::<u64>(v, "prazo")).transpose()?.unwrap_or(0);
            let max = args.get(1).map(|v| num::<u64>(v, "limite")).transpose()?;
            Cmd::VerifyStill(prazo, max)
        }
        "atomic" => match args {
            [x] if x == "start" => Cmd::AtomicStart,
            [x] if x == "report" => Cmd::AtomicReport,
            _ => return Err("`atomic` espera `start` ou `report`".into()),
        },
        "verify-atomic" => {
            if args.len() > 1 {
                return Err("`verify-atomic` espera [MIN_QUADROS_DISTINTOS]".into());
            }
            Cmd::VerifyAtomic(args.first().map(|v| num::<u64>(v, "mínimo de quadros distintos")).transpose()?)
        }
        "waitaudio" => {
            a(1)?;
            Cmd::WaitAudio(num(&args[0], "prazo")?)
        }
        "audiostats" => match args {
            [] => Cmd::AudioStats(false),
            [r] if r == "reset" => Cmd::AudioStats(true),
            _ => return Err("`audiostats` espera nada ou `reset`".into()),
        },
        "verify-audio" => {
            a(2)?;
            let (fe, fd): (f64, f64) = (num(&args[0], "frequência esquerda")?, num(&args[1], "frequência direita")?);
            if !(20.0..=20_000.0).contains(&fe) || !(20.0..=20_000.0).contains(&fd) {
                return Err("`verify-audio`: frequências entre 20 e 20000 Hz".into());
            }
            Cmd::VerifyAudio(fe, fd)
        }
        "micstats" => match args {
            [x] if x == "start" => Cmd::MicStatsStart,
            [x] if x == "report" => Cmd::MicStatsReport,
            _ => return Err("`micstats` espera `start` ou `report`".into()),
        },
        "waitmic" => match args {
            [x, ms] if x == "on" || x == "off" => Cmd::WaitMic(x == "on", num(ms, "prazo")?),
            _ => return Err("`waitmic` espera `on` ou `off` e o prazo em ms".into()),
        },
        "quit" => {
            a(0)?;
            Cmd::Quit
        }
        outro => return Err(format!("comando desconhecido: {outro:?}")),
    };
    Ok(Some(c))
}

/// Interpreta o arquivo inteiro; o erro diz a linha.
pub fn parse_script(texto: &str) -> Result<Vec<Cmd>, String> {
    let mut cmds = Vec::new();
    for (i, linha) in texto.lines().enumerate() {
        match parse_line(linha) {
            Ok(Some(c)) => cmds.push(c),
            Ok(None) => {}
            Err(e) => return Err(format!("linha {}: {e}", i + 1)),
        }
    }
    Ok(cmds)
}

/// Os atalhos antigos `--resize` e `--type` viram um roteiro: o pedido de tamanho 1,5 s depois
/// da primeira janela e a digitação 2 s depois dela.
pub fn legacy_script(type_text: Option<String>, resize_to: Option<(u32, u32)>) -> Vec<Cmd> {
    let mut cmds = Vec::new();
    let mut decorrido = 0;
    if let Some((w, h)) = resize_to {
        cmds.push(Cmd::Wait(1500));
        cmds.push(Cmd::Resize(w, h));
        decorrido = 1500;
    }
    if let Some(t) = type_text {
        cmds.push(Cmd::Wait(2000 - decorrido));
        cmds.push(Cmd::Type(t));
    }
    cmds
}

// A composição (janela com os popups) vive no núcleo (`crate::compose`): o cliente Android a reaproveita.
pub use crate::compose::{compose_with_popups, frame_of, paste_bgrx};

/// O que o executor do roteiro precisa do resto do cliente.
pub struct Ctx {
    pub out: Sender<Message>,
    pub registry: Arc<Registry>,
    pub ui: Arc<dyn UiSink>,
    pub shot_dir: PathBuf,
    pub clip: Arc<ClipboardSync>,
    pub stats: Arc<Stats>,
    pub audio: Option<Arc<AudioEngine>>,
    pub mic: Arc<crate::mic::Mic>,
}

struct Runner {
    ctx: Ctx,
    /// Número (ordem de criação) da janela alvo.
    target: Option<usize>,
    /// Raiz que o roteiro anunciou como focada.
    focused: Option<u32>,
    /// Resultado do último `atomic report`, para o `verify-atomic`.
    last_atomic: Option<crate::atomic::AtomicReport>,
}

/// Inicia o roteiro numa thread própria. Espera a primeira janela aparecer (até 60 s).
pub fn spawn(cmds: Vec<Cmd>, ctx: Ctx) {
    std::thread::Builder::new()
        .name("psc-roteiro".into())
        .spawn(move || {
            let mut r = Runner { ctx, target: None, focused: None, last_atomic: None };
            if !r.ctx.registry.wait_until(Duration::from_secs(60), |w| !w.is_empty()) {
                eprintln!("[psc] roteiro: nenhuma janela apareceu em 60 s");
                return;
            }
            r.select(&Target::Num(1));
            for c in cmds {
                if !r.run(c) {
                    return;
                }
            }
            println!("[psc] roteiro: fim");
        })
        .expect("criar thread do roteiro");
}

fn pausa(ms: u64) {
    std::thread::sleep(Duration::from_millis(ms));
}

impl Runner {
    fn send(&self, m: Message) {
        let _ = self.ctx.out.send(m);
    }

    fn target_info(&self) -> Option<WinInfo> {
        let info = self.target.and_then(|n| self.ctx.registry.by_num(n));
        match info {
            Some(i) if i.alive => Some(i),
            _ => {
                eprintln!("[psc] roteiro: a janela alvo não existe (mais)");
                None
            }
        }
    }

    fn find(&self, t: &Target) -> Option<WinInfo> {
        match t {
            Target::Num(n) => self.ctx.registry.by_num(*n).filter(|w| w.alive),
            Target::Title(s) => self.ctx.registry.by_title(s),
        }
    }

    /// Seleciona a janela alvo; se for uma janela de nível superior, passa o foco a ela.
    fn select(&mut self, t: &Target) {
        let Some(w) = self.find(t) else {
            eprintln!("[psc] roteiro: janela {t:?} não existe");
            return;
        };
        self.target = Some(w.num);
        println!("[psc] roteiro: alvo = janela {} (id {}) \"{}\"", w.num, w.id, w.title);
        if !w.popup && self.focused != Some(w.id) {
            if let Some(old) = self.focused {
                self.send(Message::Focus { win: old, focused: false });
            }
            self.send(Message::Focus { win: w.id, focused: true });
            self.focused = Some(w.id);
        }
    }

    /// Id que recebe o teclado: a raiz da cadeia da janela alvo.
    fn key_win(&self) -> Option<u32> {
        let w = self.target_info()?;
        Some(self.ctx.registry.root_of(w.id))
    }

    fn key(&self, win: u32, code: u32, pressed: bool) {
        self.send(Message::Key { win, keycode: code, pressed });
    }

    fn press_chord(&self, win: u32, c: &Chord, hold_ms: u64) {
        for m in &c.mods {
            self.key(win, *m, true);
            pausa(10);
        }
        self.key(win, c.key, true);
        pausa(hold_ms);
        self.key(win, c.key, false);
        pausa(10);
        for m in c.mods.iter().rev() {
            self.key(win, *m, false);
            pausa(10);
        }
    }

    /// Executa um comando. `false` encerra o roteiro.
    fn run(&mut self, c: Cmd) -> bool {
        println!("[psc] roteiro: {c:?}");
        match c {
            Cmd::Wait(ms) => pausa(ms),
            Cmd::Win(t) => self.select(&t),
            Cmd::WaitWin(t, ms) => {
                let ok = self.ctx.registry.wait_until(Duration::from_millis(ms), |ws| match &t {
                    Target::Num(n) => ws.iter().any(|w| w.num == *n && w.alive),
                    Target::Title(s) => ws.iter().any(|w| w.alive && w.title.to_lowercase().contains(&s.to_lowercase())),
                });
                if !ok {
                    eprintln!("[psc] roteiro: janela {t:?} não apareceu em {ms} ms");
                }
            }
            Cmd::Move(x, y) => {
                if let Some(w) = self.target_info() {
                    self.send(Message::PointerMove { win: w.id, x, y });
                }
            }
            Cmd::Click(b, fase) => {
                if let Some(w) = self.target_info() {
                    match fase {
                        Some(p) => self.send(Message::PointerButton { win: w.id, button: b, pressed: p }),
                        None => {
                            self.send(Message::PointerButton { win: w.id, button: b, pressed: true });
                            pausa(40);
                            self.send(Message::PointerButton { win: w.id, button: b, pressed: false });
                        }
                    }
                }
            }
            Cmd::Scroll(dx, dy) => {
                if let Some(w) = self.target_info() {
                    self.send(Message::PointerAxis { win: w.id, dx, dy });
                }
            }
            Cmd::Type(texto) => {
                if let Some(win) = self.key_win() {
                    for st in text_to_strokes(&texto) {
                        if st.shift {
                            self.key(win, KEY_LEFTSHIFT, true);
                        }
                        self.key(win, st.code, true);
                        pausa(30);
                        self.key(win, st.code, false);
                        if st.shift {
                            self.key(win, KEY_LEFTSHIFT, false);
                        }
                        pausa(30);
                    }
                }
            }
            Cmd::Key(ch) => {
                if let Some(win) = self.key_win() {
                    self.press_chord(win, &ch, 30);
                }
            }
            Cmd::Hold(ch, ms) => {
                if let Some(win) = self.key_win() {
                    self.press_chord(win, &ch, ms);
                }
            }
            Cmd::Clip(texto) => {
                if !self.ctx.clip.force_set(&texto) {
                    eprintln!("[psc] roteiro: não foi possível definir a área de transferência do sistema");
                }
            }
            Cmd::ShowClip => match self.ctx.clip.host_text() {
                Some(t) => println!("[psc] área de transferência do hospedeiro: {t:?}"),
                None => println!("[psc] área de transferência do hospedeiro: vazia ou sem texto"),
            },
            Cmd::Shot(nome) => self.shot(&nome),
            Cmd::Resize(w, h) => {
                if let Some(i) = self.target_info() {
                    self.send(Message::ResizeReq { win: i.id, width: w, height: h });
                }
            }
            Cmd::Echo(t) => println!("[psc] roteiro: {t}"),
            Cmd::WaitVideoStart(ms) => self.wait_video(ms, true),
            Cmd::WaitVideoStop(ms) => self.wait_video(ms, false),
            Cmd::VerifyVideo(db, lim) => self.verify_video(db, lim),
            Cmd::VerifyStill(prazo, max) => self.verify_still(prazo, max),
            Cmd::VerifyNoVideo => self.verify_novideo(),
            Cmd::VideoStatsStart => {
                self.ctx.stats.window_start();
                println!("[psc] roteiro: janela de medição aberta");
            }
            Cmd::VideoStatsReport(min, max) => self.videostats_report(min, max),
            Cmd::AtomicStart => {
                self.ctx.stats.atomic_start();
                println!("[psc] roteiro: medição da apresentação atômica aberta");
            }
            Cmd::AtomicReport => match self.ctx.stats.atomic_report() {
                Some(r) => {
                    println!("{}", r.line());
                    self.last_atomic = Some(r);
                }
                None => {
                    println!("ATOMIC FALHOU motivo=\"nenhuma medição aberta (atomic start)\"");
                    self.fail();
                }
            },
            Cmd::VerifyAtomic(min) => self.verify_atomic(min),
            Cmd::WaitAudio(ms) => self.wait_audio(ms),
            Cmd::AudioStats(reset) => self.audiostats(reset),
            Cmd::VerifyAudio(fe, fd) => self.verify_audio(fe, fd),
            Cmd::MicStatsStart => {
                self.ctx.mic.reset_stats();
                println!("MICSTATS start");
            }
            Cmd::MicStatsReport => println!("{}", self.ctx.mic.report().line()),
            Cmd::WaitMic(on, ms) => self.wait_mic(on, ms),
            Cmd::Quit => {
                let _ = self.ctx.ui.send(UserEvent::Quit);
                return false;
            }
        }
        true
    }

    /// Imagem composta da janela alvo (a mesma do `shot`, sem popups) e o estado do vídeo, no mesmo instante.
    fn snapshot(&self) -> Option<(u32, u32, Vec<u8>, VideoState)> {
        let w = self.target_info()?;
        let g = w.shared.inner.lock().unwrap_or_else(|e| e.into_inner());
        Some((g.decoder.width(), g.decoder.height(), g.display_pixels().to_vec(), g.video.clone()))
    }

    fn video_state(&self) -> Option<VideoState> {
        let w = self.target_info()?;
        let g = w.shared.inner.lock().unwrap_or_else(|e| e.into_inner());
        Some(g.video.clone())
    }

    fn fail(&self) {
        SCRIPT_FAILED.store(true, Ordering::SeqCst);
    }

    /// `waitvideostart` (`ativo = true`) ou `waitvideostop`: sondagem a cada 5 ms até o prazo.
    fn wait_video(&self, ms: u64, ativo: bool) {
        let nome = if ativo { "waitvideostart" } else { "waitvideostop" };
        let fim = Instant::now() + Duration::from_millis(ms);
        loop {
            let Some(v) = self.video_state() else {
                self.fail();
                return;
            };
            let pronto = if ativo { v.active.is_some() } else { v.active.is_none() && v.starts > 0 && v.stops >= v.starts };
            if pronto {
                let (instante, rotulo) = if ativo { (v.start_at, "START") } else { (v.stop_at, "STOP") };
                let t = instante.map_or_else(|| "?".to_string(), |i| v.rel_text(i));
                println!("[psc] roteiro: {nome} ok: {rotulo} em t={t} desde o primeiro FRAME");
                return;
            }
            if Instant::now() >= fim {
                println!("[psc] roteiro: {nome} FALHOU: sem {} em {ms} ms (iniciados {}, encerrados {})", if ativo { "VIDEO_START" } else { "VIDEO_STOP" }, v.starts, v.stops);
                self.fail();
                return;
            }
            pausa(5);
        }
    }

    fn verify_novideo(&self) {
        let Some(v) = self.video_state() else {
            println!("VERIFY novideo FALHOU motivo=\"a janela alvo não existe\"");
            self.fail();
            return;
        };
        if v.starts == 0 && v.active.is_none() && v.video_frames == 0 {
            println!("VERIFY novideo ok video_start=0 video_frames=0");
        } else {
            println!("VERIFY novideo FALHOU video_start={} video_frames={} ativo={} motivo=\"a janela teve vídeo\"", v.starts, v.video_frames, v.active.is_some());
            self.fail();
        }
    }

    /// `waitaudio`: sondagem a cada 5 ms até chegar o primeiro pacote.
    fn wait_audio(&self, ms: u64) {
        let Some(a) = &self.ctx.audio else {
            println!("[psc] roteiro: waitaudio FALHOU: o áudio está desativado (--no-audio)");
            self.fail();
            return;
        };
        let fim = Instant::now() + Duration::from_millis(ms);
        while a.stats().first_packet_ms.is_none() {
            if Instant::now() >= fim {
                println!("[psc] roteiro: waitaudio FALHOU: nenhum AUDIO_DATA em {ms} ms");
                self.fail();
                return;
            }
            pausa(5);
        }
        println!("[psc] roteiro: waitaudio ok: primeiro AUDIO_DATA {:.0} ms depois da abertura do áudio", a.stats().first_packet_ms.unwrap_or(0.0));
    }

    /// `waitmic on|off`: sondagem a cada 5 ms até a captura abrir (ou fechar).
    fn wait_mic(&self, on: bool, ms: u64) {
        let t0 = Instant::now();
        let fim = t0 + Duration::from_millis(ms);
        while self.ctx.mic.is_open() != on {
            if Instant::now() >= fim {
                println!("[psc] roteiro: waitmic {} FALHOU: a captura continua {} depois de {ms} ms", if on { "on" } else { "off" }, if on { "fechada" } else { "aberta" });
                self.fail();
                return;
            }
            pausa(5);
        }
        println!("[psc] roteiro: waitmic {} ok: captura {} depois de {:.0} ms", if on { "on" } else { "off" }, if on { "aberta" } else { "fechada" }, t0.elapsed().as_secs_f64() * 1000.0);
    }

    fn audiostats(&self, reset: bool) {
        let Some(a) = &self.ctx.audio else {
            println!("AUDIOSTATS FALHOU motivo=\"o áudio está desativado (--no-audio)\"");
            self.fail();
            return;
        };
        if reset {
            a.reset_stats();
            println!("AUDIOSTATS reset");
            return;
        }
        println!("{}", a.stats().audiostats_line());
    }

    /// `verify-audio`: espera até 3 s por 200 ms contíguos de som e confere as frequências dos canais.
    fn verify_audio(&self, fe: f64, fd: f64) {
        let Some(a) = &self.ctx.audio else {
            println!("VERIFY audio FALHOU motivo=\"o áudio está desativado (--no-audio)\"");
            self.fail();
            return;
        };
        let fim = Instant::now() + Duration::from_secs(3);
        let (pkts, x) = loop {
            let (p, x) = a.recent_sound();
            if p >= 20 || Instant::now() >= fim {
                break (p, x);
            }
            pausa(20);
        };
        if pkts < 20 {
            println!("VERIFY audio FALHOU motivo=\"só {pkts} pacotes seguidos com som (precisa de 20)\"");
            self.fail();
            return;
        }
        let (l, r) = ps_audio::synth::split_stereo(&x);
        let g = ps_audio::synth::goertzel;
        let (le, ld, re, rd) = (g(&l, fe), g(&l, fd), g(&r, fe), g(&r, fd));
        let alvo = ps_audio::synth::AMPLITUDE;
        let mut problemas = Vec::new();
        if (le - alvo).abs() > 0.02 {
            problemas.push(format!("canal esquerdo tem {le:.3} em {fe} Hz (esperado {alvo})"));
        }
        if (rd - alvo).abs() > 0.02 {
            problemas.push(format!("canal direito tem {rd:.3} em {fd} Hz (esperado {alvo})"));
        }
        if fe != fd {
            if ld > 0.02 {
                problemas.push(format!("canal esquerdo vaza {ld:.3} em {fd} Hz"));
            }
            if re > 0.02 {
                problemas.push(format!("canal direito vaza {re:.3} em {fe} Hz"));
            }
        }
        let linha = format!("esq[{fe}]={le:.3} esq[{fd}]={ld:.3} dir[{fe}]={re:.3} dir[{fd}]={rd:.3} pacotes={pkts}");
        if problemas.is_empty() {
            println!("VERIFY audio ok {linha}");
        } else {
            println!("VERIFY audio FALHOU {linha} motivo=\"{}\"", problemas.join("; "));
            self.fail();
        }
    }

    /// `verify-atomic`: o último `atomic report` não pode ter imagem mista (código 3 se tiver).
    fn verify_atomic(&self, min_distintos: Option<u64>) {
        let Some(r) = &self.last_atomic else {
            println!("VERIFY atomic FALHOU motivo=\"nenhum `atomic report` antes\"");
            self.fail();
            return;
        };
        let mut problemas = Vec::new();
        if r.mixed > 0 {
            problemas.push(format!("{} de {} apresentações misturaram quadros diferentes", r.mixed, r.presentations));
        }
        if r.presentations == 0 {
            problemas.push("nenhuma apresentação medida".to_string());
        }
        if let Some(min) = min_distintos {
            if r.distinct_frames < min {
                problemas.push(format!("{} quadros distintos, abaixo de {min}", r.distinct_frames));
            }
        }
        if problemas.is_empty() {
            println!("VERIFY atomic ok apresentacoes={} mistas=0 quadros_distintos={}", r.presentations, r.distinct_frames);
        } else {
            println!("VERIFY atomic FALHOU apresentacoes={} mistas={} quadros_distintos={} motivo=\"{}\"", r.presentations, r.mixed, r.distinct_frames, problemas.join("; "));
            self.fail();
        }
    }

    fn videostats_report(&self, min_fps: Option<f64>, max_rtt_ms: Option<f64>) {
        let Some(r) = self.ctx.stats.window_report() else {
            println!("VIDEOSTATS FALHOU motivo=\"nenhuma janela de medição aberta (videostats start)\"");
            self.fail();
            return;
        };
        let mut problemas = Vec::new();
        if let Some(min) = min_fps {
            if r.video_fps < min {
                problemas.push(format!("{:.1} quadros de vídeo por s, abaixo de {min}", r.video_fps));
            }
        }
        if let Some(max) = max_rtt_ms {
            match r.rtt_median_ms {
                Some(m) if m < max => {}
                Some(m) => problemas.push(format!("RTT mediano {m:.2} ms, não abaixo de {max} ms")),
                None => problemas.push("sem amostras de RTT".into()),
            }
        }
        let linha = r.videostats_line();
        if problemas.is_empty() {
            println!("{linha}");
        } else {
            println!("{linha} FALHOU motivo=\"{}\"", problemas.join("; "));
            self.fail();
        }
    }

    fn verify_video(&self, min_db: f64, limite_entrada: Option<f64>) {
        let Some((w, h, px, v)) = self.snapshot() else {
            println!("VERIFY video FALHOU motivo=\"a janela alvo não existe\"");
            self.fail();
            return;
        };
        let active = v.active.map(|(_, r)| r);
        let center = v.last_capture_us.map(verify::frame_from_capture_us);
        let r = verify::check_video(&px, w, h, active, center, verify::SEARCH_RADIUS, min_db);
        let entrada = v.start_at.and_then(|i| v.rel(i));
        let mut problemas: Vec<String> = r.problem.clone().into_iter().collect();
        if r.outside_diff.count > 0 {
            problemas.push(format!("{} pixels diferentes fora da região", r.outside_diff.count));
        }
        if r.psnr < min_db {
            problemas.push(format!("PSNR {:.1} dB abaixo do mínimo de {min_db} dB", r.psnr));
        }
        match (limite_entrada, entrada) {
            (Some(lim), Some(t)) if t > lim => problemas.push(format!("entrada em vídeo em {t:.2} s, acima do limite de {lim} s")),
            (Some(_), None) => problemas.push("entrada em vídeo sem instante registrado".into()),
            _ => {}
        }
        let psnr = if r.psnr.is_infinite() { "inf".to_string() } else { format!("{:.1}dB", r.psnr) };
        let bbox = r.outside_diff.bbox.map_or(String::new(), |b| format!(" retangulo_diferente={}x{}+{}+{}", b.w, b.h, b.x, b.y));
        let entrada_txt = entrada.map_or(String::new(), |t| format!(" entrada_em={t:.2}s"));
        if problemas.is_empty() {
            println!("VERIFY video ok quadro={} psnr={psnr} fora_da_regiao_diferentes={}{entrada_txt}", r.frame, r.outside_diff.count);
        } else {
            println!("VERIFY video FALHOU quadro={} psnr={psnr} fora_da_regiao_diferentes={}{bbox}{entrada_txt} motivo=\"{}\"", r.frame, r.outside_diff.count, problemas.join("; "));
            self.fail();
        }
    }

    fn verify_still(&self, prazo_ms: u64, max_depois_ms: Option<u64>) {
        let fim = Instant::now() + Duration::from_millis(prazo_ms);
        let mut tentativas = 0u32;
        loop {
            let Some((w, h, px, v)) = self.snapshot() else {
                println!("VERIFY still FALHOU motivo=\"a janela alvo não existe\"");
                self.fail();
                return;
            };
            tentativas += 1;
            let center = v.last_capture_us.map(verify::frame_from_capture_us);
            // Com o vídeo ainda ativo `check_still` reprova sem comparar; o laço repete a cada 50 ms.
            let r = verify::check_still(&px, w, h, v.active.is_some(), center, verify::SEARCH_RADIUS);
            if r.ok {
                // A janela é idêntica desde a última vez que os pixels mudaram: o tempo que vale é o dessa
                // mudança, não o da checagem (que gasta algumas centenas de ms na busca do quadro).
                let ms_entre = |a: Option<Instant>, b: Option<Instant>| match (a, b) {
                    (Some(a), Some(b)) => Some(a.saturating_duration_since(b).as_millis() as u64),
                    _ => None,
                };
                let depois = ms_entre(v.last_update_at, v.last_video_frame_at);
                let desde_stop = ms_entre(v.last_update_at, v.stop_at);
                let mut txt = format!("VERIFY still ok quadro={} diferentes=0 tentativas={tentativas}", r.frame);
                if let Some(d) = depois {
                    txt += &format!(" apos_ultimo_video_frame={d}ms");
                }
                if let Some(d) = desde_stop {
                    txt += &format!(" apos_video_stop={d}ms");
                }
                match (max_depois_ms, depois) {
                    (Some(max), Some(d)) if d > max => {
                        println!("{txt} FALHOU motivo=\"{d} ms entre o último VIDEO_FRAME e a janela idêntica, acima de {max} ms\"");
                        self.fail();
                    }
                    (Some(_), None) => {
                        println!("{txt} FALHOU motivo=\"nenhum VIDEO_FRAME recebido: não há como medir o intervalo\"");
                        self.fail();
                    }
                    _ => println!("{txt}"),
                }
                return;
            }
            if Instant::now() >= fim {
                let motivo = r.problem.clone().unwrap_or_else(|| "a janela difere da origem".into());
                let bbox = r.diff.bbox.map_or(String::new(), |b| format!(" retangulo={}x{}+{}+{}", b.w, b.h, b.x, b.y));
                println!("VERIFY still FALHOU quadro_mais_proximo={} menor_contagem_diferentes={}{bbox} tentativas={tentativas} motivo=\"{motivo}\"", r.frame, r.diff.count);
                self.fail();
                return;
            }
            pausa(50);
        }
    }

    fn shot(&self, nome: &str) {
        let todas = self.ctx.registry.snapshot();
        let mut n = 0;
        for w in todas.iter().filter(|w| w.alive) {
            let (fw, fh, px) = frame_of(w);
            let path = self.ctx.shot_dir.join(format!("{nome}-{}.png", w.num));
            match crate::png::save_bgrx(&path, fw, fh, &px) {
                Ok(()) => {
                    n += 1;
                    let tipo = if w.popup { format!("popup de {} em ({}, {})", w.parent, w.x, w.y) } else { "janela".to_string() };
                    println!("[psc] shot: {} -> {} ({fw}x{fh}, {tipo}, \"{}\")", w.num, path.display(), w.title);
                }
                Err(e) => eprintln!("[psc] shot: não foi possível gravar {}: {e}", path.display()),
            }
            if !w.popup {
                if let Some((cw, ch, canvas)) = compose_with_popups(w, &todas) {
                    let path = self.ctx.shot_dir.join(format!("{nome}-composto-{}.png", w.num));
                    match crate::png::save_bgrx(&path, cw, ch, &canvas) {
                        Ok(()) => println!("[psc] shot: janela {} com os popups -> {} ({cw}x{ch})", w.num, path.display()),
                        Err(e) => eprintln!("[psc] shot: não foi possível gravar {}: {e}", path.display()),
                    }
                }
            }
        }
        if n == 0 {
            eprintln!("[psc] shot: nenhuma janela viva");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parser_dos_comandos_minimos() {
        let t = r#"
            # comentário
            wait 500
            win 2
            win title Salvar como
            move 120 40.5
            click left
            click right down
            scroll 0 -45
            type "texto ação\n"
            key ctrl+shift+s
            hold a 1500
            clip "pistick-teste-123"
            showclip
            shot menu
            resize 800 500
            waitwin 3 2000
            waitwin title Salvar
            waitwin title Salvar como 3000
            echo oi mundo
            waitvideostart 5000
            waitvideostop 15000
            verify-video 30
            verify-video 30 2.0
            verify-still
            verify-still 1500
            verify-still 1500 1500
            verify-novideo
            videostats start
            videostats report
            videostats report 27 120
            waitaudio 5000
            audiostats
            audiostats reset
            verify-audio 440 880
            micstats start
            micstats report
            waitmic on 5000
            waitmic off 1500
            atomic start
            atomic report
            verify-atomic
            verify-atomic 3
            quit
        "#;
        let c = parse_script(t).unwrap();
        assert_eq!(c[0], Cmd::Wait(500));
        assert_eq!(c[1], Cmd::Win(Target::Num(2)));
        assert_eq!(c[2], Cmd::Win(Target::Title("Salvar como".into())));
        assert_eq!(c[3], Cmd::Move(120.0, 40.5));
        assert_eq!(c[4], Cmd::Click(BTN_LEFT, None));
        assert_eq!(c[5], Cmd::Click(BTN_RIGHT, Some(true)));
        assert_eq!(c[6], Cmd::Scroll(0.0, -45.0));
        assert_eq!(c[7], Cmd::Type("texto ação\n".into()));
        assert_eq!(c[8], Cmd::Key(Chord { mods: vec![29, 42], key: 31 }));
        assert_eq!(c[9], Cmd::Hold(Chord { mods: vec![], key: 30 }, 1500));
        assert_eq!(c[10], Cmd::Clip("pistick-teste-123".into()));
        assert_eq!(c[11], Cmd::ShowClip);
        assert_eq!(c[12], Cmd::Shot("menu".into()));
        assert_eq!(c[13], Cmd::Resize(800, 500));
        assert_eq!(c[14], Cmd::WaitWin(Target::Num(3), 2000));
        assert_eq!(c[15], Cmd::WaitWin(Target::Title("Salvar".into()), 10_000));
        assert_eq!(c[16], Cmd::WaitWin(Target::Title("Salvar como".into()), 3000));
        assert_eq!(c[17], Cmd::Echo("oi mundo".into()));
        assert_eq!(c[18], Cmd::WaitVideoStart(5000));
        assert_eq!(c[19], Cmd::WaitVideoStop(15000));
        assert_eq!(c[20], Cmd::VerifyVideo(30.0, None));
        assert_eq!(c[21], Cmd::VerifyVideo(30.0, Some(2.0)));
        assert_eq!(c[22], Cmd::VerifyStill(0, None));
        assert_eq!(c[23], Cmd::VerifyStill(1500, None));
        assert_eq!(c[24], Cmd::VerifyStill(1500, Some(1500)));
        assert_eq!(c[25], Cmd::VerifyNoVideo);
        assert_eq!(c[26], Cmd::VideoStatsStart);
        assert_eq!(c[27], Cmd::VideoStatsReport(None, None));
        assert_eq!(c[28], Cmd::VideoStatsReport(Some(27.0), Some(120.0)));
        assert_eq!(c[29], Cmd::WaitAudio(5000));
        assert_eq!(c[30], Cmd::AudioStats(false));
        assert_eq!(c[31], Cmd::AudioStats(true));
        assert_eq!(c[32], Cmd::VerifyAudio(440.0, 880.0));
        assert_eq!(c[33], Cmd::MicStatsStart);
        assert_eq!(c[34], Cmd::MicStatsReport);
        assert_eq!(c[35], Cmd::WaitMic(true, 5000));
        assert_eq!(c[36], Cmd::WaitMic(false, 1500));
        assert_eq!(c[37], Cmd::AtomicStart);
        assert_eq!(c[38], Cmd::AtomicReport);
        assert_eq!(c[39], Cmd::VerifyAtomic(None));
        assert_eq!(c[40], Cmd::VerifyAtomic(Some(3)));
        assert_eq!(c[41], Cmd::Quit);
        assert_eq!(c.len(), 42);
    }

    #[test]
    fn erros_dizem_a_linha() {
        let e = parse_script("wait 1\nclick esquerdo\n").unwrap_err();
        assert!(e.starts_with("linha 2:"), "{e}");
        assert!(parse_script("type \"abc").is_err());
        assert!(parse_script("key hyper+a").is_err());
        assert!(parse_script("win 0").is_err());
        assert!(parse_script("fazer").is_err());
        assert!(parse_script("verify-video").is_err());
        assert!(parse_script("verify-video abc").is_err());
        assert!(parse_script("verify-video -1").is_err());
        assert!(parse_script("verify-still 1 2 3").is_err());
        assert!(parse_script("waitvideostart").is_err());
        assert!(parse_script("verify-audio 440").is_err());
        assert!(parse_script("verify-audio 440 5").is_err());
        assert!(parse_script("audiostats zerar").is_err());
        assert!(parse_script("waitaudio").is_err());
        assert!(parse_script("atomic").is_err());
        assert!(parse_script("atomic parar").is_err());
        assert!(parse_script("verify-atomic 1 2").is_err());
        assert!(parse_script("micstats").is_err());
        assert!(parse_script("micstats parar").is_err());
        assert!(parse_script("waitmic").is_err());
        assert!(parse_script("waitmic on").is_err());
        assert!(parse_script("waitmic talvez 100").is_err());
    }

    #[test]
    fn combinacoes() {
        assert_eq!(parse_chord("ctrl+c").unwrap(), Chord { mods: vec![29], key: 46 });
        assert_eq!(parse_chord("altgr+q").unwrap(), Chord { mods: vec![100], key: 16 });
        assert_eq!(parse_chord("Enter").unwrap(), Chord { mods: vec![], key: 28 });
        assert_eq!(parse_chord("f10").unwrap().key, 68);
        assert_eq!(parse_chord("ç").unwrap().key, 39);
        assert!(parse_chord("ctrl+").is_err());
    }

    #[test]
    fn atalhos_antigos_viram_roteiro() {
        let c = legacy_script(Some("echo".into()), Some((800, 500)));
        assert_eq!(c, vec![Cmd::Wait(1500), Cmd::Resize(800, 500), Cmd::Wait(500), Cmd::Type("echo".into())]);
        assert_eq!(legacy_script(Some("x".into()), None), vec![Cmd::Wait(2000), Cmd::Type("x".into())]);
        assert!(legacy_script(None, None).is_empty());
    }

    fn janela(id: u32, parent: u32, popup: bool, x: i32, y: i32, w: u32, h: u32) -> WinInfo {
        WinInfo { num: 0, id, parent, popup, grab: popup, x, y, title: String::new(), shared: crate::net::SharedWindow::new(w, h), alive: true }
    }

    #[test]
    fn composicao_coloca_o_popup_onde_o_cliente_o_mostra() {
        let raiz = janela(1, 0, false, 0, 0, 100, 80);
        // Sem popup: nada a compor.
        assert!(compose_with_popups(&raiz, std::slice::from_ref(&raiz)).is_none());
        // Popup que passa da borda direita e de baixo: a tela de composição cresce.
        let menu = janela(2, 1, true, 50, 60, 100, 80);
        let (w, h, px) = compose_with_popups(&raiz, &[raiz.clone(), menu.clone()]).unwrap();
        assert_eq!((w, h), (150, 140));
        assert_eq!(px.len(), 150 * 140 * 4);
        // Submenu relativo ao menu, com posição negativa: o canto da tela de composição se desloca.
        let sub = janela(3, 2, true, -70, -30, 40, 40);
        let (w, h, _) = compose_with_popups(&raiz, &[raiz.clone(), menu, sub]).unwrap();
        // Submenu em (50 - 70, 60 - 30) = (-20, 30): de x = -20 a 150 e de y = 0 a 140.
        assert_eq!((w, h), (170, 140));
    }

    #[test]
    fn colagem_recorta_nas_bordas() {
        // Destino 3x2 preto; origem 2x2 com B=1..4 em (-1, 1): só o pixel (1,0) da origem cai em (0,1).
        let mut dst = vec![0u8; 3 * 2 * 4];
        let src: Vec<u8> = (1..=4u8).flat_map(|b| [b, 0, 0, 0xFF]).collect();
        paste_bgrx(&mut dst, 3, 2, &src, 2, 2, -1, 1);
        assert_eq!(&dst[12..13], &[2]);
        assert_eq!(dst.iter().filter(|b| **b != 0).count(), 2); // um pixel: B=2 e X=0xFF
    }
}
