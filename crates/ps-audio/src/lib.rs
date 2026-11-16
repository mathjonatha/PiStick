
#![allow(unknown_lints, clippy::chunks_exact_to_as_chunks)]

pub mod psa1;
pub mod synth;
pub mod wavcheck;

pub use psa1::{decode, encode, DecodeError, EncodeError, MAX_FRAMES};

pub const SAMPLE_RATE: u32 = 48_000;

pub const CODEC_PCM: u8 = 0;
pub const CODEC_PSA1: u8 = 1;
pub const CODEC_OPUS_RESERVED: u8 = 2;

pub fn pcm_to_samples(bytes: &[u8], out: &mut Vec<i16>) {
    out.extend(bytes.chunks_exact(2).map(|c| i16::from_le_bytes([c[0], c[1]])));
}

pub fn samples_to_pcm(samples: &[i16], out: &mut Vec<u8>) {
    out.reserve(samples.len() * 2);
    for s in samples {
        out.extend_from_slice(&s.to_le_bytes());
    }
}

pub fn is_silent(samples: &[i16]) -> bool {
    samples.iter().all(|&s| s == 0)
}

#[cfg(test)]
mod tests;
