
use std::fs;
use std::io::{self, Read};
use std::path::Path;

use ps_secure::chaves::write_atomic;
use ps_secure::{wipe, Keypair};

pub const MAGIC: &[u8] = b"PSDPAPI1";

const MAX_FILE: u64 = 16 * 1024;

pub fn load_or_create_client_key(path: &Path) -> io::Result<(Keypair, bool)> {
    let mut dados = match read_limited(path) {
        Ok(d) => d,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return create(path),
        Err(e) => return Err(e),
    };
    let r = if dados.starts_with(MAGIC) {
        open_protected(path, &dados)
    } else if dados.len() == 32 {
        open_plain(path, &dados)
    } else {
        Err(io::Error::new(io::ErrorKind::InvalidData, format!("{}: formato desconhecido ({} bytes); nada foi alterado", path.display(), dados.len())))
    };
    wipe(&mut dados);
    r.map(|k| (k, false))
}

fn read_limited(path: &Path) -> io::Result<Vec<u8>> {
    let mut b = Vec::new();
    fs::File::open(path)?.take(MAX_FILE + 1).read_to_end(&mut b)?;
    if b.len() as u64 > MAX_FILE {
        return Err(io::Error::new(io::ErrorKind::InvalidData, format!("{}: arquivo grande demais para ser uma chave; nada foi alterado", path.display())));
    }
    Ok(b)
}

fn keypair_from(bytes: &[u8]) -> io::Result<Keypair> {
    let mut a = [0u8; 32];
    if bytes.len() != 32 {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "a chave deveria ter 32 bytes"));
    }
    a.copy_from_slice(bytes);
    let kp = Keypair::from_secret(a);
    wipe(&mut a);
    Ok(kp)
}

#[cfg(windows)]
fn create(path: &Path) -> io::Result<(Keypair, bool)> {
    let kp = Keypair::generate().map_err(|e| io::Error::other(e.to_string()))?;
    let arquivo = protected_file(kp.secret_bytes())?;
    write_atomic(path, &arquivo)?;
    Ok((kp, true))
}

#[cfg(windows)]
fn protected_file(secret: &[u8; 32]) -> io::Result<Vec<u8>> {
    let bloco = crate::dpapi::protect(secret).map_err(|e| io::Error::other(format!("não foi possível proteger a chave do cliente com a DPAPI do Windows: {e}")))?;
    let mut f = Vec::with_capacity(MAGIC.len() + bloco.len());
    f.extend_from_slice(MAGIC);
    f.extend_from_slice(&bloco);
    Ok(f)
}

#[cfg(windows)]
fn open_protected(path: &Path, dados: &[u8]) -> io::Result<Keypair> {
    match crate::dpapi::unprotect(&dados[MAGIC.len()..]) {
        Ok(mut plano) => {
            let r = keypair_from(&plano);
            wipe(&mut plano);
            r.map_err(|e| unreadable(path, &format!("o conteúdo decifrado é inválido ({e})")))
        }
        Err(e) => Err(unreadable(path, &format!("a DPAPI do Windows não conseguiu abri-lo ({e})"))),
    }
}

#[cfg(windows)]
fn open_plain(path: &Path, dados: &[u8]) -> io::Result<Keypair> {
    let kp = keypair_from(dados)?;
    let migrado = protected_file(kp.secret_bytes()).and_then(|arquivo| {
        let bloco = &arquivo[MAGIC.len()..];
        let mut volta = crate::dpapi::unprotect(bloco)?;
        let igual = ps_secure::ct_eq(&volta, kp.secret_bytes());
        wipe(&mut volta);
        if !igual {
            return Err(io::Error::other("a conferência da proteção falhou"));
        }
        write_atomic(path, &arquivo)
    });
    match migrado {
        Ok(()) => println!("[psc] chave do cliente migrada para a proteção do Windows (DPAPI): {}", path.display()),
        Err(e) => eprintln!("[psc] aviso: não foi possível proteger a chave do cliente com a DPAPI ({e}); o arquivo antigo foi mantido e a chave segue valendo."),
    }
    Ok(kp)
}

#[cfg(windows)]
fn unreadable(path: &Path, motivo: &str) -> io::Error {
    let mut nome = path.file_name().map(|n| n.to_os_string()).unwrap_or_default();
    nome.push(".ilegivel");
    let copia = path.with_file_name(nome);
    if !copia.exists() {
        let _ = fs::copy(path, &copia);
    }
    io::Error::new(
        io::ErrorKind::InvalidData,
        format!(
            "{}: o arquivo da chave deste computador está protegido pelo Windows e não pôde ser aberto: {motivo}. \
Isso acontece se o arquivo veio de outro usuário ou de outro computador, ou se foi alterado ou cortado. \
Nada foi sobrescrito e nenhuma chave nova foi criada; uma cópia ficou em {}. \
Se a chave não tem como ser recuperada, apague o arquivo e pareie de novo (`psd pair` no aparelho; o aparelho antigo pode ser removido com `pistick-pair --remove`).",
            path.display(),
            copia.display()
        ),
    )
}

#[cfg(not(windows))]
fn create(path: &Path) -> io::Result<(Keypair, bool)> {
    let kp = Keypair::generate().map_err(|e| io::Error::other(e.to_string()))?;
    write_atomic(path, kp.secret_bytes())?;
    Ok((kp, true))
}

#[cfg(not(windows))]
fn open_protected(path: &Path, _dados: &[u8]) -> io::Result<Keypair> {
    Err(io::Error::new(io::ErrorKind::InvalidData, format!("{}: este arquivo foi protegido pela DPAPI do Windows e não pode ser aberto neste sistema; nada foi alterado", path.display())))
}

#[cfg(not(windows))]
fn open_plain(_path: &Path, dados: &[u8]) -> io::Result<Keypair> {
    keypair_from(dados)
}

pub const MAGIC_AKS: &[u8] = b"PSAKS001";
pub const AKS_NONCE_LEN: usize = 12;
const AKS_MAX_BLOCK: usize = 256;

pub trait KeyProtector {
    fn protect(&self, secret: &[u8; 32]) -> io::Result<(Vec<u8>, Vec<u8>)>;
    fn unprotect(&self, nonce: &[u8], block: &[u8]) -> io::Result<Vec<u8>>;
}

pub fn aks_compose(nonce: &[u8], block: &[u8]) -> io::Result<Vec<u8>> {
    if nonce.len() != AKS_NONCE_LEN {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, format!("o nonce deveria ter {AKS_NONCE_LEN} bytes, tem {}", nonce.len())));
    }
    if block.is_empty() || block.len() > AKS_MAX_BLOCK {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, format!("bloco cifrado com {} bytes", block.len())));
    }
    let mut f = Vec::with_capacity(MAGIC_AKS.len() + nonce.len() + block.len());
    f.extend_from_slice(MAGIC_AKS);
    f.extend_from_slice(nonce);
    f.extend_from_slice(block);
    Ok(f)
}

pub fn aks_parse(file: &[u8]) -> io::Result<(&[u8], &[u8])> {
    let invalido = |m: &str| io::Error::new(io::ErrorKind::InvalidData, format!("arquivo da chave (Keystore) inválido: {m}"));
    if !file.starts_with(MAGIC_AKS) {
        return Err(invalido("cabeçalho desconhecido"));
    }
    let resto = &file[MAGIC_AKS.len()..];
    if resto.len() < AKS_NONCE_LEN + 1 {
        return Err(invalido("truncado (sem nonce ou sem bloco cifrado)"));
    }
    let (nonce, bloco) = resto.split_at(AKS_NONCE_LEN);
    if bloco.len() > AKS_MAX_BLOCK {
        return Err(invalido("bloco cifrado grande demais"));
    }
    Ok((nonce, bloco))
}

pub fn load_or_create_with(path: &Path, p: &dyn KeyProtector) -> io::Result<(Keypair, bool)> {
    let mut dados = match read_limited(path) {
        Ok(d) => d,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return create_with(path, p),
        Err(e) => return Err(e),
    };
    let r = if dados.starts_with(MAGIC_AKS) {
        open_aks(path, &dados, p)
    } else {
        Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("{}: formato desconhecido ({} bytes); nada foi alterado (chave em claro não é aceita neste sistema)", path.display(), dados.len()),
        ))
    };
    wipe(&mut dados);
    r.map(|k| (k, false))
}

fn create_with(path: &Path, p: &dyn KeyProtector) -> io::Result<(Keypair, bool)> {
    let kp = Keypair::generate().map_err(|e| io::Error::other(e.to_string()))?;
    let (nonce, bloco) = p.protect(kp.secret_bytes()).map_err(|e| io::Error::other(format!("não foi possível proteger a chave do cliente com o Keystore: {e}")))?;
    let arquivo = aks_compose(&nonce, &bloco)?;
    let mut volta = p.unprotect(&nonce, &bloco).map_err(|e| io::Error::other(format!("a conferência da proteção da chave falhou: {e}")))?;
    let igual = ps_secure::ct_eq(&volta, kp.secret_bytes());
    wipe(&mut volta);
    if !igual {
        return Err(io::Error::other("a conferência da proteção da chave falhou: o protetor não devolveu a mesma chave"));
    }
    write_atomic(path, &arquivo)?;
    Ok((kp, true))
}

fn open_aks(path: &Path, dados: &[u8], p: &dyn KeyProtector) -> io::Result<Keypair> {
    let (nonce, bloco) = aks_parse(dados).map_err(|e| unreadable_aks(path, &e.to_string()))?;
    match p.unprotect(nonce, bloco) {
        Ok(mut plano) => {
            let r = keypair_from(&plano);
            wipe(&mut plano);
            r.map_err(|e| unreadable_aks(path, &format!("o conteúdo decifrado é inválido ({e})")))
        }
        Err(e) => Err(unreadable_aks(path, &format!("o Keystore não conseguiu abri-lo ({e})"))),
    }
}

fn unreadable_aks(path: &Path, motivo: &str) -> io::Error {
    let mut nome = path.file_name().map(|n| n.to_os_string()).unwrap_or_default();
    nome.push(".ilegivel");
    let copia = path.with_file_name(nome);
    if !copia.exists() {
        let _ = fs::copy(path, &copia);
    }
    io::Error::new(
        io::ErrorKind::InvalidData,
        format!(
            "{}: o arquivo da chave deste aparelho está protegido pelo Android e não pôde ser aberto: {motivo}. \
Nada foi sobrescrito e nenhuma chave nova foi criada; uma cópia ficou em {}. \
Se a chave não tem como ser recuperada, apague os dados do aplicativo e pareie de novo (`pistick-pair --remove` no aparelho remove o cliente antigo).",
            path.display(),
            copia.display()
        ),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn tmp(nome: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("pistick-keystore-{}-{nome}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    #[cfg(windows)]
    fn contem(arquivo: &[u8], chave: &[u8; 32]) -> bool {
        arquivo.windows(32).any(|j| j == chave)
    }

    #[test]
    fn cria_e_a_segunda_leitura_devolve_a_mesma_chave() {
        let d = tmp("cria");
        let p = d.join("chave-do-cliente");
        let (a, nova) = load_or_create_client_key(&p).unwrap();
        assert!(nova);
        let (b, nova) = load_or_create_client_key(&p).unwrap();
        assert!(!nova);
        assert_eq!(a.public(), b.public());
        let arq = fs::read(&p).unwrap();
        #[cfg(windows)]
        {
            assert!(arq.starts_with(MAGIC) && arq.len() > 32);
            assert!(!contem(&arq, a.secret_bytes()), "a chave não pode estar em claro no arquivo");
        }
        #[cfg(not(windows))]
        assert_eq!(arq.len(), 32);
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn tamanho_estranho_e_erro_e_o_arquivo_fica() {
        let d = tmp("estranho");
        let p = d.join("chave-do-cliente");
        fs::write(&p, b"curto").unwrap();
        let e = load_or_create_client_key(&p).expect_err("deveria dar erro");
        assert_eq!(e.kind(), io::ErrorKind::InvalidData);
        assert_eq!(fs::read(&p).unwrap(), b"curto");
        fs::write(&p, vec![1u8; 20_000]).unwrap();
        assert!(load_or_create_client_key(&p).is_err());
        let _ = fs::remove_dir_all(&d);
    }

    #[cfg(windows)]
    #[test]
    fn arquivo_antigo_em_claro_migra_sozinho_e_a_chave_e_a_mesma() {
        let d = tmp("migra");
        let p = d.join("chave-do-cliente");
        let kp = Keypair::generate().unwrap();
        fs::write(&p, kp.secret_bytes()).unwrap();
        assert_eq!(fs::read(&p).unwrap().len(), 32);
        let (lida, nova) = load_or_create_client_key(&p).unwrap();
        assert!(!nova);
        assert_eq!(lida.public(), kp.public(), "a identidade (e o pareamento) continua a mesma");
        let arq = fs::read(&p).unwrap();
        assert!(arq.starts_with(MAGIC), "migrou para o formato protegido");
        assert!(!contem(&arq, kp.secret_bytes()));
        let (de_novo, _) = load_or_create_client_key(&p).unwrap();
        assert_eq!(de_novo.public(), kp.public());
        let _ = fs::remove_dir_all(&d);
    }

    #[cfg(windows)]
    #[test]
    fn arquivo_protegido_adulterado_da_erro_claro_sem_chave_nova_e_preserva_o_ruim() {
        let d = tmp("adulterado");
        let p = d.join("chave-do-cliente");
        let (original, _) = load_or_create_client_key(&p).unwrap();
        let mut ruim = fs::read(&p).unwrap();
        let meio = MAGIC.len() + (ruim.len() - MAGIC.len()) / 2;
        ruim[meio] ^= 0x40;
        fs::write(&p, &ruim).unwrap();
        let e = load_or_create_client_key(&p).expect_err("adulterado deveria dar erro");
        assert_eq!(e.kind(), io::ErrorKind::InvalidData);
        let texto = e.to_string();
        assert!(texto.contains("protegido pelo Windows") && texto.contains("Nada foi sobrescrito"), "{texto}");
        assert_eq!(fs::read(&p).unwrap(), ruim);
        assert_eq!(fs::read(d.join("chave-do-cliente.ilegivel")).unwrap(), ruim);
        assert!(load_or_create_client_key(&p).is_err());
        assert_eq!(fs::read(&p).unwrap(), ruim);
        let _ = original;
        let _ = fs::remove_dir_all(&d);
    }

    #[cfg(windows)]
    #[test]
    fn arquivo_protegido_truncado_ou_so_com_cabecalho_da_erro_sem_panico() {
        let d = tmp("truncado");
        let p = d.join("chave-do-cliente");
        let _ = load_or_create_client_key(&p).unwrap();
        let bom = fs::read(&p).unwrap();
        for corte in [MAGIC.len(), MAGIC.len() + 1, bom.len() / 2, bom.len() - 1] {
            fs::write(&p, &bom[..corte]).unwrap();
            assert!(load_or_create_client_key(&p).is_err(), "corte em {corte}");
            assert_eq!(fs::read(&p).unwrap(), &bom[..corte], "o arquivo não pode ser tocado");
        }
        let _ = fs::remove_dir_all(&d);
    }

    #[cfg(not(windows))]
    #[test]
    fn arquivo_protegido_fora_do_windows_e_erro_claro() {
        let d = tmp("fora");
        let p = d.join("chave-do-cliente");
        let mut f = MAGIC.to_vec();
        f.extend_from_slice(&[0u8; 100]);
        fs::write(&p, &f).unwrap();
        let e = load_or_create_client_key(&p).err().unwrap();
        assert!(e.to_string().contains("DPAPI"));
        assert_eq!(fs::read(&p).unwrap(), f);
        let _ = fs::remove_dir_all(&d);
    }

    struct Falso {
        falha_ao_criar: bool,
        recusa_abrir: bool,
        mentiroso: bool,
    }

    impl Falso {
        fn bom() -> Falso {
            Falso { falha_ao_criar: false, recusa_abrir: false, mentiroso: false }
        }
    }

    impl KeyProtector for Falso {
        fn protect(&self, secret: &[u8; 32]) -> io::Result<(Vec<u8>, Vec<u8>)> {
            if self.falha_ao_criar {
                return Err(io::Error::other("Keystore indisponível"));
            }
            let nonce = vec![7u8; AKS_NONCE_LEN];
            let mut bloco: Vec<u8> = secret.iter().map(|b| b ^ 0x5A).collect();
            bloco.extend_from_slice(b"TAG-GCM-16-bytes");
            Ok((nonce, bloco))
        }

        fn unprotect(&self, nonce: &[u8], block: &[u8]) -> io::Result<Vec<u8>> {
            if self.recusa_abrir || nonce != [7u8; AKS_NONCE_LEN] || block.len() != 48 || &block[32..] != b"TAG-GCM-16-bytes" {
                return Err(io::Error::other("etiqueta de autenticação inválida"));
            }
            let mut plano: Vec<u8> = block[..32].iter().map(|b| b ^ 0x5A).collect();
            if self.mentiroso {
                plano[0] ^= 1;
            }
            Ok(plano)
        }
    }

    #[test]
    fn aks_cria_e_a_segunda_leitura_devolve_a_mesma_chave_sem_chave_em_claro() {
        let d = tmp("aks-cria");
        let p = d.join("chave-do-cliente");
        let (a, nova) = load_or_create_with(&p, &Falso::bom()).unwrap();
        assert!(nova);
        let arq = fs::read(&p).unwrap();
        assert!(arq.starts_with(MAGIC_AKS));
        assert_eq!(arq.len(), MAGIC_AKS.len() + AKS_NONCE_LEN + 48);
        assert!(!arq.windows(32).any(|j| j == a.secret_bytes()), "a chave não pode estar em claro no arquivo");
        let (b, nova) = load_or_create_with(&p, &Falso::bom()).unwrap();
        assert!(!nova);
        assert_eq!(a.public(), b.public());
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn aks_adulterado_da_erro_claro_preserva_o_ruim_e_nao_cria_chave_nova() {
        let d = tmp("aks-adulterado");
        let p = d.join("chave-do-cliente");
        load_or_create_with(&p, &Falso::bom()).unwrap();
        let mut ruim = fs::read(&p).unwrap();
        let ultimo = ruim.len() - 1;
        ruim[ultimo] ^= 0x01;
        fs::write(&p, &ruim).unwrap();
        let e = load_or_create_with(&p, &Falso::bom()).expect_err("adulterado");
        assert_eq!(e.kind(), io::ErrorKind::InvalidData);
        let t = e.to_string();
        assert!(t.contains("Nada foi sobrescrito") && t.contains("nenhuma chave nova"), "{t}");
        assert_eq!(fs::read(&p).unwrap(), ruim, "o arquivo continua como estava");
        assert_eq!(fs::read(d.join("chave-do-cliente.ilegivel")).unwrap(), ruim, "uma cópia ficou ao lado");
        assert!(load_or_create_with(&p, &Falso::bom()).is_err(), "o erro se repete: nunca vira chave nova");
        assert_eq!(fs::read(&p).unwrap(), ruim);
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn aks_truncado_ou_so_com_cabecalho_da_erro_sem_panico_e_sem_tocar_no_arquivo() {
        let d = tmp("aks-truncado");
        let p = d.join("chave-do-cliente");
        load_or_create_with(&p, &Falso::bom()).unwrap();
        let bom = fs::read(&p).unwrap();
        for corte in [1, 3, MAGIC_AKS.len(), MAGIC_AKS.len() + 1, MAGIC_AKS.len() + AKS_NONCE_LEN, MAGIC_AKS.len() + AKS_NONCE_LEN + 1, bom.len() / 2, bom.len() - 1] {
            fs::write(&p, &bom[..corte]).unwrap();
            assert!(load_or_create_with(&p, &Falso::bom()).is_err(), "corte em {corte}");
            assert_eq!(fs::read(&p).unwrap(), &bom[..corte], "o arquivo não pode ser tocado (corte {corte})");
        }
        fs::write(&p, b"").unwrap();
        assert!(load_or_create_with(&p, &Falso::bom()).is_err());
        assert_eq!(fs::read(&p).unwrap(), b"");
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn aks_protetor_que_recusa_abrir_e_erro_claro() {
        let d = tmp("aks-recusa");
        let p = d.join("chave-do-cliente");
        load_or_create_with(&p, &Falso::bom()).unwrap();
        let antes = fs::read(&p).unwrap();
        let e = load_or_create_with(&p, &Falso { recusa_abrir: true, ..Falso::bom() }).expect_err("recusa");
        assert!(e.to_string().contains("não conseguiu abri-lo"), "{e}");
        assert_eq!(fs::read(&p).unwrap(), antes);
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn aks_protetor_que_falha_ao_criar_nao_grava_nada() {
        let d = tmp("aks-falha-criar");
        let p = d.join("chave-do-cliente");
        let e = load_or_create_with(&p, &Falso { falha_ao_criar: true, ..Falso::bom() }).expect_err("falha");
        assert!(e.to_string().contains("proteger a chave"), "{e}");
        assert!(!p.exists(), "sem proteção não há arquivo, e muito menos chave em claro");
        let e = load_or_create_with(&p, &Falso { mentiroso: true, ..Falso::bom() }).expect_err("conferência");
        assert!(e.to_string().contains("conferência"), "{e}");
        assert!(!p.exists());
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn aks_formato_em_claro_ou_desconhecido_nao_e_aceito() {
        let d = tmp("aks-claro");
        let p = d.join("chave-do-cliente");
        fs::write(&p, [3u8; 32]).unwrap();
        let e = load_or_create_with(&p, &Falso::bom()).expect_err("em claro");
        assert!(e.to_string().contains("não é aceita"), "{e}");
        assert_eq!(fs::read(&p).unwrap(), [3u8; 32]);
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn aks_compose_e_parse_ida_e_volta_e_entradas_invalidas() {
        let nonce = [9u8; AKS_NONCE_LEN];
        let bloco = [1u8; 48];
        let f = aks_compose(&nonce, &bloco).unwrap();
        let (n, b) = aks_parse(&f).unwrap();
        assert_eq!((n, b), (&nonce[..], &bloco[..]));
        assert!(aks_compose(&[0u8; 11], &bloco).is_err());
        assert!(aks_compose(&nonce, &[]).is_err());
        assert!(aks_compose(&nonce, &[0u8; 257]).is_err());
        assert!(aks_parse(b"").is_err());
        assert!(aks_parse(b"PSAKS002xxxxxxxxxxxxxxxxxxxxxxxx").is_err());
        assert!(aks_parse(MAGIC_AKS).is_err());
        assert!(aks_parse(&f[..f.len() - 48]).is_err(), "só cabeçalho e nonce");
        let mut grande = f.clone();
        grande.extend_from_slice(&[0u8; 300]);
        assert!(aks_parse(&grande).is_err());
    }
}
