
use std::io::Read;
use std::net::{TcpListener, TcpStream};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use ps_audio::synth;
use ps_client::mic::{file_run, LogFn, Mic, WavLoop};
use ps_proto::{read_hello, read_message, write_message, Message, ProtoError, AUDIO_STREAM_MIC, CAP_BLOCK_CODEC, CAP_MIC};
use ps_server::mic::{LinkFeed, MicHub, OpenFeed};
use ps_server::{serve_loop, ServerOptions};
use ps_transport::TcpAcceptor;

struct LeitorCanal {
    rx: mpsc::Receiver<Option<Vec<u8>>>,
    resto: Vec<u8>,
    fim: bool,
}

impl Read for LeitorCanal {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if self.resto.is_empty() && !self.fim {
            match self.rx.recv() {
                Ok(Some(v)) => self.resto = v,
                _ => self.fim = true,
            }
        }
        let n = self.resto.len().min(buf.len());
        buf[..n].copy_from_slice(&self.resto[..n]);
        self.resto.drain(..n);
        Ok(n)
    }
}

fn tom() -> WavLoop {
    let amostras: Vec<i16> = (0..96_000u64)
        .flat_map(|i| {
            let (l, r) = synth::tone_frame(i, 440.0, 880.0, 0.25);
            [l, r]
        })
        .collect();
    WavLoop::from_stereo(amostras).unwrap()
}

#[test]
fn cliente_e_servidor_juntos_so_capturam_com_gravador_e_o_tom_chega() {
    let pcm = TcpListener::bind("127.0.0.1:0").unwrap();
    let pcm_addr = pcm.local_addr().unwrap().to_string();
    let (monitor, rx_monitor) = mpsc::channel::<Option<Vec<u8>>>();
    let rx_monitor = Arc::new(Mutex::new(Some(rx_monitor)));
    let tx2 = monitor.clone();
    let open: OpenFeed = Arc::new(move || {
        let rx = rx_monitor.lock().unwrap().take().ok_or_else(|| std::io::Error::other("uma abertura só"))?;
        let tx3 = tx2.clone();
        let kill: Arc<dyn Fn() + Send + Sync> = Arc::new(move || {
            let _ = tx3.send(None);
        });
        Ok(LinkFeed { reader: Box::new(LeitorCanal { rx, resto: Vec::new(), fim: false }), kill })
    });
    let hub = MicHub::new(open, &pcm_addr, Arc::new(|_| {}));
    let acceptor = TcpAcceptor::bind("127.0.0.1:0").unwrap();
    let addr = acceptor.local_addr().unwrap();
    let opts = ServerOptions { quiet: true, mic: Some(hub), ..ServerOptions::default() };
    let server = std::thread::spawn(move || serve_loop(&acceptor, &opts, Some(1)).unwrap());

    let mut s = TcpStream::connect(addr).unwrap();
    write_message(&mut s, &Message::hello_with_caps("teste do microfone", CAP_BLOCK_CODEC | CAP_MIC)).unwrap();
    let (caps_srv, _) = read_hello(&mut s).unwrap();
    assert_ne!(caps_srv & CAP_MIC, 0);
    s.set_read_timeout(Some(Duration::from_millis(20))).unwrap();
    let (tx, rx) = mpsc::channel::<Message>();
    let mut w = s.try_clone().unwrap();
    let escritor = std::thread::spawn(move || {
        for m in rx {
            if write_message(&mut w, &m).is_err() {
                break;
            }
        }
    });
    let log: LogFn = Arc::new(|_| {});
    let mic = Mic::new(Some(file_run(tom())), true, tx.clone(), log);

    let vivo = |s: &mut TcpStream, mic: &Mic, dur: Duration| {
        let t0 = Instant::now();
        while t0.elapsed() < dur {
            match read_message(s) {
                Ok(Message::AudioRequest { stream, on }) => {
                    assert_eq!(stream, AUDIO_STREAM_MIC);
                    mic.request(on);
                }
                Ok(Message::Frame { win, seq, .. }) => {
                    let _ = tx.send(Message::FrameAck { win, seq });
                }
                Ok(_) => {}
                Err(ProtoError::Io(e)) if matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut) => {}
                Err(e) => panic!("erro de leitura: {e}"),
            }
        }
    };

    mic.reset_stats();
    vivo(&mut s, &mic, Duration::from_millis(1500));
    let r = mic.report();
    assert_eq!((r.packets, r.requests, r.offs), (0, 0, 0), "{}", r.line());
    assert!(!mic.is_open());
    pcm.set_nonblocking(true).unwrap();
    assert!(pcm.accept().is_err(), "ninguém escreve no PipeWire sem gravador");
    pcm.set_nonblocking(false).unwrap();

    monitor.send(Some(b"+ pistick-mic:capture_FL -> pw-record:input_FL\n+ pistick-mic:capture_FR -> pw-record:input_FR\n".to_vec())).unwrap();
    let t_gravador = Instant::now();
    while !mic.is_open() && t_gravador.elapsed() < Duration::from_secs(3) {
        vivo(&mut s, &mic, Duration::from_millis(20));
    }
    assert!(mic.is_open(), "a captura deveria abrir com o gravador");
    let (mut pcm_conn, _) = pcm.accept().unwrap();
    let mut recebido: Vec<u8> = Vec::new();
    let mut buf = [0u8; 8192];
    pcm_conn.set_read_timeout(Some(Duration::from_millis(5))).unwrap();
    let t0 = Instant::now();
    while t0.elapsed() < Duration::from_millis(1700) {
        if let Ok(n) = pcm_conn.read(&mut buf) {
            recebido.extend_from_slice(&buf[..n]);
        }
        vivo(&mut s, &mic, Duration::from_millis(1));
    }
    let amostras: Vec<i16> = recebido.as_chunks::<2>().0.iter().map(|c| i16::from_le_bytes(*c)).collect();
    assert!(amostras.len() >= 2 * 48_000, "recebeu {} quadros", amostras.len() / 2);
    let (l, r) = synth::split_stereo(&amostras[(2 * 24_000)..(2 * 72_000)]);
    assert!((synth::goertzel(&l, 440.0) - 0.25).abs() < 0.02, "esq 440 = {}", synth::goertzel(&l, 440.0));
    assert!((synth::goertzel(&r, 880.0) - 0.25).abs() < 0.02, "dir 880 = {}", synth::goertzel(&r, 880.0));
    assert!(synth::goertzel(&l, 880.0) < 0.02 && synth::goertzel(&r, 440.0) < 0.02, "vazamento entre canais");
    let r = mic.report();
    assert_eq!(r.requests, 1);
    assert!(r.packets >= 100, "{}", r.line());

    let saiu = Instant::now();
    monitor.send(Some(b"- pistick-mic:capture_FL -> pw-record:input_FL\n- pistick-mic:capture_FR -> pw-record:input_FR\n".to_vec())).unwrap();
    while mic.is_open() && saiu.elapsed() < Duration::from_secs(3) {
        vivo(&mut s, &mic, Duration::from_millis(10));
    }
    let demora = saiu.elapsed();
    assert!(!mic.is_open());
    assert!(demora < Duration::from_secs(1), "a captura levou {demora:?} para fechar");
    let p1 = mic.report().packets;
    vivo(&mut s, &mic, Duration::from_millis(400));
    assert_eq!(mic.report().packets, p1, "nenhum pacote novo depois do desligamento");
    assert_eq!(mic.report().offs, 1);

    tx.send(Message::Ping { value: 42 }).unwrap();
    let t = Instant::now();
    let mut pong = false;
    while t.elapsed() < Duration::from_secs(2) && !pong {
        match read_message(&mut s) {
            Ok(Message::Pong { echo: 42 }) => pong = true,
            Ok(Message::Frame { win, seq, .. }) => {
                let _ = tx.send(Message::FrameAck { win, seq });
            }
            _ => {}
        }
    }
    assert!(pong, "a sessão deveria continuar viva");
    mic.shutdown();
    drop(tx);
    drop(s);
    let _ = escritor.join();
    server.join().unwrap();
}
