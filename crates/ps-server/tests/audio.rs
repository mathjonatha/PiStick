//! Marco 7: o áudio do servidor sintético chega pelo protocolo, com PSA1 e com PCM, só para quem anunciou
//! `CAP_AUDIO`, e a conexão aguenta pacotes de áudio e quadros de imagem juntos.

use std::net::TcpStream;
use std::time::{Duration, Instant};

use ps_audio::{decode, pcm_to_samples, synth};
use ps_proto::{read_hello, read_message, write_message, Message, AUDIO_FLAG_DISCONTINUITY, CAP_AUDIO, CAP_BLOCK_CODEC};
use ps_server::audio::{spawn_synthetic, AudioCodec, AudioHub};
use ps_server::{serve_loop, ServerOptions};
use ps_transport::TcpAcceptor;

struct Coleta {
    caps_do_servidor: u32,
    start: Option<(u8, u8, u32, u16)>,
    /// (seq, flags, pts_us, amostras decodificadas)
    pacotes: Vec<(u32, u8, u64, Vec<i16>)>,
    outras: usize,
}

/// Conecta, anuncia `caps` e coleta mensagens por `dur`.
fn coletar(codec: AudioCodec, caps: u32, dur: Duration) -> Coleta {
    let acceptor = TcpAcceptor::bind("127.0.0.1:0").unwrap();
    let addr = acceptor.local_addr().unwrap();
    let hub = AudioHub::new(codec);
    let sintetico = spawn_synthetic(hub.clone());
    let opts = ServerOptions { quiet: true, audio: Some(hub.clone()), ..ServerOptions::default() };
    let server = std::thread::spawn(move || serve_loop(&acceptor, &opts, Some(1)).unwrap());

    let mut s = TcpStream::connect(addr).unwrap();
    write_message(&mut s, &Message::hello_with_caps("teste de áudio", caps)).unwrap();
    let (caps_do_servidor, _) = read_hello(&mut s).unwrap();
    s.set_read_timeout(Some(Duration::from_millis(100))).unwrap();
    let mut c = Coleta { caps_do_servidor, start: None, pacotes: Vec::new(), outras: 0 };
    let t0 = Instant::now();
    let mut seq_esperado = 0u32;
    while t0.elapsed() < dur {
        match read_message(&mut s) {
            Ok(Message::AudioStart { codec, channels, rate, frames, .. }) => c.start = Some((codec, channels, rate, frames)),
            Ok(Message::AudioData { stream, flags, seq, pts_us, data }) => {
                assert_eq!(stream, 0);
                assert_eq!(seq, seq_esperado, "seq deve crescer de 1 em 1");
                seq_esperado += 1;
                let mut out = Vec::new();
                match codec {
                    AudioCodec::Pcm => pcm_to_samples(&data, &mut out),
                    AudioCodec::Psa1 => {
                        decode(&data, 2, &mut out).unwrap();
                    }
                }
                c.pacotes.push((seq, flags, pts_us, out));
            }
            Ok(Message::Frame { win, seq, .. }) => {
                write_message(&mut s, &Message::FrameAck { win, seq }).unwrap();
                c.outras += 1;
            }
            Ok(_) => c.outras += 1,
            Err(ps_proto::ProtoError::Io(e)) if matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut) => {}
            Err(e) => panic!("erro de leitura: {e}"),
        }
    }
    drop(s);
    server.join().unwrap();
    hub.stop();
    sintetico.join().unwrap();
    c
}

fn analisa_som_e_silencio(c: &Coleta) {
    let (codec, canais, taxa, quadros) = c.start.expect("faltou AUDIO_START");
    assert!(codec <= 1 && canais == 2 && taxa == 48000 && quadros == 480);
    // A senoide dura 1 s (100 pacotes), depois 200 ms de silêncio enviado e então nada.
    let primeiros: Vec<i16> = c.pacotes.iter().take(100).flat_map(|p| p.3.iter().copied()).collect();
    assert_eq!(primeiros.len(), 100 * 480 * 2);
    let (l, r) = synth::split_stereo(&primeiros);
    assert!((synth::goertzel(&l, 440.0) - synth::AMPLITUDE).abs() < 0.01);
    assert!((synth::goertzel(&r, 880.0) - synth::AMPLITUDE).abs() < 0.01);
    assert!(synth::goertzel(&l, 880.0) < 0.01 && synth::goertzel(&r, 440.0) < 0.01);
    // O primeiro pacote leva a marca (início do fluxo); os 99 seguintes não.
    assert_eq!(c.pacotes[0].1, AUDIO_FLAG_DISCONTINUITY);
    assert!(c.pacotes[1..100].iter().all(|p| p.1 == 0));
    // pts crescendo de 10 ms em 10 ms (tolerância do arredondamento).
    for w in c.pacotes[..100].windows(2) {
        let d = w[1].2 as i64 - w[0].2 as i64;
        assert!((9_990..=10_010).contains(&d), "intervalo de pts {d}");
    }
    // Depois do som, 20 pacotes de silêncio (200 ms) e nenhum mais até o próximo som.
    for p in &c.pacotes[100..120] {
        assert!(p.3.iter().all(|&x| x == 0), "esperava silêncio no pacote {}", p.0);
    }
}

#[test]
fn sintetico_com_psa1_chega_decodificavel_e_para_no_silencio() {
    let c = coletar(AudioCodec::Psa1, CAP_BLOCK_CODEC | CAP_AUDIO, Duration::from_millis(1700));
    assert_ne!(c.caps_do_servidor & CAP_AUDIO, 0);
    analisa_som_e_silencio(&c);
    // Em 1,7 s: 1 s de som + 200 ms de silêncio = 120 pacotes; nenhum a mais (o silêncio parou o fluxo).
    assert!((118..=122).contains(&c.pacotes.len()), "pacotes: {}", c.pacotes.len());
    assert!(c.outras > 0, "as janelas continuam chegando junto com o áudio");
}

#[test]
fn sintetico_com_pcm_chega_igual() {
    let c = coletar(AudioCodec::Pcm, CAP_BLOCK_CODEC | CAP_AUDIO, Duration::from_millis(1400));
    analisa_som_e_silencio(&c);
}

#[test]
fn sem_o_bit_do_cliente_nao_ha_audio() {
    let c = coletar(AudioCodec::Psa1, CAP_BLOCK_CODEC, Duration::from_millis(600));
    assert_ne!(c.caps_do_servidor & CAP_AUDIO, 0, "o servidor anuncia o bit");
    assert!(c.start.is_none());
    assert!(c.pacotes.is_empty());
    assert!(c.outras > 0);
}
