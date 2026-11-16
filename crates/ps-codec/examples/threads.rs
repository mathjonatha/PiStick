//! Mede o custo de criar threads por quadro e o limiar de blocos sujos a partir do qual
//! a codificação paralela compensa.
//!
//! `cargo run --release -p ps-codec --example threads [N_THREADS]`

use std::time::Instant;

use ps_codec::testutil::Rng;
use ps_codec::{Encoder, Rect, BLOCK};

const W: usize = 1920;
const H: usize = 1080;

fn mediana(mut v: Vec<f64>) -> f64 {
    v.sort_by(|a, b| a.partial_cmp(b).unwrap());
    v[v.len() / 2]
}

/// Conteúdo "de interface": fundo com traços de duas cores, diferente a cada chamada.
fn pinta_bloco(img: &mut [u8], bx: usize, by: usize, rng: &mut Rng) {
    for y in by * BLOCK..((by + 1) * BLOCK).min(H) {
        for x in bx * BLOCK..((bx + 1) * BLOCK).min(W) {
            let i = (y * W + x) * 4;
            let c = if rng.below(4) == 0 { [0xE6, 0xE6, 0xE6] } else { [0x3A, 0x34, 0x34] };
            img[i..i + 3].copy_from_slice(&c);
            img[i + 3] = 0xFF;
        }
    }
}

fn mede(threads: usize, k: usize, iters: usize) -> f64 {
    let mut rng = Rng::new(5);
    let mut img = vec![0x30u8; W * H * 4];
    let mut enc = Encoder::with_threads(W as u32, H as u32, threads);
    enc.set_parallel_min_blocks(0); // sempre paralelo quando threads > 1
    let mut out = Vec::new();
    enc.encode(&img, W * 4, None, &mut out);
    let bh = H.div_ceil(BLOCK);
    let mut tempos = Vec::new();
    for _ in 0..iters + 5 {
        // k blocos espalhados entre as linhas de blocos (faixas), para que haja o que repartir.
        let rects: Vec<Rect> = (0..k)
            .map(|i| {
                let (bx, by) = (i / bh, i % bh);
                pinta_bloco(&mut img, bx, by, &mut rng);
                Rect::new((bx * BLOCK) as u32, (by * BLOCK) as u32, BLOCK as u32, BLOCK as u32)
            })
            .collect();
        out.clear();
        let t = Instant::now();
        enc.encode(&img, W * 4, Some(&rects), &mut out);
        tempos.push(t.elapsed().as_secs_f64() * 1e6);
    }
    mediana(tempos[5..].to_vec())
}

fn main() {
    let n: usize = std::env::args().nth(1).and_then(|a| a.parse().ok()).unwrap_or_else(|| std::thread::available_parallelism().map_or(1, |n| n.get()));

    // Custo de criar e juntar `n` threads sem trabalho, por quadro.
    let mut t = Vec::new();
    for _ in 0..300 {
        let i = Instant::now();
        std::thread::scope(|s| {
            for _ in 0..n {
                s.spawn(|| std::hint::black_box(0));
            }
        });
        t.push(i.elapsed().as_secs_f64() * 1e6);
    }
    println!("Custo de `thread::scope` criando e juntando {n} threads vazias: mediana {:.0} µs por quadro\n", mediana(t));

    println!("Quadro 1920x1080, k blocos sujos (espalhados entre as faixas), tempo mediano de codificação em µs:\n");
    println!("| blocos sujos | 1 thread | {n} threads | melhor |");
    println!("|---|---|---|---|");
    for k in [2, 4, 8, 12, 16, 24, 32, 48, 64, 128, 256, 510] {
        let a = mede(1, k, 60);
        let b = mede(n, k, 60);
        println!("| {k} | {a:.0} | {b:.0} | {} |", if b < a { format!("{n} threads") } else { "1 thread".into() });
    }
}
