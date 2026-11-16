//! Chaves, listas de pares, impressão digital e código de pareamento.
//!
//! Arquivos (texto simples, sem formato binário a manter):
//! - chave estática: 32 bytes brutos, modo 0600 (pasta 0700) nos sistemas Unix;
//! - lista de pares: uma linha por par, `chave-pública-em-hex<TAB>nome<TAB>extra`. No servidor `extra` é a data
//!   do pareamento; no cliente, o endereço da última conexão.
//!
//! Toda gravação é atômica: arquivo temporário, `fsync`, `rename`. Uma queda de energia no meio deixa a lista
//! antiga inteira (ou a nova inteira), nunca pela metade.

use std::fs;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};

use crate::ct_eq;
use crate::noise::Keypair;
use sha2::{Digest, Sha256};

/// Tamanho máximo, em bytes, do nome de um aparelho ou cliente.
pub const MAX_NAME: usize = 63;

// ---------------------------------------------------------------------------------------------
// Texto: hexadecimal, impressão digital, código
// ---------------------------------------------------------------------------------------------

pub fn hex_encode(b: &[u8]) -> String {
    let mut s = String::with_capacity(b.len() * 2);
    for x in b {
        s.push_str(&format!("{x:02x}"));
    }
    s
}

/// Lê 64 dígitos hexadecimais como uma chave de 32 bytes.
pub fn hex_decode32(s: &str) -> Option<[u8; 32]> {
    let s = s.trim();
    if s.len() != 64 || !s.is_ascii() {
        return None;
    }
    let mut out = [0u8; 32];
    for (i, o) in out.iter_mut().enumerate() {
        *o = u8::from_str_radix(&s[i * 2..i * 2 + 2], 16).ok()?;
    }
    Some(out)
}

/// Impressão digital de uma chave pública: os 8 primeiros bytes do SHA-256, em hexadecimal, em grupos de 4
/// (`"0123 4567 89ab cdef"`).
pub fn fingerprint(public: &[u8; 32]) -> String {
    let h: [u8; 32] = Sha256::digest(public).into();
    let hex = hex_encode(&h[..8]);
    hex.as_bytes().chunks(4).map(|c| std::str::from_utf8(c).unwrap_or("")).collect::<Vec<_>>().join(" ")
}

/// Código de pareamento: os 4 primeiros bytes de `h` (o hash final do Noise), inteiro big-endian, módulo
/// 1 000 000, com zeros à esquerda, como 6 dígitos ASCII.
pub fn pairing_code(handshake_hash: &[u8; 32]) -> [u8; 6] {
    let n = u32::from_be_bytes([handshake_hash[0], handshake_hash[1], handshake_hash[2], handshake_hash[3]]) % 1_000_000;
    let s = format!("{n:06}");
    let mut out = [0u8; 6];
    out.copy_from_slice(s.as_bytes());
    out
}

/// `"123 456"`, para mostrar ao usuário.
pub fn format_code(code: &[u8; 6]) -> String {
    let s: String = code.iter().map(|&b| b as char).collect();
    format!("{} {}", &s[..3], &s[3..])
}

/// Troca controles (tab, quebra de linha...) por espaço e corta em [`MAX_NAME`] bytes sem partir um caractere.
pub fn sanitize_name(name: &str) -> String {
    let mut s: String = name.chars().map(|c| if c.is_control() { ' ' } else { c }).collect();
    let s2 = s.trim().to_string();
    s = s2;
    if s.len() > MAX_NAME {
        let mut n = MAX_NAME;
        while n > 0 && !s.is_char_boundary(n) {
            n -= 1;
        }
        s.truncate(n);
    }
    s
}

/// Data UTC `AAAA-MM-DD` de um instante em segundos desde 1970 (algoritmo de Howard Hinnant, sem crate de data).
pub fn date_utc(unix_secs: u64) -> String {
    let z = (unix_secs / 86_400) as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!("{y:04}-{m:02}-{d:02}")
}

/// Data de hoje (UTC).
pub fn today() -> String {
    let secs = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    date_utc(secs)
}

// ---------------------------------------------------------------------------------------------
// Gravação atômica
// ---------------------------------------------------------------------------------------------

fn create_private_dir(dir: &Path) -> io::Result<()> {
    if dir.as_os_str().is_empty() || dir.is_dir() {
        return Ok(());
    }
    fs::create_dir_all(dir)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(dir, fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

fn open_private(path: &Path) -> io::Result<fs::File> {
    let mut o = fs::OpenOptions::new();
    o.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        o.mode(0o600);
    }
    o.open(path)
}

/// Grava `data` em `path` de forma atômica. `before_rename` roda entre o `fsync` do temporário e o `rename`
/// (os testes o usam para simular uma queda de energia nesse ponto).
pub fn write_atomic_with(path: &Path, data: &[u8], before_rename: impl FnOnce() -> io::Result<()>) -> io::Result<()> {
    let dir = path.parent().map(Path::to_path_buf).unwrap_or_default();
    create_private_dir(&dir)?;
    let mut tmp: PathBuf = path.to_path_buf();
    let mut nome = tmp.file_name().map(|n| n.to_os_string()).unwrap_or_default();
    nome.push(".tmp");
    tmp.set_file_name(nome);
    {
        let mut f = open_private(&tmp)?;
        f.write_all(data)?;
        f.sync_all()?;
    }
    before_rename()?;
    fs::rename(&tmp, path)?;
    #[cfg(unix)]
    if let Ok(d) = fs::File::open(if dir.as_os_str().is_empty() { Path::new(".") } else { &dir }) {
        let _ = d.sync_all();
    }
    Ok(())
}

pub fn write_atomic(path: &Path, data: &[u8]) -> io::Result<()> {
    write_atomic_with(path, data, || Ok(()))
}

// ---------------------------------------------------------------------------------------------
// Chave estática
// ---------------------------------------------------------------------------------------------

/// Lê a chave estática de `path`; se o arquivo não existe, gera uma e grava. Arquivo com tamanho errado é
/// erro (não é apagado nem substituído em silêncio). O booleano diz se a chave foi criada agora.
pub fn load_or_create_key(path: &Path) -> io::Result<(Keypair, bool)> {
    match fs::File::open(path) {
        Ok(f) => {
            let mut b = Vec::new();
            f.take(65).read_to_end(&mut b)?;
            if b.len() != 32 {
                return Err(io::Error::new(io::ErrorKind::InvalidData, format!("{}: a chave deveria ter 32 bytes", path.display())));
            }
            let mut a = [0u8; 32];
            a.copy_from_slice(&b);
            let kp = Keypair::from_secret(a);
            zeroize::Zeroize::zeroize(&mut a);
            zeroize::Zeroize::zeroize(&mut b);
            Ok((kp, false))
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            let kp = Keypair::generate().map_err(|e| io::Error::other(e.to_string()))?;
            write_atomic(path, kp.secret_bytes())?;
            Ok((kp, true))
        }
        Err(e) => Err(e),
    }
}

// ---------------------------------------------------------------------------------------------
// Lista de pares
// ---------------------------------------------------------------------------------------------

/// Um par conhecido: chave pública, nome e uma anotação (data ou endereço).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Peer {
    pub key: [u8; 32],
    pub name: String,
    pub extra: String,
}

/// Lista de pares, na ordem em que foram acrescentados.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PeerList {
    pub peers: Vec<Peer>,
}

impl PeerList {
    /// Lê o texto. Linha inválida é ignorada e vira um aviso; as outras valem.
    pub fn parse(text: &str) -> (PeerList, Vec<String>) {
        let mut list = PeerList::default();
        let mut avisos = Vec::new();
        for (i, linha) in text.lines().enumerate() {
            let t = linha.trim_end_matches('\r');
            if t.trim().is_empty() || t.trim_start().starts_with('#') {
                continue;
            }
            let mut it = t.splitn(3, '\t');
            let key = it.next().and_then(hex_decode32);
            let (Some(key), Some(name)) = (key, it.next()) else {
                avisos.push(format!("linha {} ignorada: formato inválido", i + 1));
                continue;
            };
            if name.is_empty() {
                avisos.push(format!("linha {} ignorada: nome vazio", i + 1));
                continue;
            }
            if list.find_key(&key).is_some() {
                avisos.push(format!("linha {} ignorada: chave repetida", i + 1));
                continue;
            }
            list.peers.push(Peer { key, name: sanitize_name(name), extra: it.next().unwrap_or("").to_string() });
        }
        (list, avisos)
    }

    pub fn to_text(&self) -> String {
        let mut s = String::new();
        for p in &self.peers {
            s.push_str(&format!("{}\t{}\t{}\n", hex_encode(&p.key), sanitize_name(&p.name), p.extra.replace(['\t', '\n', '\r'], " ")));
        }
        s
    }

    pub fn len(&self) -> usize {
        self.peers.len()
    }

    pub fn is_empty(&self) -> bool {
        self.peers.is_empty()
    }

    /// Busca por chave, comparando em tempo constante.
    pub fn find_key(&self, key: &[u8; 32]) -> Option<&Peer> {
        let mut achado = None;
        for p in &self.peers {
            if ct_eq(&p.key, key) {
                achado = Some(p);
            }
        }
        achado
    }

    pub fn find_name(&self, name: &str) -> Option<&Peer> {
        self.peers.iter().find(|p| p.name.eq_ignore_ascii_case(name))
    }

    /// Acrescenta; se a chave já existe, atualiza nome e anotação.
    pub fn upsert(&mut self, key: [u8; 32], name: &str, extra: &str) {
        let name = sanitize_name(name);
        if let Some(p) = self.peers.iter_mut().find(|p| ct_eq(&p.key, &key)) {
            p.name = name;
            p.extra = extra.to_string();
        } else {
            self.peers.push(Peer { key, name, extra: extra.to_string() });
        }
    }

    /// Remove por nome (sem distinguir maiúsculas) ou por impressão digital (com ou sem espaços). Devolve os
    /// removidos.
    pub fn remove(&mut self, selector: &str) -> Vec<Peer> {
        let sel = selector.trim();
        let sel_fp: String = sel.chars().filter(|c| !c.is_whitespace()).collect::<String>().to_ascii_lowercase();
        let mut fora = Vec::new();
        self.peers.retain(|p| {
            let casa = p.name.eq_ignore_ascii_case(sel) || (!sel_fp.is_empty() && fingerprint(&p.key).replace(' ', "") == sel_fp);
            if casa {
                fora.push(p.clone());
            }
            !casa
        });
        fora
    }
}

/// Lê a lista de `path`. Arquivo ausente = lista vazia. Os avisos (linhas inválidas) vêm junto.
pub fn load_list(path: &Path) -> io::Result<(PeerList, Vec<String>)> {
    match fs::read(path) {
        Ok(b) => Ok(PeerList::parse(&String::from_utf8_lossy(&b))),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok((PeerList::default(), Vec::new())),
        Err(e) => Err(e),
    }
}

pub fn save_list(path: &Path, list: &PeerList) -> io::Result<()> {
    write_atomic(path, list.to_text().as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmpdir(nome: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("ps-secure-teste-{}-{nome}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn codigo_de_seis_digitos_com_zeros_a_esquerda() {
        let mut h = [0u8; 32];
        assert_eq!(&pairing_code(&h), b"000000");
        h[..4].copy_from_slice(&1_234_567u32.to_be_bytes());
        assert_eq!(&pairing_code(&h), b"234567");
        h[..4].copy_from_slice(&42u32.to_be_bytes());
        assert_eq!(&pairing_code(&h), b"000042");
        assert_eq!(format_code(b"000042"), "000 042");
        h[..4].copy_from_slice(&u32::MAX.to_be_bytes());
        assert_eq!(&pairing_code(&h), b"967295"); // 4294967295 % 1000000
    }

    #[test]
    fn impressao_digital_tem_quatro_grupos_de_quatro() {
        let fp = fingerprint(&[7u8; 32]);
        let grupos: Vec<&str> = fp.split(' ').collect();
        assert_eq!(grupos.len(), 4);
        assert!(grupos.iter().all(|g| g.len() == 4 && g.chars().all(|c| c.is_ascii_hexdigit())));
        assert_ne!(fp, fingerprint(&[8u8; 32]));
    }

    #[test]
    fn datas_utc_conhecidas() {
        assert_eq!(date_utc(0), "1970-01-01");
        assert_eq!(date_utc(951_782_400), "2000-02-29");
        assert_eq!(date_utc(1_789_430_400), "2026-09-15");
        assert_eq!(date_utc(1_804_000_000), "2027-03-02");
    }

    #[test]
    fn hex_ida_e_volta_e_recusa_lixo() {
        let k = [0xabu8; 32];
        assert_eq!(hex_decode32(&hex_encode(&k)), Some(k));
        assert_eq!(hex_decode32("zz"), None);
        assert_eq!(hex_decode32(&"g".repeat(64)), None);
        assert_eq!(hex_decode32(&"é".repeat(32)), None);
    }

    #[test]
    fn linha_invalida_vira_aviso_e_as_outras_valem() {
        let ok1 = hex_encode(&[1u8; 32]);
        let ok2 = hex_encode(&[2u8; 32]);
        let texto = format!("{ok1}\tnotebook\t2026-11-15\nlixo sem tab\n{}\tcurta\n{ok2}\tcelular\t2026-11-16\n{ok1}\trepetida\t-\n\n# comentário\n{ok2}\t\tsem nome\n", "ab".repeat(10));
        let (l, avisos) = PeerList::parse(&texto);
        assert_eq!(l.len(), 2);
        assert_eq!(l.peers[0].name, "notebook");
        assert_eq!(l.peers[1].name, "celular");
        assert_eq!(avisos.len(), 4, "{avisos:?}");
    }

    #[test]
    fn ida_e_volta_da_lista_e_remocao_por_nome_ou_impressao() {
        let mut l = PeerList::default();
        l.upsert([1u8; 32], "Notebook", "2026-11-15");
        l.upsert([2u8; 32], "Celular\tcom tab", "2026-11-16");
        let (l2, avisos) = PeerList::parse(&l.to_text());
        assert!(avisos.is_empty());
        assert_eq!(l, l2);
        assert_eq!(l2.peers[1].name, "Celular com tab");
        let mut l3 = l2.clone();
        assert_eq!(l3.remove("notebook").len(), 1);
        assert_eq!(l3.len(), 1);
        let fp = fingerprint(&[2u8; 32]);
        assert_eq!(l3.remove(&fp.replace(' ', "")).len(), 1);
        assert!(l3.is_empty());
        assert!(l3.remove("nada").is_empty());
    }

    #[test]
    fn nome_longo_e_cortado_sem_partir_caractere() {
        let n = sanitize_name(&"é".repeat(100));
        assert!(n.len() <= MAX_NAME && n.chars().all(|c| c == 'é'));
        assert_eq!(sanitize_name("a\nb\tc"), "a b c");
    }

    #[test]
    fn chave_e_criada_uma_vez_e_relida() {
        let d = tmpdir("chave");
        let p = d.join("sub").join("chave-do-aparelho");
        let (k1, nova) = load_or_create_key(&p).unwrap();
        assert!(nova);
        let (k2, nova) = load_or_create_key(&p).unwrap();
        assert!(!nova);
        assert_eq!(k1.public(), k2.public());
        assert_eq!(fs::metadata(&p).unwrap().len(), 32);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(fs::metadata(&p).unwrap().permissions().mode() & 0o777, 0o600);
            assert_eq!(fs::metadata(p.parent().unwrap()).unwrap().permissions().mode() & 0o777, 0o700);
        }
        // Arquivo com tamanho errado: erro, e o arquivo continua como estava.
        fs::write(&p, b"curto").unwrap();
        assert!(load_or_create_key(&p).is_err());
        assert_eq!(fs::read(&p).unwrap(), b"curto");
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn queda_de_energia_entre_o_temporario_e_o_rename_nao_corrompe_a_lista() {
        let d = tmpdir("queda");
        let p = d.join("pareados");
        let mut l = PeerList::default();
        l.upsert([1u8; 32], "um", "2026-11-15");
        save_list(&p, &l).unwrap();
        let antes = fs::read(&p).unwrap();
        // A "queda": o temporário já foi gravado e sincronizado, o rename não acontece.
        let mut l2 = l.clone();
        l2.upsert([2u8; 32], "dois", "2026-11-16");
        let r = write_atomic_with(&p, l2.to_text().as_bytes(), || Err(io::Error::other("queda de energia simulada")));
        assert!(r.is_err());
        assert_eq!(fs::read(&p).unwrap(), antes, "a lista antiga precisa continuar inteira");
        let (lida, avisos) = load_list(&p).unwrap();
        assert!(avisos.is_empty());
        assert_eq!(lida, l);
        // Um temporário pela metade (queda durante a escrita dele) também não atrapalha.
        fs::write(d.join("pareados.tmp"), b"deadbeef\tmeio").unwrap();
        let (lida, _) = load_list(&p).unwrap();
        assert_eq!(lida, l);
        // E a próxima gravação normal troca tudo certinho.
        save_list(&p, &l2).unwrap();
        assert_eq!(load_list(&p).unwrap().0, l2);
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn arquivo_ausente_e_lista_vazia() {
        let d = tmpdir("ausente");
        let (l, a) = load_list(&d.join("nada")).unwrap();
        assert!(l.is_empty() && a.is_empty());
        let _ = fs::remove_dir_all(&d);
    }
}
