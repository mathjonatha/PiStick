use super::*;
use crate::testutil::{normalize_x, Rng};

fn img_solida(w: usize, h: usize, bgr: [u8; 3]) -> Vec<u8> {
    let mut v = vec![0u8; w * h * 4];
    for px in v.chunks_exact_mut(4) {
        px[..3].copy_from_slice(&bgr);
        px[3] = 0x00;
    }
    v
}

fn img_aleatoria(w: usize, h: usize, rng: &mut Rng) -> Vec<u8> {
    let mut v = vec![0u8; w * h * 4];
    rng.fill(&mut v);
    v
}

fn img_poucas_cores(w: usize, h: usize, k: usize, rng: &mut Rng) -> Vec<u8> {
    let cores: Vec<[u8; 3]> = (0..k).map(|_| [rng.next_u64() as u8, rng.next_u64() as u8, rng.next_u64() as u8]).collect();
    let mut v = vec![0u8; w * h * 4];
    for px in v.chunks_exact_mut(4) {
        let c = cores[rng.below(k as u32) as usize];
        px[..3].copy_from_slice(&c);
        px[3] = rng.next_u64() as u8;
    }
    v
}

fn img_degrade(w: usize, h: usize) -> Vec<u8> {
    let mut v = vec![0u8; w * h * 4];
    for y in 0..h {
        for x in 0..w {
            let i = (y * w + x) * 4;
            v[i] = (x * 255 / w.max(1)) as u8;
            v[i + 1] = (y * 255 / h.max(1)) as u8;
            v[i + 2] = ((x + y) * 255 / (w + h).max(1)) as u8;
            v[i + 3] = 0xFF;
        }
    }
    v
}

fn pinta(img: &mut [u8], w: usize, r: Rect, rng: &mut Rng, ruido: bool) {
    let cor = [rng.next_u64() as u8, rng.next_u64() as u8, rng.next_u64() as u8];
    for y in r.y as usize..(r.y + r.h) as usize {
        for x in r.x as usize..(r.x + r.w) as usize {
            let i = (y * w + x) * 4;
            if ruido {
                img[i] = rng.next_u64() as u8;
                img[i + 1] = rng.next_u64() as u8;
                img[i + 2] = rng.next_u64() as u8;
            } else {
                img[i..i + 3].copy_from_slice(&cor);
            }
            img[i + 3] = rng.next_u64() as u8;
        }
    }
}

fn igual_normalizado(a: &[u8], b: &[u8]) -> bool {
    let mut a = a.to_vec();
    let mut b = b.to_vec();
    normalize_x(&mut a);
    normalize_x(&mut b);
    a == b
}

fn passo_rects(enc: &mut Encoder, dec: &mut Decoder, w: usize, frame: &[u8], damage: Option<&[Rect]>) -> (Vec<u8>, Vec<Rect>) {
    let mut out = Vec::new();
    enc.encode(frame, w * 4, damage, &mut out);
    let rects = dec.decode(&out).expect("decodificação");
    assert!(igual_normalizado(dec.pixels(), frame), "pixels diferentes ({}x{})", w, frame.len() / 4 / w);
    (out, rects)
}

fn passo(enc: &mut Encoder, dec: &mut Decoder, w: usize, frame: &[u8], damage: Option<&[Rect]>) -> Vec<u8> {
    passo_rects(enc, dec, w, frame, damage).0
}

fn ida_e_volta_inteiro(w: usize, h: usize, frame: &[u8]) -> Vec<u8> {
    let mut enc = Encoder::new(w as u32, h as u32);
    let mut dec = Decoder::new(w as u32, h as u32);
    let out = passo(&mut enc, &mut dec, w, frame, None);
    let out2 = passo(&mut enc, &mut dec, w, frame, None);
    assert_eq!(out2.len(), blocks_across(w) * blocks_across(h));
    assert!(out2.iter().all(|&m| m == MODE_SKIP));
    out
}

const TAMANHOS: &[(usize, usize)] = &[(1, 1), (3, 2), (63, 63), (64, 64), (65, 70), (130, 129), (200, 17), (1920, 1080)];

#[test]
fn ida_e_volta_aleatoria() {
    let mut rng = Rng::new(1);
    for &(w, h) in TAMANHOS {
        let img = img_aleatoria(w, h, &mut rng);
        ida_e_volta_inteiro(w, h, &img);
    }
}

#[test]
fn ida_e_volta_solida() {
    for &(w, h) in TAMANHOS {
        let img = img_solida(w, h, [10, 200, 33]);
        let out = ida_e_volta_inteiro(w, h, &img);
        assert_eq!(out.len(), blocks_across(w) * blocks_across(h) * 4);
    }
}

#[test]
fn ida_e_volta_poucas_cores() {
    let mut rng = Rng::new(2);
    for &(w, h) in TAMANHOS {
        for k in [2, 3, 4, 5, 16] {
            let img = img_poucas_cores(w, h, k, &mut rng);
            ida_e_volta_inteiro(w, h, &img);
        }
    }
}

#[test]
fn ida_e_volta_degrade() {
    for &(w, h) in TAMANHOS {
        let img = img_degrade(w, h);
        ida_e_volta_inteiro(w, h, &img);
    }
}

#[test]
fn modo_escolhido_por_tipo_de_bloco() {
    let mut rng = Rng::new(3);
    let modo = |img: &[u8]| {
        let mut enc = Encoder::new(64, 64);
        let mut out = Vec::new();
        enc.encode(img, 64 * 4, None, &mut out);
        out[0]
    };
    assert_eq!(modo(&img_solida(64, 64, [1, 2, 3])), MODE_SOLID);
    assert_eq!(modo(&img_poucas_cores(64, 64, 2, &mut rng)), MODE_PALETTE);
    assert_eq!(modo(&img_poucas_cores(64, 64, 16, &mut rng)), MODE_PALETTE);
    let mut faixa = vec![0u8; 64 * 64 * 4];
    for (i, px) in faixa.chunks_exact_mut(4).enumerate() {
        px[..3].copy_from_slice(&[(i % 64) as u8 * 4, 100, 200 - (i % 64) as u8]);
    }
    assert_eq!(modo(&faixa), MODE_LZ);
    assert_eq!(modo(&img_aleatoria(64, 64, &mut rng)), MODE_RAW);
    assert_ne!(modo(&img_poucas_cores(64, 64, 40, &mut rng)), MODE_PALETTE);
}

#[test]
fn tamanho_da_paleta() {
    let mut rng = Rng::new(4);
    for (k, bits) in [(2usize, 1usize), (3, 2), (4, 2), (5, 4), (16, 4)] {
        let mut img = vec![0u8; 64 * 64 * 4];
        for (i, px) in img.chunks_exact_mut(4).enumerate() {
            let c = if i < k { i } else { rng.below(k as u32) as usize };
            px[..3].copy_from_slice(&[c as u8 * 10, 7, 9]);
        }
        let mut enc = Encoder::new(64, 64);
        let mut out = Vec::new();
        enc.encode(&img, 256, None, &mut out);
        assert_eq!(out[0], MODE_PALETTE);
        let n = out[1] as usize;
        assert!(n == k && palette_bits(n) == bits, "k={k} n={n}");
        assert_eq!(out.len(), 2 + n * 3 + 64 * (64 * bits / 8));
    }
}

#[test]
fn stride_maior_que_a_linha() {
    let mut rng = Rng::new(5);
    let (w, h, stride) = (70usize, 66usize, 70 * 4 + 24);
    let mut buf = vec![0u8; stride * h];
    rng.fill(&mut buf);
    let mut enc = Encoder::new(w as u32, h as u32);
    let mut dec = Decoder::new(w as u32, h as u32);
    let mut out = Vec::new();
    enc.encode(&buf, stride, None, &mut out);
    dec.decode(&out).unwrap();
    for y in 0..h {
        let a = &buf[y * stride..y * stride + w * 4];
        let b = &dec.pixels()[y * w * 4..(y + 1) * w * 4];
        assert!(igual_normalizado(a, b), "linha {y}");
    }
}

#[test]
fn sequencia_com_dano_parcial() {
    let mut rng = Rng::new(6);
    for &(w, h) in &[(65usize, 70usize), (300, 200), (1920, 1080)] {
        let mut img = img_degrade(w, h);
        let mut enc = Encoder::new(w as u32, h as u32);
        let mut dec = Decoder::new(w as u32, h as u32);
        let primeiro = passo(&mut enc, &mut dec, w, &img, None);
        let mut menor = usize::MAX;
        let rodadas = if w * h > 100_000 { 6 } else { 40 };
        for _ in 0..rodadas {
            let anterior = img.clone();
            let n = 1 + rng.below(3);
            let mut dano = Vec::new();
            for _ in 0..n {
                let rw = 1 + rng.below(w as u32 / 3 + 1);
                let rh = 1 + rng.below(h as u32 / 3 + 1);
                let rx = rng.below(w as u32 - rw + 1);
                let ry = rng.below(h as u32 - rh + 1);
                let r = Rect::new(rx, ry, rw, rh);
                let ruido = rng.below(2) == 0;
                pinta(&mut img, w, r, &mut rng, ruido);
                dano.push(r);
            }
            let (out, rects) = passo_rects(&mut enc, &mut dec, w, &img, Some(&dano));
            menor = menor.min(out.len());
            for y in 0..h {
                for x in 0..w {
                    let i = (y * w + x) * 4;
                    if anterior[i..i + 3] != img[i..i + 3] {
                        let coberto = rects.iter().any(|r| {
                            (x as u32) >= r.x && (x as u32) < r.x + r.w && (y as u32) >= r.y && (y as u32) < r.y + r.h
                        });
                        assert!(coberto, "pixel ({x},{y}) mudou fora dos retângulos devolvidos");
                    }
                }
            }
        }
        assert!(menor < primeiro.len(), "quadros com pouco dano devem ser menores que o inteiro");
    }
}

#[test]
fn dano_vazio_gera_so_skips() {
    let mut rng = Rng::new(7);
    let (w, h) = (200usize, 100usize);
    let mut img = img_aleatoria(w, h, &mut rng);
    let mut enc = Encoder::new(w as u32, h as u32);
    let mut dec = Decoder::new(w as u32, h as u32);
    passo(&mut enc, &mut dec, w, &img, None);
    let mut out = Vec::new();
    pinta(&mut img, w, Rect::new(0, 0, 10, 10), &mut rng, true);
    enc.encode(&img, w * 4, Some(&[]), &mut out);
    assert_eq!(out.len(), blocks_across(w) * blocks_across(h));
    assert!(out.iter().all(|&m| m == MODE_SKIP));
    let mut out2 = Vec::new();
    enc.encode(&img, w * 4, None, &mut out2);
    dec.decode(&out).unwrap();
    dec.decode(&out2).unwrap();
    assert!(igual_normalizado(dec.pixels(), &img));
}

#[test]
fn alteracao_so_no_byte_x_vira_skip() {
    let (w, h) = (100usize, 80usize);
    let mut img = img_degrade(w, h);
    let mut enc = Encoder::new(w as u32, h as u32);
    let mut out = Vec::new();
    enc.encode(&img, w * 4, None, &mut out);
    for px in img.chunks_exact_mut(4) {
        px[3] = 0x00;
    }
    out.clear();
    enc.encode(&img, w * 4, None, &mut out);
    assert!(out.iter().all(|&m| m == MODE_SKIP));
}

#[test]
fn primeiro_quadro_depois_do_reset_nao_tem_skip() {
    let (w, h) = (130usize, 70usize);
    let img = img_solida(w, h, [0, 0, 0]);
    let mut enc = Encoder::new(w as u32, h as u32);
    let mut out = Vec::new();
    enc.encode(&img, w * 4, Some(&[]), &mut out);
    assert_eq!(out.len(), blocks_across(w) * blocks_across(h) * 4);
    assert!(out.chunks(4).all(|b| b[0] == MODE_SOLID));
    let mut dec = Decoder::new(w as u32, h as u32);
    dec.decode(&out).unwrap();
    enc.reset(64, 64);
    let img2 = img_solida(64, 64, [9, 9, 9]);
    out.clear();
    enc.encode(&img2, 256, None, &mut out);
    assert_eq!(out, vec![MODE_SOLID, 9, 9, 9]);
    dec.reset(64, 64);
    dec.decode(&out).unwrap();
    assert!(igual_normalizado(dec.pixels(), &img2));
}

#[test]
fn dano_fora_do_quadro_e_ignorado() {
    let (w, h) = (100usize, 100usize);
    let mut rng = Rng::new(8);
    let mut img = img_aleatoria(w, h, &mut rng);
    let mut enc = Encoder::new(w as u32, h as u32);
    let mut dec = Decoder::new(w as u32, h as u32);
    passo(&mut enc, &mut dec, w, &img, None);
    pinta(&mut img, w, Rect::new(90, 90, 10, 10), &mut rng, false);
    let dano = [Rect::new(500, 500, 10, 10), Rect::new(90, 90, 4_000_000_000, 4_000_000_000), Rect::new(3, 3, 0, 9)];
    passo(&mut enc, &mut dec, w, &img, Some(&dano));
}

#[test]
fn decoder_com_dados_truncados_nunca_entra_em_panico() {
    let mut rng = Rng::new(9);
    let (w, h) = (256usize, 64usize);
    let mut img = vec![0u8; w * h * 4];
    let blocos = [
        img_solida(64, 64, [1, 2, 3]),
        img_poucas_cores(64, 64, 3, &mut rng),
        img_degrade(64, 64),
        img_aleatoria(64, 64, &mut rng),
    ];
    for (b, bl) in blocos.iter().enumerate() {
        for y in 0..64 {
            img[(y * w + b * 64) * 4..(y * w + b * 64 + 64) * 4].copy_from_slice(&bl[y * 256..(y + 1) * 256]);
        }
    }
    let mut enc = Encoder::new(w as u32, h as u32);
    let mut out = Vec::new();
    enc.encode(&img, w * 4, None, &mut out);
    let mut dec = Decoder::new(w as u32, h as u32);
    dec.decode(&out).unwrap();
    assert!(igual_normalizado(dec.pixels(), &img));

    for corte in 0..out.len() {
        let mut d = Decoder::new(w as u32, h as u32);
        assert!(d.decode(&out[..corte]).is_err(), "corte {corte} deveria falhar");
    }
    let mut mais = out.clone();
    mais.push(0);
    assert_eq!(Decoder::new(w as u32, h as u32).decode(&mais), Err(DecodeError::TrailingData));
}

#[test]
fn decoder_com_dados_corrompidos_nunca_entra_em_panico() {
    let mut rng = Rng::new(10);
    let (w, h) = (130usize, 70usize);
    let mut img = img_degrade(w, h);
    pinta(&mut img, w, Rect::new(0, 0, 64, 64), &mut rng, true);
    pinta(&mut img, w, Rect::new(64, 0, 64, 64), &mut rng, false);
    let mut enc = Encoder::new(w as u32, h as u32);
    let mut out = Vec::new();
    enc.encode(&img, w * 4, None, &mut out);
    for _ in 0..20_000 {
        let mut x = out.clone();
        for _ in 0..1 + rng.below(4) {
            let p = rng.below(x.len() as u32) as usize;
            x[p] = rng.next_u64() as u8;
        }
        let mut d = Decoder::new(w as u32, h as u32);
        let _ = d.decode(&x);
    }
    for _ in 0..5_000 {
        let mut x = vec![0u8; rng.below(300) as usize];
        rng.fill(&mut x);
        let mut d = Decoder::new(w as u32, h as u32);
        let _ = d.decode(&x);
    }
}

#[test]
fn erros_especificos() {
    let mut d = Decoder::new(64, 64);
    assert_eq!(d.decode(&[9]), Err(DecodeError::BadMode(9)));
    assert_eq!(d.decode(&[MODE_PALETTE, 1, 0, 0, 0]), Err(DecodeError::BadPalette(1)));
    assert_eq!(d.decode(&[MODE_PALETTE, 17]), Err(DecodeError::BadPalette(17)));
    let mut x = vec![MODE_PALETTE, 3, 0, 0, 0, 1, 1, 1, 2, 2, 2];
    x.extend(std::iter::repeat_n(0b11, 64 * 16));
    assert_eq!(d.decode(&x), Err(DecodeError::BadPaletteIndex));
    assert_eq!(d.decode(&[MODE_LZ, 255, 255, 0, 0, 1]), Err(DecodeError::Truncated));
    assert_eq!(d.decode(&[MODE_LZ, 255, 255, 255, 255, 1]), Err(DecodeError::Truncated));
    assert_eq!(d.decode(&[]), Err(DecodeError::Truncated));
}

#[test]
fn rect_uniao_e_intersecao() {
    let a = Rect::new(0, 0, 10, 10);
    let b = Rect::new(5, 5, 10, 10);
    assert_eq!(a.union(b), Rect::new(0, 0, 15, 15));
    assert_eq!(a.intersect(b), Some(Rect::new(5, 5, 5, 5)));
    assert_eq!(a.intersect(Rect::new(10, 0, 5, 5)), None);
    assert_eq!(Rect::default().union(b), b);
}

#[test]
fn threads_geram_bytes_identicos() {
    let mut rng = Rng::new(21);
    for &(w, h) in &[(1usize, 1usize), (65, 70), (200, 300), (130, 129), (640, 480), (1920, 1080)] {
        let mut encs: Vec<Encoder> = [1usize, 2, 4, 7]
            .iter()
            .map(|&t| {
                let mut e = Encoder::with_threads(w as u32, h as u32, t);
                e.set_parallel_min_blocks(0);
                e
            })
            .collect();
        assert_eq!(encs[2].threads(), 4);
        let mut img = img_poucas_cores(w, h, 5, &mut rng);
        pinta(&mut img, w, Rect::new(0, 0, (w / 2) as u32, (h / 3) as u32), &mut rng, true);
        for rodada in 0..6 {
            let dano: Vec<Rect> = if rodada == 0 {
                Vec::new()
            } else {
                let rw = 1 + rng.below(w as u32 / 2 + 1);
                let rh = 1 + rng.below(h as u32 / 2 + 1);
                let r = Rect::new(rng.below(w as u32 - rw + 1), rng.below(h as u32 - rh + 1), rw, rh);
                let ruido = rng.below(2) == 0;
                pinta(&mut img, w, r, &mut rng, ruido);
                vec![r]
            };
            let damage = if rodada == 0 { None } else { Some(dano.as_slice()) };
            let saidas: Vec<Vec<u8>> = encs
                .iter_mut()
                .map(|e| {
                    let mut o = Vec::new();
                    e.encode(&img, w * 4, damage, &mut o);
                    o
                })
                .collect();
            for s in &saidas[1..] {
                assert!(*s == saidas[0], "bytes diferentes ({w}x{h}, rodada {rodada})");
            }
            let mut dec = Decoder::new(w as u32, h as u32);
            if rodada == 0 {
                dec.decode(&saidas[0]).unwrap();
                assert!(igual_normalizado(dec.pixels(), &img));
            }
        }
    }
}

#[test]
fn fatias_em_sequencia_equivalem_ao_quadro_inteiro() {
    let mut rng = Rng::new(31);
    for &(w, h) in &[(65usize, 70usize), (300, 400), (1920, 1080)] {
        let mut a = img_degrade(w, h);
        pinta(&mut a, w, Rect::new(0, 0, w as u32 / 2, h as u32 / 2), &mut rng, true);
        let mut b = a.clone();
        let dano = [Rect::new(0, 0, w as u32, h as u32 / 2 + 3), Rect::new(w as u32 / 3, h as u32 / 2, w as u32 / 3, h as u32 / 2)];
        for r in dano {
            let ruido = rng.below(2) == 0;
            pinta(&mut b, w, r, &mut rng, ruido);
        }

        let mut inteiro = Encoder::with_threads(w as u32, h as u32, 1);
        let mut dec_inteiro = Decoder::new(w as u32, h as u32);
        passo(&mut inteiro, &mut dec_inteiro, w, &a, None);
        passo(&mut inteiro, &mut dec_inteiro, w, &b, Some(&dano));

        let mut fatiado: Vec<Encoder> = [1usize, 4].iter().map(|&t| {
            let mut e = Encoder::with_threads(w as u32, h as u32, t);
            e.set_parallel_min_blocks(0);
            e
        }).collect();
        let mut decs: Vec<Decoder> = (0..2).map(|_| Decoder::new(w as u32, h as u32)).collect();
        for (e, d) in fatiado.iter_mut().zip(decs.iter_mut()) {
            passo(e, d, w, &a, None);
        }
        let n = fatiado[0].strip_count();
        let mut ini = 0;
        while ini < n {
            let fim = (ini + 2).min(n);
            let mut saidas = Vec::new();
            for (e, d) in fatiado.iter_mut().zip(decs.iter_mut()) {
                let mut o = Vec::new();
                e.encode_range(&b, w * 4, Some(&dano), ini..fim, &mut o);
                assert!(o.len() >= blocks_across(w) * blocks_across(h));
                d.decode(&o).unwrap();
                saidas.push(o);
            }
            assert!(saidas[0] == saidas[1], "1 e 4 threads divergem na fatia {ini}..{fim}");
            ini = fim;
        }
        for d in &decs {
            assert!(igual_normalizado(d.pixels(), dec_inteiro.pixels()), "fatias != quadro inteiro ({w}x{h})");
            assert!(igual_normalizado(d.pixels(), &b));
        }
    }
}

#[test]
fn fatia_nao_atualiza_a_referencia_das_faixas_nao_enviadas() {
    let (w, h) = (128usize, 256usize);
    let mut rng = Rng::new(32);
    let a = img_solida(w, h, [1, 2, 3]);
    let mut b = a.clone();
    pinta(&mut b, w, Rect::new(0, 0, 128, 256), &mut rng, true);
    let mut enc = Encoder::with_threads(w as u32, h as u32, 1);
    let mut o = Vec::new();
    enc.encode(&a, w * 4, None, &mut o);
    let mut o1 = Vec::new();
    let n1 = enc.encode_range(&b, w * 4, None, 0..1, &mut o1);
    assert_eq!(n1, 2);
    assert!(o1[o1.len() - 6..].iter().all(|&m| m == MODE_SKIP) || o1.iter().rev().take(6).all(|&m| m == MODE_SKIP));
    let mut o2 = Vec::new();
    let n2 = enc.encode(&b, w * 4, None, &mut o2);
    assert_eq!(n2, 6);
}

#[test]
fn sem_referencia_a_fatia_vira_quadro_inteiro() {
    let (w, h) = (128usize, 256usize);
    let img = img_solida(w, h, [9, 9, 9]);
    let mut enc = Encoder::with_threads(w as u32, h as u32, 1);
    let mut o = Vec::new();
    let n = enc.encode_range(&img, w * 4, Some(&[]), 1..2, &mut o);
    assert_eq!(n, 8, "o primeiro quadro não pode ter SKIP");
    assert!(o.chunks(4).all(|b| b[0] == MODE_SOLID));
}

#[test]
fn contagem_de_blocos_sujos_por_faixa() {
    let c = dirty_blocks_per_strip(Some(&[Rect::new(10, 10, 100, 10), Rect::new(0, 130, 200, 5)]), 200, 200);
    assert_eq!(c, vec![2, 0, 4, 0]);
    assert_eq!(dirty_blocks_per_strip(None, 130, 70), vec![3, 3]);
    assert!(dirty_blocks_per_strip(None, 0, 5).is_empty());
}

fn marcados(flags: &[bool]) -> Vec<usize> {
    flags.iter().enumerate().filter(|(_, &f)| f).map(|(i, _)| i).collect()
}

#[test]
fn blocos_codificados_marcam_so_os_que_nao_sairam_skip() {
    let (w, h) = (320usize, 256usize);
    for threads in [1usize, 3] {
        let mut rng = Rng::new(40);
        let a = img_aleatoria(w, h, &mut rng);
        let mut enc = Encoder::with_threads(w as u32, h as u32, threads);
        enc.set_parallel_min_blocks(0);
        assert_eq!((enc.block_cols(), enc.block_rows()), (5, 4));
        assert_eq!(enc.coded_blocks().len(), 20);
        assert!(enc.coded_blocks().iter().all(|&c| !c), "antes do primeiro quadro nada foi codificado");
        let mut o = Vec::new();
        enc.encode(&a, w * 4, None, &mut o);
        assert!(enc.coded_blocks().iter().all(|&c| c), "o primeiro quadro não tem SKIP");

        let mut b = a.clone();
        pinta(&mut b, w, Rect::new(2 * 64, 64, 64, 64), &mut rng, true);
        pinta(&mut b, w, Rect::new(3 * 64, 2 * 64, 64, 64), &mut rng, true);
        let dano = [Rect::new(2 * 64, 64, 2 * 64, 64), Rect::new(3 * 64, 2 * 64, 64, 64)];
        let mut o = Vec::new();
        let n = enc.encode(&b, w * 4, Some(&dano), &mut o);
        assert_eq!(n, 2);
        assert_eq!(marcados(enc.coded_blocks()), vec![7, 13], "{threads} threads");

        enc.encode(&b, w * 4, None, &mut Vec::new());
        assert!(marcados(enc.coded_blocks()).is_empty());

        let mut c = b.clone();
        pinta(&mut c, w, Rect::new(0, 0, w as u32, h as u32), &mut rng, true);
        enc.encode_range(&c, w * 4, None, 1..3, &mut Vec::new());
        assert_eq!(marcados(enc.coded_blocks()), (5..15).collect::<Vec<_>>());
        enc.reset(w as u32, h as u32);
        assert!(marcados(enc.coded_blocks()).is_empty(), "reset zera os flags");
    }
}

#[test]
fn blocos_tentados_sao_os_sujos_dentro_das_faixas_e_incluem_os_que_sairam_skip() {
    let (w, h) = (320usize, 256usize);
    let mut rng = Rng::new(41);
    let a = img_aleatoria(w, h, &mut rng);
    let mut enc = Encoder::with_threads(w as u32, h as u32, 1);
    assert!(enc.attempted_blocks().is_empty() || enc.attempted_blocks().iter().all(|&t| !t));
    enc.encode(&a, w * 4, None, &mut Vec::new());
    assert_eq!(enc.attempted_blocks().len(), 20);
    assert!(enc.attempted_blocks().iter().all(|&t| t), "o primeiro quadro tenta tudo");

    let mut b = a.clone();
    pinta(&mut b, w, Rect::new(2 * 64, 64, 64, 64), &mut rng, true);
    let dano = [Rect::new(2 * 64, 64, 2 * 64, 64)];
    enc.encode(&b, w * 4, Some(&dano), &mut Vec::new());
    assert_eq!(marcados(enc.coded_blocks()), vec![7]);
    assert_eq!(marcados(enc.attempted_blocks()), vec![7, 8]);

    let mut c = b.clone();
    pinta(&mut c, w, Rect::new(0, 0, w as u32, h as u32), &mut rng, true);
    enc.encode_range(&c, w * 4, None, 1..3, &mut Vec::new());
    assert_eq!(marcados(enc.attempted_blocks()), (5..15).collect::<Vec<_>>());
    assert!(enc.coded_blocks().iter().zip(enc.attempted_blocks()).all(|(&c, &t)| !c || t));
    enc.set_excluded(Some(Rect::new(64, 64, 128, 128)));
    enc.encode(&c, w * 4, None, &mut Vec::new());
    assert!(![6usize, 7, 11, 12].iter().any(|&i| enc.attempted_blocks()[i]));
    enc.reset(w as u32, h as u32);
    assert!(marcados(enc.attempted_blocks()).is_empty(), "reset zera os flags");
}

const REGIAO: Rect = Rect::new(64, 64, 128, 128);

fn indices_da_regiao() -> Vec<usize> {
    vec![6, 7, 11, 12]
}

#[test]
fn bloco_excluido_sai_skip_e_nao_toca_a_referencia() {
    let (w, h) = (320usize, 256usize);
    for threads in [1usize, 3] {
        let mut rng = Rng::new(41);
        let a = img_aleatoria(w, h, &mut rng);
        let mut enc = Encoder::with_threads(w as u32, h as u32, threads);
        enc.set_parallel_min_blocks(0);
        let mut dec = Decoder::new(w as u32, h as u32);
        passo(&mut enc, &mut dec, w, &a, None);

        enc.set_excluded(Some(REGIAO));
        let mut b = a.clone();
        pinta(&mut b, w, Rect::new(0, 0, w as u32, h as u32), &mut rng, true);
        assert_eq!(enc.dirty_per_strip(None).iter().sum::<usize>(), 16, "a estimativa já desconta a região");
        let mut o = Vec::new();
        let n = enc.encode(&b, w * 4, None, &mut o);
        assert_eq!(n, 16);
        let esperado: Vec<usize> = (0..20).filter(|i| !indices_da_regiao().contains(i)).collect();
        assert_eq!(marcados(enc.coded_blocks()), esperado, "{threads} threads");
        let rects = dec.decode(&o).unwrap();
        let mut a_norm = a.clone();
        normalize_x(&mut a_norm);
        for y in 64..192usize {
            assert_eq!(&dec.pixels()[(y * w + 64) * 4..(y * w + 192) * 4], &a_norm[(y * w + 64) * 4..(y * w + 192) * 4], "linha {y}");
        }
        assert!(rects.iter().all(|r| r.intersect(REGIAO).is_none()), "{rects:?}");

        let mut c = b.clone();
        pinta(&mut c, w, Rect::new(70, 70, 10, 10), &mut rng, true);
        assert_eq!(enc.dirty_per_strip(Some(&[Rect::new(70, 70, 10, 10)])).iter().sum::<usize>(), 0);
        let n = enc.encode(&c, w * 4, Some(&[Rect::new(70, 70, 10, 10)]), &mut Vec::new());
        assert_eq!(n, 0);
    }
}

#[test]
fn depois_de_invalidar_a_regiao_sai_sem_skip_e_o_decodificador_reproduz_o_quadro() {
    let (w, h) = (320usize, 256usize);
    for threads in [1usize, 3] {
        let mut rng = Rng::new(42);
        let a = img_aleatoria(w, h, &mut rng);
        let mut enc = Encoder::with_threads(w as u32, h as u32, threads);
        enc.set_parallel_min_blocks(0);
        let mut dec = Decoder::new(w as u32, h as u32);
        passo(&mut enc, &mut dec, w, &a, None);

        enc.set_excluded(Some(REGIAO));
        let mut b = a.clone();
        pinta(&mut b, w, REGIAO, &mut rng, true);
        pinta(&mut b, w, Rect::new(0, 0, 64, 64), &mut rng, true);
        let mut o = Vec::new();
        enc.encode(&b, w * 4, Some(&[Rect::new(0, 0, w as u32, h as u32)]), &mut o);
        dec.decode(&o).unwrap();
        assert_eq!(marcados(enc.coded_blocks()), (0..20).filter(|i| !indices_da_regiao().contains(i) && *i == 0).collect::<Vec<_>>());

        enc.set_excluded(None);
        enc.invalidate_rect(REGIAO);
        assert_eq!(enc.dirty_per_strip(Some(&[])).iter().sum::<usize>(), 4);
        let mut o = Vec::new();
        let n = enc.encode(&b, w * 4, Some(&[]), &mut o);
        assert_eq!(n, 4);
        assert_eq!(marcados(enc.coded_blocks()), indices_da_regiao(), "{threads} threads");
        dec.decode(&o).unwrap();
        assert!(igual_normalizado(dec.pixels(), &b), "o quadro inteiro tem de bater bit a bit");

        let n = enc.encode(&b, w * 4, Some(&[]), &mut Vec::new());
        assert_eq!(n, 0);
    }
}

#[test]
fn invalidar_codifica_mesmo_com_conteudo_igual_ao_da_referencia() {
    let (w, h) = (320usize, 256usize);
    let mut rng = Rng::new(43);
    let a = img_aleatoria(w, h, &mut rng);
    let mut enc = Encoder::with_threads(w as u32, h as u32, 1);
    enc.encode(&a, w * 4, None, &mut Vec::new());
    enc.invalidate_rect(REGIAO);
    let mut o = Vec::new();
    let n = enc.encode(&a, w * 4, Some(&[]), &mut o);
    assert_eq!(n, 4, "o conteúdo é igual, mas a referência foi invalidada");
    assert_eq!(marcados(enc.coded_blocks()), indices_da_regiao());
}

#[test]
fn exclusao_num_quadro_sem_referencia_invalida_os_blocos_excluidos() {
    let (w, h) = (320usize, 256usize);
    let mut rng = Rng::new(44);
    let a = img_aleatoria(w, h, &mut rng);
    let mut enc = Encoder::with_threads(w as u32, h as u32, 1);
    let mut dec = Decoder::new(w as u32, h as u32);
    enc.set_excluded(Some(REGIAO));
    let mut o = Vec::new();
    let n = enc.encode(&a, w * 4, None, &mut o);
    assert_eq!(n, 16);
    dec.decode(&o).unwrap();
    enc.set_excluded(None);
    let mut o = Vec::new();
    let n = enc.encode(&a, w * 4, Some(&[]), &mut o);
    assert_eq!(n, 4);
    dec.decode(&o).unwrap();
    assert!(igual_normalizado(dec.pixels(), &a));
    enc.set_excluded(Some(REGIAO));
    enc.invalidate_rect(REGIAO);
    enc.reset(w as u32, h as u32);
    let n = enc.encode(&a, w * 4, None, &mut Vec::new());
    assert_eq!(n, 20);
}

#[test]
fn invalidacao_espera_a_faixa_quando_o_quadro_e_fatiado() {
    let (w, h) = (320usize, 256usize);
    let mut rng = Rng::new(45);
    let a = img_aleatoria(w, h, &mut rng);
    let mut enc = Encoder::with_threads(w as u32, h as u32, 1);
    let mut dec = Decoder::new(w as u32, h as u32);
    passo(&mut enc, &mut dec, w, &a, None);
    let mut b = a.clone();
    pinta(&mut b, w, REGIAO, &mut rng, true);
    enc.invalidate_rect(REGIAO);

    let mut o = Vec::new();
    enc.encode_range(&b, w * 4, Some(&[]), 1..2, &mut o);
    assert_eq!(marcados(enc.coded_blocks()), vec![6, 7], "só a faixa 1");
    dec.decode(&o).unwrap();
    assert_eq!(enc.dirty_per_strip(Some(&[])), vec![0, 0, 2, 0], "a faixa 2 continua invalidada");

    let mut o = Vec::new();
    enc.encode_range(&b, w * 4, Some(&[]), 2..3, &mut o);
    assert_eq!(marcados(enc.coded_blocks()), vec![11, 12]);
    dec.decode(&o).unwrap();
    assert!(igual_normalizado(dec.pixels(), &b));
    assert_eq!(enc.dirty_per_strip(Some(&[])), vec![0; 4]);
}

#[test]
fn sem_exclusao_nem_invalidacao_dirty_per_strip_e_igual_ao_calculo_antigo() {
    let (w, h) = (200u32, 200u32);
    let mut enc = Encoder::with_threads(w, h, 1);
    let dano = [Rect::new(10, 10, 100, 10), Rect::new(0, 130, 200, 5)];
    assert_eq!(enc.dirty_per_strip(Some(&dano)), dirty_blocks_per_strip(None, w, h));
    enc.encode(&vec![0u8; (w * h * 4) as usize], (w * 4) as usize, None, &mut Vec::new());
    assert_eq!(enc.dirty_per_strip(Some(&dano)), dirty_blocks_per_strip(Some(&dano), w, h));
    assert_eq!(enc.dirty_per_strip(None), dirty_blocks_per_strip(None, w, h));
}

#[test]
fn write_region_escreve_pixels_de_fora_do_codec_e_recorta() {
    let mut dec = Decoder::new(100, 80);
    let src: Vec<u8> = (0..40 * 30 * 4).map(|i| (i % 251) as u8).collect();
    assert!(dec.write_region(Rect::new(10, 20, 40, 30), &src));
    for row in 0..30usize {
        for col in 0..40usize {
            let d = ((20 + row) * 100 + 10 + col) * 4;
            let s = (row * 40 + col) * 4;
            assert_eq!(&dec.pixels()[d..d + 3], &src[s..s + 3]);
            assert_eq!(dec.pixels()[d + 3], 0xFF);
        }
    }
    assert_eq!(&dec.pixels()[0..4], &[0, 0, 0, 0xFF]);
    assert!(dec.write_region(Rect::new(80, 70, 40, 30), &vec![7u8; 40 * 30 * 4]));
    assert_eq!(&dec.pixels()[(79 * 100 + 99) * 4..(79 * 100 + 99) * 4 + 3], &[7, 7, 7]);
    assert!(!dec.write_region(Rect::new(0, 0, 10, 10), &[0u8; 8]));
    let mut enc = Encoder::with_threads(100, 80, 1);
    let mut o = Vec::new();
    enc.encode(&vec![0u8; 100 * 80 * 4], 400, None, &mut o);
    let before = dec.pixels().to_vec();
    enc.encode(&vec![0u8; 100 * 80 * 4], 400, None, &mut o);
    dec.decode(&o[o.len() - 4..]).unwrap();
    assert_eq!(dec.pixels(), before.as_slice());
}
