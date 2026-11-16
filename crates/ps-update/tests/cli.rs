//! O binário `ps-update` de ponta a ponta: keygen, manifest, sign e verify, com os códigos de saída.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn pasta(nome: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("pistick-update-cli-{}-{nome}", std::process::id()));
    let _ = fs::remove_dir_all(&d);
    fs::create_dir_all(&d).unwrap();
    d
}

fn rodar(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_ps-update")).args(args).output().unwrap()
}

fn s(p: &Path) -> &str {
    p.to_str().unwrap()
}

fn texto(o: &Output) -> String {
    format!("{}{}", String::from_utf8_lossy(&o.stdout), String::from_utf8_lossy(&o.stderr))
}

#[test]
fn fluxo_completo_e_recusas_com_codigos_de_saida() {
    let raiz = pasta("fluxo");
    let dir = raiz.join("conteudo");
    fs::create_dir_all(dir.join("bin")).unwrap();
    fs::write(dir.join("bin/psd"), b"binario de teste").unwrap();
    fs::write(dir.join("LEIA.txt"), b"oi").unwrap();
    let chave = raiz.join("chave");
    let man = raiz.join("manifesto");
    let publica = raiz.join("chave.pub");

    let o = rodar(&["keygen", s(&chave)]);
    assert!(o.status.success(), "{}", texto(&o));
    assert!(chave.exists() && publica.exists());
    // Não sobrescreve.
    assert_eq!(rodar(&["keygen", s(&chave)]).status.code(), Some(1));

    let o = rodar(&["manifest", s(&dir), "--serial", "12", "--versao", "0.3.0", "--out", s(&man)]);
    assert!(o.status.success(), "{}", texto(&o));
    let o = rodar(&["sign", s(&man), "--key", s(&chave)]);
    assert!(o.status.success(), "{}", texto(&o));

    let o = rodar(&["verify", s(&man), "--pub", s(&publica), "--dir", s(&dir), "--min-serial", "12"]);
    assert!(o.status.success(), "{}", texto(&o));
    assert!(texto(&o).contains("VERIFY ok serial=12 versao=0.3.0 arquivos=2 pasta_conferida=2"), "{}", texto(&o));

    // Serial mínimo maior: recusado (código 1).
    let o = rodar(&["verify", s(&man), "--pub", s(&publica), "--min-serial", "13"]);
    assert_eq!(o.status.code(), Some(1), "{}", texto(&o));
    assert!(texto(&o).contains("retrocesso"), "{}", texto(&o));

    // Arquivo alterado na pasta: recusado.
    fs::write(dir.join("LEIA.txt"), b"oI").unwrap();
    let o = rodar(&["verify", s(&man), "--pub", s(&publica), "--dir", s(&dir)]);
    assert_eq!(o.status.code(), Some(1), "{}", texto(&o));
    fs::write(dir.join("LEIA.txt"), b"oi").unwrap();

    // Arquivo a mais: recusado.
    fs::write(dir.join("intruso"), b"x").unwrap();
    let o = rodar(&["verify", s(&man), "--pub", s(&publica), "--dir", s(&dir)]);
    assert_eq!(o.status.code(), Some(1), "{}", texto(&o));
    fs::remove_file(dir.join("intruso")).unwrap();

    // Manifesto alterado: recusado.
    let original = fs::read(&man).unwrap();
    let mut ruim = original.clone();
    ruim[20] ^= 1;
    fs::write(&man, &ruim).unwrap();
    assert_eq!(rodar(&["verify", s(&man), "--pub", s(&publica)]).status.code(), Some(1));
    fs::write(&man, &original).unwrap();

    // Chave pública de outro par: recusada.
    let outra = raiz.join("outra");
    assert!(rodar(&["keygen", s(&outra)]).status.success());
    let o = rodar(&["verify", s(&man), "--pub", s(&raiz.join("outra.pub"))]);
    assert_eq!(o.status.code(), Some(1), "{}", texto(&o));
    assert!(texto(&o).contains("outra chave"), "{}", texto(&o));

    // Uso errado: código 2. Sem --pub não há chave embutida.
    assert_eq!(rodar(&[]).status.code(), Some(2));
    assert_eq!(rodar(&["verify", s(&man)]).status.code(), Some(2));
    assert_eq!(rodar(&["verify", s(&man), "--pub", s(&publica), "--min-serial", "-1"]).status.code(), Some(2));
    assert_eq!(rodar(&["voar"]).status.code(), Some(2));
    let _ = fs::remove_dir_all(&raiz);
}

#[test]
fn keygen_dentro_de_um_repositorio_e_recusado_pelo_binario() {
    let raiz = pasta("repo");
    fs::create_dir_all(raiz.join("r/.git")).unwrap();
    let o = rodar(&["keygen", s(&raiz.join("r/chave"))]);
    assert_eq!(o.status.code(), Some(1), "{}", texto(&o));
    assert!(texto(&o).contains("repositório git"), "{}", texto(&o));
    assert!(!raiz.join("r/chave").exists());
    let _ = fs::remove_dir_all(&raiz);
}
