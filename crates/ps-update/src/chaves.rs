//! Chaves de publicação Ed25519: gerar, gravar, ler e impressão digital.
//!
//! Arquivos de texto, uma forma só:
//! - privada: `pistick-chave-privada 1`, depois `semente <64 hex>` (os 32 bytes da semente do RFC 8032), modo 0600;
//! - pública: `pistick-chave-publica 1`, depois `chave <64 hex>`.
//!
//! **A chave privada de publicação nunca entra no repositório nem no Pi.** [`gerar`] recusa gravar dentro de uma árvore
//! que tenha `.git` e recusa sobrescrever um arquivo que já existe.

use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use ed25519_dalek::{SigningKey, VerifyingKey};
use sha2::{Digest, Sha256};
use zeroize::Zeroize;

use crate::{hex_decode, hex_encode, Erro};

const CAB_PRIVADA: &str = "pistick-chave-privada 1";
const CAB_PUBLICA: &str = "pistick-chave-publica 1";
/// As chaves são arquivos pequenos; nada maior é lido.
const MAX_ARQUIVO: u64 = 4096;

/// Chave privada de publicação (zerada da memória ao soltar).
pub struct ChavePrivada(pub(crate) SigningKey);

/// Chave pública de verificação.
#[derive(Clone)]
#[derive(Debug)]
pub struct ChavePublica(pub(crate) VerifyingKey);

impl ChavePrivada {
    /// A partir de uma semente de 32 bytes (RFC 8032).
    pub fn de_semente(semente: &[u8; 32]) -> ChavePrivada {
        ChavePrivada(SigningKey::from_bytes(semente))
    }

    pub fn publica(&self) -> ChavePublica {
        ChavePublica(self.0.verifying_key())
    }
}

impl ChavePublica {
    /// A partir dos 32 bytes da chave. Recusa pontos inválidos.
    pub fn de_bytes(b: &[u8; 32]) -> Result<ChavePublica, Erro> {
        VerifyingKey::from_bytes(b).map(ChavePublica).map_err(|_| Erro::Chave("a chave pública não é um ponto válido da curva".into()))
    }

    pub fn bytes(&self) -> [u8; 32] {
        self.0.to_bytes()
    }

    /// Impressão digital: os 8 primeiros bytes do SHA-256 da chave pública, em 16 dígitos hexadecimais.
    pub fn impressao(&self) -> String {
        let h: [u8; 32] = Sha256::digest(self.0.to_bytes()).into();
        hex_encode(&h[..8])
    }
}

fn erro_io(caminho: &Path, e: std::io::Error) -> Erro {
    Erro::Io(format!("{}: {e}", caminho.display()))
}

/// Lê um arquivo pequeno como texto, sem passar de [`MAX_ARQUIVO`] bytes.
fn ler_pequeno(caminho: &Path) -> Result<String, Erro> {
    let mut b = Vec::new();
    fs::File::open(caminho).map_err(|e| erro_io(caminho, e))?.take(MAX_ARQUIVO + 1).read_to_end(&mut b).map_err(|e| erro_io(caminho, e))?;
    if b.len() as u64 > MAX_ARQUIVO {
        b.zeroize();
        return Err(Erro::Chave(format!("{}: arquivo grande demais para ser uma chave", caminho.display())));
    }
    match String::from_utf8(b) {
        Ok(s) => Ok(s),
        Err(e) => {
            let mut b = e.into_bytes();
            b.zeroize();
            Err(Erro::Chave(format!("{}: não é texto UTF-8", caminho.display())))
        }
    }
}

/// `CAB\nPREFIXO <64 hex>\n` → os 32 bytes.
fn ler_hex_de(texto: &str, cab: &str, prefixo: &str) -> Option<[u8; 32]> {
    let mut l = texto.strip_suffix('\n')?.split('\n');
    if l.next()? != cab {
        return None;
    }
    let v = hex_decode(l.next()?.strip_prefix(prefixo)?.strip_prefix(' ')?, 32)?;
    if l.next().is_some() {
        return None;
    }
    let mut a = [0u8; 32];
    a.copy_from_slice(&v);
    Some(a)
}

pub fn ler_privada(caminho: &Path) -> Result<ChavePrivada, Erro> {
    let mut t = ler_pequeno(caminho)?;
    let r = ler_hex_de(&t, CAB_PRIVADA, "semente");
    t.zeroize();
    match r {
        Some(mut s) => {
            let k = ChavePrivada::de_semente(&s);
            s.zeroize();
            Ok(k)
        }
        None => Err(Erro::Chave(format!("{}: não é uma chave privada do PiStick (formato esperado: {CAB_PRIVADA:?} e 'semente' com 64 dígitos hexadecimais minúsculos)", caminho.display()))),
    }
}

pub fn ler_publica(caminho: &Path) -> Result<ChavePublica, Erro> {
    let t = ler_pequeno(caminho)?;
    match ler_hex_de(&t, CAB_PUBLICA, "chave") {
        Some(b) => ChavePublica::de_bytes(&b),
        None => Err(Erro::Chave(format!("{}: não é uma chave pública do PiStick (formato esperado: {CAB_PUBLICA:?} e 'chave' com 64 dígitos hexadecimais minúsculos)", caminho.display()))),
    }
}

/// `true` se `caminho` está dentro de uma árvore que tem `.git` (pasta ou arquivo) em algum ancestral.
/// O caminho não precisa existir: vale o ancestral mais próximo que existe.
pub fn dentro_de_repositorio(caminho: &Path) -> bool {
    let mut base: PathBuf = if caminho.is_absolute() { caminho.to_path_buf() } else { std::env::current_dir().map(|c| c.join(caminho)).unwrap_or_else(|_| caminho.to_path_buf()) };
    // O ancestral existente mais próximo, resolvido (links simbólicos e `..` somem).
    let resolvido = loop {
        if let Ok(r) = fs::canonicalize(&base) {
            break r;
        }
        if !base.pop() {
            break base;
        }
    };
    resolvido.ancestors().any(|a| a.join(".git").exists())
}

fn gravar_novo(caminho: &Path, conteudo: &str, privado: bool) -> Result<(), Erro> {
    let mut o = fs::OpenOptions::new();
    o.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        o.mode(if privado { 0o600 } else { 0o644 });
    }
    #[cfg(not(unix))]
    let _ = privado;
    let mut f = o.open(caminho).map_err(|e| {
        if e.kind() == std::io::ErrorKind::AlreadyExists {
            Erro::Chave(format!("{} já existe; não vou sobrescrever", caminho.display()))
        } else {
            erro_io(caminho, e)
        }
    })?;
    f.write_all(conteudo.as_bytes()).map_err(|e| erro_io(caminho, e))?;
    f.sync_all().map_err(|e| erro_io(caminho, e))
}

/// Gera um par de chaves: a privada em `saida` e a pública em `saida` + `.pub`. Recusa gravar dentro de uma árvore com
/// `.git` e recusa sobrescrever.
pub fn gerar(saida: &Path) -> Result<ChavePublica, Erro> {
    let mut pub_nome = saida.file_name().ok_or_else(|| Erro::Chave("caminho de saída sem nome de arquivo".into()))?.to_os_string();
    pub_nome.push(".pub");
    let saida_pub = saida.with_file_name(pub_nome);
    for c in [saida, saida_pub.as_path()] {
        if dentro_de_repositorio(c) {
            return Err(Erro::Chave(format!(
                "{} fica dentro de um repositório git: a chave de publicação nunca pode ir para o repositório. Escolha uma pasta fora dele.",
                c.display()
            )));
        }
        if c.exists() {
            return Err(Erro::Chave(format!("{} já existe; não vou sobrescrever", c.display())));
        }
    }
    let mut semente = [0u8; 32];
    getrandom::fill(&mut semente).map_err(|e| Erro::Chave(format!("sem fonte de números aleatórios: {e}")))?;
    let privada = ChavePrivada::de_semente(&semente);
    let mut texto = format!("{CAB_PRIVADA}\nsemente {}\n", hex_encode(&semente));
    semente.zeroize();
    let r = gravar_novo(saida, &texto, true);
    texto.zeroize();
    r?;
    let publica = privada.publica();
    if let Err(e) = gravar_novo(&saida_pub, &format!("{CAB_PUBLICA}\nchave {}\n", hex_encode(&publica.bytes())), false) {
        let _ = fs::remove_file(saida);
        return Err(e);
    }
    Ok(publica)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pasta(nome: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("pistick-update-chaves-{}-{nome}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn gera_le_e_nao_sobrescreve() {
        let d = pasta("gera");
        let k = d.join("pub-key");
        let p = gerar(&k).unwrap();
        let lida = ler_privada(&k).unwrap();
        assert_eq!(lida.publica().bytes(), p.bytes());
        assert_eq!(ler_publica(&d.join("pub-key.pub")).unwrap().bytes(), p.bytes());
        assert_eq!(p.impressao().len(), 16);
        assert!(matches!(gerar(&k), Err(Erro::Chave(_))), "não sobrescreve");
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn recusa_gravar_dentro_de_repositorio_git() {
        let d = pasta("repo");
        fs::create_dir_all(d.join(".git")).unwrap();
        fs::create_dir_all(d.join("sub/dir")).unwrap();
        let e = gerar(&d.join("sub/dir/chave")).expect_err("deveria recusar");
        assert!(e.to_string().contains("repositório git"), "{e}");
        assert!(!d.join("sub/dir/chave").exists() && !d.join("sub/dir/chave.pub").exists());
        // Pasta que ainda não existe, dentro do repositório, também é recusada.
        assert!(gerar(&d.join("nova/pasta/chave")).is_err());
        // `.git` como arquivo (worktree, submódulo) também vale.
        let d2 = pasta("repo2");
        fs::write(d2.join(".git"), "gitdir: x\n").unwrap();
        assert!(gerar(&d2.join("chave")).is_err());
        let _ = fs::remove_dir_all(&d);
        let _ = fs::remove_dir_all(&d2);
    }

    #[test]
    fn arquivos_de_chave_malformados_sao_recusados() {
        let d = pasta("ruim");
        let hex = "ab".repeat(32);
        let casos: Vec<(&str, String)> = vec![
            ("a", "lixo
".to_string()),
            ("b", "pistick-chave-privada 1
semente 00
".to_string()),
            ("c", "pistick-chave-privada 1
semente ".to_string()),
            ("d", format!("pistick-chave-privada 1
semente {hex}
extra
")),
            ("e", format!("pistick-chave-privada 1
semente {hex}")),
            ("f", format!("pistick-chave-privada 1
semente {}
", hex.to_uppercase())),
        ];
        for (nome, texto) in casos {
            fs::write(d.join(nome), texto).unwrap();
            assert!(ler_privada(&d.join(nome)).is_err(), "{nome}");
        }
        fs::write(d.join("g"), vec![b'x'; 10_000]).unwrap();
        assert!(ler_privada(&d.join("g")).is_err());
        assert!(ler_privada(&d.join("nao-existe")).is_err());
        fs::write(d.join("h"), "pistick-chave-publica 1\nchave 0000000000000000000000000000000000000000000000000000000000000002\n").unwrap();
        // 0x02... pode ou não ser ponto válido; o que importa é não entrar em pânico.
        let _ = ler_publica(&d.join("h"));
        let _ = fs::remove_dir_all(&d);
    }
}
