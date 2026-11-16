//! Marco 7B: o microfone pelo protocolo, com um PipeWire falso (monitor de ligações por canal, soquete de PCM por
//! TCP local). O cliente de teste fala o protocolo direto.

use std::io::Read;
use std::net::{TcpListener, TcpStream};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use ps_audio::{encode, synth, CODEC_PSA1};
use ps_proto::{read_hello, read_message, write_message, Message, ProtoError, AUDIO_STREAM_MIC, CAP_AUDIO, CAP_BLOCK_CODEC, CAP_MIC};
use ps_server::mic::{LinkFeed, MicHub, OpenFeed, OFF_DELAY};
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

struct Cenario {
    addr: std::net::SocketAddr,
    pcm: TcpListener,
    monitor: mpsc::Sender<Option<Vec<u8>>>,
    server: std::thread::JoinHandle<()>,
}

fn monta(com_mic: bool) -> Cenario {
    let pcm = TcpListener::bind("127.0.0.1:0").unwrap();
    let pcm_addr = pcm.local_addr().unwrap().to_string();
    let (tx, rx) = mpsc::channel::<Option<Vec<u8>>>();
    let rx = Arc::new(Mutex::new(Some(rx)));
    let tx2 = tx.clone();
    let open: OpenFeed = Arc::new(move || {
        let rx = rx.lock().unwrap().take().ok_or_else(|| std::io::Error::other("uma abertura só"))?;
        let tx3 = tx2.clone();
        let kill: Arc<dyn Fn() + Send + Sync> = Arc::new(move || {
            let _ = tx3.send(None);
        });
        Ok(LinkFeed { reader: Box::new(LeitorCanal { rx, resto: Vec::new(), fim: false }), kill })
    });
    let hub = MicHub::new(open, &pcm_addr, Arc::new(|_| {}));
    let acceptor = TcpAcceptor::bind("127.0.0.1:0").unwrap();
    let addr = acceptor.local_addr().unwrap();
    let opts = ServerOptions { quiet: true, mic: if com_mic { Some(hub) } else { None }, ..ServerOptions::default() };
    let server = std::thread::spawn(move || serve_loop(&acceptor, &opts, Some(1)).unwrap());
    Cenario { addr, pcm, monitor: tx, server }
}

fn conecta(c: &Cenario, caps: u32) -> (TcpStream, u32) {
    let mut s = TcpStream::connect(c.addr).unwrap();
    write_message(&mut s, &Message::hello_with_caps("teste do microfone", caps)).unwrap();
    let (caps_srv, _) = read_hello(&mut s).unwrap();
    s.set_read_timeout(Some(Duration::from_millis(50))).unwrap();
    (s, caps_srv)
}

/// Lê mensagens por até `dur` (ou até haver `ate_n` pedidos em `resp`), confirmando os quadros de imagem.
fn pedidos(s: &mut TcpStream, dur: Duration, ate_n: usize, resp: &mut Vec<(Instant, bool)>) {
    let t0 = Instant::now();
    while t0.elapsed() < dur && resp.len() < ate_n {
        match read_message(s) {
            Ok(Message::AudioRequest { stream, on }) => {
                assert_eq!(stream, AUDIO_STREAM_MIC);
                resp.push((Instant::now(), on));
            }
            Ok(Message::Frame { win, seq, .. }) => write_message(s, &Message::FrameAck { win, seq }).unwrap(),
            Ok(_) => {}
            Err(ProtoError::Io(e)) if matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut) => {}
            Err(e) => panic!("erro de leitura: {e}"),
        }
    }
}

/// Manda um `Ping` e espera o `Pong`: prova que a sessão continua viva.
fn sessao_viva(s: &mut TcpStream, valor: u64) -> bool {
    write_message(s, &Message::Ping { value: valor }).unwrap();
    let t = Instant::now();
    while t.elapsed() < Duration::from_secs(2) {
        match read_message(s) {
            Ok(Message::Pong { echo }) if echo == valor => return true,
            Ok(Message::Frame { win, seq, .. }) => write_message(s, &Message::FrameAck { win, seq }).unwrap(),
            _ => {}
        }
    }
    false
}

#[test]
fn pedido_nasce_do_gravador_e_o_pcm_chega_ao_soquete() {
    let c = monta(true);
    let (mut s, caps_srv) = conecta(&c, CAP_BLOCK_CODEC | CAP_MIC);
    assert_ne!(caps_srv & CAP_MIC, 0);
    let mut ped = Vec::new();
    // Sem gravador: nenhum pedido em 700 ms e ninguém conecta no soquete do PipeWire.
    pedidos(&mut s, Duration::from_millis(700), 1, &mut ped);
    assert!(ped.is_empty(), "sem gravador não há pedido: {ped:?}");
    c.pcm.set_nonblocking(true).unwrap();
    assert!(c.pcm.accept().is_err());
    c.pcm.set_nonblocking(false).unwrap();

    // O gravador entra.
    c.monitor.send(Some(b"+ pistick-mic:capture_FL -> pw-record:input_FL\n+ pistick-mic:capture_FR -> pw-record:input_FR\n".to_vec())).unwrap();
    pedidos(&mut s, Duration::from_secs(2), 1, &mut ped);
    assert_eq!(ped.len(), 1);
    assert!(ped[0].1, "o pedido liga");

    // O cliente responde e manda 1 s de tom em pacotes de 10 ms (grade absoluta: o sleep do Windows é grosso).
    write_message(&mut s, &Message::AudioStart { stream: AUDIO_STREAM_MIC, codec: CODEC_PSA1, channels: 2, rate: 48000, frames: 480 }).unwrap();
    let (mut pcm, _) = c.pcm.accept().unwrap();
    let t0 = Instant::now();
    let mut seq = 0u32;
    let mut recebido: Vec<u8> = Vec::new();
    let mut buf = [0u8; 8192];
    while t0.elapsed() < Duration::from_millis(1000) {
        while u128::from(seq) * 10 <= t0.elapsed().as_millis() && seq < 100 {
            let bloco: Vec<i16> = (0..480u64)
                .flat_map(|i| {
                    let (l, r) = synth::frame(u64::from(seq) * 480 + i);
                    [l, r]
                })
                .collect();
            let mut carga = Vec::new();
            encode(&bloco, 2, &mut carga).unwrap();
            write_message(&mut s, &Message::AudioData { stream: AUDIO_STREAM_MIC, flags: 0, seq, pts_us: u64::from(seq) * 10_000, data: carga }).unwrap();
            seq += 1;
        }
        // Drena o PCM e as mensagens do servidor sem bloquear muito.
        pcm.set_read_timeout(Some(Duration::from_millis(2))).unwrap();
        if let Ok(n) = pcm.read(&mut buf) {
            recebido.extend_from_slice(&buf[..n]);
        }
        pedidos(&mut s, Duration::from_millis(1), 99, &mut ped);
    }
    // A escrita no PipeWire não para sozinha: lê só mais 200 ms.
    let fim_leitura = Instant::now() + Duration::from_millis(200);
    while Instant::now() < fim_leitura {
        match pcm.read(&mut buf) {
            Ok(n) if n > 0 => recebido.extend_from_slice(&buf[..n]),
            _ => break,
        }
    }
    let amostras: Vec<i16> = recebido.as_chunks::<2>().0.iter().map(|c| i16::from_le_bytes(*c)).collect();
    assert!(amostras.len() >= 480 * 2 * 80, "recebeu {} quadros", amostras.len() / 2);
    let (l, r) = synth::split_stereo(&amostras[(30 * 480 * 2)..(80 * 480 * 2)]);
    assert!((synth::goertzel(&l, 440.0) - 0.25).abs() < 0.02, "esq 440 = {}", synth::goertzel(&l, 440.0));
    assert!((synth::goertzel(&r, 880.0) - 0.25).abs() < 0.02, "dir 880 = {}", synth::goertzel(&r, 880.0));

    // O gravador sai: o pedido de desligar chega depois da espera e em menos de 1 s.
    let saiu = Instant::now();
    c.monitor.send(Some(b"- pistick-mic:capture_FL -> pw-record:input_FL\n- pistick-mic:capture_FR -> pw-record:input_FR\n".to_vec())).unwrap();
    let mut desl = Vec::new();
    pedidos(&mut s, Duration::from_secs(2), 1, &mut desl);
    assert_eq!(desl.len(), 1);
    assert!(!desl[0].1, "o pedido desliga");
    let atraso = desl[0].0.duration_since(saiu);
    assert!(atraso >= OFF_DELAY - Duration::from_millis(60) && atraso < Duration::from_secs(1), "atraso do desligamento {atraso:?}");
    // Pacotes que ainda estavam a caminho depois do pedido são descartados sem derrubar a sessão.
    write_message(&mut s, &Message::AudioData { stream: AUDIO_STREAM_MIC, flags: 0, seq, pts_us: 0, data: vec![0, 0, 0, 0] }).unwrap();
    write_message(&mut s, &Message::AudioStop { stream: AUDIO_STREAM_MIC }).unwrap();
    assert!(sessao_viva(&mut s, 77), "a sessão continua viva");
    drop(s);
    c.server.join().unwrap();
}

#[test]
fn sem_o_bit_do_cliente_nao_ha_pedido_mesmo_com_gravador() {
    let c = monta(true);
    let (mut s, caps_srv) = conecta(&c, CAP_BLOCK_CODEC | CAP_AUDIO);
    assert_ne!(caps_srv & CAP_MIC, 0, "o servidor anuncia o bit");
    // Mesmo com um gravador, o servidor não pede (e nem sobe o monitor de ligações: a origem falsa nem é aberta).
    let _ = c.monitor.send(Some(b"+ pistick-mic:capture_FL -> pw-record:input_FL\n".to_vec()));
    let mut ped = Vec::new();
    pedidos(&mut s, Duration::from_millis(1200), 1, &mut ped);
    assert!(ped.is_empty());
    // Mensagem do microfone sem a capacidade: descartada, a sessão segue.
    write_message(&mut s, &Message::AudioData { stream: AUDIO_STREAM_MIC, flags: 0, seq: 0, pts_us: 0, data: vec![0; 8] }).unwrap();
    assert!(sessao_viva(&mut s, 5));
    c.pcm.set_nonblocking(true).unwrap();
    assert!(c.pcm.accept().is_err(), "ninguém escreve no PipeWire");
    drop(s);
    c.server.join().unwrap();
}

#[test]
fn servidor_sem_microfone_nao_anuncia_o_bit() {
    let c = monta(false);
    let (s, caps_srv) = conecta(&c, CAP_BLOCK_CODEC | CAP_MIC);
    assert_eq!(caps_srv & CAP_MIC, 0);
    drop(s);
    c.server.join().unwrap();
}
