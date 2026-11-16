//! Áudio do PiStick (marco 7): codec sem perdas PSA1 (predição linear fixa mais código de Rice, a
//! ideia do FLAC), gerador do sinal sintético e análise por Goertzel. Sem dependências e sem `unsafe`.
//! Formato de bits em `docs/05-protocolo.md`, seção "Extensão do marco 7: áudio".

// Lint novo do clippy do Rust 1.100 (Pi); o Rust 1.95 do notebook não o conhece como erro.
#![allow(unknown_lints, clippy::chunks_exact_to_as_chunks)]

pub mod psa1;
pub mod synth;
pub mod wavcheck;

pub use psa1::{decode, encode, DecodeError, EncodeError, MAX_FRAMES};

/// Taxa de amostragem única do fio.
pub const SAMPLE_RATE: u32 = 48_000;

/// Códigos do campo `codec` de `AUDIO_START`.
pub const CODEC_PCM: u8 = 0;
pub const CODEC_PSA1: u8 = 1;
pub const CODEC_OPUS_RESERVED: u8 = 2;

/// Converte bytes PCM S16LE em amostras. Bytes sobrando (menos que um par) são ignorados.
pub fn pcm_to_samples(bytes: &[u8], out: &mut Vec<i16>) {
    out.extend(bytes.chunks_exact(2).map(|c| i16::from_le_bytes([c[0], c[1]])));
}

/// Converte amostras em bytes PCM S16LE.
pub fn samples_to_pcm(samples: &[i16], out: &mut Vec<u8>) {
    out.reserve(samples.len() * 2);
    for s in samples {
        out.extend_from_slice(&s.to_le_bytes());
    }
}

/// `true` se todas as amostras são zero.
pub fn is_silent(samples: &[i16]) -> bool {
    samples.iter().all(|&s| s == 0)
}

#[cfg(test)]
mod tests;
