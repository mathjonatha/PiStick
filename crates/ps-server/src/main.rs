
use std::process::ExitCode;

use ps_server::secure::serve_secure;
use ps_server::ServerOptions;
use ps_transport::TcpAcceptor;

const AJUDA: &str = "\
psd — daemon do PiStick

USO:
    psd pair [--list | --remove NOME|IMPRESSÃO | --reset] [--socket CAMINHO]   (veja `psd pair --help`)
    psd verify-wav ARQUIVO.wav FREQ_ESQ FREQ_DIR [AMPLITUDE]   (confere uma gravação; veja abaixo)
    psd --synthetic [--listen ENDEREÇO] [--fps N]
    psd --synthetic-video [--synthetic-video-pause-after SEGUNDOS] [--listen ENDEREÇO]
    psd --synthetic --synthetic-stripes [--max-rate KIB] [--listen ENDEREÇO]
    psd --synthetic --synthetic-audio [--audio-codec psa1|pcm] [--listen ENDEREÇO]
    psd --wayland [--listen ENDEREÇO] [--spawn COMANDO]...

OPÇÕES:
    verify-wav          confere uma gravação (por exemplo, do pw-record da fonte pistick-mic) contra dois tons: a frequência
                        FREQ_ESQ no canal esquerdo e FREQ_DIR no direito, com amplitude (fração da escala cheia, padrão
                        0,25) dentro de 5%, sem vazamento nem buracos; sai com 0 se passa, 1 se falha, 2 se o arquivo é inválido
    --synthetic         duas janelas de teste animadas
    --synthetic-video   uma janela de teste de 1280x720 com uma região animada de 640x384 (vídeo, marco 4)
    --synthetic-video-pause-after SEGUNDOS
                        (com --synthetic-video) a animação para depois deste tempo
    --video auto|off    (Linux) vídeo por região em H.264 pelo codificador do Pi, /dev/video11 (padrão auto:
                        usa se existir); off mantém tudo no codec de blocos
    --video-bitrate BITS taxa de bits do H.264 (padrão 5000000)
    --video-max-qp N    teto de QP do H.264, de 1 a 51 (padrão 36; 51 = sem teto; menor = mais qualidade e mais bits)
    --synthetic-audio   (com --synthetic) áudio de teste sem PipeWire: 440 Hz à esquerda, 880 Hz à direita,
                        1 s de som e 1 s de silêncio (marco 7)
    --audio auto|off    (com --wayland) saída de áudio do Pi para o hospedeiro: auto sobe o PipeWire do projeto
                        (pipewire, wireplumber e pipewire-pulse, configuração em /etc/pistick/pipewire ou em
                        tools/pi/pipewire) e envia o som dos apps; sem os programas, segue sem áudio (padrão auto)
    --audio-codec C     psa1 (sem perdas, padrão) ou pcm (cru, 1,5 Mbit/s)
    --audio-addr ENDEREÇO  soquete de PCM cru do PipeWire (padrão 127.0.0.1:4711)
    --mic auto|off      (com --wayland e áudio ligado) microfone do hospedeiro para os apps do Pi (marco 7B): auto anuncia a
                        capacidade e, só quando um app do Pi grava da fonte pistick-mic, pede o microfone ao cliente
                        (`psc --mic`); off nunca pede (padrão auto)
    --mic-max-ms MS     atraso máximo da fila do microfone no servidor, de 20 a 300 (padrão 150): o alvo da fila segue o jitter da
                        rede entre 20 ms e esse valor, e começa nele; menos atraso, mais falhas se o Wi-Fi atrasar pacotes
    --mic-addr ENDEREÇO soquete de reprodução do PipeWire que alimenta a fonte pistick-mic (padrão 127.0.0.1:4712)
    --wayland           compositor Wayland sem tela (só Linux): apps aparecem como janelas no cliente
    --spawn COMANDO     (com --wayland) inicia COMANDO com WAYLAND_DISPLAY apontando para o compositor;
                        pode ser repetido para iniciar vários apps (cada janela é independente no cliente)
    --listen ENDEREÇO   onde escutar (padrão 127.0.0.1:7700)
    --no-announce       (Linux) não anuncia o serviço _pistick._tcp por mDNS (o anúncio é o padrão quando
                        --listen não é um endereço de laço)
    --name NOME         nome da instância anunciada (padrão: nome da máquina)
    --synthetic-stripes (com --synthetic) uma janela de 960x600 de ruído que não comprime, com um marcador de cor sólida
                        (número do quadro) no começo de cada faixa de 32 linhas, a 30 quadros por segundo; serve
                        para provar a apresentação atômica dos quadros fatiados (psc: `atomic start` / `atomic report`)
    --max-rate KIB      (só para teste) limita a taxa dos quadros de imagem a KIB KiB/s e fixa o orçamento do
                        controle de fluxo nesse valor (sem adaptação), para forçar quadros fatiados
    --fps N             taxa do conteúdo sintético (padrão 30)
    --ui-size LxA       tamanho inicial da janela de interface (padrão 640x420)
    --gradient-size LxA tamanho inicial da janela de degradê (padrão 640x400)
    --insecure          (só desenvolvimento) aceita conexões em texto claro de qualquer origem. Sem esta opção o psd só
                        aceita texto claro de 127.0.0.1/::1; o resto precisa do canal cifrado Noise e de pareamento
    --state-dir PASTA   onde ficam a chave do aparelho e a lista de pareados (padrão: $STATE_DIRECTORY, senão
                        $XDG_STATE_HOME/pistick, senão ~/.local/state/pistick)
    --ctl-socket CAMINHO soquete de controle do `psd pair` (padrão: $XDG_RUNTIME_DIR/pistick-ctl)
    -h, --help          mostra esta ajuda
";

fn main() -> ExitCode {
    if std::env::args().nth(1).as_deref() == Some("verify-wav") {
        let resto: Vec<String> = std::env::args().skip(2).collect();
        return verify_wav(&resto);
    }
    if std::env::args().nth(1).as_deref() == Some("pair") {
        #[cfg(unix)]
        {
            let resto: Vec<String> = std::env::args().skip(2).collect();
            return ps_server::ctl::run_cli(&resto);
        }
        #[cfg(not(unix))]
        return erro("`psd pair` só existe no Linux");
    }
    let mut insecure = false;
    let mut state_dir: Option<std::path::PathBuf> = None;
    let mut ctl_socket: Option<std::path::PathBuf> = None;
    let mut synthetic = false;
    let mut wayland = false;
    let mut synthetic_video = false;
    let mut synthetic_stripes = false;
    let mut video_auto = true;
    let mut pause_after: Option<std::time::Duration> = None;
    let mut spawn: Vec<String> = Vec::new();
    let mut listen = "127.0.0.1:7700".to_string();
    let mut synthetic_audio = false;
    let mut audio_auto = true;
    let mut audio_codec = ps_server::audio::AudioCodec::Psa1;
    let mut audio_addr = ps_server::audio::PIPEWIRE_PCM_ADDR.to_string();
    let mut mic_auto = true;
    let mut mic_addr = ps_server::mic::PIPEWIRE_MIC_ADDR.to_string();
    let mut mic_max_ms = ps_server::mic::QUEUE_MAX_MS;
    let mut announce = true;
    let mut announce_name: Option<String> = None;
    let mut opts = ServerOptions::default();

    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--synthetic" => synthetic = true,
            "--wayland" => wayland = true,
            "--synthetic-video" => synthetic_video = true,
            "--video" => match args.next().as_deref() {
                Some("auto") => video_auto = true,
                Some("off") => video_auto = false,
                _ => return erro("--video precisa de auto ou off"),
            },
            "--video-bitrate" => match args.next().and_then(|v| v.parse::<u32>().ok()).filter(|n| (25_000..=25_000_000).contains(n)) {
                Some(n) => opts.video_bitrate = n,
                None => return erro("--video-bitrate precisa de um número de bits por segundo entre 25000 e 25000000"),
            },
            "--video-max-qp" => match args.next().and_then(|v| v.parse::<u32>().ok()).filter(|n| (1..=51).contains(n)) {
                Some(n) => opts.video_max_qp = n,
                None => return erro("--video-max-qp precisa de um número de 1 a 51"),
            },
            "--synthetic-stripes" => synthetic_stripes = true,
            "--max-rate" => match args.next().and_then(|v| v.parse::<f64>().ok()).filter(|n| n.is_finite() && *n >= 16.0) {
                Some(kib) => {
                    opts.max_rate = Some(kib * 1024.0);
                    opts.flow_initial_throughput = kib * 1024.0;
                    opts.flow_adaptive = false;
                }
                None => return erro("--max-rate precisa de um número de KiB por segundo (mínimo 16)"),
            },
            "--synthetic-video-pause-after" => match args.next().and_then(|v| v.parse::<f64>().ok()).filter(|s| s.is_finite() && *s >= 0.0) {
                Some(s) => pause_after = Some(std::time::Duration::from_secs_f64(s)),
                None => return erro("--synthetic-video-pause-after precisa de um número de segundos"),
            },
            "--spawn" => match args.next() {
                Some(v) => spawn.push(v),
                None => return erro("--spawn precisa de um comando"),
            },
            "--synthetic-audio" => synthetic_audio = true,
            "--audio" => match args.next().as_deref() {
                Some("auto") => audio_auto = true,
                Some("off") => audio_auto = false,
                _ => return erro("--audio precisa de auto ou off"),
            },
            "--audio-codec" => match args.next().as_deref() {
                Some("psa1") => audio_codec = ps_server::audio::AudioCodec::Psa1,
                Some("pcm") => audio_codec = ps_server::audio::AudioCodec::Pcm,
                _ => return erro("--audio-codec precisa de psa1 ou pcm"),
            },
            "--mic" => match args.next().as_deref() {
                Some("auto") => mic_auto = true,
                Some("off") => mic_auto = false,
                _ => return erro("--mic precisa de auto ou off"),
            },
            "--mic-max-ms" => match args.next().and_then(|v| v.parse::<usize>().ok()).filter(|v| (20..=300).contains(v)) {
                Some(v) => mic_max_ms = v,
                None => return erro("--mic-max-ms precisa de um número entre 20 e 300"),
            },
            "--mic-addr" => match args.next().filter(|a| a.contains(':')) {
                Some(a) => mic_addr = a,
                None => return erro("--mic-addr precisa de ENDEREÇO:PORTA"),
            },
            "--audio-addr" => match args.next().filter(|a| a.contains(':')) {
                Some(a) => audio_addr = a,
                None => return erro("--audio-addr precisa de ENDEREÇO:PORTA"),
            },
            "--no-announce" => announce = false,
            "--insecure" => insecure = true,
            "--state-dir" => match args.next() {
                Some(d) if !d.is_empty() => state_dir = Some(std::path::PathBuf::from(d)),
                _ => return erro("--state-dir precisa de uma pasta"),
            },
            "--ctl-socket" => match args.next() {
                Some(d) if !d.is_empty() => ctl_socket = Some(std::path::PathBuf::from(d)),
                _ => return erro("--ctl-socket precisa de um caminho"),
            },
            "--name" => match args.next().filter(|n| !n.is_empty() && n.len() <= ps_discovery::MAX_LABEL) {
                Some(n) => announce_name = Some(n),
                None => return erro("--name precisa de um nome de 1 a 63 bytes"),
            },
            "--listen" => match args.next() {
                Some(v) => listen = v,
                None => return erro("--listen precisa de um endereço"),
            },
            "--fps" => match args.next().and_then(|v| v.parse::<u32>().ok()).filter(|n| (1..=240).contains(n)) {
                Some(n) => opts.fps = n,
                None => return erro("--fps precisa de um número entre 1 e 240"),
            },
            "--ui-size" | "--gradient-size" => {
                let tam = args.next().and_then(|v| {
                    let (l, a) = v.split_once('x')?;
                    Some((l.parse::<u32>().ok()?, a.parse::<u32>().ok()?))
                });
                match tam {
                    Some(t) if (64..=4096).contains(&t.0) && (64..=4096).contains(&t.1) => {
                        if a == "--ui-size" {
                            opts.ui_size = t
                        } else {
                            opts.gradient_size = t
                        }
                    }
                    _ => return erro(&format!("{a} precisa de LARGURAxALTURA entre 64 e 4096")),
                }
            }
            "-h" | "--help" => {
                print!("{AJUDA}");
                return ExitCode::SUCCESS;
            }
            outro => return erro(&format!("argumento desconhecido: {outro}")),
        }
    }
    if pause_after.is_some() && !synthetic_video {
        return erro("--synthetic-video-pause-after só vale com --synthetic-video");
    }
    if synthetic_video {
        synthetic = true;
        opts.synthetic_video = Some(ps_server::SyntheticVideo { pause_after });
    }
    if synthetic_stripes && !synthetic {
        return erro("--synthetic-stripes só vale com --synthetic");
    }
    opts.synthetic_stripes = synthetic_stripes;
    if synthetic == wayland {
        return erro("escolha exatamente um modo: --synthetic ou --wayland");
    }
    if synthetic_audio && !synthetic {
        return erro("--synthetic-audio só vale com --synthetic");
    }
    if !spawn.is_empty() && !wayland {
        return erro("--spawn só vale com --wayland");
    }

    #[cfg(target_os = "linux")]
    if video_auto {
        use ps_server::video::v4l2::{V4l2Factory, DEFAULT_DEVICE};
        if V4l2Factory::default_available() {
            println!("[psd] codificador H.264: {DEFAULT_DEVICE}, {} bit/s, QP máximo {}", opts.video_bitrate, opts.video_max_qp);
            opts.video = Some(std::sync::Arc::new(V4l2Factory::default()));
        } else {
            println!("[psd] {DEFAULT_DEVICE} não existe: sem vídeo H.264, tudo no codec de blocos");
        }
    }
    #[cfg(not(target_os = "linux"))]
    let _ = video_auto;

    let mut audio_threads = Vec::new();
    if synthetic_audio {
        let hub = ps_server::audio::AudioHub::new(audio_codec);
        audio_threads.push(ps_server::audio::spawn_synthetic(hub.clone()));
        opts.audio = Some(hub);
        println!("[psd] áudio sintético ({audio_codec:?}): 440 Hz à esquerda, 880 Hz à direita, 1 s de som e 1 s de silêncio");
    }
    let audio_cfg = AudioCfg { auto: audio_auto, codec: audio_codec, addr: audio_addr, mic: mic_auto, mic_addr, mic_max_ms };

    let acceptor = match TcpAcceptor::bind(&listen) {
        Ok(a) => a,
        Err(e) => return erro(&format!("não foi possível escutar em {listen}: {e}")),
    };
    if insecure {
        println!("[psd] AVISO: --insecure aceita conexões em texto claro de qualquer origem (só para desenvolvimento)");
    }
    let Some(dir) = state_dir.or_else(ps_server::secure::default_state_dir) else {
        return erro("não achei onde guardar a chave do aparelho (defina HOME ou use --state-dir PASTA)");
    };
    let device_name = announce_name.clone().unwrap_or_else(device_name);
    let sec = match ps_server::secure::Security::open(&dir, &device_name, insecure, |m| println!("[psd] {m}")) {
        Ok(s) => s,
        Err(e) => return erro(&format!("não foi possível abrir {}: {e}", dir.display())),
    };
    #[cfg(target_os = "linux")]
    {
        let s = sec.clone();
        ps_server::discovery::set_shutdown_hook(move || {
            s.shutdown_session();
        });
    }
    #[cfg(unix)]
    match ctl_socket.or_else(ps_server::ctl::default_socket_path) {
        Some(p) => match ps_server::ctl::serve(sec.clone(), &p) {
            Ok(()) => println!("[psd] soquete de controle (psd pair): {}", p.display()),
            Err(e) => eprintln!("[psd] aviso: sem soquete de controle em {}: {e} (o `psd pair` não vai funcionar)", p.display()),
        },
        None => eprintln!("[psd] aviso: sem XDG_RUNTIME_DIR; sem soquete de controle (use --ctl-socket CAMINHO)"),
    }
    #[cfg(not(unix))]
    let _ = ctl_socket;
    #[cfg(target_os = "linux")]
    let _announcer = start_announcer(&acceptor, announce, announce_name, &sec);
    #[cfg(not(target_os = "linux"))]
    let _ = (announce, announce_name);
    if wayland {
        return run_wayland(&acceptor, &opts, &spawn, &audio_cfg, sec);
    }
    let make = || match opts.synthetic_video {
        Some(sv) => Box::new(ps_server::synthetic::SyntheticSource::video(sv.pause_after.map(|d| d.as_millis() as u64))) as Box<dyn ps_server::WindowSource>,
        None if opts.synthetic_stripes => Box::new(ps_server::synthetic::SyntheticSource::stripes()) as Box<dyn ps_server::WindowSource>,
        None => Box::new(ps_server::synthetic::SyntheticSource::new(opts.fps, opts.ui_size, opts.gradient_size)) as Box<dyn ps_server::WindowSource>,
    };
    match serve_secure(&acceptor, &opts, sec, None, make) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => erro(&format!("erro ao aceitar conexões: {e}")),
    }
}

fn device_name() -> String {
    std::fs::read_to_string("/proc/sys/kernel/hostname")
        .ok()
        .map(|h| h.trim().split('.').next().unwrap_or("").to_string())
        .filter(|h| !h.is_empty())
        .or_else(|| std::env::var("COMPUTERNAME").ok().filter(|h| !h.is_empty()))
        .unwrap_or_else(|| "pistick".to_string())
}

#[cfg(target_os = "linux")]
fn start_announcer(acceptor: &TcpAcceptor, announce: bool, name: Option<String>, sec: &std::sync::Arc<ps_server::secure::Security>) -> Option<ps_server::discovery::Announcer> {
    use ps_server::discovery::{install_term_handler, Announcer, Config, PairFlag};
    if !announce {
        return None;
    }
    let local = acceptor.local_addr().ok()?;
    if local.ip().is_loopback() {
        return None;
    }
    let mut cfg = Config::new(name, local.port());
    let s = sec.clone();
    cfg.pair = Some(PairFlag(std::sync::Arc::new(move || s.first_use_open())));
    match Announcer::start(cfg) {
        Ok(a) => {
            install_term_handler();
            println!("[psd] anunciando _pistick._tcp.local por mDNS (porta {})", local.port());
            Some(a)
        }
        Err(e) => {
            eprintln!("[psd] mDNS: o anúncio não foi iniciado: {e}");
            None
        }
    }
}

#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
struct AudioCfg {
    auto: bool,
    codec: ps_server::audio::AudioCodec,
    addr: String,
    mic: bool,
    mic_addr: String,
    mic_max_ms: usize,
}

fn verify_wav(args: &[String]) -> ExitCode {
    use ps_audio::wavcheck::{parse_wav, verify_tones};
    let (Some(path), Some(fe), Some(fd)) = (args.first(), args.get(1).and_then(|a| a.parse::<f64>().ok()), args.get(2).and_then(|a| a.parse::<f64>().ok())) else {
        eprintln!("uso: psd verify-wav ARQUIVO.wav FREQ_ESQ FREQ_DIR [AMPLITUDE]");
        return ExitCode::from(2);
    };
    let amp = match args.get(3) {
        Some(a) => match a.parse::<f64>() {
            Ok(v) if v > 0.0 && v <= 1.0 => v,
            _ => {
                eprintln!("psd verify-wav: AMPLITUDE precisa estar entre 0 e 1");
                return ExitCode::from(2);
            }
        },
        None => 0.25,
    };
    let bytes = match std::fs::read(path) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("psd verify-wav: não foi possível ler {path}: {e}");
            return ExitCode::from(2);
        }
    };
    let wav = match parse_wav(&bytes) {
        Ok(w) => w,
        Err(e) => {
            eprintln!("psd verify-wav: {path}: {e}");
            return ExitCode::from(2);
        }
    };
    let r = verify_tones(&wav, fe, fd, amp);
    println!(
        "VERIFY wav {} {path}: {:.2} s, janela {:.2} s a {:.2} s | esq[{fe}]={:.4} dir[{fd}]={:.4} (mediana dos blocos de 100 ms; coerente {:.4}/{:.4}) vazamentos={:.4}/{:.4} | blocos sem o tom: {} de {}, abaixo de 90%: {}",
        if r.ok() { "ok" } else { "FALHOU" },
        r.seconds,
        r.window.0,
        r.window.1,
        r.left,
        r.right,
        r.coherent.0,
        r.coherent.1,
        r.leak_left,
        r.leak_right,
        r.holes,
        r.blocks,
        r.weak
    );
    for p in &r.problems {
        println!("  motivo: {p}");
    }
    if r.ok() {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(1)
    }
}

fn erro(msg: &str) -> ExitCode {
    eprintln!("psd: {msg}\n\n{AJUDA}");
    ExitCode::from(2)
}

#[cfg(target_os = "linux")]
fn run_wayland(acceptor: &TcpAcceptor, opts: &ServerOptions, spawn: &[String], audio: &AudioCfg, sec: std::sync::Arc<ps_server::secure::Security>) -> ExitCode {
    use ps_server::wayland::{SocketSpec, WaylandCompositor};

    ps_server::discovery::install_term_handler();
    let mut opts = opts.clone();
    opts.name = format!("psd {} (wayland)", env!("CARGO_PKG_VERSION"));
    let comp = match WaylandCompositor::start(SocketSpec::Auto("pistick".into())) {
        Ok(c) => c,
        Err(e) => return erro(&format!("não foi possível criar o socket Wayland: {e}")),
    };
    println!("[psd] socket Wayland: WAYLAND_DISPLAY={} (em {})", comp.socket_name, comp.socket_path.display());
    let (audio_hub, _pipewire) = start_audio(audio);
    opts.audio = audio_hub.clone();
    if audio_hub.is_some() && audio.mic {
        let log: ps_server::audio::LogFn = std::sync::Arc::new(|m: &str| println!("[psd] {m}"));
        opts.mic = Some(ps_server::mic::MicHub::with_max_ms(ps_server::mic::pw_link_feed(), &audio.mic_addr, log, audio.mic_max_ms));
        println!("[psd] microfone: anunciado (CAP_MIC); só é pedido ao cliente quando um app grava de pistick-mic (PCM em {})", audio.mic_addr);
    } else if audio_hub.is_some() {
        println!("[psd] microfone desligado (--mic off)");
    }
    let mut children: Vec<std::process::Child> = Vec::new();
    for cmd in spawn {
        println!("[psd] iniciando: {cmd}");
        let r = std::process::Command::new("sh")
            .arg("-c")
            .arg(cmd)
            .env("WAYLAND_DISPLAY", &comp.socket_name)
            .env("XDG_RUNTIME_DIR", &comp.socket_path)
            .spawn();
        match r {
            Ok(c) => {
                ps_server::discovery::register_child(c.id());
                children.push(c);
            }
            Err(e) => eprintln!("psd: não foi possível iniciar \"{cmd}\": {e}"),
        }
    }
    let r = serve_secure(acceptor, &opts, sec, None, || comp.source());
    for c in &mut children {
        let _ = c.kill();
        let _ = c.wait();
    }
    comp.shutdown();
    if let Some(h) = &audio_hub {
        h.stop();
    }
    match r {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => erro(&format!("erro ao aceitar conexões: {e}")),
    }
}

#[cfg(target_os = "linux")]
fn start_audio(cfg: &AudioCfg) -> (Option<std::sync::Arc<ps_server::audio::AudioHub>>, Option<ps_server::audio::pipewire::PipewireProcs>) {
    use ps_server::audio::{pipewire, AudioHub, LogFn};
    if !cfg.auto {
        println!("[psd] áudio desligado (--audio off)");
        return (None, None);
    }
    let log: LogFn = std::sync::Arc::new(|m: &str| println!("[psd] {m}"));
    let Some(dir) = pipewire::find_config_dir() else {
        println!("[psd] sem áudio: configuração do PipeWire não encontrada ({} nem {} relativo ao diretório atual)", pipewire::INSTALLED_CONFIG_DIR, pipewire::DEV_CONFIG_DIR);
        return (None, None);
    };
    let procs = match pipewire::start(&cfg.addr, &dir, &log) {
        Ok(pipewire::Started::Procs(p)) => Some(p),
        Ok(pipewire::Started::AlreadyRunning) => None,
        Err(e) => {
            println!("[psd] sem áudio: {e}");
            return (None, None);
        }
    };
    let hub = AudioHub::new(cfg.codec);
    let _ = ps_server::audio::spawn_pcm_reader(hub.clone(), cfg.addr.clone(), log);
    println!("[psd] áudio: {:?}, PCM lido de {}", cfg.codec, cfg.addr);
    (Some(hub), procs)
}

#[cfg(not(target_os = "linux"))]
fn run_wayland(_: &TcpAcceptor, _: &ServerOptions, _: &[String], _: &AudioCfg, _: std::sync::Arc<ps_server::secure::Security>) -> ExitCode {
    erro("o modo --wayland só existe no Linux")
}
