
#![forbid(unsafe_code)]

pub mod canal;
pub mod chaves;
pub mod noise;
pub mod transporte;

pub use canal::{ChannelError, PeerInfo, SecureReader, SecureWriter};
pub use noise::{Established, Keypair, NoiseError};
pub use transporte::SecureConn;

pub fn wipe(b: &mut [u8]) {
    zeroize::Zeroize::zeroize(b);
}

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
