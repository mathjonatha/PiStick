
#![forbid(unsafe_code)]

pub mod assinatura;
pub mod chaves;
pub mod diretorio;
pub mod manifesto;

use std::fmt;

#[derive(Debug)]
pub enum Erro {
    Manifesto(String),
    FormatoAssinatura(String),
    AssinaturaInvalida(String),
    SerialAntigo { serial: u64, minimo: u64 },
    Conteudo(String),
    Chave(String),
    Io(String),
}

impl fmt::Display for Erro {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Erro::Manifesto(m) => write!(f, "manifesto inválido: {m}"),
            Erro::FormatoAssinatura(m) => write!(f, "arquivo de assinatura inválido: {m}"),
            Erro::AssinaturaInvalida(m) => write!(f, "assinatura recusada: {m}"),
            Erro::SerialAntigo { serial, minimo } => write!(f, "serial {serial} menor que o mínimo aceito ({minimo}): atualização antiga (retrocesso de versão)"),
            Erro::Conteudo(m) => write!(f, "conteúdo recusado: {m}"),
            Erro::Chave(m) => write!(f, "chave: {m}"),
            Erro::Io(m) => write!(f, "{m}"),
        }
    }
}

impl std::error::Error for Erro {}

pub fn hex_encode(b: &[u8]) -> String {
    const D: &[u8; 16] = b"0123456789abcdef";
    let mut s = String::with_capacity(b.len() * 2);
    for &x in b {
        s.push(D[(x >> 4) as usize] as char);
        s.push(D[(x & 15) as usize] as char);
    }
    s
}

pub fn hex_decode(s: &str, n: usize) -> Option<Vec<u8>> {
    let b = s.as_bytes();
    if b.len() != n * 2 {
        return None;
    }
    let digito = |c: u8| match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        _ => None,
    };
    let mut out = Vec::with_capacity(n);
    for par in b.chunks(2) {
        out.push((digito(par[0])? << 4) | digito(par[1])?);
    }
    Some(out)
}
