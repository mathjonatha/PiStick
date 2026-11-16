//! Atualizações assinadas do PiStick (marco 9B, `docs/25`, decisão 5).
//!
//! Uma atualização é uma pasta de arquivos, descrita por um **manifesto** de texto e assinada por uma chave
//! **Ed25519** de publicação:
//!
//! - [`manifesto`]: o texto `pistick-update 1`, `serial N`, `versao TEXTO` e uma linha `sha256 tamanho caminho` por
//!   arquivo (caminhos relativos, sem `..`, sem repetição, em ordem). O texto é **canônico**: só se aceita exatamente
//!   o que [`manifesto::Manifest::to_text`] produz, então não há duas leituras para os mesmos bytes.
//! - [`assinatura`]: a assinatura destacada (`.sig`), Ed25519 do manifesto inteiro em hexadecimal mais a impressão
//!   digital da chave que assinou; assinar e verificar (com serial mínimo contra retrocesso de versão).
//! - [`chaves`]: gerar, gravar e ler as chaves; a chave privada **nunca** é gravada dentro de uma árvore com `.git`.
//! - [`diretorio`]: monta o manifesto de uma pasta e confere uma pasta contra um manifesto (tamanho e SHA-256 de cada
//!   arquivo; arquivo a mais é recusado).
//!
//! A chave privada de publicação nunca entra no repositório nem no Pi, e não existe chave pública embutida: quem
//! verifica informa a pública (`--pub`). A primitiva criptográfica é a crate `ed25519-dalek`; o resto é nosso.
//!
//! Ninguém aplica a atualização ainda: falta o instalador (marco 8) chamar o verificador.

#![forbid(unsafe_code)]

pub mod assinatura;
pub mod chaves;
pub mod diretorio;
pub mod manifesto;

use std::fmt;

/// Por que uma operação foi recusada. As mensagens são para o usuário.
#[derive(Debug)]
pub enum Erro {
    /// O manifesto está malformado (a mensagem diz onde).
    Manifesto(String),
    /// A assinatura (`.sig`) está malformada.
    FormatoAssinatura(String),
    /// A assinatura não confere com o manifesto e a chave pública informada.
    AssinaturaInvalida(String),
    /// Serial menor que o mínimo aceito (retrocesso de versão).
    SerialAntigo { serial: u64, minimo: u64 },
    /// Arquivo da pasta difere do manifesto, falta, ou sobra.
    Conteudo(String),
    /// Chave malformada, ou gravação recusada.
    Chave(String),
    /// Falha de leitura ou escrita.
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

/// Hexadecimal minúsculo.
pub fn hex_encode(b: &[u8]) -> String {
    const D: &[u8; 16] = b"0123456789abcdef";
    let mut s = String::with_capacity(b.len() * 2);
    for &x in b {
        s.push(D[(x >> 4) as usize] as char);
        s.push(D[(x & 15) as usize] as char);
    }
    s
}

/// Lê hexadecimal minúsculo de `n` bytes (exatamente `2 * n` dígitos, sem espaços; maiúsculas são recusadas para o
/// texto ter uma só forma).
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
