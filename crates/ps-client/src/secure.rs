//! Canal cifrado e pareamento do lado do cliente (marco 9, parte A; projeto em `docs/21`).
//!
//! - `chave-do-cliente`: a identidade deste computador (chave estática X25519), criada no primeiro uso. No Windows
//!   o arquivo guarda a chave cifrada com a DPAPI (veja `keystore`); nos outros sistemas, 32 bytes com modo 0600.
//! - `aparelhos-conhecidos`: uma linha por aparelho, `chave-em-hex<TAB>nome<TAB>endereço da última conexão`
//!   (como o `known_hosts` do SSH). Aparelho conhecido que apresenta chave diferente da guardada: não conecta.
//!
//! Pasta das chaves: `%APPDATA%\PiStick\` (fora do Windows, `$XDG_CONFIG_HOME/pistick` ou
//! `~/.config/pistick`). A variável `PISTICK_KEYS_DIR` troca a pasta e existe **só para testes**: com ela
//! um mesmo computador faz o papel de vários clientes.

use std::io;
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::time::Duration;

use ps_proto::{read_message, Message, AUTH_PAIRED, AUTH_PAIRED_NOW, AUTH_PENDING};
use ps_secure::canal::{client_handshake, ChannelError};
use ps_secure::chaves::{self, fingerprint, format_code, load_list, sanitize_name, save_list};
use ps_secure::{ct_eq, SecureConn};
use ps_transport::{with_deadline, Split, TcpReader, TcpTransport, Transport};

/// Código de saída do `psc` quando o aparelho recusa o cliente (ele não está pareado).
pub const EXIT_REFUSED: u8 = 4;
/// Código de saída do `psc` quando o aparelho apresenta uma chave diferente da guardada.
pub const EXIT_KEY_MISMATCH: u8 = 5;

/// Limite de tempo do aperto de mão.
pub const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(5);
/// O servidor responde o estado de pareamento logo depois do aperto de mão.
const STATUS_TIMEOUT: Duration = Duration::from_secs(10);
/// O servidor espera até 120 s pela aprovação no aparelho.
const APPROVAL_TIMEOUT: Duration = Duration::from_secs(135);

/// Função chamada com o código de pareamento mostrado ao usuário.
pub type CodeHook = std::sync::Arc<dyn Fn(&str) + Send + Sync>;

/// Como o cliente trata a segurança de uma conexão.
#[derive(Clone)]
pub struct SecurityOptions {
    /// `--insecure`: texto claro mesmo fora do laço local.
    pub insecure: bool,
    /// `--secure` (testes): Noise mesmo em laço local.
    pub force_noise: bool,
    /// Nome anunciado ao aparelho (até 63 bytes).
    pub client_name: String,
    /// Pasta de `chave-do-cliente` e `aparelhos-conhecidos`.
    pub keys_dir: PathBuf,
    /// Chamado com o código de pareamento (`"123 456"`) quando o aparelho pede aprovação; os testes o usam
    /// para ler o código que o usuário veria.
    pub on_code: Option<CodeHook>,
}

impl SecurityOptions {
    pub fn new(insecure: bool, name: Option<String>) -> SecurityOptions {
        SecurityOptions { insecure, force_noise: false, client_name: sanitize_name(&name.unwrap_or_else(computer_name)), keys_dir: keys_dir(), on_code: None }
    }
}

/// Nome do computador, para anunciar ao aparelho (até 63 bytes).
pub fn computer_name() -> String {
    ["COMPUTERNAME", "HOSTNAME"].iter().find_map(|v| std::env::var(v).ok().filter(|s| !s.trim().is_empty())).unwrap_or_else(|| "psc".to_string())
}

/// Pasta das chaves do cliente (veja o texto do módulo).
pub fn keys_dir() -> PathBuf {
    if let Some(d) = std::env::var_os("PISTICK_KEYS_DIR").filter(|d| !d.is_empty()) {
        return PathBuf::from(d);
    }
    if let Some(a) = std::env::var_os("APPDATA").filter(|d| !d.is_empty()) {
        return PathBuf::from(a).join("PiStick");
    }
    if let Some(x) = std::env::var_os("XDG_CONFIG_HOME").filter(|d| !d.is_empty()) {
        return PathBuf::from(x).join("pistick");
    }
    if let Some(h) = std::env::var_os("HOME").filter(|d| !d.is_empty()) {
        return PathBuf::from(h).join(".config").join("pistick");
    }
    PathBuf::from("pistick-chaves")
}

pub fn key_file(dir: &Path) -> PathBuf {
    dir.join("chave-do-cliente")
}

fn known_file(dir: &Path) -> PathBuf {
    dir.join("aparelhos-conhecidos")
}

/// Por que a conexão não abriu.
#[derive(Debug)]
pub enum ConnectError {
    /// O aparelho recusou este cliente (estado 3): falta parear. Código de saída 4.
    Refused(String),
    /// O aparelho apresentou chave diferente da guardada. Código de saída 5.
    KeyMismatch(String),
    /// Qualquer outra falha (rede, formato, tempo).
    Other(String),
}

impl ConnectError {
    pub fn message(&self) -> &str {
        match self {
            ConnectError::Refused(m) | ConnectError::KeyMismatch(m) | ConnectError::Other(m) => m,
        }
    }

    pub fn exit_code(&self) -> u8 {
        match self {
            ConnectError::Refused(_) => EXIT_REFUSED,
            ConnectError::KeyMismatch(_) => EXIT_KEY_MISMATCH,
            ConnectError::Other(_) => 1,
        }
    }
}

impl std::fmt::Display for ConnectError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.message())
    }
}

/// Conexão pronta para a sessão: texto claro (laço local ou `--insecure`) ou canal cifrado.
pub enum Connected {
    Plain(TcpTransport),
    Secure(Box<SecureConn<TcpReader, TcpStream>>),
}

fn mismatch_text(nome: &str, guardada: &[u8; 32], apresentada: &[u8; 32]) -> String {
    format!(
        "o aparelho \"{nome}\" apresentou uma chave diferente da que este computador guardou (guardada: {}; apresentada: {}). \
Pode ser outro aparelho com o mesmo nome, ou alguém no meio do caminho: a conexão foi cancelada e nada foi enviado. \
Se você trocou o aparelho de propósito (ou regravou o cartão), rode `psc --forget \"{nome}\"` e conecte de novo.",
        fingerprint(guardada),
        fingerprint(apresentada)
    )
}

/// Decide texto claro ou Noise, faz o aperto de mão, confere o aparelho, mostra o estado de pareamento e
/// devolve a conexão pronta para o `HELLO`. `address` é o endereço usado, só para guardar na lista.
pub fn open(tcp: TcpTransport, address: &str, so: &SecurityOptions) -> Result<Connected, ConnectError> {
    let loopback = tcp.peer_ip().is_some_and(|i| i.is_loopback());
    if so.insecure || (loopback && !so.force_noise) {
        if so.insecure && !loopback {
            eprintln!("[psc] AVISO: --insecure: conexão em texto claro com {address}.");
        }
        return Ok(Connected::Plain(tcp));
    }
    let io_err = |e: io::Error| ConnectError::Other(format!("pasta de chaves {}: {e}", so.keys_dir.display()));
    let (key, nova) = crate::keystore::load_or_create_client_key(&key_file(&so.keys_dir)).map_err(io_err)?;
    if nova {
        println!("[psc] chave deste computador criada; impressão {}", fingerprint(key.public()));
    }
    open_with_key(tcp, address, so, &key)
}

/// Como [`open`] para o canal cifrado, com a chave do cliente já em mãos (o cliente Android a recebe da casca, que a
/// abriu com o Keystore). Não decide texto claro e não lê `chave-do-cliente`; só usa `keys_dir` para a lista de
/// `aparelhos-conhecidos`.
pub fn open_with_key(tcp: TcpTransport, address: &str, so: &SecurityOptions, key: &ps_secure::Keypair) -> Result<Connected, ConnectError> {
    let io_err = |e: io::Error| ConnectError::Other(format!("pasta de chaves {}: {e}", so.keys_dir.display()));
    let known_path = known_file(&so.keys_dir);
    let (mut known, avisos) = load_list(&known_path).map_err(io_err)?;
    for a in avisos {
        eprintln!("[psc] aviso: aparelhos-conhecidos: {a}");
    }

    let peer = tcp.peer();
    let Split { mut reader, mut writer, closer } = tcp.split().map_err(|e| ConnectError::Other(format!("transporte: {e}")))?;
    let (res, expirou) = with_deadline(&closer, HANDSHAKE_TIMEOUT, || {
        client_handshake(&mut reader, &mut writer, key, &so.client_name, |k, nome| match known.find_name(nome) {
            Some(p) if !ct_eq(&p.key, k) => Err(mismatch_text(nome, &p.key, k)),
            _ => Ok(()),
        })
    });
    let (est, info) = match res {
        Ok(v) if !expirou => v,
        Err(ChannelError::Rejected(m)) => return Err(ConnectError::KeyMismatch(m)),
        _ if expirou => return Err(ConnectError::Other(format!("o aperto de mão com {peer} não terminou em {} s (o aparelho fala o canal cifrado? use --insecure só para desenvolvimento)", HANDSHAKE_TIMEOUT.as_secs()))),
        Err(e) => return Err(ConnectError::Other(format!("aperto de mão com {peer} falhou: {e}"))),
        Ok(_) => return Err(ConnectError::Other("aperto de mão fora do prazo".into())),
    };
    let nome = sanitize_name(&info.name);
    let mut conn = SecureConn::new(reader, writer, closer.clone(), est.send, est.recv, peer.clone());

    // Primeira mensagem do servidor dentro do canal: o estado de pareamento.
    let mut status = status_message(&mut conn, &closer, STATUS_TIMEOUT)?;
    if let Message::AuthStatus { state: AUTH_PENDING, code } = status {
        // O código mostrado é calculado AQUI, do hash do aperto de mão desta conexão, e não o que veio na
        // mensagem: um intermediário que repassasse o código do aparelho faria os dois lados mostrarem o mesmo
        // número e a conferência não serviria de nada.
        let local = chaves::pairing_code(&info.handshake_hash);
        if local != code {
            return Err(ConnectError::KeyMismatch(format!(
                "o código de pareamento que o aparelho \"{nome}\" enviou ({}) difere do calculado aqui ({}): há alguém no meio da conexão (ou um defeito). Conexão cancelada.",
                format_code(&code),
                format_code(&local)
            )));
        }
        let texto = format_code(&local);
        if let Some(f) = &so.on_code {
            f(&texto);
        }
        println!("[psc] \"{nome}\" ainda não conhece este computador. Confira se o código de pareamento é o mesmo que o `psd pair` mostra no aparelho: {texto}");
        println!("[psc] aguardando a aprovação no aparelho (até 2 minutos)...");
        status = status_message(&mut conn, &closer, APPROVAL_TIMEOUT)?;
    }
    let Message::AuthStatus { state, .. } = status else {
        return Err(ConnectError::Other("o aparelho não enviou o estado de pareamento".into()));
    };
    match state {
        AUTH_PAIRED | AUTH_PAIRED_NOW => {
            let era_novo = known.find_key(&info.static_key).is_none();
            known.upsert(info.static_key, &nome, address);
            if let Err(e) = save_list(&known_path, &known) {
                eprintln!("[psc] aviso: não foi possível gravar {}: {e}", known_path.display());
            }
            if state == AUTH_PAIRED_NOW {
                println!("[psc] pareado com \"{nome}\", impressão {} (anote para conferir depois)", fingerprint(&info.static_key));
            } else if era_novo {
                println!("[psc] aparelho \"{nome}\" registrado, impressão {}", fingerprint(&info.static_key));
            }
            Ok(Connected::Secure(Box::new(conn)))
        }
        _ => Err(ConnectError::Refused(format!(
            "o aparelho \"{nome}\" não aceitou este computador (\"{}\", impressão {}): ele ainda não está pareado. \
Para parear, rode `psd pair` no aparelho (por SSH ou console; no serviço, `pistick-pair`), confira o código de 6 dígitos que aparece nos dois lados, confirme e conecte de novo.",
            so.client_name,
            fingerprint(key.public())
        ))),
    }
}

fn status_message(conn: &mut SecureConn<TcpReader, TcpStream>, closer: &ps_transport::Closer, timeout: Duration) -> Result<Message, ConnectError> {
    let (r, expirou) = with_deadline(closer, timeout, || read_message(&mut conn.reader));
    match r {
        Ok(m) if !expirou => Ok(m),
        _ if expirou => Err(ConnectError::Other("o aparelho não respondeu o estado de pareamento a tempo".into())),
        Err(e) => Err(ConnectError::Other(format!("falha ao ler o estado de pareamento: {e}"))),
        Ok(_) => Err(ConnectError::Other("fora do prazo".into())),
    }
}

/// `psc --forget NOME|IMPRESSÃO`: apaga o aparelho da lista de conhecidos. Devolve quantos foram apagados.
pub fn forget(dir: &Path, selector: &str) -> io::Result<usize> {
    let path = known_file(dir);
    let (mut list, _) = load_list(&path)?;
    let n = list.remove(selector).len();
    if n > 0 {
        save_list(&path, &list)?;
    }
    Ok(n)
}

/// Nomes, impressões e endereços dos aparelhos conhecidos (para mensagens e testes).
pub fn known_devices(dir: &Path) -> io::Result<Vec<(String, String, String)>> {
    let (list, _) = load_list(&known_file(dir))?;
    Ok(list.peers.iter().map(|p| (p.name.clone(), fingerprint(&p.key), p.extra.clone())).collect())
}
