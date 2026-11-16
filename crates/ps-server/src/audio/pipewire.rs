//! Sobe o PipeWire do projeto como processos filhos do `psd`: `pipewire`, `wireplumber` (perfil `policy`,
//! sem hardware) e `pipewire-pulse`, com a configuração de `tools/pi/pipewire/`. Veja `docs/16`.

use std::fs::{File, OpenOptions};
use std::net::{TcpStream, ToSocketAddrs};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use super::LogFn;

/// Onde a configuração é instalada (provisionamento e pacote).
pub const INSTALLED_CONFIG_DIR: &str = "/etc/pistick/pipewire";
/// Onde ela fica no repositório, relativo ao diretório de trabalho (desenvolvimento).
pub const DEV_CONFIG_DIR: &str = "tools/pi/pipewire";

/// Procura a configuração: primeiro a instalada, depois a do repositório. `None` se nenhuma tem
/// `pipewire.conf` e `pipewire-pulse.conf`.
pub fn find_config_dir() -> Option<PathBuf> {
    [Path::new(INSTALLED_CONFIG_DIR).to_path_buf(), PathBuf::from(DEV_CONFIG_DIR)]
        .into_iter()
        .find(|d| d.join("pipewire.conf").is_file() && d.join("pipewire-pulse.conf").is_file())
}

/// Os processos que o `psd` iniciou. Ao ser largado, encerra todos.
pub struct PipewireProcs {
    children: Vec<(&'static str, Child)>,
}

impl Drop for PipewireProcs {
    fn drop(&mut self) {
        for (_, c) in self.children.iter_mut().rev() {
            let _ = c.kill();
            let _ = c.wait();
        }
    }
}

/// Resultado de [`start`].
pub enum Started {
    /// Já havia algo servindo o PCM no endereço (por exemplo, um `psd` anterior morto por sinal): reaproveitado.
    AlreadyRunning,
    Procs(PipewireProcs),
}

fn reachable(addr: &str, timeout: Duration) -> bool {
    addr.to_socket_addrs().ok().and_then(|mut a| a.next()).is_some_and(|sa| TcpStream::connect_timeout(&sa, timeout).is_ok())
}

fn runtime_dir() -> Option<PathBuf> {
    std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from).filter(|p| p.is_dir())
}

fn log_file() -> Option<File> {
    let dir = runtime_dir().unwrap_or_else(std::env::temp_dir);
    OpenOptions::new().create(true).append(true).open(dir.join("pistick-audio.log")).ok()
}

fn spawn(name: &'static str, args: &[&str], log: Option<&File>) -> Result<Child, String> {
    let mut cmd = Command::new(name);
    cmd.args(args)
        .stdin(Stdio::null())
        // Sem barramento de sessão: o PipeWire não tenta falar com o RTKit nem com o D-Bus de um login qualquer.
        .env_remove("DBUS_SESSION_BUS_ADDRESS");
    match log.and_then(|f| Some((f.try_clone().ok()?, f.try_clone().ok()?))) {
        Some((o, e)) => cmd.stdout(Stdio::from(o)).stderr(Stdio::from(e)),
        None => cmd.stdout(Stdio::null()).stderr(Stdio::null()),
    };
    let filho = cmd.spawn();
    // Encerrado com o `psd` quando ele sai por SIGTERM ou SIGINT (veja `discovery::install_term_handler`).
    #[cfg(target_os = "linux")]
    if let Ok(c) = &filho {
        crate::discovery::register_child(c.id());
    }
    filho.map_err(|e| if e.kind() == std::io::ErrorKind::NotFound { format!("o programa {name} não está instalado") } else { format!("não foi possível iniciar {name}: {e}") })
}

/// Sobe o PipeWire do projeto e espera o soquete de PCM (`addr`) e o soquete do PulseAudio
/// (`$XDG_RUNTIME_DIR/pulse/native`) ficarem prontos (até 5 s). Em erro, nada fica rodando.
pub fn start(addr: &str, config_dir: &Path, log: &LogFn) -> Result<Started, String> {
    if reachable(addr, Duration::from_millis(200)) {
        log(&format!("áudio: já há um servidor de PCM em {addr}; reaproveitado"));
        return Ok(Started::AlreadyRunning);
    }
    let Some(rt) = runtime_dir() else {
        return Err("XDG_RUNTIME_DIR não está definido ou não existe".into());
    };
    let logf = log_file();
    // O `-c` do PipeWire trata caminho relativo como nome de configuração e não o acha: precisa ser absoluto.
    let config_dir = &std::fs::canonicalize(config_dir).map_err(|e| format!("configuração do PipeWire em {}: {e}", config_dir.display()))?;
    let conf = config_dir.join("pipewire.conf");
    let pulse_conf = config_dir.join("pipewire-pulse.conf");
    let conf_s = conf.to_string_lossy().into_owned();
    let pulse_s = pulse_conf.to_string_lossy().into_owned();
    let mut procs = PipewireProcs { children: Vec::new() };
    procs.children.push(("pipewire", spawn("pipewire", &["-c", &conf_s], logf.as_ref())?));
    let deadline = Instant::now() + Duration::from_secs(5);
    while !reachable(addr, Duration::from_millis(100)) {
        if let Some((_, c)) = procs.children.first_mut() {
            if let Ok(Some(st)) = c.try_wait() {
                return Err(format!("o pipewire terminou logo ao subir ({st}); veja {}/pistick-audio.log (talvez haja outro PipeWire usando {})", rt.display(), rt.display()));
            }
        }
        if Instant::now() > deadline {
            return Err(format!("o PipeWire não abriu {addr} em 5 s"));
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    procs.children.push(("wireplumber", spawn("wireplumber", &["-p", "policy"], logf.as_ref())?));
    procs.children.push(("pipewire-pulse", spawn("pipewire-pulse", &["-c", &pulse_s], logf.as_ref())?));
    let pulse = rt.join("pulse").join("native");
    while !pulse.exists() {
        if Instant::now() > deadline {
            return Err("o pipewire-pulse não criou o soquete do PulseAudio em 5 s".into());
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    log(&format!("áudio: PipeWire iniciado (configuração em {}, PCM em {addr})", config_dir.display()));
    Ok(Started::Procs(procs))
}
