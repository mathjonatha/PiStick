//! O manifesto de uma atualização: texto canônico, estrito e sem ambiguidade.
//!
//! ```text
//! pistick-update 1
//! serial 7
//! versao 0.2.0-abc1234
//! <sha256 em 64 hex> <tamanho> <caminho relativo>
//! ...
//! ```
//!
//! Regras (tudo que foge delas é recusado, e o leitor nunca entra em pânico):
//! - codificação UTF-8, linhas terminadas em `\n` (sem `\r`), com `\n` também na última;
//! - `serial` é um inteiro decimal sem zeros à esquerda e sem sinal (cresce a cada publicação, contra retrocesso);
//! - `versao` tem de 1 a 64 caracteres entre letras, dígitos, `.`, `_`, `+`, `-` e `~`;
//! - cada arquivo: SHA-256 em 64 dígitos hexadecimais minúsculos, um espaço, o tamanho em decimal (sem zeros à
//!   esquerda), um espaço e o caminho até o fim da linha;
//! - caminho relativo, com `/` como separador, sem `..`, sem `.`, sem componente vazio, sem barra inicial ou final,
//!   sem `\`, sem `:`, sem caracteres de controle, sem espaço no começo ou no fim; sem repetição;
//! - as linhas de arquivo estão em ordem crescente dos bytes do caminho (uma só ordem possível).

use crate::{hex_decode, hex_encode, Erro};

pub const CABECALHO: &str = "pistick-update 1";
/// Maior manifesto aceito (bytes): barra entradas absurdas antes de ler.
pub const MAX_BYTES: usize = 8 * 1024 * 1024;
/// Maior número de arquivos num manifesto.
pub const MAX_ARQUIVOS: usize = 100_000;
/// Maior tamanho de um caminho (bytes).
pub const MAX_CAMINHO: usize = 1024;

/// Um arquivo da atualização.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entrada {
    pub sha256: [u8; 32],
    pub tamanho: u64,
    pub caminho: String,
}

/// O manifesto lido ou montado.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Manifest {
    pub serial: u64,
    pub versao: String,
    pub arquivos: Vec<Entrada>,
}

fn erro(m: impl Into<String>) -> Erro {
    Erro::Manifesto(m.into())
}

/// `versao` válida (veja o texto do módulo).
pub fn versao_valida(v: &str) -> bool {
    !v.is_empty() && v.len() <= 64 && v.bytes().all(|c| c.is_ascii_alphanumeric() || matches!(c, b'.' | b'_' | b'+' | b'-' | b'~'))
}

/// Inteiro decimal canônico: só dígitos, sem zero à esquerda (menos o próprio "0"), cabendo em `u64`.
fn decimal(s: &str) -> Option<u64> {
    if s.is_empty() || !s.bytes().all(|c| c.is_ascii_digit()) || (s.len() > 1 && s.starts_with('0')) {
        return None;
    }
    s.parse().ok()
}

/// Verifica o caminho relativo de um arquivo. `Err` traz o motivo.
pub fn caminho_valido(c: &str) -> Result<(), String> {
    if c.is_empty() {
        return Err("caminho vazio".into());
    }
    if c.len() > MAX_CAMINHO {
        return Err("caminho comprido demais".into());
    }
    if c.starts_with('/') {
        return Err(format!("caminho absoluto: {c:?}"));
    }
    if c.contains('\\') {
        return Err(format!("barra invertida no caminho: {c:?}"));
    }
    if c.contains(':') {
        return Err(format!("':' no caminho (letra de unidade ou fluxo alternativo): {c:?}"));
    }
    if c.chars().any(|ch| ch.is_control()) {
        return Err(format!("caractere de controle no caminho: {c:?}"));
    }
    if c.starts_with(' ') || c.ends_with(' ') {
        return Err(format!("espaço no começo ou no fim do caminho: {c:?}"));
    }
    for comp in c.split('/') {
        match comp {
            "" => return Err(format!("componente vazio no caminho (barra dupla ou final): {c:?}")),
            "." => return Err(format!("'.' no caminho: {c:?}")),
            ".." => return Err(format!("'..' no caminho: {c:?}")),
            _ => {}
        }
        if comp.starts_with(' ') || comp.ends_with(' ') || comp.ends_with('.') {
            return Err(format!("componente com espaço ou ponto nas pontas: {c:?}"));
        }
    }
    Ok(())
}

impl Manifest {
    /// O texto canônico. Não valida: use [`Manifest::validar`] antes de assinar.
    pub fn to_text(&self) -> String {
        let mut s = String::new();
        s.push_str(CABECALHO);
        s.push('\n');
        s.push_str(&format!("serial {}\n", self.serial));
        s.push_str(&format!("versao {}\n", self.versao));
        for e in &self.arquivos {
            s.push_str(&format!("{} {} {}\n", hex_encode(&e.sha256), e.tamanho, e.caminho));
        }
        s
    }

    /// Confere as regras que valem para um manifesto montado em memória (versão, caminhos, ordem, repetição).
    pub fn validar(&self) -> Result<(), Erro> {
        if !versao_valida(&self.versao) {
            return Err(erro("versão inválida (1 a 64 caracteres entre letras, dígitos, '.', '_', '+', '-' e '~')"));
        }
        if self.arquivos.len() > MAX_ARQUIVOS {
            return Err(erro("arquivos demais"));
        }
        let mut anterior: Option<&str> = None;
        for e in &self.arquivos {
            caminho_valido(&e.caminho).map_err(erro)?;
            if let Some(a) = anterior {
                if a.as_bytes() == e.caminho.as_bytes() {
                    return Err(erro(format!("caminho repetido: {:?}", e.caminho)));
                }
                if a.as_bytes() > e.caminho.as_bytes() {
                    return Err(erro(format!("fora de ordem: {:?} depois de {a:?}", e.caminho)));
                }
            }
            anterior = Some(&e.caminho);
        }
        Ok(())
    }

    /// Lê o manifesto. Estrito: a leitura e a escrita são inversas (qualquer desvio do texto canônico é recusado).
    pub fn parse(bytes: &[u8]) -> Result<Manifest, Erro> {
        if bytes.len() > MAX_BYTES {
            return Err(erro("arquivo grande demais"));
        }
        let texto = std::str::from_utf8(bytes).map_err(|_| erro("não é UTF-8"))?;
        if !texto.ends_with('\n') {
            return Err(erro("falta a quebra de linha final"));
        }
        let mut linhas = texto[..texto.len() - 1].split('\n');
        if linhas.next() != Some(CABECALHO) {
            return Err(erro(format!("a primeira linha deveria ser {CABECALHO:?}")));
        }
        let serial = linhas
            .next()
            .and_then(|l| l.strip_prefix("serial "))
            .and_then(decimal)
            .ok_or_else(|| erro("a segunda linha deveria ser 'serial N' (inteiro decimal sem zeros à esquerda)"))?;
        let versao = linhas.next().and_then(|l| l.strip_prefix("versao ")).ok_or_else(|| erro("a terceira linha deveria ser 'versao TEXTO'"))?.to_string();
        let mut arquivos = Vec::new();
        for (i, l) in linhas.enumerate() {
            let n = i + 4;
            if arquivos.len() >= MAX_ARQUIVOS {
                return Err(erro("arquivos demais"));
            }
            let mut p = l.splitn(3, ' ');
            let (Some(h), Some(t), Some(c)) = (p.next(), p.next(), p.next()) else {
                return Err(erro(format!("linha {n}: deveria ser 'sha256 tamanho caminho'")));
            };
            let hash = hex_decode(h, 32).ok_or_else(|| erro(format!("linha {n}: SHA-256 inválido (64 dígitos hexadecimais minúsculos)")))?;
            let mut sha256 = [0u8; 32];
            sha256.copy_from_slice(&hash);
            let tamanho = decimal(t).ok_or_else(|| erro(format!("linha {n}: tamanho inválido")))?;
            arquivos.push(Entrada { sha256, tamanho, caminho: c.to_string() });
        }
        let m = Manifest { serial, versao, arquivos };
        m.validar()?;
        // Forma canônica única: o que entrou tem de ser o que sairia.
        if m.to_text().as_bytes() != bytes {
            return Err(erro("fora da forma canônica"));
        }
        Ok(m)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn exemplo() -> Manifest {
        Manifest {
            serial: 7,
            versao: "0.2.0-abc1234".into(),
            arquivos: vec![
                Entrada { sha256: [1; 32], tamanho: 10, caminho: "a.txt".into() },
                Entrada { sha256: [2; 32], tamanho: 0, caminho: "pasta/com espaco/b.bin".into() },
                Entrada { sha256: [3; 32], tamanho: 123_456, caminho: "pasta/z".into() },
            ],
        }
    }

    #[test]
    fn ida_e_volta() {
        let m = exemplo();
        let t = m.to_text();
        assert_eq!(Manifest::parse(t.as_bytes()).unwrap(), m);
    }

    #[test]
    fn caminhos_recusados() {
        for c in ["", "/etc/passwd", "../x", "a/../b", "a/./b", "a//b", "a/", "a\\b", "C:/x", "a:b", " a", "a ", "a/ b", "a/b.", "a\nb", "a\u{7}b", "./a", "a/.."] {
            assert!(caminho_valido(c).is_err(), "{c:?} deveria ser recusado");
        }
        for c in ["a", "a/b", "a b/c.d", "ação/ç.txt", ".oculto", "a/.b"] {
            assert!(caminho_valido(c).is_ok(), "{c:?} deveria valer");
        }
    }

    #[test]
    fn recusa_serial_e_versao_e_ordem_invalidos() {
        let h = "00".repeat(32);
        let ok = |t: String| Manifest::parse(t.as_bytes());
        assert!(ok(format!("pistick-update 1\nserial 1\nversao v\n{h} 0 a\n")).is_ok());
        assert!(ok(format!("pistick-update 1\nserial 01\nversao v\n{h} 0 a\n")).is_err());
        assert!(ok(format!("pistick-update 1\nserial -1\nversao v\n{h} 0 a\n")).is_err());
        assert!(ok(format!("pistick-update 1\nserial +1\nversao v\n{h} 0 a\n")).is_err());
        assert!(ok(format!("pistick-update 1\nserial 99999999999999999999999\nversao v\n{h} 0 a\n")).is_err());
        assert!(ok(format!("pistick-update 1\nserial 1\nversao \n{h} 0 a\n")).is_err());
        assert!(ok(format!("pistick-update 1\nserial 1\nversao a b\n{h} 0 a\n")).is_err());
        assert!(ok(format!("pistick-update 1\nserial 1\nversao v\n{h} 0 b\n{h} 0 a\n")).is_err(), "fora de ordem");
        assert!(ok(format!("pistick-update 1\nserial 1\nversao v\n{h} 0 a\n{h} 0 a\n")).is_err(), "repetido");
        assert!(ok(format!("pistick-update 1\nserial 1\nversao v\n{h} 00 a\n")).is_err(), "tamanho com zero à esquerda");
        assert!(ok(format!("pistick-update 1\nserial 1\nversao v\n{}  0 a\n", h.to_uppercase())).is_err());
        assert!(ok("pistick-update 1\r\nserial 1\r\nversao v\r\n".to_string()).is_err(), "CRLF");
        assert!(ok("pistick-update 1\nserial 1\nversao v".to_string()).is_err(), "sem quebra final");
        assert!(ok("pistick-update 2\nserial 1\nversao v\n".to_string()).is_err());
        assert!(ok("pistick-update 1\nserial 1\nversao v\n".to_string()).is_ok(), "sem arquivos vale");
    }
}
