
use crate::SAMPLE_RATE;

pub const FREQ_LEFT: f64 = 440.0;
pub const FREQ_RIGHT: f64 = 880.0;
pub const AMPLITUDE: f64 = 0.25;
pub const SEGMENT_FRAMES: u64 = SAMPLE_RATE as u64;

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

pub fn tone_frame(n: u64, freq_l: f64, freq_r: f64, amplitude: f64) -> (i16, i16) {
    let t = n as f64 / SAMPLE_RATE as f64;
    let v = |f: f64| (amplitude * (2.0 * std::f64::consts::PI * f * t).sin() * 32767.0).round() as i16;
    (v(freq_l), v(freq_r))
}

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

pub fn split_stereo(x: &[i16]) -> (Vec<i16>, Vec<i16>) {
    let mut l = Vec::with_capacity(x.len() / 2);
    let mut r = Vec::with_capacity(x.len() / 2);
    for f in x.chunks_exact(2) {
        l.push(f[0]);
        r.push(f[1]);
    }
    (l, r)
}
