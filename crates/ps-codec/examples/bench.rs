
#![allow(unknown_lints, clippy::chunks_exact_to_as_chunks)]

use std::time::Instant;

use ps_codec::testutil::Rng;
use ps_codec::{Decoder, Encoder, Rect, BLOCK};

const W: usize = 1920;
const H: usize = 1080;
const ITER: usize = 40;

fn fill(img: &mut [u8], r: Rect, bgr: [u8; 3]) {
    for y in r.y as usize..(r.y + r.h) as usize {
        for x in r.x as usize..(r.x + r.w) as usize {
            let i = (y * W + x) * 4;
            img[i..i + 3].copy_from_slice(&bgr);
            img[i + 3] = 0xFF;
        }
    }
}

fn texto(img: &mut [u8], r: Rect, rng: &mut Rng, fg: [u8; 3], bg: [u8; 3]) {
    fill(img, r, bg);
    let mut y = r.y + 4;
    while y + 10 < r.y + r.h {
        let mut x = r.x + 6;
        while x + 8 < r.x + r.w {
            let largura = 3 + rng.below(40);
            if rng.below(5) != 0 {
                fill(img, Rect::new(x, y, largura.min(r.x + r.w - x - 2), 9), fg);
            }
            x += largura + 6;
        }
        y += 16;
    }
}

fn interface(rng: &mut Rng) -> Vec<u8> {
    let mut img = vec![0u8; W * H * 4];
    fill(&mut img, Rect::new(0, 0, W as u32, H as u32), [0x30, 0x2B, 0x2B]);
    fill(&mut img, Rect::new(0, 0, W as u32, 40), [0x22, 0x1E, 0x1E]);
    fill(&mut img, Rect::new(0, 40, 260, H as u32 - 40), [0x29, 0x24, 0x24]);
    texto(&mut img, Rect::new(10, 60, 240, 600), rng, [0xE6, 0xE6, 0xE6], [0x29, 0x24, 0x24]);
    for cy in 0..3u32 {
        for cx in 0..3u32 {
            let r = Rect::new(290 + cx * 540, 70 + cy * 330, 520, 310);
            fill(&mut img, r, [0xA8, 0x5F, 0x3A]);
            fill(&mut img, Rect::new(r.x + 3, r.y + 3, r.w - 6, r.h - 6), [0x3A, 0x34, 0x34]);
            texto(&mut img, Rect::new(r.x + 12, r.y + 12, r.w - 24, r.h - 24), rng, [0xE0, 0xE0, 0xE0], [0x3A, 0x34, 0x34]);
        }
    }
    for y in 0..128usize {
        for x in 0..256usize {
            let i = ((y + 100) * W + x + 1500) * 4;
            img[i] = x as u8;
            img[i + 1] = (y * 2) as u8;
            img[i + 2] = (x as u8) ^ (y as u8);
            img[i + 3] = 0xFF;
        }
    }
    img
}

fn degrade(phase: usize) -> Vec<u8> {
    let mut img = vec![0u8; W * H * 4];
    let blue: Vec<u8> = (0..W).map(|x| ((x * 255 / W + phase) & 0xFF) as u8).collect();
    for y in 0..H {
        let g = (((y * 255 / H) + phase / 2) & 0xF0) as u8;
        let r = (((y + phase) / 4) & 0xFF) as u8;
        for (px, &b) in img[y * W * 4..(y + 1) * W * 4].chunks_exact_mut(4).zip(&blue) {
            px.copy_from_slice(&[b, g, r, 0xFF]);
        }
    }
    img
}

fn ruido(rng: &mut Rng) -> Vec<u8> {
    let mut img = vec![0u8; W * H * 4];
    rng.fill(&mut img);
    img
}

fn histograma(data: &[u8]) -> [usize; 5] {
    let mut h = [0usize; 5];
    let mut pos = 0;
    for by in 0..H.div_ceil(BLOCK) {
        let bh = BLOCK.min(H - by * BLOCK);
        for bx in 0..W.div_ceil(BLOCK) {
            let bw = BLOCK.min(W - bx * BLOCK);
            let mode = data[pos];
            pos += 1;
            h[mode as usize] += 1;
            pos += match mode {
                0 => 0,
                1 => 3,
                2 => {
                    let n = data[pos] as usize;
                    let bits = if n <= 2 {
                        1
                    } else if n <= 4 {
                        2
                    } else {
                        4
                    };
                    1 + n * 3 + bh * (bw * bits).div_ceil(8)
                }
                3 => 4 + u32::from_le_bytes([data[pos], data[pos + 1], data[pos + 2], data[pos + 3]]) as usize,
                _ => bw * bh * 3,
            };
        }
    }
    h
}

struct Medida {
    nome: &'static str,
    enc_us: Vec<f64>,
    dec_us: Vec<f64>,
    bytes: usize,
    hist: [usize; 5],
}

fn stats(v: &[f64]) -> (f64, f64, f64) {
    let mut s = v.to_vec();
    s.sort_by(|a, b| a.partial_cmp(b).unwrap());
    (s[0], s[s.len() / 2], s.iter().sum::<f64>() / s.len() as f64)
}

fn quadro_inteiro(threads: usize, nome: &'static str, mut gera: impl FnMut(usize) -> Vec<u8>) -> Medida {
    let mut enc = Encoder::with_threads(W as u32, H as u32, threads);
    let mut dec = Decoder::new(W as u32, H as u32);
    let (mut e, mut d) = (Vec::new(), Vec::new());
    let mut bytes = 0;
    let mut hist = [0; 5];
    for i in 0..ITER + 3 {
        let img = gera(i);
        enc.reset(W as u32, H as u32);
        let mut out = Vec::with_capacity(W * H * 4);
        let t = Instant::now();
        enc.encode(&img, W * 4, None, &mut out);
        let te = t.elapsed().as_secs_f64() * 1e6;
        dec.reset(W as u32, H as u32);
        let t = Instant::now();
        dec.decode(&out).unwrap();
        let td = t.elapsed().as_secs_f64() * 1e6;
        if i >= 3 {
            e.push(te);
            d.push(td);
            bytes = out.len();
            hist = histograma(&out);
        }
    }
    Medida { nome, enc_us: e, dec_us: d, bytes, hist }
}

fn com_dano(threads: usize, nome: &'static str, rng: &mut Rng, base: Vec<u8>, conteudo: impl Fn(&mut [u8], Rect, &mut Rng)) -> Medida {
    let mut enc = Encoder::with_threads(W as u32, H as u32, threads);
    let mut dec = Decoder::new(W as u32, H as u32);
    let mut img = base;
    let mut out = Vec::new();
    enc.encode(&img, W * 4, None, &mut out);
    dec.decode(&out).unwrap();
    let rects: Vec<Rect> = (0..8).map(|i| Rect::new(40 + (i % 4) * 470, 90 + (i / 4) * 520, 114, 114)).collect();
    let area: u32 = rects.iter().map(|r| r.w * r.h).sum();
    assert!((area as f64 / (W * H) as f64 - 0.05).abs() < 0.002);
    let (mut e, mut d) = (Vec::new(), Vec::new());
    let mut bytes = 0;
    let mut hist = [0; 5];
    for i in 0..ITER + 3 {
        for &r in &rects {
            conteudo(&mut img, r, rng);
        }
        out.clear();
        let t = Instant::now();
        enc.encode(&img, W * 4, Some(&rects), &mut out);
        let te = t.elapsed().as_secs_f64() * 1e6;
        let t = Instant::now();
        dec.decode(&out).unwrap();
        let td = t.elapsed().as_secs_f64() * 1e6;
        if i >= 3 {
            e.push(te);
            d.push(td);
            bytes = out.len();
            hist = histograma(&out);
        }
    }
    Medida { nome, enc_us: e, dec_us: d, bytes, hist }
}

fn main() {
    let mut lista: Vec<usize> = std::env::args().skip(1).filter_map(|a| a.parse().ok()).collect();
    if lista.is_empty() {
        let n = std::thread::available_parallelism().map_or(1, |n| n.get());
        lista = if n > 1 { vec![1, n] } else { vec![1] };
    }
    let bruto = W * H * 4;
    println!("Quadro bruto BGRX: {} bytes ({:.2} MiB). {ITER} iterações por caso, depois de 3 de aquecimento.
", bruto, bruto as f64 / 1048576.0);
    let mut rng = Rng::new(2026);
    let ui = interface(&mut rng);

    for &threads in &lista {
        let mut r = vec![
            quadro_inteiro(threads, "interface, quadro inteiro", |_| ui.clone()),
            quadro_inteiro(threads, "degradê animado, quadro inteiro", |i| degrade(i * 4)),
        ];
        let mut rng2 = Rng::new(7);
        r.push(quadro_inteiro(threads, "ruído, quadro inteiro", |_| ruido(&mut rng2)));
        let mut rng3 = Rng::new(9);
        r.push(com_dano(threads, "interface, 5% de dano", &mut rng3, ui.clone(), |img, rect, rng| {
            let fg = if rng.below(2) == 0 { [0xE6, 0xE6, 0xE6] } else { [0x4C, 0xAF, 0x50] };
            texto(img, rect, rng, fg, [0x3A, 0x34, 0x34])
        }));
        let mut rng4 = Rng::new(11);
        r.push(com_dano(threads, "ruído, 5% de dano", &mut rng4, ui.clone(), |img, rect, rng| {
            for y in rect.y as usize..(rect.y + rect.h) as usize {
                let a = (y * W + rect.x as usize) * 4;
                rng.fill(&mut img[a..a + rect.w as usize * 4]);
            }
        }));
        println!("### {threads} thread(s)
");
        println!("| caso | codificar mín / mediana / média (ms) | decodificar mediana (ms) | bytes | razão | blocos SKIP/SOLID/PAL/LZ/RAW |");
        println!("|---|---|---|---|---|---|");
        for m in &r {
            let (emin, emed, emean) = stats(&m.enc_us);
            let (_, dmed, _) = stats(&m.dec_us);
            println!(
                "| {} | {:.2} / {:.2} / {:.2} | {:.2} | {} | {:.1}:1 | {}/{}/{}/{}/{} |",
                m.nome, emin / 1000.0, emed / 1000.0, emean / 1000.0, dmed / 1000.0, m.bytes,
                bruto as f64 / m.bytes as f64, m.hist[0], m.hist[1], m.hist[2], m.hist[3], m.hist[4]
            );
        }
        println!();
    }

    println!("### Tempo por fase, uma thread, quadro inteiro (ms por quadro; fases medidas separadas)
");
    println!("| caso | comparação (blocos iguais) | passada da paleta | pacote da paleta | BGRX→BGR (só LZ/RAW) | LZ | blocos sólido/paleta/LZ+RAW |");
    println!("|---|---|---|---|---|---|---|");
    let mut rng2 = Rng::new(7);
    let casos: Vec<(&str, Vec<u8>)> = vec![("interface", ui.clone()), ("degradê", degrade(40)), ("ruído", ruido(&mut rng2))];
    for (nome, img) in &casos {
        let mut melhor: Option<ps_codec::PhaseTimes> = None;
        for _ in 0..10 {
            let t = ps_codec::profile_phases(img, W, H);
            let tot = |t: &ps_codec::PhaseTimes| t.compare_ns + t.gather_ns + t.palette_ns + t.pack_ns + t.lz_ns;
            if melhor.as_ref().is_none_or(|m| tot(&t) < tot(m)) {
                melhor = Some(t);
            }
        }
        let t = melhor.unwrap();
        let ms = |n: u64| n as f64 / 1e6;
        println!(
            "| {nome} | {:.2} | {:.2} | {:.2} | {:.2} | {:.2} | {}/{}/{} |",
            ms(t.compare_ns), ms(t.palette_ns), ms(t.pack_ns), ms(t.gather_ns), ms(t.lz_ns), t.blocks_solid, t.blocks_palette, t.blocks_lz_or_raw
        );
    }
}
