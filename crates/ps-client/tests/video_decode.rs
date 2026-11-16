//! Decodificador H.264 do Windows contra as amostras gravadas pelo codificador do Pi
//! (`tests/data/*.psh2`; formato descrito em `crates/ps-server/examples/gen_h264_sample.rs`).
#![cfg(windows)]

use std::time::Instant;

use ps_client::video::mf::MfDecoder;
use ps_client::video::nv12::{Matrix, Range};
use ps_client::video::VideoDecoder;
use ps_codec::testutil::{bars, video};

struct Sample {
    width: u32,
    height: u32,
    origin: (u32, u32),
    first: u64,
    content: u32,
    frames: Vec<(bool, Vec<u8>)>,
}

fn load(name: &str) -> Sample {
    load_path(&std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests").join("data").join(name))
}

fn load_path(path: &std::path::Path) -> Sample {
    let d = std::fs::read(path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    assert_eq!(&d[..4], b"PSH2");
    let u = |i: usize| u32::from_le_bytes(d[4 + i * 4..8 + i * 4].try_into().unwrap());
    let (n, mut pos) = (u(2) as usize, 4 + 8 * 4);
    let mut frames = Vec::new();
    for _ in 0..n {
        let key = d[pos] & 1 != 0;
        let len = u32::from_le_bytes(d[pos + 1..pos + 5].try_into().unwrap()) as usize;
        frames.push((key, d[pos + 5..pos + 5 + len].to_vec()));
        pos += 5 + len;
    }
    assert_eq!(pos, d.len());
    Sample { width: u(0), height: u(1), origin: (u(3), u(4)), first: u64::from(u(5)), content: u(7), frames }
}

/// Quadro de origem (BGRX, linhas contíguas) do quadro `i` da amostra.
fn origem(s: &Sample, i: usize) -> Vec<u8> {
    let frame = s.first + i as u64;
    if s.content == 1 {
        return bars::render(s.width, s.height, frame);
    }
    let img = video::render(video::WIDTH, video::HEIGHT, frame);
    let mut out = Vec::with_capacity(s.width as usize * s.height as usize * 4);
    for row in 0..s.height as usize {
        let o = ((s.origin.1 as usize + row) * video::WIDTH as usize + s.origin.0 as usize) * 4;
        out.extend_from_slice(&img[o..o + s.width as usize * 4]);
    }
    out
}

/// PSNR em dB sobre os canais B, G e R.
fn psnr(a: &[u8], b: &[u8]) -> f64 {
    assert_eq!(a.len(), b.len());
    let (mut se, mut n) = (0f64, 0f64);
    for (p, q) in a.chunks_exact(4).zip(b.chunks_exact(4)) {
        for k in 0..3 {
            let d = f64::from(p[k]) - f64::from(q[k]);
            se += d * d;
            n += 1.0;
        }
    }
    if se == 0.0 {
        return 99.0;
    }
    10.0 * (255.0f64 * 255.0 / (se / n)).log10()
}

/// Erro médio absoluto por canal (B, G, R) num retângulo.
fn erro_medio(a: &[u8], b: &[u8], stride_px: usize, x: u32, y: u32, w: u32, h: u32) -> [f64; 3] {
    let mut s = [0f64; 3];
    for row in y..y + h {
        for col in x..x + w {
            let i = (row as usize * stride_px + col as usize) * 4;
            for k in 0..3 {
                s[k] += (f64::from(a[i + k]) - f64::from(b[i + k])).abs();
            }
        }
    }
    let n = f64::from(w) * f64::from(h);
    [s[0] / n, s[1] / n, s[2] / n]
}

struct Resultado {
    psnr: Vec<f64>,
    saidas: usize,
    ms: Vec<f64>,
    decodificadas: Vec<Vec<u8>>,
    cor: Option<(Matrix, Range)>,
}

fn decodifica(s: &Sample) -> Resultado {
    let mut dec = MfDecoder::open(s.width, s.height).expect("abrir o decodificador do Media Foundation");
    let (mut psnrs, mut ms, mut saidas, mut todas) = (Vec::new(), Vec::new(), 0, Vec::new());
    let mut out = Vec::new();
    for (i, (_, data)) in s.frames.iter().enumerate() {
        let t = Instant::now();
        let r = dec.decode(data, &mut out).unwrap_or_else(|e| panic!("quadro {i}: {e}"));
        ms.push(t.elapsed().as_secs_f64() * 1000.0);
        if let Some((w, h)) = r {
            assert_eq!((w, h), (s.width, s.height), "o recorte de exibição tem de ser o tamanho da região (quadro {i})");
            assert_eq!(out.len(), (w * h * 4) as usize);
            saidas += 1;
            psnrs.push(psnr(&out, &origem(s, i)));
            todas.push(out.clone());
        } else {
            psnrs.push(f64::NAN);
            todas.push(Vec::new());
        }
    }
    Resultado { psnr: psnrs, saidas, ms, decodificadas: todas, cor: dec.last_color().map(|c| (c.matrix, c.range)) }
}

#[test]
fn amostra_640x384_da_animacao_tem_psnr_de_30_db_ou_mais() {
    let s = load("video-640x384.psh2");
    let r = decodifica(&s);
    let validos: Vec<f64> = r.psnr.iter().copied().filter(|p| !p.is_nan()).collect();
    let media = validos.iter().sum::<f64>() / validos.len() as f64;
    let min = validos.iter().cloned().fold(f64::MAX, f64::min);
    let tempo = r.ms.iter().sum::<f64>() / r.ms.len() as f64;
    let max = r.ms.iter().cloned().fold(0.0, f64::max);
    eprintln!("640x384: {} saídas de {} quadros; PSNR média {media:.2} dB, mín {min:.2} dB; decodificação média {tempo:.2} ms, máx {max:.2} ms; cor {:?}", r.saidas, s.frames.len(), r.cor);
    assert_eq!(r.saidas, s.frames.len(), "em baixa latência cada unidade de acesso devolve um quadro");
    assert!(min >= 30.0, "PSNR mínimo {min:.2} dB");
    // Área chapada: o quadrado cinza (0xF0), afastado 6 px da borda (a cor é subamostrada).
    for i in (0..s.frames.len()).step_by(3) {
        let b = video::box_rect(s.first + i as u64);
        let e = erro_medio(&r.decodificadas[i], &origem(&s, i), s.width as usize, b.x + 6, b.y + 6, b.w - 12, b.h - 12);
        assert!(e.iter().all(|&v| v <= 6.0), "quadro {i}: erro médio por canal {e:?} no quadrado chapado");
    }
}

#[test]
fn amostra_de_barras_320x200_recorta_o_sps_e_acerta_as_cores() {
    let s = load("video-barras-320x200.psh2");
    assert_eq!((s.width, s.height), (320, 200), "o SPS codifica 320x208 e recorta para 200");
    let r = decodifica(&s);
    let validos: Vec<f64> = r.psnr.iter().copied().filter(|p| !p.is_nan()).collect();
    let min = validos.iter().cloned().fold(f64::MAX, f64::min);
    let tempo = r.ms.iter().sum::<f64>() / r.ms.len() as f64;
    eprintln!("barras 320x200: {} saídas de {} quadros; PSNR mín {min:.2} dB; decodificação média {tempo:.2} ms; cor {:?}", r.saidas, s.frames.len(), r.cor);
    assert_eq!(r.saidas, s.frames.len());
    assert!(min >= 30.0, "PSNR mínimo {min:.2} dB");
    let mut pior = 0f64;
    for i in 0..s.frames.len() {
        let origem = origem(&s, i);
        for a in bars::flat_areas(s.width, s.height) {
            let e = erro_medio(&r.decodificadas[i], &origem, s.width as usize, a[0], a[1], a[2], a[3]);
            pior = pior.max(e[0]).max(e[1]).max(e[2]);
            assert!(e.iter().all(|&v| v <= 6.0), "quadro {i}, barra em x={}: erro médio por canal {e:?} (matriz ou faixa de cor erradas?)", a[0]);
        }
    }
    eprintln!("barras: pior erro médio por canal nas áreas chapadas: {pior:.2}");
}

#[test]
fn decodificador_aceita_varias_aberturas_e_recusa_dado_invalido_sem_panico() {
    let s = load("video-640x384.psh2");
    // Um segundo decodificador na mesma thread, em paralelo ao primeiro.
    let mut a = MfDecoder::open(s.width, s.height).unwrap();
    let mut b = MfDecoder::open(s.width, s.height).unwrap();
    let (mut oa, mut ob) = (Vec::new(), Vec::new());
    a.decode(&s.frames[0].1, &mut oa).unwrap();
    b.decode(&s.frames[0].1, &mut ob).unwrap();
    assert_eq!(oa, ob, "o mesmo fluxo decodifica igual");
    assert!(a.decode(&[], &mut oa).is_err());
    // Lixo no lugar de um quadro: erro ou nenhuma saída, nunca pânico.
    let _ = a.decode(&[0, 0, 1, 0x41, 0xFF, 0xFF, 0xFF, 0xFF], &mut oa);
}

/// Abrir e fechar o decodificador muitas vezes seguidas: cada `MFStartup` tem de ter o seu `MFShutdown`
/// (um vazamento de contagem ou um desfazer a mais apareceria como erro ou queda numa das aberturas).
#[test]
fn abrir_e_fechar_varias_vezes_seguidas() {
    let s = load("video-640x384.psh2");
    for i in 0..20 {
        let mut d = MfDecoder::open(s.width, s.height).unwrap_or_else(|e| panic!("abertura {i}: {e}"));
        if i % 5 == 0 {
            let mut out = Vec::new();
            d.decode(&s.frames[0].1, &mut out).unwrap_or_else(|e| panic!("abertura {i}: {e}"));
        }
    }
}

/// Erro durante `open` (depois do `MFStartup`) não pode deixar o Media Foundation num estado ruim:
/// depois dele, uma abertura válida e a decodificação têm de funcionar.
#[test]
fn erro_na_abertura_nao_estraga_a_proxima() {
    let s = load("video-640x384.psh2");
    let mut recusados = 0;
    for (w, h) in [(0u32, 0u32), (0, 384), (640, 0), (1, 1), (u32::MAX, u32::MAX)] {
        match MfDecoder::open(w, h) {
            Ok(_) => eprintln!("tamanho {w}x{h}: a MFT aceitou"),
            Err(e) => {
                recusados += 1;
                eprintln!("tamanho {w}x{h}: recusado: {e}");
            }
        }
        // Depois de cada tentativa, recusada ou não, o decodificador válido funciona.
        let mut d = MfDecoder::open(s.width, s.height).unwrap_or_else(|e| panic!("abertura válida depois de {w}x{h}: {e}"));
        let mut out = Vec::new();
        d.decode(&s.frames[0].1, &mut out).unwrap();
        assert_eq!(out.len(), (s.width * s.height * 4) as usize);
    }
    eprintln!("tamanhos recusados na abertura: {recusados} de 5");
}

/// Regressão do teste instável: a MFT decodifica em threads próprias e, no primeiro quadro de um decodificador
/// recém-aberto com a máquina carregada, o `ProcessOutput` às vezes dizia `NEED_MORE_INPUT` antes de o quadro estar
/// pronto (o quadro saía junto com o seguinte). Com a máquina carregada de propósito, o primeiro quadro de cada
/// decodificador novo tem de sair no próprio `decode`.
#[test]
fn o_primeiro_quadro_de_cada_decodificador_sai_mesmo_com_a_maquina_carregada() {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;
    let s = load("video-640x384.psh2");
    let parar = Arc::new(AtomicBool::new(false));
    let n = std::thread::available_parallelism().map_or(4, |n| n.get());
    let carga: Vec<_> = (0..n)
        .map(|_| {
            let parar = parar.clone();
            std::thread::spawn(move || {
                let mut x = 1u64;
                while !parar.load(Ordering::Relaxed) {
                    x = std::hint::black_box(x.wrapping_mul(6364136223846793005).wrapping_add(1));
                }
            })
        })
        .collect();
    let mut sem_saida = Vec::new();
    for i in 0..40 {
        let mut d = MfDecoder::open(s.width, s.height).unwrap();
        let mut out = Vec::new();
        if d.decode(&s.frames[0].1, &mut out).unwrap().is_none() || out.len() != (s.width * s.height * 4) as usize {
            sem_saida.push(i);
        }
    }
    parar.store(true, Ordering::Relaxed);
    for t in carga {
        t.join().unwrap();
    }
    assert!(sem_saida.is_empty(), "primeiro quadro sem saída nas aberturas {sem_saida:?}");
}

#[test]
fn o_criterio_de_cor_pega_matriz_e_faixa_erradas() {
    let s = load("video-barras-320x200.psh2");
    for (matrix, range) in [(Matrix::Bt709, Range::Limited), (Matrix::Bt601, Range::Full)] {
        let mut dec = MfDecoder::open(s.width, s.height).unwrap();
        dec.set_color_override(Some(ps_client::video::nv12::ColorSpec { matrix, range }));
        let mut out = Vec::new();
        let mut pior = 0f64;
        for (i, (_, data)) in s.frames.iter().enumerate() {
            dec.decode(data, &mut out).unwrap().expect("quadro");
            let o = origem(&s, i);
            for a in bars::flat_areas(s.width, s.height) {
                let e = erro_medio(&out, &o, s.width as usize, a[0], a[1], a[2], a[3]);
                pior = pior.max(e[0]).max(e[1]).max(e[2]);
            }
        }
        eprintln!("{matrix:?} {range:?} (errada): pior erro médio por canal {pior:.1}");
        assert!(pior > 6.0, "o critério de erro médio <= 6 deixou passar {matrix:?} {range:?}: pior erro {pior:.2}");
    }
}

/// Medição opcional com uma amostra maior (por exemplo 1280x720), que não vai para o repositório:
/// `PS_AMOSTRA_EXTRA=caminho.psh2 cargo test --release -p ps-client --test video_decode extra -- --nocapture`.
#[test]
fn amostra_extra_opcional() {
    let Ok(path) = std::env::var("PS_AMOSTRA_EXTRA") else {
        eprintln!("PULADO: PS_AMOSTRA_EXTRA não definida");
        return;
    };
    let s = load_path(std::path::Path::new(&path));
    let r = decodifica(&s);
    let validos: Vec<f64> = r.psnr.iter().copied().filter(|p| !p.is_nan()).collect();
    let tempo = r.ms.iter().sum::<f64>() / r.ms.len() as f64;
    let max = r.ms.iter().cloned().fold(0.0, f64::max);
    eprintln!(
        "{}x{}: {} saídas de {}; PSNR média {:.2} dB, mín {:.2} dB; decodificação média {tempo:.2} ms, máx {max:.2} ms; cor {:?}",
        s.width,
        s.height,
        r.saidas,
        s.frames.len(),
        validos.iter().sum::<f64>() / validos.len() as f64,
        validos.iter().cloned().fold(f64::MAX, f64::min),
        r.cor
    );
}
