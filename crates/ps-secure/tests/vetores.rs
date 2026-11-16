
use ps_secure::noise::{HandshakeState, Keypair, Role};

const NOME: &str = "Noise_XX_25519_ChaChaPoly_SHA256";

fn hex(s: &str) -> Vec<u8> {
    assert!(s.len().is_multiple_of(2));
    (0..s.len() / 2).map(|i| u8::from_str_radix(&s[i * 2..i * 2 + 2], 16).unwrap()).collect()
}

fn hex32(s: &str) -> [u8; 32] {
    hex(s).try_into().unwrap()
}

fn campo<'a>(trecho: &'a str, chave: &str) -> &'a str {
    let padrao = format!("\"{chave}\": \"");
    let i = trecho.find(&padrao).unwrap_or_else(|| panic!("campo {chave} ausente")) + padrao.len();
    let f = trecho[i..].find('"').unwrap();
    &trecho[i..i + f]
}

fn campos<'a>(trecho: &'a str, chave: &str) -> Vec<&'a str> {
    let padrao = format!("\"{chave}\": \"");
    let mut out = Vec::new();
    let mut resto = trecho;
    while let Some(i) = resto.find(&padrao) {
        let ini = i + padrao.len();
        let f = resto[ini..].find('"').unwrap();
        out.push(&resto[ini..ini + f]);
        resto = &resto[ini + f..];
    }
    out
}

fn entrada() -> String {
    let texto = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/data/cacophony.txt")).expect("vetores");
    let marca = format!("\"protocol_name\": \"{NOME}\"");
    let i = texto.find(&marca).expect("entrada do XX com ChaChaPoly e SHA256");
    let fim = texto[i + marca.len()..].find("\"protocol_name\"").map_or(texto.len(), |f| i + marca.len() + f);
    texto[i..fim].to_string()
}

#[test]
fn vetores_oficiais_do_noise_xx_25519_chachapoly_sha256() {
    let v = entrada();
    let prologo = hex(campo(&v, "init_prologue"));
    assert_eq!(prologo, hex(campo(&v, "resp_prologue")));
    let mut i = HandshakeState::new(Role::Initiator, &prologo, Keypair::from_secret(hex32(campo(&v, "init_static")))).with_ephemeral_for_test(hex32(campo(&v, "init_ephemeral")));
    let mut r = HandshakeState::new(Role::Responder, &prologo, Keypair::from_secret(hex32(campo(&v, "resp_static")))).with_ephemeral_for_test(hex32(campo(&v, "resp_ephemeral")));
    let payloads = campos(&v, "payload");
    let cifrados = campos(&v, "ciphertext");
    assert_eq!(payloads.len(), 6);
    assert_eq!(cifrados.len(), 6);

    for n in 0..3 {
        let (escritor, leitor) = if n % 2 == 0 { (&mut i, &mut r) } else { (&mut r, &mut i) };
        let mut saida = Vec::new();
        escritor.write_message(&hex(payloads[n]), &mut saida).unwrap();
        assert_eq!(saida, hex(cifrados[n]), "recado {n} do aperto de mão difere do vetor");
        assert_eq!(leitor.read_message(&saida).unwrap(), hex(payloads[n]), "carga do recado {n}");
    }
    assert!(i.is_finished() && r.is_finished());
    let esperado = hex32(campo(&v, "handshake_hash"));
    assert_eq!(i.handshake_hash(), esperado, "hash do aperto de mão (iniciador)");
    assert_eq!(r.handshake_hash(), esperado, "hash do aperto de mão (respondedor)");

    let (mut ei, mut er) = (i.finish().unwrap(), r.finish().unwrap());
    for n in 3..6 {
        let (env, rec) = if n % 2 == 0 { (&mut ei, &mut er) } else { (&mut er, &mut ei) };
        let mut buf = hex(payloads[n]);
        env.send.encrypt_in_place(&[], &mut buf).unwrap();
        assert_eq!(buf, hex(cifrados[n]), "recado de transporte {n} difere do vetor");
        rec.recv.decrypt_in_place(&[], &mut buf).unwrap();
        assert_eq!(buf, hex(payloads[n]));
    }
}
