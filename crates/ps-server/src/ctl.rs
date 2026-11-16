
use std::io::{self, BufRead, BufReader, Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use ps_secure::chaves::{fingerprint, format_code};

use crate::secure::{PairWait, Security};

pub const SOCKET_NAME: &str = "pistick-ctl";

pub fn default_socket_path() -> Option<PathBuf> {
    if let Some(d) = std::env::var_os("XDG_RUNTIME_DIR").filter(|d| !d.is_empty()) {
        return Some(PathBuf::from(d).join(SOCKET_NAME));
    }
    let status = std::fs::read_to_string("/proc/self/status").ok()?;
    let uid = status.lines().find_map(|l| l.strip_prefix("Uid:"))?.split_whitespace().next()?.to_string();
    let dir = PathBuf::from("/run/user").join(uid);
    dir.is_dir().then(|| dir.join(SOCKET_NAME))
}

pub fn serve(sec: Arc<Security>, path: &Path) -> io::Result<()> {
    if path.exists() {
        if UnixStream::connect(path).is_ok() {
            return Err(io::Error::new(io::ErrorKind::AddrInUse, format!("{}: já há um psd atendendo este soquete", path.display())));
        }
        std::fs::remove_file(path)?;
    }
    let listener = UnixListener::bind(path)?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    std::thread::Builder::new().name("ctl".into()).spawn(move || {
        for conn in listener.incoming() {
            let Ok(conn) = conn else { continue };
            let sec = sec.clone();
            let _ = std::thread::Builder::new().name("ctl-conexao".into()).spawn(move || {
                let _ = handle(conn, &sec);
            });
        }
    })?;
    Ok(())
}

fn say(w: &mut impl Write, line: &str) -> io::Result<()> {
    w.write_all(line.as_bytes())?;
    w.write_all(b"\n")?;
    w.flush()
}

fn handle(conn: UnixStream, sec: &Security) -> io::Result<()> {
    conn.set_read_timeout(Some(Duration::from_secs(10)))?;
    let mut w = conn.try_clone()?;
    let mut r = BufReader::new(conn.try_clone()?);
    let mut line = String::new();
    r.read_line(&mut line)?;
    let line = line.trim_end_matches(['\r', '\n']);
    let (cmd, arg) = line.split_once(' ').unwrap_or((line, ""));
    match cmd {
        "LIST" => {
            for (p, online) in sec.list() {
                say(&mut w, &format!("C\t{}\t{}\t{}\t{}", fingerprint(&p.key), p.name, p.extra, u8::from(online)))?;
            }
            say(&mut w, "END")
        }
        "REMOVE" => match sec.remove(arg) {
            Ok(v) if v.is_empty() => say(&mut w, &format!("ERR nenhum cliente pareado casa com \"{arg}\"")),
            Ok(v) => say(&mut w, &format!("OK {}", v.len())),
            Err(e) => say(&mut w, &format!("ERR {e}")),
        },
        "RESET" => match sec.reset() {
            Ok(n) => say(&mut w, &format!("OK {n}")),
            Err(e) => say(&mut w, &format!("ERR {e}")),
        },
        "PAIR" => pair(&conn, &mut r, &mut w, sec),
        _ => say(&mut w, "ERR comando desconhecido"),
    }
}

fn pair(conn: &UnixStream, r: &mut BufReader<UnixStream>, w: &mut UnixStream, sec: &Security) -> io::Result<()> {
    let timeout = sec.timing().pair_timeout;
    say(w, &format!("WAIT {}", timeout.as_secs()))?;
    conn.set_nonblocking(true)?;
    let probe = conn.try_clone()?;
    let alive = move || {
        let mut b = [0u8; 1];
        match (&probe).read(&mut b) {
            Ok(0) => false,
            Ok(_) => true,
            Err(e) => e.kind() == io::ErrorKind::WouldBlock || e.kind() == io::ErrorKind::Interrupted,
        }
    };
    let ev = sec.open_pair_window(timeout, &alive);
    conn.set_nonblocking(false)?;
    match ev {
        PairWait::Busy => say(w, "BUSY"),
        PairWait::Timeout => say(w, "TIMEOUT"),
        PairWait::Abandoned => Ok(()),
        PairWait::Request(req) => {
            say(w, &format!("REQ {}\t{}\t{}\t{}", req.id, req.name, req.fingerprint, format_code(&req.code)))?;
            conn.set_read_timeout(Some(timeout))?;
            let mut resp = String::new();
            let ok = r.read_line(&mut resp).is_ok_and(|n| n > 0);
            let aprovar = ok && resp.trim() == "YES";
            match sec.decide(req.id, aprovar) {
                Ok(name) if aprovar => say(w, &format!("OK {name}")),
                Ok(_) => say(w, "DENIED"),
                Err(e) if aprovar => say(w, &format!("EXPIRED {e}")),
                Err(_) => say(w, "DENIED"),
            }
        }
    }
}

const AJUDA_PAIR: &str = "\
psd pair — pareamento de clientes com o psd em execução

USO:
    psd pair                       espera um pedido de pareamento (até 120 s), mostra o nome do cliente e o
                                   código de 6 dígitos, e pergunta se confere
    psd pair --list                lista os clientes pareados
    psd pair --remove NOME|IMPRESSÃO   remove um cliente (encerra a sessão dele, se estiver conectado)
    psd pair --reset               apaga todos os pareados e reabre o primeiro uso por 10 minutos
    psd pair ... --socket CAMINHO  soquete de controle (padrão: $XDG_RUNTIME_DIR/pistick-ctl)

No serviço use o atalho `pistick-pair` (instalado pelo provisionar.sh), que roda este comando como o usuário do
serviço.
";

fn connect(socket: Option<PathBuf>) -> Result<UnixStream, ExitCode> {
    let Some(path) = socket.or_else(default_socket_path) else {
        eprintln!("psd pair: não achei o soquete de controle (defina XDG_RUNTIME_DIR ou use --socket CAMINHO).");
        return Err(ExitCode::from(2));
    };
    UnixStream::connect(&path).map_err(|e| {
        eprintln!("psd pair: não consegui falar com o psd em {}: {e}\n  O psd está rodando? No serviço use `pistick-pair` (ou: sudo -u USUARIO XDG_RUNTIME_DIR=/run/pistick psd pair).", path.display());
        ExitCode::from(2)
    })
}

pub fn run_cli(args: &[String]) -> ExitCode {
    let mut socket = None;
    let mut cmd: Option<(&str, String)> = None;
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "-h" | "--help" => {
                print!("{AJUDA_PAIR}");
                return ExitCode::SUCCESS;
            }
            "--socket" => match it.next() {
                Some(p) => socket = Some(PathBuf::from(p)),
                None => return uso(),
            },
            "--list" if cmd.is_none() => cmd = Some(("LIST", String::new())),
            "--reset" if cmd.is_none() => cmd = Some(("RESET", String::new())),
            "--remove" if cmd.is_none() => match it.next() {
                Some(s) if !s.trim().is_empty() => cmd = Some(("REMOVE", s.clone())),
                _ => return uso(),
            },
            _ => return uso(),
        }
    }
    let (verbo, arg) = cmd.unwrap_or(("PAIR", String::new()));
    let conn = match connect(socket) {
        Ok(c) => c,
        Err(c) => return c,
    };
    let mut w = match conn.try_clone() {
        Ok(w) => w,
        Err(e) => {
            eprintln!("psd pair: {e}");
            return ExitCode::from(2);
        }
    };
    let mut r = BufReader::new(conn);
    let pedido = if arg.is_empty() { verbo.to_string() } else { format!("{verbo} {arg}") };
    if say(&mut w, &pedido).is_err() {
        eprintln!("psd pair: a conexão com o psd caiu.");
        return ExitCode::from(2);
    }
    match verbo {
        "LIST" => cli_list(&mut r),
        "PAIR" => cli_pair(&mut r, &mut w),
        _ => cli_simples(verbo, &mut r),
    }
}

fn uso() -> ExitCode {
    eprint!("{AJUDA_PAIR}");
    ExitCode::from(2)
}

fn ler(r: &mut impl BufRead) -> Option<String> {
    let mut l = String::new();
    match r.read_line(&mut l) {
        Ok(n) if n > 0 => Some(l.trim_end_matches(['\r', '\n']).to_string()),
        _ => None,
    }
}

fn cli_list(r: &mut impl BufRead) -> ExitCode {
    let mut n = 0;
    while let Some(l) = ler(r) {
        if l == "END" {
            if n == 0 {
                println!("Nenhum cliente pareado.");
            }
            return ExitCode::SUCCESS;
        }
        let c: Vec<&str> = l.split('\t').collect();
        if c.len() >= 5 && c[0] == "C" {
            n += 1;
            println!("{}  {:<24} pareado em {}{}", c[1], c[2], c[3], if c[4] == "1" { "  (conectado agora)" } else { "" });
        }
    }
    eprintln!("psd pair: resposta incompleta do psd.");
    ExitCode::from(2)
}

fn cli_simples(verbo: &str, r: &mut impl BufRead) -> ExitCode {
    match ler(r) {
        Some(l) if l.starts_with("OK") => {
            let n = l.trim_start_matches("OK").trim();
            if verbo == "REMOVE" {
                println!("Removido ({n}). Se estava conectado, a sessão foi encerrada; ele passa a ser recusado.");
            } else {
                println!("Lista apagada ({n} cliente(s)). O primeiro uso está aberto por 10 minutos: o próximo cliente a conectar é pareado sem confirmação.");
            }
            ExitCode::SUCCESS
        }
        Some(l) => {
            eprintln!("psd pair: {}", l.trim_start_matches("ERR").trim());
            ExitCode::from(1)
        }
        None => {
            eprintln!("psd pair: a conexão com o psd caiu.");
            ExitCode::from(2)
        }
    }
}

fn cli_pair(r: &mut impl BufRead, w: &mut impl Write) -> ExitCode {
    while let Some(l) = ler(r) {
        let (tag, resto) = l.split_once(' ').unwrap_or((l.as_str(), ""));
        match tag {
            "WAIT" => {
                println!("Aguardando um pedido de pareamento por até {resto} s. No cliente, rode `psc` e conecte a este aparelho.");
                let _ = std::io::stdout().flush();
            }
            "REQ" => {
                let c: Vec<&str> = resto.split('\t').collect();
                if c.len() < 4 {
                    eprintln!("psd pair: pedido malformado.");
                    return ExitCode::from(2);
                }
                print!("Cliente \"{}\" (impressão {}) quer parear. Código: {}. Confere? [s/N] ", c[1], c[2], c[3]);
                let _ = std::io::stdout().flush();
                let mut resp = String::new();
                let _ = std::io::stdin().read_line(&mut resp);
                let sim = matches!(resp.trim().to_lowercase().as_str(), "s" | "sim" | "y" | "yes");
                if say(w, if sim { "YES" } else { "NO" }).is_err() {
                    eprintln!("psd pair: a conexão com o psd caiu.");
                    return ExitCode::from(2);
                }
            }
            "OK" => {
                println!("Pareado: \"{resto}\". O cliente já pode conectar.");
                return ExitCode::SUCCESS;
            }
            "DENIED" => {
                println!("Pareamento negado.");
                return ExitCode::from(1);
            }
            "EXPIRED" => {
                eprintln!("O pedido venceu antes da confirmação. Peça ao cliente para tentar de novo.");
                return ExitCode::from(1);
            }
            "TIMEOUT" => {
                println!("Nenhum pedido de pareamento chegou no prazo.");
                return ExitCode::from(1);
            }
            "BUSY" => {
                eprintln!("Já há um `psd pair` esperando ou um pedido em curso. Tente de novo em instantes.");
                return ExitCode::from(1);
            }
            _ => {
                eprintln!("psd pair: {l}");
                return ExitCode::from(2);
            }
        }
    }
    eprintln!("psd pair: a conexão com o psd caiu.");
    ExitCode::from(2)
}
