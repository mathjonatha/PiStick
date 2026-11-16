//! Atualização inteira: pasta de teste, manifesto, assinatura, verificação, e tudo o que deve ser recusado.
//! As chaves são geradas na hora, numa pasta temporária fora de qualquer repositório; nada é commitado.

use std::fs;
use std::path::{Path, PathBuf};

use ps_update::assinatura::{assinar, verificar};
use ps_update::chaves::{self, ChavePrivada, ChavePublica};
use ps_update::diretorio::{conferir, montar};
use ps_update::manifesto::Manifest;
use ps_update::Erro;

fn pasta(nome: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("pistick-update-teste-{}-{nome}", std::process::id()));
    let _ = fs::remove_dir_all(&d);
    fs::create_dir_all(&d).unwrap();
    d
}

/// Cria uma pasta de atualização de teste com subpastas, um arquivo vazio e um com acento no nome.
fn atualizacao(raiz: &Path) -> PathBuf {
    let dir = raiz.join("conteudo");
    fs::create_dir_all(dir.join("bin")).unwrap();
    fs::create_dir_all(dir.join("etc/pistick")).unwrap();
    fs::write(dir.join("bin/psd"), (0..300u32).map(|i| (i * 7 % 251) as u8).collect::<Vec<u8>>()).unwrap();
    fs::write(dir.join("bin/pistick-pair"), b"#!/bin/sh\necho oi\n").unwrap();
    fs::write(dir.join("etc/pistick/psd.conf"), b"LISTEN=0.0.0.0:7700\n").unwrap();
    fs::write(dir.join("vazio"), b"").unwrap();
    fs::write(dir.join("ação.txt"), "conteúdo com acentuação\n").unwrap();
    dir
}

struct Pacote {
    dir: PathBuf,
    manifesto: Vec<u8>,
    sig: Vec<u8>,
    publica: ChavePublica,
}

fn par_de_chaves(raiz: &Path, nome: &str) -> (ChavePrivada, ChavePublica) {
    let arq = raiz.join(nome);
    let publica = chaves::gerar(&arq).unwrap();
    let privada = chaves::ler_privada(&arq).unwrap();
    assert_eq!(privada.publica().bytes(), publica.bytes());
    (privada, publica)
}

fn pacote(raiz: &Path, serial: u64) -> Pacote {
    let dir = atualizacao(raiz);
    let (privada, publica) = par_de_chaves(raiz, "chave");
    let m = montar(&dir, serial, "0.2.0-teste").unwrap();
    let manifesto = m.to_text().into_bytes();
    let sig = assinar(&manifesto, &privada).unwrap().into_bytes();
    Pacote { dir, manifesto, sig, publica }
}

#[test]
fn manifesto_de_uma_pasta_assina_e_verifica() {
    let raiz = pasta("ok");
    let p = pacote(&raiz, 5);
    let m = verificar(&p.manifesto, &p.sig, &p.publica, Some(5)).unwrap();
    assert_eq!((m.serial, m.versao.as_str(), m.arquivos.len()), (5, "0.2.0-teste", 5));
    assert_eq!(conferir(&p.dir, &m).unwrap(), 5);
    // Caminhos em ordem de bytes ("ação.txt" vem antes de "bin": 'a' < 'b'), com `/`.
    let nomes: Vec<&str> = m.arquivos.iter().map(|e| e.caminho.as_str()).collect();
    assert_eq!(nomes, ["ação.txt", "bin/pistick-pair", "bin/psd", "etc/pistick/psd.conf", "vazio"]);
    let _ = fs::remove_dir_all(&raiz);
}

#[test]
fn um_bit_trocado_em_qualquer_byte_do_manifesto_e_recusado() {
    let raiz = pasta("bit-manifesto");
    let p = pacote(&raiz, 1);
    for i in 0..p.manifesto.len() * 8 {
        let mut m = p.manifesto.clone();
        m[i / 8] ^= 1 << (i % 8);
        assert!(verificar(&m, &p.sig, &p.publica, None).is_err(), "bit {i} do manifesto foi aceito");
    }
    let _ = fs::remove_dir_all(&raiz);
}

#[test]
fn um_bit_trocado_em_qualquer_byte_da_assinatura_e_recusado() {
    let raiz = pasta("bit-sig");
    let p = pacote(&raiz, 1);
    for i in 0..p.sig.len() * 8 {
        let mut s = p.sig.clone();
        s[i / 8] ^= 1 << (i % 8);
        assert!(verificar(&p.manifesto, &s, &p.publica, None).is_err(), "bit {i} da assinatura foi aceito");
    }
    // Truncada, vazia e com lixo no fim.
    for corte in [0, 1, p.sig.len() / 2, p.sig.len() - 1] {
        assert!(verificar(&p.manifesto, &p.sig[..corte], &p.publica, None).is_err());
    }
    let mut mais = p.sig.clone();
    mais.extend_from_slice(b"extra\n");
    assert!(verificar(&p.manifesto, &mais, &p.publica, None).is_err());
    let _ = fs::remove_dir_all(&raiz);
}

#[test]
fn um_bit_trocado_em_qualquer_arquivo_da_pasta_e_recusado() {
    let raiz = pasta("bit-arquivo");
    let p = pacote(&raiz, 1);
    let m = verificar(&p.manifesto, &p.sig, &p.publica, None).unwrap();
    for e in &m.arquivos {
        let c = p.dir.join(&e.caminho);
        let original = fs::read(&c).unwrap();
        for i in 0..original.len() * 8 {
            let mut b = original.clone();
            b[i / 8] ^= 1 << (i % 8);
            fs::write(&c, &b).unwrap();
            let r = conferir(&p.dir, &m);
            assert!(matches!(r, Err(Erro::Conteudo(_))), "{}: bit {i} aceito", e.caminho);
        }
        // Byte a mais, byte a menos.
        let mut mais = original.clone();
        mais.push(0);
        fs::write(&c, &mais).unwrap();
        assert!(conferir(&p.dir, &m).is_err(), "{}: byte a mais", e.caminho);
        if !original.is_empty() {
            fs::write(&c, &original[..original.len() - 1]).unwrap();
            assert!(conferir(&p.dir, &m).is_err(), "{}: byte a menos", e.caminho);
        }
        fs::write(&c, &original).unwrap();
        assert_eq!(conferir(&p.dir, &m).unwrap(), 5, "restaurado volta a valer");
    }
    let _ = fs::remove_dir_all(&raiz);
}

#[test]
fn arquivo_a_mais_ou_a_menos_na_pasta_e_recusado() {
    let raiz = pasta("extra");
    let p = pacote(&raiz, 1);
    let m = verificar(&p.manifesto, &p.sig, &p.publica, None).unwrap();
    // A mais: no raiz, numa subpasta existente, numa subpasta nova, vazio, e no lugar certo da ordem.
    for novo in ["extra.txt", "bin/extra", "novo/dir/extra", "aaa", "zzz", "vazio2"] {
        let c = p.dir.join(novo);
        fs::create_dir_all(c.parent().unwrap()).unwrap();
        fs::write(&c, b"").unwrap();
        let e = conferir(&p.dir, &m).expect_err(novo);
        assert!(e.to_string().contains("a mais"), "{novo}: {e}");
        fs::remove_file(&c).unwrap();
        let _ = fs::remove_dir_all(p.dir.join("novo"));
    }
    // O manifesto e a assinatura dentro da pasta contam como arquivo a mais.
    fs::write(p.dir.join("manifesto"), &p.manifesto).unwrap();
    assert!(conferir(&p.dir, &m).is_err());
    fs::remove_file(p.dir.join("manifesto")).unwrap();
    // A menos.
    for c in ["bin/psd", "vazio", "ação.txt"] {
        let original = fs::read(p.dir.join(c)).unwrap();
        fs::remove_file(p.dir.join(c)).unwrap();
        let e = conferir(&p.dir, &m).expect_err(c);
        assert!(e.to_string().contains("falta"), "{c}: {e}");
        fs::write(p.dir.join(c), original).unwrap();
    }
    assert_eq!(conferir(&p.dir, &m).unwrap(), 5);
    let _ = fs::remove_dir_all(&raiz);
}

#[test]
fn serial_menor_que_o_minimo_e_recusado() {
    let raiz = pasta("serial");
    let p = pacote(&raiz, 7);
    assert!(verificar(&p.manifesto, &p.sig, &p.publica, Some(7)).is_ok());
    assert!(verificar(&p.manifesto, &p.sig, &p.publica, Some(1)).is_ok());
    assert!(verificar(&p.manifesto, &p.sig, &p.publica, None).is_ok());
    let e = verificar(&p.manifesto, &p.sig, &p.publica, Some(8)).expect_err("serial antigo");
    assert!(matches!(e, Erro::SerialAntigo { serial: 7, minimo: 8 }), "{e}");
    assert!(verificar(&p.manifesto, &p.sig, &p.publica, Some(u64::MAX)).is_err());
    let _ = fs::remove_dir_all(&raiz);
}

#[test]
fn chave_publica_errada_e_recusada() {
    let raiz = pasta("outra-chave");
    let p = pacote(&raiz, 1);
    let (_, outra) = par_de_chaves(&raiz, "outra");
    let e = verificar(&p.manifesto, &p.sig, &outra, None).expect_err("chave errada");
    assert!(matches!(e, Erro::AssinaturaInvalida(_)), "{e}");
    let _ = fs::remove_dir_all(&raiz);
}

#[test]
fn a_assinatura_de_um_manifesto_nao_serve_para_outro() {
    let raiz = pasta("troca");
    let p = pacote(&raiz, 1);
    let outro = Manifest::parse(&p.manifesto).unwrap();
    let mut outro = outro;
    outro.serial = 2;
    let texto = outro.to_text().into_bytes();
    assert!(verificar(&texto, &p.sig, &p.publica, None).is_err(), "assinatura reaproveitada com serial maior");
    let _ = fs::remove_dir_all(&raiz);
}

#[test]
fn caminhos_perigosos_no_manifesto_sao_recusados_ao_montar_e_ao_assinar() {
    let raiz = pasta("caminhos");
    let (privada, _) = par_de_chaves(&raiz, "chave");
    let h = "00".repeat(32);
    for c in ["../fora", "a/../b", "/abs", "C:/x", "a//b", "a/./b", "a\\b"] {
        let texto = format!("pistick-update 1\nserial 1\nversao v\n{h} 0 {c}\n");
        assert!(assinar(texto.as_bytes(), &privada).is_err(), "{c}");
        assert!(Manifest::parse(texto.as_bytes()).is_err(), "{c}");
    }
    let _ = fs::remove_dir_all(&raiz);
}

/// Gerador pseudoaleatório pequeno (xorshift64*), determinístico: o teste é repetível.
struct Prng(u64);

impl Prng {
    fn proximo(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    fn ate(&mut self, n: usize) -> usize {
        (self.proximo() % n as u64) as usize
    }
}

#[test]
fn dez_mil_manifestos_pseudoaleatorios_nao_causam_panico() {
    let raiz = pasta("fuzz");
    let p = pacote(&raiz, 1);
    let mut r = Prng(0x9E37_79B9_7F4A_7C15);
    let pedacos: [&[u8]; 14] = [b"pistick-update 1\n", b"serial ", b"versao ", b"\n", b" ", b"..", b"/", b"\\", b"0", b"9", &[0xff, 0xfe], b"\r\n", b"%", "ç".as_bytes()];
    let mut recusados = 0;
    for i in 0..10_000 {
        let bytes: Vec<u8> = match i % 4 {
            // Bytes totalmente aleatórios.
            0 => (0..r.ate(300)).map(|_| r.proximo() as u8).collect(),
            // Pedaços de sintaxe do manifesto em ordem aleatória.
            1 => {
                let mut v = Vec::new();
                for _ in 0..r.ate(40) {
                    v.extend_from_slice(pedacos[r.ate(pedacos.len())]);
                }
                v
            }
            // Um manifesto válido com 1 a 4 bytes trocados, apagados ou inseridos.
            2 => {
                let mut v = p.manifesto.clone();
                for _ in 0..1 + r.ate(4) {
                    let pos = r.ate(v.len().max(1));
                    match r.ate(3) {
                        0 if !v.is_empty() => v[pos] = r.proximo() as u8,
                        1 if !v.is_empty() => {
                            v.remove(pos);
                        }
                        _ => v.insert(pos.min(v.len()), r.proximo() as u8),
                    }
                }
                v
            }
            // Um manifesto válido cortado em qualquer ponto.
            _ => p.manifesto[..r.ate(p.manifesto.len() + 1)].to_vec(),
        };
        if Manifest::parse(&bytes).is_err() {
            recusados += 1;
        }
        // Com a assinatura boa (de outro conteúdo) e com uma assinatura aleatória: sempre recusa, nunca entra em pânico.
        assert!(verificar(&bytes, &p.sig, &p.publica, None).is_err() || bytes == p.manifesto);
        let sig_aleatoria: Vec<u8> = (0..r.ate(260)).map(|_| r.proximo() as u8).collect();
        assert!(verificar(&bytes, &sig_aleatoria, &p.publica, None).is_err());
    }
    assert!(recusados > 9_000, "quase tudo deveria ser recusado ({recusados})");
    let _ = fs::remove_dir_all(&raiz);
}

#[test]
fn keygen_dentro_de_um_repositorio_e_recusado() {
    let raiz = pasta("repo");
    fs::create_dir_all(raiz.join("meu-repo/.git")).unwrap();
    fs::create_dir_all(raiz.join("meu-repo/chaves")).unwrap();
    let e = chaves::gerar(&raiz.join("meu-repo/chaves/pub")).expect_err("dentro do repositório");
    assert!(e.to_string().contains("repositório git"), "{e}");
    assert!(!raiz.join("meu-repo/chaves/pub").exists());
    assert!(!raiz.join("meu-repo/chaves/pub.pub").exists());
    // Fora do repositório vale.
    assert!(chaves::gerar(&raiz.join("fora")).is_ok());
    let _ = fs::remove_dir_all(&raiz);
}
