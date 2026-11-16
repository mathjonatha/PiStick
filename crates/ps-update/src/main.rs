
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use ps_update::{assinatura, chaves, diretorio, manifesto::Manifest, Erro};

const USO: &str = "uso:
  ps-update keygen SAÍDA
  ps-update manifest PASTA --serial N --versao V [--out ARQ]
  ps-update sign MANIFESTO --key ARQ [--out SIG]
  ps-update verify MANIFESTO --pub ARQ [--sig SIG] [--dir PASTA] [--min-serial N]";

struct Args {
    soltos: Vec<String>,
    opcoes: Vec<(String, String)>,
}

fn ler_args(args: &[String], validas: &[&str]) -> Result<Args, String> {
    let mut a = Args { soltos: Vec::new(), opcoes: Vec::new() };
    let mut it = args.iter();
    while let Some(x) = it.next() {
        if let Some(nome) = x.strip_prefix("--") {
            if !validas.contains(&nome) {
                return Err(format!("opção desconhecida: --{nome}"));
            }
            let v = it.next().ok_or_else(|| format!("falta o valor de --{nome}"))?;
            if a.opcoes.iter().any(|(n, _)| n == nome) {
                return Err(format!("--{nome} repetida"));
            }
            a.opcoes.push((nome.to_string(), v.clone()));
        } else {
            a.soltos.push(x.clone());
        }
    }
    Ok(a)
}

impl Args {
    fn opcao(&self, nome: &str) -> Option<&str> {
        self.opcoes.iter().find(|(n, _)| n == nome).map(|(_, v)| v.as_str())
    }

    fn exige(&self, nome: &str) -> Result<&str, String> {
        self.opcao(nome).ok_or_else(|| format!("falta --{nome}"))
    }

    fn um_solto(&self, o_que: &str) -> Result<&str, String> {
        match self.soltos.as_slice() {
            [x] => Ok(x),
            _ => Err(format!("esperava exatamente um argumento: {o_que}")),
        }
    }
}

fn numero(s: &str, nome: &str) -> Result<u64, String> {
    if s.is_empty() || !s.bytes().all(|c| c.is_ascii_digit()) {
        return Err(format!("--{nome}: não é um inteiro decimal"));
    }
    s.parse().map_err(|_| format!("--{nome}: número grande demais"))
}

fn ler_arquivo(c: &Path) -> Result<Vec<u8>, Erro> {
    let meta = std::fs::metadata(c).map_err(|e| Erro::Io(format!("{}: {e}", c.display())))?;
    if meta.len() > ps_update::manifesto::MAX_BYTES as u64 {
        return Err(Erro::Io(format!("{}: arquivo grande demais", c.display())));
    }
    std::fs::read(c).map_err(|e| Erro::Io(format!("{}: {e}", c.display())))
}

fn sig_ao_lado(man: &Path) -> PathBuf {
    let mut n = man.as_os_str().to_os_string();
    n.push(".sig");
    PathBuf::from(n)
}

enum Falha {
    Uso(String),
    Erro(Erro),
}

impl From<String> for Falha {
    fn from(m: String) -> Falha {
        Falha::Uso(m)
    }
}

impl From<Erro> for Falha {
    fn from(e: Erro) -> Falha {
        Falha::Erro(e)
    }
}

fn executar(args: &[String]) -> Result<(), Falha> {
    let Some((cmd, resto)) = args.split_first() else {
        return Err(Falha::Uso("falta o comando".into()));
    };
    match cmd.as_str() {
        "keygen" => {
            let a = ler_args(resto, &[])?;
            let saida = PathBuf::from(a.um_solto("o caminho de saída")?);
            let publica = chaves::gerar(&saida)?;
            println!("chave privada: {} (guarde fora do repositório e fora do Pi)", saida.display());
            println!("chave pública: {}.pub", saida.display());
            println!("impressão digital: {}", publica.impressao());
            Ok(())
        }
        "manifest" => {
            let a = ler_args(resto, &["serial", "versao", "out"])?;
            let pasta = PathBuf::from(a.um_solto("a pasta")?);
            let serial = numero(a.exige("serial")?, "serial")?;
            let m = diretorio::montar(&pasta, serial, a.exige("versao")?)?;
            let texto = m.to_text();
            match a.opcao("out") {
                Some(out) => {
                    if Path::new(out).exists() {
                        return Err(Erro::Io(format!("{out} já existe; não vou sobrescrever")).into());
                    }
                    std::fs::write(out, texto.as_bytes()).map_err(|e| Erro::Io(format!("{out}: {e}")))?;
                    eprintln!("manifesto: {out} ({} arquivos, serial {serial})", m.arquivos.len());
                }
                None => print!("{texto}"),
            }
            Ok(())
        }
        "sign" => {
            let a = ler_args(resto, &["key", "out"])?;
            let man = PathBuf::from(a.um_solto("o manifesto")?);
            let chave = chaves::ler_privada(Path::new(a.exige("key")?))?;
            let texto = assinatura::assinar(&ler_arquivo(&man)?, &chave)?;
            let out = a.opcao("out").map(PathBuf::from).unwrap_or_else(|| sig_ao_lado(&man));
            if out.exists() {
                return Err(Erro::Io(format!("{} já existe; não vou sobrescrever", out.display())).into());
            }
            std::fs::write(&out, texto.as_bytes()).map_err(|e| Erro::Io(format!("{}: {e}", out.display())))?;
            println!("assinatura: {}", out.display());
            Ok(())
        }
        "verify" => {
            let a = ler_args(resto, &["pub", "sig", "dir", "min-serial"])?;
            let man = PathBuf::from(a.um_solto("o manifesto")?);
            let publica = chaves::ler_publica(Path::new(a.exige("pub")?))?;
            let sig = a.opcao("sig").map(PathBuf::from).unwrap_or_else(|| sig_ao_lado(&man));
            let minimo = a.opcao("min-serial").map(|s| numero(s, "min-serial")).transpose()?;
            let m: Manifest = assinatura::verificar(&ler_arquivo(&man)?, &ler_arquivo(&sig)?, &publica, minimo)?;
            let mut conferidos = String::new();
            if let Some(dir) = a.opcao("dir") {
                let n = diretorio::conferir(Path::new(dir), &m)?;
                conferidos = format!(" pasta_conferida={n}");
            }
            println!("VERIFY ok serial={} versao={} arquivos={}{conferidos}", m.serial, m.versao, m.arquivos.len());
            Ok(())
        }
        "--help" | "-h" | "help" => {
            println!("{USO}");
            Ok(())
        }
        outro => Err(Falha::Uso(format!("comando desconhecido: {outro}"))),
    }
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match executar(&args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(Falha::Uso(m)) => {
            eprintln!("ps-update: {m}\n{USO}");
            ExitCode::from(2)
        }
        Err(Falha::Erro(e)) => {
            eprintln!("VERIFY recusado: {e}");
            ExitCode::from(1)
        }
    }
}
