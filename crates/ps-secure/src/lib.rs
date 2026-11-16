//! Canal cifrado e pareamento do PiStick (marco 9, parte A; projeto em `docs/21`).
//!
//! - [`noise`]: máquina de estados do `Noise_XX_25519_ChaChaPoly_SHA256` (nossa; as primitivas vêm de crates).
//! - [`canal`]: abertura `PSN1`, aperto de mão e registros cifrados sobre qualquer `Read + Write`.
//! - [`chaves`]: chave estática, listas de pares, impressão digital e código de 6 dígitos.
//! - [`transporte`]: o canal como `Transport` do PiStick.

#![forbid(unsafe_code)]

pub mod canal;
pub mod chaves;
pub mod noise;
pub mod transporte;

pub use canal::{ChannelError, PeerInfo, SecureReader, SecureWriter};
pub use noise::{Established, Keypair, NoiseError};
pub use transporte::SecureConn;

/// Zera um segredo em memória de um jeito que o compilador não pode descartar (`zeroize`).
pub fn wipe(b: &mut [u8]) {
    zeroize::Zeroize::zeroize(b);
}

/// Comparação em tempo constante (não para no primeiro byte diferente). Tamanhos diferentes: `false`.
pub fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut d = 0u8;
    for (x, y) in a.iter().zip(b) {
        d |= x ^ y;
    }
    std::hint::black_box(d) == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ct_eq_basico() {
        assert!(ct_eq(b"abc", b"abc"));
        assert!(!ct_eq(b"abc", b"abd"));
        assert!(!ct_eq(b"abc", b"ab"));
        assert!(ct_eq(b"", b""));
    }
}
