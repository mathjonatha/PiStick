
#![allow(unknown_lints, clippy::chunks_exact_to_as_chunks)]

use ps_audio::synth::{goertzel, split_stereo, AMPLITUDE};

fn main() {
    let Some(path) = std::env::args().nth(1) else {
        eprintln!("uso: verificar_wav ARQ.wav");
        std::process::exit(2);
    };
    let b = std::fs::read(&path).unwrap_or_else(|e| {
        eprintln!("não foi possível ler {path}: {e}");
        std::process::exit(2);
    });
    if b.len() < 44 || &b[0..4] != b"RIFF" || &b[36..40] != b"data" {
        eprintln!("{path}: não é o WAV do psc");
        std::process::exit(2);
    }
    let n = (u32::from_le_bytes([b[40], b[41], b[42], b[43]]) as usize).min(b.len() - 44);
    let x: Vec<i16> = b[44..44 + n].chunks_exact(2).map(|c| i16::from_le_bytes([c[0], c[1]])).collect();
    let frames = x.len() / 2;
    println!("{path}: {frames} quadros, {:.3} s", frames as f64 / 48000.0);

    let mut segs: Vec<(bool, usize, usize)> = Vec::new();
    let mut i = 0;
    while i < frames {
        let zero = |k: usize| x[k * 2] == 0 && x[k * 2 + 1] == 0;
        let mut j = i;
        if zero(i) {
            while j < frames && zero(j) {
                j += 1;
            }
            if j - i >= 480 || j == frames {
                segs.push((false, i, j));
            } else {
                match segs.last_mut() {
                    Some(last) if last.0 => last.2 = j,
                    _ => segs.push((true, i, j)),
                }
            }
        } else {
            while j < frames && !(zero(j) && (j + 480 > frames || (j..j + 480).all(zero))) {
                j += 1;
            }
            match segs.last_mut() {
                Some(last) if last.0 => last.2 = j,
                _ => segs.push((true, i, j)),
            }
        }
        i = j;
    }
    let mut m: Vec<(bool, usize, usize)> = Vec::new();
    for s in segs {
        match m.last_mut() {
            Some(l) if l.0 == s.0 => l.2 = s.2,
            _ => m.push(s),
        }
    }
    let mut ok = true;
    let ms = |q: usize| q as f64 / 48.0;
    for (k, (som, a, z)) in m.iter().enumerate() {
        let ultimo = k + 1 == m.len();
        let dur = ms(z - a);
        let mut nota = String::new();
        let esperado_1s = (dur - 1000.0).abs() <= 20.0;
        if !ultimo && !esperado_1s {
            nota = "  FALHOU: duração fora de 1000 ± 20 ms".to_string();
            ok = false;
        }
        if *som && !ultimo {
            let (l, r) = split_stereo(&x[a * 2..z * 2]);
            let (le, rd, ld, re) = (goertzel(&l, 440.0), goertzel(&r, 880.0), goertzel(&l, 880.0), goertzel(&r, 440.0));
            nota += &format!("  esq440={le:.3} dir880={rd:.3} vazamentos={ld:.3}/{re:.3}");
            if (le - AMPLITUDE).abs() > 0.02 || (rd - AMPLITUDE).abs() > 0.02 || ld > 0.02 || re > 0.02 {
                nota += "  FALHOU: frequências";
                ok = false;
            }
        }
        println!("{} {:8.1} ms a {:8.1} ms ({:7.1} ms){}{nota}", if *som { "som     " } else { "silêncio" }, ms(*a), ms(*z), dur, if ultimo { " (último, não conferido)" } else { "" });
    }
    if m.first().is_none_or(|s| !s.0) {
        println!("FALHOU: o arquivo deveria começar por som");
        ok = false;
    }
    if m.iter().filter(|s| s.0).count() < 2 {
        println!("FALHOU: menos de dois trechos de som");
        ok = false;
    }
    println!("{}", if ok { "WAV ok" } else { "WAV FALHOU" });
    std::process::exit(if ok { 0 } else { 1 });
}
