
use ed25519_dalek::{Signature, Signer};

use crate::chaves::{ChavePrivada, ChavePublica};
use crate::manifesto::Manifest;
use crate::{hex_decode, hex_encode, Erro};

pub const CABECALHO: &str = "pistick-assinatura 1";
const MAX_SIG: usize = 4096;

pub fn assinar(manifesto: &[u8], chave: &ChavePrivada) -> Result<String, Erro> {
    Manifest::parse(manifesto)?;
    Ok(texto_sig(manifesto, chave))
}

fn texto_sig(manifesto: &[u8], chave: &ChavePrivada) -> String {
    let s = chave.0.sign(manifesto);
    format!("{CABECALHO}\nchave {}\nassinatura {}\n", chave.publica().impressao(), hex_encode(&s.to_bytes()))
}

pub fn assinar_bruta(mensagem: &[u8], chave: &ChavePrivada) -> [u8; 64] {
    chave.0.sign(mensagem).to_bytes()
}

struct Sig {
    impressao: String,
    assinatura: [u8; 64],
}

fn ler_sig(texto: &[u8]) -> Result<Sig, Erro> {
    let ruim = |m: &str| Erro::FormatoAssinatura(m.to_string());
    if texto.len() > MAX_SIG {
        return Err(ruim("arquivo grande demais"));
    }
    let t = std::str::from_utf8(texto).map_err(|_| ruim("não é UTF-8"))?;
    let mut l = t.strip_suffix('\n').ok_or_else(|| ruim("falta a quebra de linha final"))?.split('\n');
    if l.next() != Some(CABECALHO) {
        return Err(ruim("a primeira linha deveria ser \"pistick-assinatura 1\""));
    }
    let impressao = l.next().and_then(|x| x.strip_prefix("chave ")).filter(|x| hex_decode(x, 8).is_some()).ok_or_else(|| ruim("a segunda linha deveria ser 'chave' com 16 dígitos hexadecimais minúsculos"))?;
    let hex = l.next().and_then(|x| x.strip_prefix("assinatura ")).ok_or_else(|| ruim("a terceira linha deveria ser 'assinatura'"))?;
    let bytes = hex_decode(hex, 64).ok_or_else(|| ruim("a assinatura deveria ter 128 dígitos hexadecimais minúsculos"))?;
    if l.next().is_some() {
        return Err(ruim("linhas a mais"));
    }
    let mut assinatura = [0u8; 64];
    assinatura.copy_from_slice(&bytes);
    Ok(Sig { impressao: impressao.to_string(), assinatura })
}

pub fn verificar_assinatura_bruta(mensagem: &[u8], assinatura: &[u8; 64], chave: &ChavePublica) -> bool {
    chave.0.verify_strict(mensagem, &Signature::from_bytes(assinatura)).is_ok()
}

pub fn verificar(manifesto: &[u8], sig: &[u8], chave: &ChavePublica, serial_minimo: Option<u64>) -> Result<Manifest, Erro> {
    let s = ler_sig(sig)?;
    if s.impressao != chave.impressao() {
        return Err(Erro::AssinaturaInvalida(format!("o manifesto foi assinado por outra chave (impressão {}); a chave pública informada é {}", s.impressao, chave.impressao())));
    }
    if !verificar_assinatura_bruta(manifesto, &s.assinatura, chave) {
        return Err(Erro::AssinaturaInvalida("a assinatura não confere com o manifesto (ele foi alterado, ou a assinatura é de outro conteúdo)".into()));
    }
    let m = Manifest::parse(manifesto)?;
    if let Some(minimo) = serial_minimo {
        if m.serial < minimo {
            return Err(Erro::SerialAntigo { serial: m.serial, minimo });
        }
    }
    Ok(m)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifesto::{Entrada, Manifest};

    fn chave(n: u8) -> ChavePrivada {
        ChavePrivada::de_semente(&[n; 32])
    }

    fn manifesto_com(caminho: &str) -> Vec<u8> {
        let m = Manifest { serial: 1, versao: "v".into(), arquivos: vec![Entrada { sha256: [0; 32], tamanho: 0, caminho: caminho.into() }] };
        m.to_text().into_bytes()
    }

    #[test]
    fn assinar_recusa_manifesto_com_caminho_perigoso() {
        let k = chave(1);
        for c in ["../fora", "a/../b", "/etc/passwd", "C:/x", r"a\b"] {
            assert!(assinar(&manifesto_com(c), &k).is_err(), "{c}");
        }
        assert!(assinar(&manifesto_com("ok/arquivo"), &k).is_ok());
    }

    #[test]
    fn verificar_recusa_caminho_perigoso_mesmo_com_assinatura_valida() {
        let k = chave(2);
        for c in ["../fora", "a/../b", "/etc/passwd", "C:/x"] {
            let man = manifesto_com(c);
            let sig = texto_sig(&man, &k);
            let e = verificar(&man, sig.as_bytes(), &k.publica(), None).expect_err("deveria recusar");
            assert!(matches!(e, Erro::Manifesto(_)), "{c}: {e}");
        }
    }

    #[test]
    fn chave_publica_errada_e_recusada() {
        let man = manifesto_com("a");
        let sig = texto_sig(&man, &chave(3));
        let e = verificar(&man, sig.as_bytes(), &chave(4).publica(), None).expect_err("deveria recusar");
        assert!(e.to_string().contains("outra chave"), "{e}");
        assert!(verificar(&man, sig.as_bytes(), &chave(3).publica(), None).is_ok());
    }
}
