#![cfg(target_os = "linux")]

use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use ps_codec::testutil::video;
use ps_proto::{read_hello, read_message, write_message, Message, CAP_BLOCK_CODEC, CAP_H264, VIDEO_FLAG_KEY};
use ps_server::video::annexb::{nal_units, parse_sps, NAL_IDR, NAL_PPS, NAL_SLICE, NAL_SPS};
use ps_server::video::v4l2::{V4l2Encoder, V4l2Factory, COLORSPACE_REC709, DEFAULT_DEVICE};
use ps_server::video::{VideoConfig, VideoEncoder};
use ps_server::{serve_loop, ServerOptions, SyntheticVideo};
use ps_transport::{Split, TcpAcceptor, TcpTransport, Transport};

fn disponivel() -> bool {
    if Path::new(DEFAULT_DEVICE).exists() {
        true
    } else {
        eprintln!("PULADO: {DEFAULT_DEVICE} não existe neste computador");
        false
    }
}

fn codifica(enc: &mut V4l2Encoder, n: u64, x: usize, y: usize) -> (Vec<Vec<u8>>, Vec<bool>, Vec<f64>) {
    let (mut dados, mut chaves, mut tempos) = (Vec::new(), Vec::new(), Vec::new());
    for i in 0..n {
        let img = video::render(video::WIDTH, video::HEIGHT, i);
        let off = (y * video::WIDTH as usize + x) * 4;
        let t = Instant::now();
        let f = enc.encode(&img[off..], video::WIDTH as usize * 4).expect("codificar");
        tempos.push(t.elapsed().as_secs_f64() * 1000.0);
        assert!(!f.data.is_empty());
        let c = enc.counters();
        assert_eq!((c.frames_in, c.frames_out), (i + 1, i + 1), "o driver deve devolver exatamente um quadro por quadro de entrada, sem atraso");
        dados.push(f.data);
        chaves.push(f.keyframe);
    }
    (dados, chaves, tempos)
}

#[test]
fn codifica_60_quadros_da_animacao_em_640x384() {
    if !disponivel() {
        return;
    }
    let pedido = 5_000_000u32;
    let cfg = VideoConfig { width: 640, height: 384, bitrate: pedido, fps: 30, max_qp: 51 };
    let mut enc = V4l2Encoder::open(Path::new(DEFAULT_DEVICE), &cfg).expect("abrir");
    let neg = enc.negotiated();
    eprintln!("negociado: {neg:?}");
    assert_eq!((neg.width, neg.height), (640, 384));
    assert!(neg.bytes_per_line >= 640 * 4);
    let (dados, chaves, tempos) = codifica(&mut enc, 60, 64, 64);

    let tipos0: Vec<u8> = nal_units(&dados[0]).iter().map(|(t, _)| *t).collect();
    eprintln!("NAL do primeiro quadro: {tipos0:?}");
    assert!(chaves[0] && tipos0.contains(&NAL_SPS) && tipos0.contains(&NAL_PPS) && tipos0.contains(&NAL_IDR), "{tipos0:?}");
    for (i, d) in dados.iter().enumerate().skip(1) {
        let t: Vec<u8> = nal_units(d).iter().map(|(t, _)| *t).collect();
        assert!(!chaves[i] && !t.contains(&NAL_IDR) && !t.contains(&NAL_SPS) && t.contains(&NAL_SLICE), "quadro {i}: {t:?}");
    }
    let sps = nal_units(&dados[0]).into_iter().find(|(t, _)| *t == NAL_SPS).map(|(_, n)| parse_sps(n).expect("SPS")).unwrap();
    eprintln!("SPS: {sps:?}");
    assert_eq!((sps.width, sps.height), (640, 384));
    assert_eq!((sps.profile_idc, sps.level_idc), (100, 40), "perfil High, nível 4.0");

    let total: usize = dados.iter().map(Vec::len).sum();
    let real = total as f64 * 8.0 / 2.0;
    let media = tempos.iter().sum::<f64>() / tempos.len() as f64;
    let max = tempos.iter().cloned().fold(0.0, f64::max);
    eprintln!("{total} bytes, {:.0} kbit/s (pedido {} kbit/s); tempo por quadro: média {media:.2} ms, máx {max:.2} ms; primeiro quadro {} bytes", real / 1000.0, pedido / 1000, dados[0].len());
    assert!(real <= f64::from(pedido) * 1.30, "taxa de bits {:.0} kbit/s acima de +30% de {} kbit/s", real / 1000.0, pedido / 1000);
    assert!(real >= f64::from(pedido) * 0.05, "taxa de bits {:.0} kbit/s absurdamente baixa", real / 1000.0);
    assert!(max < 200.0);
    assert_eq!(neg.colorspace, COLORSPACE_REC709);
}

#[test]
fn tamanhos_que_nao_sao_multiplos_de_16_sao_recortados_no_proprio_fluxo() {
    if !disponivel() {
        return;
    }
    for (w, h, x, y) in [(320u32, 200u32, 64usize, 64usize), (1280, 720, 0, 0), (642, 386, 0, 0)] {
        let cfg = VideoConfig { width: w, height: h, bitrate: 3_000_000, fps: 30, max_qp: 51 };
        let mut enc = V4l2Encoder::open(Path::new(DEFAULT_DEVICE), &cfg).unwrap_or_else(|e| panic!("abrir {w}x{h}: {e}"));
        let neg = enc.negotiated();
        let (dados, chaves, _) = codifica(&mut enc, 6, x, y);
        let sps = nal_units(&dados[0]).into_iter().find(|(t, _)| *t == NAL_SPS).map(|(_, n)| parse_sps(n).expect("SPS")).unwrap();
        eprintln!("{w}x{h}: negociado {neg:?}; SPS {sps:?}");
        assert!(chaves[0] && chaves[1..].iter().all(|c| !c));
        assert_eq!((sps.width, sps.height), (w, h), "o SPS tem de sinalizar o tamanho exato com recorte");
    }
}

#[test]
fn sessao_com_o_codificador_real_entra_em_video_e_volta_sem_perdas() {
    if !disponivel() {
        return;
    }
    let acceptor = TcpAcceptor::bind("127.0.0.1:0").unwrap();
    let addr = acceptor.local_addr().unwrap();
    let opts = ServerOptions {
        hello_timeout: Duration::from_millis(500),
        quiet: std::env::var("PS_LOG").is_err(),
        flow_initial_throughput: 1e10,
        flow_adaptive: false,
        synthetic_video: Some(SyntheticVideo { pause_after: Some(Duration::from_secs(3)) }),
        video: Some(Arc::new(V4l2Factory::default())),
        ..ServerOptions::default()
    };
    let server = std::thread::spawn(move || serve_loop(&acceptor, &opts, Some(1)).unwrap());
    let Split { mut reader, mut writer, closer } = TcpTransport::connect(addr).unwrap().split().unwrap();
    write_message(&mut writer, &Message::hello_with_caps("teste no Pi", CAP_BLOCK_CODEC | CAP_H264)).unwrap();
    let (caps, _) = read_hello(&mut reader).unwrap();
    assert_ne!(caps & CAP_H264, 0);
    let t0 = Instant::now();
    let (mut start_at, mut frames, mut bytes) = (None, 0usize, 0usize);
    let (mut parou, mut sem_perdas) = (false, false);
    while !sem_perdas {
        assert!(t0.elapsed() < Duration::from_secs(20), "tempo esgotado");
        match read_message(&mut reader).unwrap() {
            Message::VideoStart { win: 1, stream: 1, x, y, width, height, .. } => {
                assert_eq!((x, y, width, height), (64, 64, 640, 384));
                start_at = Some(t0.elapsed());
            }
            Message::VideoFrame { win, seq, flags, data, .. } => {
                let tipos: Vec<u8> = nal_units(&data).iter().map(|(t, _)| *t).collect();
                if frames == 0 {
                    assert_eq!(flags & VIDEO_FLAG_KEY, VIDEO_FLAG_KEY);
                    assert!(tipos.contains(&NAL_SPS) && tipos.contains(&NAL_PPS) && tipos.contains(&NAL_IDR), "{tipos:?}");
                } else {
                    assert_eq!(flags & VIDEO_FLAG_KEY, 0);
                    assert!(!tipos.contains(&NAL_IDR), "{tipos:?}");
                }
                frames += 1;
                bytes += data.len();
                write_message(&mut writer, &Message::FrameAck { win, seq }).unwrap();
            }
            Message::VideoStop { .. } => parou = true,
            Message::Frame { win, seq, .. } => {
                write_message(&mut writer, &Message::FrameAck { win, seq }).unwrap();
                sem_perdas = parou;
            }
            _ => {}
        }
    }
    let entrada = start_at.expect("VIDEO_START");
    eprintln!("vídeo entrou em {entrada:?}; {frames} quadros H.264, {bytes} bytes ({:.0} kbit/s)", bytes as f64 * 8.0 / (frames as f64 / 30.0) / 1000.0);
    assert!(entrada < Duration::from_secs(2));
    assert!(frames >= 30);
    closer.close();
    server.join().unwrap();
}
