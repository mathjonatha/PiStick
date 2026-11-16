
use crate::synth::{goertzel, split_stereo};

#[derive(Debug, Clone, PartialEq)]
pub struct Wav {
    pub channels: u16,
    pub rate: u32,
    pub samples: Vec<i16>,
}

pub fn parse_wav(b: &[u8]) -> Result<Wav, String> {
    if b.len() < 12 || &b[0..4] != b"RIFF" || &b[8..12] != b"WAVE" {
        return Err("não é um arquivo WAV (falta RIFF/WAVE)".into());
    }
    let mut pos = 12;
    let mut fmt: Option<(u16, u16, u32, u16)> = None;
    while pos + 8 <= b.len() {
        let id = &b[pos..pos + 4];
        let size = u32::from_le_bytes([b[pos + 4], b[pos + 5], b[pos + 6], b[pos + 7]]) as usize;
        let body = pos + 8;
        if id == b"fmt " {
            if size < 16 || body + 16 > b.len() {
                return Err("bloco fmt truncado".into());
            }
            let tag = u16::from_le_bytes([b[body], b[body + 1]]);
            let ch = u16::from_le_bytes([b[body + 2], b[body + 3]]);
            let rate = u32::from_le_bytes([b[body + 4], b[body + 5], b[body + 6], b[body + 7]]);
            let bits = u16::from_le_bytes([b[body + 14], b[body + 15]]);
            fmt = Some((tag, ch, rate, bits));
        } else if id == b"data" {
            let Some((tag, ch, rate, bits)) = fmt else { return Err("bloco data antes do fmt".into()) };
            if (tag != 1 && tag != 0xFFFE) || bits != 16 {
                return Err(format!("só PCM de 16 bits é aceito (formato {tag}, {bits} bits)"));
            }
            if ch == 0 {
                return Err("zero canais".into());
            }
            let end = body.saturating_add(size).min(b.len());
            let samples: Vec<i16> = b[body..end].chunks_exact(2).map(|c| i16::from_le_bytes([c[0], c[1]])).collect();
            let usable = samples.len() / ch as usize * ch as usize;
            return Ok(Wav { channels: ch, rate, samples: samples[..usable].to_vec() });
        }
        pos = body.saturating_add(size).saturating_add(size & 1);
    }
    Err("sem bloco data".into())
}

#[derive(Debug, Clone, PartialEq)]
pub struct ToneReport {
    pub frames: usize,
    pub seconds: f64,
    pub window: (f64, f64),
    pub left: f64,
    pub right: f64,
    pub coherent: (f64, f64),
    pub weak: usize,
    pub leak_left: f64,
    pub leak_right: f64,
    pub holes: usize,
    pub blocks: usize,
    pub problems: Vec<String>,
}

impl ToneReport {
    pub fn ok(&self) -> bool {
        self.problems.is_empty()
    }
}

pub fn verify_tones(w: &Wav, freq_l: f64, freq_r: f64, amplitude: f64) -> ToneReport {
    let rate = f64::from(w.rate.max(1));
    let frames = w.samples.len() / w.channels.max(1) as usize;
    let mut r = ToneReport { frames, seconds: frames as f64 / rate, window: (0.0, 0.0), left: 0.0, right: 0.0, coherent: (0.0, 0.0), weak: 0, leak_left: 0.0, leak_right: 0.0, holes: 0, blocks: 0, problems: Vec::new() };
    if w.channels != 2 {
        r.problems.push(format!("a gravação tem {} canais (esperado 2)", w.channels));
        return r;
    }
    if w.rate != 48000 {
        r.problems.push(format!("a gravação tem {} Hz (esperado 48000)", w.rate));
        return r;
    }
    let start = (0.5 * rate) as usize;
    let end = frames.saturating_sub((0.25 * rate) as usize);
    if end <= start + (rate as usize) {
        r.problems.push(format!("gravação curta demais ({:.2} s; precisa de pelo menos 1,75 s)", r.seconds));
        return r;
    }
    r.window = (start as f64 / rate, end as f64 / rate);
    let (l, rr) = split_stereo(&w.samples[start * 2..end * 2]);
    r.coherent = (goertzel(&l, freq_l), goertzel(&rr, freq_r));
    let block = (rate / 10.0) as usize;
    let mut amps_l: Vec<f64> = l.chunks_exact(block).map(|c| goertzel(c, freq_l)).collect();
    let mut amps_r: Vec<f64> = rr.chunks_exact(block).map(|c| goertzel(c, freq_r)).collect();
    r.blocks = amps_l.len();
    r.weak = amps_l.iter().zip(&amps_r).filter(|(a, b)| **a < amplitude * 0.9 || **b < amplitude * 0.9).count();
    r.holes = amps_l.iter().zip(&amps_r).filter(|(a, b)| **a < amplitude * 0.5 || **b < amplitude * 0.5).count();
    let median = |v: &mut Vec<f64>| {
        v.sort_by(|a, b| a.total_cmp(b));
        v.get(v.len() / 2).copied().unwrap_or(0.0)
    };
    r.left = median(&mut amps_l);
    r.right = median(&mut amps_r);
    let distintos = (freq_l - freq_r).abs() > f64::EPSILON;
    r.leak_left = if distintos { goertzel(&l, freq_r) } else { 0.0 };
    r.leak_right = if distintos { goertzel(&rr, freq_l) } else { 0.0 };
    let tol = amplitude * 0.05;
    if (r.left - amplitude).abs() > tol {
        r.problems.push(format!("canal esquerdo tem {:.4} em {freq_l} Hz (esperado {amplitude} ± 5%)", r.left));
    }
    if (r.right - amplitude).abs() > tol {
        r.problems.push(format!("canal direito tem {:.4} em {freq_r} Hz (esperado {amplitude} ± 5%)", r.right));
    }
    if r.leak_left > 0.02 {
        r.problems.push(format!("canal esquerdo vaza {:.4} em {freq_r} Hz", r.leak_left));
    }
    if r.leak_right > 0.02 {
        r.problems.push(format!("canal direito vaza {:.4} em {freq_l} Hz", r.leak_right));
    }
    if r.holes > 0 {
        r.problems.push(format!("{} de {} blocos de 100 ms com menos da metade da amplitude (buracos ou silêncio)", r.holes, r.blocks));
    }
    if r.weak * 20 > r.blocks {
        r.problems.push(format!("{} de {} blocos de 100 ms com menos de 90% da amplitude (mais de 5%)", r.weak, r.blocks));
    }
    r
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wav_bytes(rate: u32, ch: u16, samples: &[i16], extra_chunk: bool) -> Vec<u8> {
        let mut b = Vec::new();
        b.extend_from_slice(b"RIFF");
        b.extend_from_slice(&0u32.to_le_bytes());
        b.extend_from_slice(b"WAVE");
        b.extend_from_slice(b"fmt ");
        b.extend_from_slice(&16u32.to_le_bytes());
        b.extend_from_slice(&1u16.to_le_bytes());
        b.extend_from_slice(&ch.to_le_bytes());
        b.extend_from_slice(&rate.to_le_bytes());
        b.extend_from_slice(&(rate * u32::from(ch) * 2).to_le_bytes());
        b.extend_from_slice(&(ch * 2).to_le_bytes());
        b.extend_from_slice(&16u16.to_le_bytes());
        if extra_chunk {
            b.extend_from_slice(b"LIST");
            b.extend_from_slice(&3u32.to_le_bytes());
            b.extend_from_slice(&[1, 2, 3, 0]);
        }
        b.extend_from_slice(b"data");
        b.extend_from_slice(&((samples.len() * 2) as u32).to_le_bytes());
        for s in samples {
            b.extend_from_slice(&s.to_le_bytes());
        }
        b
    }

    fn tons(segundos: f64, fl: f64, fr: f64, amp: f64) -> Vec<i16> {
        let n = (segundos * 48000.0) as usize;
        let mut v = Vec::with_capacity(n * 2);
        for i in 0..n {
            let t = i as f64 / 48000.0;
            v.push((amp * (2.0 * std::f64::consts::PI * fl * t).sin() * 32767.0).round() as i16);
            v.push((amp * (2.0 * std::f64::consts::PI * fr * t).sin() * 32767.0).round() as i16);
        }
        v
    }

    #[test]
    fn le_wav_com_bloco_extra_e_tamanho_mentiroso() {
        let s = tons(0.1, 440.0, 880.0, 0.25);
        let w = parse_wav(&wav_bytes(48000, 2, &s, true)).unwrap();
        assert_eq!((w.channels, w.rate, w.samples.len()), (2, 48000, s.len()));
        let mut b = wav_bytes(48000, 2, &s, false);
        let pos = b.windows(4).position(|x| x == b"data").unwrap();
        b[pos + 4..pos + 8].copy_from_slice(&0xFFFF_FFFFu32.to_le_bytes());
        assert_eq!(parse_wav(&b).unwrap().samples, s);
    }

    #[test]
    fn recusa_entrada_invalida_sem_panico() {
        assert!(parse_wav(b"").is_err());
        assert!(parse_wav(b"RIFFxxxxWAVE").is_err());
        let b = wav_bytes(48000, 2, &[1, 2, 3, 4], false);
        for corte in 0..b.len() {
            let _ = parse_wav(&b[..corte]);
        }
        let mut c = b.clone();
        let pos = c.windows(4).position(|x| x == b"fmt ").unwrap();
        c[pos + 8 + 14] = 8;
        assert!(parse_wav(&c).is_err());
    }

    #[test]
    fn aprova_o_tom_certo_e_reprova_os_errados() {
        let w = Wav { channels: 2, rate: 48000, samples: tons(4.0, 440.0, 880.0, 0.25) };
        let r = verify_tones(&w, 440.0, 880.0, 0.25);
        assert!(r.ok(), "{:?}", r.problems);
        assert!((r.left - 0.25).abs() < 0.005 && (r.right - 0.25).abs() < 0.005);
        assert!(!verify_tones(&w, 880.0, 440.0, 0.25).ok());
        let baixa = Wav { channels: 2, rate: 48000, samples: tons(4.0, 440.0, 880.0, 0.225) };
        assert!(!verify_tones(&baixa, 440.0, 880.0, 0.25).ok());
        let quase = Wav { channels: 2, rate: 48000, samples: tons(4.0, 440.0, 880.0, 0.2425) };
        assert!(verify_tones(&quase, 440.0, 880.0, 0.25).ok());
        assert!(!verify_tones(&Wav { channels: 2, rate: 48000, samples: tons(1.0, 440.0, 880.0, 0.25) }, 440.0, 880.0, 0.25).ok());
        assert!(!verify_tones(&Wav { channels: 1, rate: 48000, samples: vec![0; 48000 * 3] }, 440.0, 880.0, 0.25).ok());
    }

    #[test]
    fn salto_de_fase_de_meio_milissegundo_nao_reprova_mas_e_informado() {
        let mut s = tons(4.0, 440.0, 880.0, 0.25);
        s.drain(96_000 * 2..(96_000 + 30) * 2);
        let r = verify_tones(&Wav { channels: 2, rate: 48000, samples: s }, 440.0, 880.0, 0.25);
        assert!(r.ok(), "{:?}", r.problems);
        assert!((r.left - 0.25).abs() < 0.0125 && (r.right - 0.25).abs() < 0.0125);
        assert!(r.coherent.0 < 0.2, "a soma coerente cai: {:?}", r.coherent);
        assert!(r.weak <= 2, "fracos {}", r.weak);
        let mut ruim = tons(4.0, 440.0, 880.0, 0.25);
        for k in (1..35).rev() {
            let at = (k * 4800 + 2400) * 2;
            ruim.drain(at..at + 20);
        }
        let r = verify_tones(&Wav { channels: 2, rate: 48000, samples: ruim }, 440.0, 880.0, 0.25);
        assert!(!r.ok() && r.weak * 20 > r.blocks, "{:?} fracos {}", r.problems, r.weak);
    }

    #[test]
    fn um_buraco_no_meio_e_apontado() {
        let mut s = tons(4.0, 440.0, 880.0, 0.25);
        for v in &mut s[96000 * 2..(96000 + 14400) * 2] {
            *v = 0;
        }
        let r = verify_tones(&Wav { channels: 2, rate: 48000, samples: s }, 440.0, 880.0, 0.25);
        assert!(!r.ok());
        assert!(r.holes >= 2, "buracos {}", r.holes);
    }
}
