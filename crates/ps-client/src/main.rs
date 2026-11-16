
use std::path::PathBuf;
use std::process::ExitCode;

use std::sync::atomic::Ordering;

use ps_client::script::{legacy_script, parse_script, SCRIPT_FAILED};
use ps_client::discovery;
use ps_client::secure::{self, SecurityOptions};
use ps_client::{run_connected, ExitReason, ForcedFocus, RunError, RunOptions};

fn main() -> ExitCode {
    let mut addr = None;
    let mut focus = None;
    let mut type_text = None;
    let mut resize_to = None;
    let mut script_file: Option<String> = None;
    let mut shot_dir: Option<PathBuf> = None;
    let mut no_video = false;
    let mut no_atomic = false;
    let mut audio = ps_client::audio::AudioOptions::default();
    let mut mic = ps_client::mic::MicOptions::default();
    let mut list = false;
    let mut insecure = false;
    let mut force_noise = false;
    let mut client_name: Option<String> = None;
    let mut forget: Option<String> = None;
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "-h" | "--help" => return uso(),
            "--focus" => {
                focus = match args.next().as_deref() {
                    Some("first") => Some(ForcedFocus::First),
                    Some(n) => match n.parse::<u32>() {
                        Ok(id) => Some(ForcedFocus::Win(id)),
                        Err(_) => return uso(),
                    },
                    None => return uso(),
                }
            }
            "--no-video" => no_video = true,
            "--no-atomic" => no_atomic = true,
            "--no-audio" => audio.enabled = false,
            "--audio-null" => audio.null_output = true,
            "--audio-dump" => match args.next() {
                Some(f) => audio.dump = Some(PathBuf::from(f)),
                None => return uso(),
            },
            "--audio-target" => match args.next().and_then(|v| v.parse::<u32>().ok()).filter(|v| (10..=500).contains(v)) {
                Some(v) => audio.target_ms = Some(v),
                None => return uso(),
            },
            "--audio-volume" => match args.next().and_then(|v| v.parse::<f32>().ok()).filter(|v| (0.0..=1.0).contains(v)) {
                Some(v) => audio.volume = v,
                None => return uso(),
            },
            "--mic" => mic.enabled = true,
            "--mic-file" => match args.next() {
                Some(f) => mic.file = Some(PathBuf::from(f)),
                None => return uso(),
            },
            "--list" => list = true,
            "--insecure" => insecure = true,
            "--secure" => force_noise = true,
            "--name" => match args.next().filter(|n| !n.trim().is_empty()) {
                Some(n) => client_name = Some(n),
                None => return uso(),
            },
            "--forget" => match args.next().filter(|n| !n.trim().is_empty()) {
                Some(n) => forget = Some(n),
                None => return uso(),
            },
            "--type" => type_text = args.next(),
            "--resize" => {
                resize_to = args.next().and_then(|v| {
                    let (l, a) = v.split_once('x')?;
                    Some((l.parse().ok()?, a.parse().ok()?))
                });
                if resize_to.is_none() {
                    return uso();
                }
            }
            "--script" => match args.next() {
                Some(f) => script_file = Some(f),
                None => return uso(),
            },
            "--shot-dir" => match args.next() {
                Some(d) => shot_dir = Some(PathBuf::from(d)),
                None => return uso(),
            },
            outro if addr.is_none() && !outro.starts_with('-') => addr = Some(outro.to_string()),
            _ => return uso(),
        }
    }
    if let Some(sel) = forget {
        if addr.is_some() || list {
            return uso();
        }
        return esquecer(&sel);
    }
    if list {
        if addr.is_some() {
            return uso();
        }
        return listar();
    }

    let mut script = legacy_script(type_text, resize_to);
    if let Some(f) = script_file {
        let texto = match std::fs::read_to_string(&f) {
            Ok(t) => t,
            Err(e) => {
                eprintln!("[psc] não foi possível ler o roteiro {f}: {e}");
                return ExitCode::from(2);
            }
        };
        match parse_script(&texto) {
            Ok(c) => script.extend(c),
            Err(e) => {
                eprintln!("[psc] roteiro {f}: {e}");
                return ExitCode::from(2);
            }
        }
    }
    if let Some(f) = &mic.file {
        if let Err(e) = ps_client::mic::load_wav(f) {
            eprintln!("[psc] --mic-file: {e}");
            return ExitCode::from(2);
        }
    }
    let mut opts = RunOptions { forced_focus: focus, script, video: !no_video, audio, atomic: !no_atomic, mic, ..RunOptions::default() };
    if let Some(d) = shot_dir {
        opts.shot_dir = d;
    }

    let mut seg = SecurityOptions::new(insecure, client_name);
    seg.force_noise = force_noise;
    let resultado = match addr.as_deref() {
        Some(a) if a.contains(':') => match ps_transport::TcpTransport::connect(a) {
            Ok(t) => run_connected(t, a, &seg, opts),
            Err(e) => Err(RunError::Other(format!("não foi possível conectar a {a}: {e}"))),
        },
        nome => match escolher(nome) {
            Ok(f) => match discovery::connect_found(&f) {
                Ok((t, sa)) => {
                    println!("[psc] \"{}\": conectando por {sa}", f.instance);
                    run_connected(t, &sa.to_string(), &seg, opts)
                }
                Err(e) => Err(RunError::Other(e)),
            },
            Err(codigo) => return codigo,
        },
    };
    let reprovado = SCRIPT_FAILED.load(Ordering::SeqCst);
    if reprovado {
        eprintln!("[psc] o roteiro teve verificações reprovadas (veja as linhas VERIFY ... FALHOU).");
    }
    let sucesso = if reprovado { ExitCode::from(3) } else { ExitCode::SUCCESS };
    match resultado {
        Ok(ExitReason::LastWindowClosed) => {
            println!("[psc] a última janela foi fechada; encerrando.");
            sucesso
        }
        Ok(ExitReason::ScriptQuit) => {
            println!("[psc] roteiro encerrou o cliente.");
            sucesso
        }
        Ok(ExitReason::Goodbye { reason, text }) => {
            eprintln!("[psc] sessão encerrada pelo aparelho: {}.", ps_client::net::goodbye_text(reason, &text));
            ExitCode::from(match reason {
                ps_proto::GOODBYE_REPLACED => 6,
                ps_proto::GOODBYE_SHUTDOWN => 7,
                _ => 1,
            })
        }
        Ok(ExitReason::Disconnected(motivo)) => {
            eprintln!("[psc] conexão encerrada: {motivo}.");
            ExitCode::from(1)
        }
        Err(RunError::Connect(e)) => {
            eprintln!("[psc] {e}");
            ExitCode::from(e.exit_code())
        }
        Err(e) => {
            eprintln!("[psc] erro: {e}");
            ExitCode::from(1)
        }
    }
}

fn esquecer(seletor: &str) -> ExitCode {
    let dir = secure::keys_dir();
    match secure::forget(&dir, seletor) {
        Ok(0) => {
            eprintln!("[psc] nenhum aparelho conhecido casa com \"{seletor}\". Conhecidos:");
            for (nome, fp, _) in secure::known_devices(&dir).unwrap_or_default() {
                eprintln!("  {nome}  {fp}");
            }
            ExitCode::from(1)
        }
        Ok(n) => {
            println!("[psc] {n} aparelho(s) esquecido(s). Na próxima conexão a chave dele será aceita como nova (confira a impressão digital).");
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("[psc] erro ao gravar a lista de aparelhos conhecidos: {e}");
            ExitCode::from(1)
        }
    }
}

fn listar() -> ExitCode {
    let achados = discovery::search();
    if achados.is_empty() {
        eprintln!("[psc] nenhum aparelho PiStick encontrado na rede.");
        return ExitCode::from(1);
    }
    for f in &achados {
        println!("{}", discovery::format_line(f));
    }
    ExitCode::SUCCESS
}

fn escolher(nome: Option<&str>) -> Result<ps_discovery::Found, ExitCode> {
    println!("[psc] procurando aparelhos PiStick na rede...");
    let achados = discovery::search();
    let lista = |achados: &[ps_discovery::Found]| achados.iter().map(|f| format!("  {}", discovery::format_line(f))).collect::<Vec<_>>().join("
");
    match nome {
        Some(n) => match achados.iter().find(|f| f.instance.eq_ignore_ascii_case(n)) {
            Some(f) => Ok(f.clone()),
            None if achados.is_empty() => {
                eprintln!("[psc] não achei \"{n}\" na rede (nenhum aparelho respondeu). {DICA}");
                Err(ExitCode::from(1))
            }
            None => {
                eprintln!("[psc] não achei \"{n}\" na rede. Aparelhos encontrados:
{}", lista(&achados));
                Err(ExitCode::from(1))
            }
        },
        None => match achados.len() {
            0 => {
                eprintln!("[psc] nenhum aparelho PiStick encontrado na rede. {DICA}");
                Err(ExitCode::from(1))
            }
            1 => Ok(achados[0].clone()),
            _ => {
                eprintln!("[psc] mais de um aparelho encontrado; escolha um com `psc <nome>`:
{}", lista(&achados));
                Err(ExitCode::from(1))
            }
        },
    }
}

const DICA: &str = "Confira se o notebook e o Pi estão na mesma rede, se o psd está rodando (sem --no-announce) e se o firewall do Windows deixa passar a resposta UDP; `psc <endereço:porta>` conecta sem procurar.";

fn uso() -> ExitCode {
    eprintln!(
        "uso: psc [endereço:porta | nome] [--list] [--script ARQUIVO] [--shot-dir PASTA] [--focus first|ID] [--type TEXTO] [--resize LxA] [--no-video] [--no-atomic] [--no-audio] [--insecure] [--name NOME] [--forget NOME] [--audio-dump ARQ.wav] [--audio-null] [--audio-volume 0..1] [--audio-target MS] [--mic] [--mic-file ARQ.wav]\n\n  sem endereço  procura o psd na rede por mDNS e conecta (se houver só um aparelho)\n  nome          (sem `:`) procura por mDNS o aparelho com esse nome de instância e conecta\n  --list        procura e lista os aparelhos (nome  endereço:porta  v=N  sec=1  pair=open|closed); código 1 se não achar nenhum
  --insecure    texto claro mesmo fora do laço local (só desenvolvimento; o psd também precisa de --insecure)
  --name NOME   nome deste computador anunciado ao aparelho (padrão: nome do computador)
  --forget NOME|IMPRESSÃO  esquece um aparelho conhecido (use depois de trocar o aparelho de propósito)
  Fora do laço local o psc fala o canal cifrado (Noise XX) e pareia na primeira vez; chaves em %APPDATA%\\PiStick.
  Saída 4: o aparelho recusou este computador (falta parear: `psd pair` no aparelho). Saída 5: chave do aparelho diferente da guardada. Saída 6: outro computador assumiu a sessão (o psc não reconecta sozinho). Saída 7: o aparelho está desligando.\n  --script    roteiro de entrada: um comando por linha, executado depois que a primeira janela aparece\n              (wait, win, move, click, scroll, type, key, hold, clip, shot, resize, waitvideostart,\n              waitvideostop, verify-video, verify-still, atomic, verify-atomic, waitaudio, audiostats, verify-audio, micstats, waitmic, quit; veja tools/roteiros; verificação reprovada = código 3)\n  --shot-dir  pasta dos PNG do comando `shot` (padrão .cache\\shot)\n  --focus     anuncia ao servidor que essa janela está com foco (a primeira, ou a de id ID),\n              ignorando o foco real do sistema; serve para medições\n  --no-atomic  não anuncia a apresentação atômica de quadros fatiados: cada fatia é apresentada assim que chega (o comportamento anterior; serve para provar o defeito)
  --no-audio  não anuncia áudio (o servidor não envia o som do Pi)\n  --audio-dump ARQ.wav  grava o áudio decodificado (som e silêncio na linha do tempo do servidor); nada toca no computador\n  --audio-null  a fila de áudio anda em tempo real, mas a saída é descartada (nada toca)\n  --audio-target MS  alvo da fila de áudio (padrão 40; de 10 a 500; o mínimo é metade e o máximo o triplo)\n  --audio-volume V  volume do fluxo do psc no Windows, de 0 a 1 (padrão 1)\n  --mic  oferece o microfone deste computador ao aparelho: só captura (e o Windows só mostra o indicador de microfone) enquanto um app do Pi grava da fonte pistick-mic; exige conexão cifrada e pareada ou laço local\n  --mic-file ARQ.wav  como --mic, mas manda o WAV (PCM 16 bits, 48 kHz) em laço no lugar do dispositivo; nenhum dispositivo de captura é aberto (testes)\n  --type      atalho de roteiro: digita TEXTO na primeira janela (\\n = Enter), como teclado ABNT2\n  --resize    atalho de roteiro: pede à primeira janela o tamanho LxA\n\nexemplo: psc 127.0.0.1:7700"
    );
    ExitCode::from(2)
}
