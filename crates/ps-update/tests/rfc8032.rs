
use ps_update::assinatura::{assinar_bruta, verificar_assinatura_bruta};
use ps_update::chaves::{ChavePrivada, ChavePublica};
use ps_update::{hex_decode, hex_encode};

struct Vetor {
    nome: &'static str,
    semente: &'static str,
    publica: &'static str,
    mensagem: &'static str,
    assinatura: &'static str,
}

const VETORES: &[Vetor] = &[
    Vetor {
        nome: "TEST 1",
        semente: "9d61b19deffd5a60ba844af492ec2cc44449c5697b326919703bac031cae7f60",
        publica: "d75a980182b10ab7d54bfed3c964073a0ee172f3daa62325af021a68f707511a",
        mensagem: "",
        assinatura: "e5564300c360ac729086e2cc806e828a84877f1eb8e5d974d873e065224901555fb8821590a33bacc61e39701cf9b46bd25bf5f0595bbe24655141438e7a100b",
    },
    Vetor {
        nome: "TEST 2",
        semente: "4ccd089b28ff96da9db6c346ec114e0f5b8a319f35aba624da8cf6ed4fb8a6fb",
        publica: "3d4017c3e843895a92b70aa74d1b7ebc9c982ccf2ec4968cc0cd55f12af4660c",
        mensagem: "72",
        assinatura: "92a009a9f0d4cab8720e820b5f642540a2b27b5416503f8fb3762223ebdb69da085ac1e43e15996e458f3613d0f11d8c387b2eaeb4302aeeb00d291612bb0c00",
    },
    Vetor {
        nome: "TEST 3",
        semente: "c5aa8df43f9f837bedb7442f31dcb7b166d38535076f094b85ce3a2e0b4458f7",
        publica: "fc51cd8e6218a1a38da47ed00230f0580816ed13ba3303ac5deb911548908025",
        mensagem: "af82",
        assinatura: "6291d657deec24024827e69c3abe01a30ce548a284743a445e3680d7db5ac3ac18ff9b538d16f290ae67f760984dc6594a7c15e9716ed28dc027beceea1ec40a",
    },
    Vetor {
        nome: "TEST SHA(abc)",
        semente: "833fe62409237b9d62ec77587520911e9a759cec1d19755b7da901b96dca3d42",
        publica: "ec172b93ad5e563bf4932c70e1245034c35467ef2efd4d64ebf819683467e2bf",
        mensagem: "ddaf35a193617abacc417349ae20413112e6fa4e89a97ea20a9eeee64b55d39a2192992a274fc1a836ba3c23a3feebbd454d4423643ce80e2a9ac94fa54ca49f",
        assinatura: "dc2a4459e7369633a52b1bf277839a00201009a3efbf3ecb69bea2186c26b58909351fc9ac90b3ecfdfbc7c66431e0303dca179c138ac17ad9bef1177331a704",
    },
];

fn bytes<const N: usize>(h: &str) -> [u8; N] {
    let v = if h.is_empty() { Vec::new() } else { hex_decode(h, N).unwrap_or_else(|| panic!("hex inválido: {h}")) };
    let mut a = [0u8; N];
    a[..v.len()].copy_from_slice(&v);
    a
}

fn mensagem(v: &Vetor) -> Vec<u8> {
    if v.mensagem.is_empty() {
        Vec::new()
    } else {
        hex_decode(v.mensagem, v.mensagem.len() / 2).unwrap()
    }
}

#[test]
fn vetores_do_rfc_8032_passam_pela_verificacao() {
    for v in VETORES {
        let pubk = ChavePublica::de_bytes(&bytes::<32>(v.publica)).unwrap();
        assert!(verificar_assinatura_bruta(&mensagem(v), &bytes::<64>(v.assinatura), &pubk), "{}: a assinatura do RFC deveria conferir", v.nome);
    }
}

#[test]
fn a_chave_publica_derivada_da_semente_e_a_assinatura_gerada_sao_as_do_rfc() {
    for v in VETORES {
        let privada = ChavePrivada::de_semente(&bytes::<32>(v.semente));
        assert_eq!(hex_encode(&privada.publica().bytes()), v.publica, "{}: chave pública", v.nome);
        assert_eq!(hex_encode(&assinar_bruta(&mensagem(v), &privada)), v.assinatura, "{}: assinatura", v.nome);
    }
}

#[test]
fn um_bit_trocado_no_vetor_e_recusado() {
    for v in VETORES {
        let pubk = ChavePublica::de_bytes(&bytes::<32>(v.publica)).unwrap();
        let sig = bytes::<64>(v.assinatura);
        let msg = mensagem(v);
        for i in 0..64 * 8 {
            let mut s = sig;
            s[i / 8] ^= 1 << (i % 8);
            assert!(!verificar_assinatura_bruta(&msg, &s, &pubk), "{}: bit {i} da assinatura", v.nome);
        }
        for i in 0..msg.len() * 8 {
            let mut m = msg.clone();
            m[i / 8] ^= 1 << (i % 8);
            assert!(!verificar_assinatura_bruta(&m, &sig, &pubk), "{}: bit {i} da mensagem", v.nome);
        }
        let mut m = msg.clone();
        m.push(0);
        assert!(!verificar_assinatura_bruta(&m, &sig, &pubk), "{}: byte a mais", v.nome);
    }
}
