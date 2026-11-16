
#[cfg(target_os = "linux")]
fn main() {
    use ps_codec::testutil::video;
    use ps_server::video::annexb::{nal_units, parse_sps, NAL_SPS};
    use ps_server::video::v4l2::V4l2Encoder;
    use ps_server::video::{VideoConfig, VideoEncoder};
    use std::time::Instant;

    let (mut size, mut frames, mut bitrate, mut origin, mut out, mut first) = ((640u32, 384u32), 30u32, 1_200_000u32, (64u32, 64u32), String::new(), 0u64);
    let mut bars = false;
    let mut a = std::env::args().skip(1);
    while let Some(k) = a.next() {
        let v = a.next().unwrap_or_default();
        let pair = |v: &str, sep: char| {
            let (x, y) = v.split_once(sep).expect("par inválido");
            (x.parse::<u32>().expect("número"), y.parse::<u32>().expect("número"))
        };
        match k.as_str() {
            "--size" => size = pair(&v, 'x'),
            "--origin" => origin = pair(&v, ','),
            "--frames" => frames = v.parse().expect("número"),
            "--bitrate" => bitrate = v.parse().expect("número"),
            "--first-frame" => first = v.parse().expect("número"),
            "--out" => out = v,
            "--content" => bars = v == "bars",
            _ => panic!("argumento desconhecido: {k}"),
        }
    }
    assert!(!out.is_empty(), "falta --out");
    let cfg = VideoConfig { width: size.0, height: size.1, bitrate, fps: 30, max_qp: 51 };
    let mut enc = V4l2Encoder::open(std::path::Path::new(ps_server::video::v4l2::DEFAULT_DEVICE), &cfg).expect("abrir o codificador");
    println!("negociado: {:?}", enc.negotiated());
    let mut file = Vec::new();
    file.extend_from_slice(b"PSH2");
    for v in [size.0, size.1, frames, origin.0, origin.1, first as u32, bitrate, u32::from(bars)] {
        file.extend_from_slice(&v.to_le_bytes());
    }
    let (mut total, mut times) = (0usize, Vec::new());
    for i in 0..u64::from(frames) {
        let (img, off, stride) = if bars {
            (ps_codec::testutil::bars::render(size.0, size.1, first + i), 0, size.0 as usize * 4)
        } else {
            (video::render(video::WIDTH, video::HEIGHT, first + i), (origin.1 as usize * video::WIDTH as usize + origin.0 as usize) * 4, video::WIDTH as usize * 4)
        };
        let t = Instant::now();
        let f = enc.encode(&img[off..], stride).expect("codificar");
        times.push(t.elapsed().as_secs_f64() * 1000.0);
        if i == 0 {
            for (t, n) in nal_units(&f.data) {
                if t == NAL_SPS {
                    println!("SPS: {:?}", parse_sps(n));
                }
            }
        }
        total += f.data.len();
        file.push(u8::from(f.keyframe));
        file.extend_from_slice(&(f.data.len() as u32).to_le_bytes());
        file.extend_from_slice(&f.data);
        println!("quadro {i}: {} bytes{}", f.data.len(), if f.keyframe { " (chave)" } else { "" });
    }
    std::fs::write(&out, &file).expect("gravar");
    let secs = f64::from(frames) / 30.0;
    println!(
        "{} quadros, {total} bytes de H.264, arquivo {} bytes, {:.0} kbit/s a 30 quadros/s (pedido {} kbit/s), tempo por quadro: média {:.2} ms, máx {:.2} ms; contadores {:?}",
        frames,
        file.len(),
        total as f64 * 8.0 / secs / 1000.0,
        bitrate / 1000,
        times.iter().sum::<f64>() / times.len() as f64,
        times.iter().cloned().fold(0.0, f64::max),
        enc.counters()
    );
}

#[cfg(not(target_os = "linux"))]
fn main() {
    eprintln!("gen_h264_sample só existe no Linux (precisa do codificador V4L2 do Pi)");
}
