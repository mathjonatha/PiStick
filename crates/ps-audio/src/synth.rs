//! Sinal sintético do marco 7 e análise de frequência (Goertzel), usados pelo servidor sintético e
//! pelo `verify-audio` do cliente.

use crate::SAMPLE_RATE;

/// Frequências do sintético: canal esquerdo e direito.
pub const FREQ_LEFT: f64 = 440.0;
pub const FREQ_RIGHT: f64 = 880.0;
/// Amplitude (fração da escala cheia) do sintético.
pub const AMPLITUDE: f64 = 0.25;
/// Duração, em quadros, de cada trecho (1 s de som, 1 s de silêncio).
pub const SEGMENT_FRAMES: u64 = SAMPLE_RATE as u64;

/// Quadro de índice `n` (desde o início do fluxo) do sintético: 1 s de som, 1 s de silêncio, repetindo.
/// A fase recomeça a cada trecho de som.
pub fn frame(n: u64) -> (i16, i16) {
    let seg = n / SEGMENT_FRAMES;
    if seg % 2 == 1 {
        return (0, 0);
    }
    let k = (n % SEGMENT_FRAMES) as f64;
    let t = k / SAMPLE_RATE as f64;
    let l = (AMPLITUDE * (2.0 * std::f64::consts::PI * FREQ_LEFT * t).sin() * 32767.0).round() as i16;
    let r = (AMPLITUDE * (2.0 * std::f64::consts::PI * FREQ_RIGHT * t).sin() * 32767.0).round() as i16;
    (l, r)
}

/// Quadro de índice `n` de um tom contínuo (sem silêncio): `freq_l` Hz no canal esquerdo e `freq_r` Hz no direito, com
/// `amplitude` (fração da escala cheia). Serve ao tom do roteiro do microfone (marco 7B).
pub fn tone_frame(n: u64, freq_l: f64, freq_r: f64, amplitude: f64) -> (i16, i16) {
    let t = n as f64 / SAMPLE_RATE as f64;
    let v = |f: f64| (amplitude * (2.0 * std::f64::consts::PI * f * t).sin() * 32767.0).round() as i16;
    (v(freq_l), v(freq_r))
}

/// Amplitude de pico (fração da escala cheia) da frequência `freq` em `x` (amostras de um canal).
/// Para uma senoide pura de amplitude A em `freq`, devolve cerca de A.
pub fn goertzel(x: &[i16], freq: f64) -> f64 {
    if x.is_empty() {
        return 0.0;
    }
    let w = 2.0 * std::f64::consts::PI * freq / SAMPLE_RATE as f64;
    let c = 2.0 * w.cos();
    let (mut a, mut b) = (0.0f64, 0.0f64);
    for &v in x {
        let s = v as f64 / 32768.0 + c * a - b;
        b = a;
        a = s;
    }
    (a * a + b * b - c * a * b).max(0.0).sqrt() / x.len() as f64 * 2.0
}

/// Separa um bloco intercalado estéreo nos dois canais.
pub fn split_stereo(x: &[i16]) -> (Vec<i16>, Vec<i16>) {
    let mut l = Vec::with_capacity(x.len() / 2);
    let mut r = Vec::with_capacity(x.len() / 2);
    for f in x.chunks_exact(2) {
        l.push(f[0]);
        r.push(f[1]);
    }
    (l, r)
}
