
use std::collections::HashMap;
use std::io;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use ps_codec::testutil::video;
use ps_codec::{Decoder, Rect};
use ps_proto::{kind, read_hello, read_message, write_message, Message, ProtoError, CAP_BLOCK_CODEC, CAP_H264, CODEC_H264, VIDEO_FLAG_KEY};
use ps_server::video::{EncodedFrame, VideoConfig, VideoEncoder, VideoEncoderFactory};
use ps_server::{serve_loop, serve_loop_with, ServerOptions, SourceEvent, SyntheticVideo, Waker, WindowSource};
use ps_transport::{Closer, Split, TcpAcceptor, TcpTransport, Transport};

const PAUSE: Duration = Duration::from_secs(3);
const WIN: u32 = 1;

struct FakeFactory {
    opens: AtomicUsize,
    encodes: Arc<AtomicUsize>,
    cfgs: Mutex<Vec<VideoConfig>>,
    fail_open: bool,
    fail_encode_at: Option<usize>,
}

impl FakeFactory {
    fn new() -> Arc<FakeFactory> {
        Arc::new(FakeFactory { opens: AtomicUsize::new(0), encodes: Arc::new(AtomicUsize::new(0)), cfgs: Mutex::new(Vec::new()), fail_open: false, fail_encode_at: None })
    }

    fn with(fail_open: bool, fail_encode_at: Option<usize>) -> Arc<FakeFactory> {
        Arc::new(FakeFactory { opens: AtomicUsize::new(0), encodes: Arc::new(AtomicUsize::new(0)), cfgs: Mutex::new(Vec::new()), fail_open, fail_encode_at })
    }
}

impl VideoEncoderFactory for FakeFactory {
    fn open(&self, cfg: &VideoConfig) -> io::Result<Box<dyn VideoEncoder>> {
        self.opens.fetch_add(1, Ordering::SeqCst);
        self.cfgs.lock().unwrap().push(*cfg);
        if self.fail_open {
            return Err(io::Error::other("codificador falso recusa abrir"));
        }
        Ok(Box::new(FakeEncoder { cfg: *cfg, encodes: self.encodes.clone(), frames: 0, fail_at: self.fail_encode_at }))
    }
}

struct FakeEncoder {
    cfg: VideoConfig,
    encodes: Arc<AtomicUsize>,
    frames: usize,
    fail_at: Option<usize>,
}

impl VideoEncoder for FakeEncoder {
    fn encode(&mut self, pixels: &[u8], stride: usize) -> io::Result<EncodedFrame> {
        self.encodes.fetch_add(1, Ordering::SeqCst);
        if self.fail_at == Some(self.frames) {
            return Err(io::Error::new(io::ErrorKind::TimedOut, "codificador falso estourou o prazo"));
        }
        let (w, h) = (self.cfg.width as usize, self.cfg.height as usize);
        assert!(pixels.len() >= stride * (h - 1) + w * 4, "região menor que o declarado");
        let mut data = Vec::with_capacity(w * h * 4);
        for row in 0..h {
            data.extend_from_slice(&pixels[row * stride..row * stride + w * 4]);
        }
        let keyframe = self.frames == 0;
        self.frames += 1;
        Ok(EncodedFrame { data, keyframe })
    }
}

type Hook = Arc<Mutex<HashMap<(u32, u32), (Vec<u8>, std::ops::Range<usize>)>>>;

fn serial() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: Mutex<()> = Mutex::new(());
    LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

struct Harness {
    _serial: std::sync::MutexGuard<'static, ()>,
    addr: std::net::SocketAddr,
    hook: Hook,
    server: Option<JoinHandle<()>>,
}

impl Harness {
    fn start(factory: Option<Arc<FakeFactory>>, pause: Option<Duration>) -> Harness {
        let acceptor = TcpAcceptor::bind("127.0.0.1:0").unwrap();
        let addr = acceptor.local_addr().unwrap();
        let hook: Hook = Arc::default();
        let opts = Self::options(factory, pause, hook.clone());
        let server = std::thread::spawn(move || serve_loop(&acceptor, &opts, Some(1)).unwrap());
        Harness { addr, hook, server: Some(server), _serial: serial() }
    }

    fn start_blink(factory: Option<Arc<FakeFactory>>) -> Harness {
        let acceptor = TcpAcceptor::bind("127.0.0.1:0").unwrap();
        let addr = acceptor.local_addr().unwrap();
        let hook: Hook = Arc::default();
        let opts = Self::options(factory, None, hook.clone());
        let server = std::thread::spawn(move || serve_loop_with(&acceptor, &opts, Some(1), || Box::new(BlinkSource::new()) as Box<dyn WindowSource>).unwrap());
        Harness { addr, hook, server: Some(server), _serial: serial() }
    }

    fn options(factory: Option<Arc<FakeFactory>>, pause: Option<Duration>, hook: Hook) -> ServerOptions {
        let h = hook;
        ServerOptions {
            hello_timeout: Duration::from_millis(400),
            quiet: std::env::var("PS_LOG").is_err(),
            flow_initial_throughput: 1e10,
            flow_adaptive: false,
            synthetic_video: Some(SyntheticVideo { pause_after: pause }),
            video: factory.map(|f| f as Arc<dyn VideoEncoderFactory>),
            frame_hook: Some(Arc::new(move |f| {
                h.lock().unwrap().insert((f.win, f.seq), (f.pixels.to_vec(), f.strips.clone()));
            })),
            ..ServerOptions::default()
        }
    }

    fn client(&self, caps: u32) -> Client {
        Client::connect(self.addr, caps, self.hook.clone())
    }

    fn finish(mut self) {
        self.server.take().unwrap().join().unwrap();
    }
}

#[derive(Debug, Clone, PartialEq)]
enum Ev {
    Create,
    Resize(u32, u32),
    Frame { rects: Vec<Rect> },
    VideoStart { stream: u32, region: Rect },
    VideoFrame { stream: u32, seq: u32, key: bool },
    VideoStop { stream: u32 },
}

struct Client {
    rx: Receiver<(Instant, Result<Message, ProtoError>)>,
    writer: std::net::TcpStream,
    closer: Closer,
    hook: Hook,
    t0: Instant,
    server_caps: u32,
    dec: Option<Decoder>,
    stream: Option<u32>,
    last_stream: u32,
    region: Rect,
    video_img: Option<Vec<u8>>,
    img_at_stop: Option<Vec<u8>>,
    last_seq: Option<u32>,
    evs: Vec<(Instant, Ev)>,
    kinds: Vec<u16>,
    auto_ack: bool,
    ack_delay: Duration,
    origem_parada: Vec<u8>,
    origem_dim: (u32, u32),
    static_checks: usize,
    check_static: bool,
}

impl Client {
    fn connect(addr: std::net::SocketAddr, caps: u32, hook: Hook) -> Client {
        let Split { mut reader, mut writer, closer } = TcpTransport::connect(addr).unwrap().split().unwrap();
        write_message(&mut writer, &Message::hello_with_caps("cliente de teste de vídeo", caps)).unwrap();
        let (server_caps, name) = read_hello(&mut reader).expect("HELLO do servidor");
        assert!(name.starts_with("psd"), "nome: {name}");
        let t0 = Instant::now();
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || loop {
            let r = read_message(&mut reader);
            let fim = r.is_err();
            if tx.send((Instant::now(), r)).is_err() || fim {
                break;
            }
        });
        Client {
            rx,
            writer,
            closer,
            hook,
            t0,
            server_caps,
            dec: None,
            stream: None,
            last_stream: 0,
            region: Rect::default(),
            video_img: None,
            img_at_stop: None,
            last_seq: None,
            evs: Vec::new(),
            kinds: Vec::new(),
            auto_ack: true,
            ack_delay: Duration::ZERO,
            origem_parada: video::render(video::WIDTH, video::HEIGHT, 0),
            origem_dim: (video::WIDTH, video::HEIGHT),
            static_checks: 0,
            check_static: true,
        }
    }

    fn send(&mut self, m: Message) {
        write_message(&mut self.writer, &m).unwrap();
    }

    fn next(&mut self, dur: Duration) -> bool {
        match self.rx.recv_timeout(dur) {
            Ok((at, Ok(m))) => {
                self.process(at, &m);
                true
            }
            Ok((_, Err(e))) => panic!("erro de leitura no cliente de teste: {e}"),
            Err(RecvTimeoutError::Timeout) => false,
            Err(RecvTimeoutError::Disconnected) => panic!("leitor do cliente terminou"),
        }
    }

    fn check_seq(&mut self, seq: u32) {
        if let Some(prev) = self.last_seq.replace(seq) {
            assert_eq!(seq, prev.wrapping_add(1), "FRAME e VIDEO_FRAME dividem o mesmo contador de seq, de 1 em 1");
        }
    }

    fn process(&mut self, at: Instant, m: &Message) {
        self.kinds.push(m.kind());
        match m {
            Message::WinCreate { win, width, height, .. } => {
                assert_eq!(*win, WIN);
                self.dec = Some(Decoder::new(*width, *height));
                self.evs.push((at, Ev::Create));
            }
            Message::WinResize { win, width, height } => {
                assert_eq!(*win, WIN);
                self.dec.as_mut().unwrap().reset(*width, *height);
                self.stream = None;
                self.video_img = None;
                self.evs.push((at, Ev::Resize(*width, *height)));
            }
            Message::Frame { win, seq, width, height, data, .. } => {
                self.check_seq(*seq);
                let dec = self.dec.as_mut().expect("quadro de janela desconhecida");
                assert_eq!((dec.width(), dec.height()), (*width, *height));
                let rects = dec.decode(data).expect("decodificação do quadro de blocos");
                let (esperado, strips) = self.hook.lock().unwrap().remove(&(*win, *seq)).expect("o servidor registrou o quadro");
                let (y0, y1) = (strips.start * 64, (strips.end * 64).min(*height as usize));
                let row = *width as usize * 4;
                let feita = self.dec.as_ref().unwrap().pixels();
                for y in y0..y1 {
                    let r = self.region;
                    let na_regiao = self.stream.is_some() && (r.y as usize..(r.y + r.h) as usize).contains(&y);
                    let (a, b) = (&feita[y * row..(y + 1) * row], &esperado[y * row..(y + 1) * row]);
                    if na_regiao {
                        let (x0, x1) = (r.x as usize * 4, (r.x + r.w) as usize * 4);
                        assert!(a[..x0] == b[..x0] && a[x1..] == b[x1..], "FRAME {seq}: linha {y} fora da região difere do que o servidor gerou");
                    } else {
                        assert!(a == b, "FRAME {seq}: linha {y} difere do que o servidor gerou");
                    }
                }
                if self.stream.is_none() {
                    self.video_img = None;
                }
                self.evs.push((at, Ev::Frame { rects }));
                if self.auto_ack {
                    if !self.ack_delay.is_zero() {
                        std::thread::sleep(self.ack_delay);
                    }
                    self.send(Message::FrameAck { win: *win, seq: *seq });
                }
            }
            Message::VideoStart { win, stream, x, y, width, height, codec } => {
                assert_eq!(*win, WIN);
                assert!(self.stream.is_none(), "um fluxo por vez");
                assert!(*stream > self.last_stream, "identificador de fluxo crescente");
                assert_eq!(*codec, CODEC_H264);
                assert!(x.is_multiple_of(64) && y.is_multiple_of(64) && width % 2 == 0 && height % 2 == 0, "origem em blocos e tamanho par");
                let dim = self.dec.as_ref().unwrap();
                assert!((x + width).is_multiple_of(64) || x + width == dim.width(), "borda direita em bloco ou na borda da janela");
                assert!((y + height).is_multiple_of(64) || y + height == dim.height(), "borda inferior em bloco ou na borda da janela");
                assert!(*width >= 256 && *height >= 256 && *width <= 1920 && *height <= 1080, "tamanho da região");
                self.last_stream = *stream;
                self.stream = Some(*stream);
                self.region = Rect::new(*x, *y, *width, *height);
                self.video_img = None;
                self.evs.push((at, Ev::VideoStart { stream: *stream, region: self.region }));
            }
            Message::VideoFrame { win, stream, seq, flags, data, .. } => {
                assert_eq!(*win, WIN);
                assert_eq!(self.stream, Some(*stream), "VIDEO_FRAME de fluxo que não está ativo");
                self.check_seq(*seq);
                let key = flags & VIDEO_FLAG_KEY != 0;
                assert_eq!(key, self.video_img.is_none(), "só o primeiro quadro do fluxo é chave (seq {seq})");
                assert_eq!(data.len(), self.region.w as usize * self.region.h as usize * 4, "tamanho do quadro falso");
                self.video_img = Some(data.clone());
                if self.check_static {
                    self.check_parte_parada();
                }
                self.evs.push((at, Ev::VideoFrame { stream: *stream, seq: *seq, key }));
                if self.auto_ack {
                    self.send(Message::FrameAck { win: *win, seq: *seq });
                }
            }
            Message::VideoStop { win, stream } => {
                assert_eq!(*win, WIN);
                assert_eq!(self.stream, Some(*stream));
                self.stream = None;
                self.img_at_stop = self.video_img.clone();
                self.evs.push((at, Ev::VideoStop { stream: *stream }));
            }
            _ => {}
        }
    }

    fn check_parte_parada(&mut self) {
        let dim = (self.dec.as_ref().unwrap().width(), self.dec.as_ref().unwrap().height());
        if self.origem_dim != dim {
            self.origem_parada = video::render(dim.0, dim.1, 0);
            self.origem_dim = dim;
        }
        let dec = self.dec.as_ref().unwrap();
        let (w, h) = (dim.0 as usize, dim.1 as usize);
        let r = self.region;
        let (rx0, rx1, ry0, ry1) = (r.x as usize, (r.x + r.w) as usize, r.y as usize, (r.y + r.h) as usize);
        let (feita, origem) = (dec.pixels(), &self.origem_parada);
        for y in 0..h {
            let linha = y * w * 4..(y + 1) * w * 4;
            if (ry0..ry1).contains(&y) {
                assert!(feita[linha.start..linha.start + rx0 * 4] == origem[linha.start..linha.start + rx0 * 4], "linha {y}, à esquerda da região");
                assert!(feita[linha.start + rx1 * 4..linha.end] == origem[linha.start + rx1 * 4..linha.end], "linha {y}, à direita da região");
            } else {
                assert!(feita[linha.clone()] == origem[linha], "linha {y}, fora da região");
            }
        }
        self.static_checks += 1;
    }

    fn composto(&self) -> Vec<u8> {
        let dec = self.dec.as_ref().unwrap();
        let mut img = dec.pixels().to_vec();
        if let Some(v) = &self.video_img {
            let (w, r) = (dec.width() as usize, self.region);
            for row in 0..r.h as usize {
                let dst = ((r.y as usize + row) * w + r.x as usize) * 4;
                img[dst..dst + r.w as usize * 4].copy_from_slice(&v[row * r.w as usize * 4..(row + 1) * r.w as usize * 4]);
            }
        }
        img
    }

    fn until(&mut self, secs: u64, mut cond: impl FnMut(&Client) -> bool) {
        let limite = Instant::now() + Duration::from_secs(secs);
        while !cond(self) {
            assert!(Instant::now() < limite, "tempo esgotado esperando a condição; eventos: {}", self.resumo());
            self.next(Duration::from_millis(20));
        }
    }

    fn drain(&mut self, dur: Duration) {
        let fim = Instant::now() + dur;
        while Instant::now() < fim {
            self.next(Duration::from_millis(10));
        }
    }

    fn resumo(&self) -> String {
        let mut s = String::new();
        let mut ultimo: Option<String> = None;
        let mut n = 0;
        for (_, e) in &self.evs {
            let nome = match e {
                Ev::Create => "Create".to_string(),
                Ev::Resize(w, h) => format!("Resize({w}x{h})"),
                Ev::Frame { .. } => "Frame".to_string(),
                Ev::VideoStart { stream, .. } => format!("VideoStart({stream})"),
                Ev::VideoFrame { .. } => "VideoFrame".to_string(),
                Ev::VideoStop { stream } => format!("VideoStop({stream})"),
            };
            if ultimo.as_deref() == Some(&nome) {
                n += 1;
            } else {
                if let Some(u) = ultimo.take() {
                    s += &format!("{u}×{n} ");
                }
                ultimo = Some(nome);
                n = 1;
            }
        }
        if let Some(u) = ultimo {
            s += &format!("{u}×{n}");
        }
        s
    }

    fn pos(&self, pred: impl Fn(&Ev) -> bool) -> Option<usize> {
        self.evs.iter().position(|(_, e)| pred(e))
    }

    fn n_video_frames(&self) -> usize {
        self.evs.iter().filter(|(_, e)| matches!(e, Ev::VideoFrame { .. })).count()
    }

    fn drop_connection(self) {
        self.closer.close();
    }
}

fn cobre(rects: &[Rect], region: Rect) -> bool {
    let area: u64 = rects.iter().filter_map(|r| r.intersect(region)).map(|r| u64::from(r.w) * u64::from(r.h)).sum();
    area == u64::from(region.w) * u64::from(region.h)
}

fn so_blocos(caps_do_cliente: u32) -> u32 {
    caps_do_cliente & !CAP_H264
}

const BLINK: Rect = Rect::new(900, 600, 48, 48);

struct BlinkSource {
    buf: Vec<u8>,
    tick: u64,
    next_ms: u64,
    started: bool,
}

impl BlinkSource {
    fn new() -> Self {
        BlinkSource { buf: Vec::new(), tick: 0, next_ms: 0, started: false }
    }

    fn aceso(tick: u64) -> bool {
        (tick / 10).is_multiple_of(2)
    }

    fn desenha(&mut self) {
        self.buf = video::render(video::WIDTH, video::HEIGHT, self.tick);
        if Self::aceso(self.tick) {
            for y in BLINK.y..BLINK.y + BLINK.h {
                let a = ((y * video::WIDTH + BLINK.x) * 4) as usize;
                self.buf[a..a + BLINK.w as usize * 4].fill(0xFF);
            }
        }
    }
}

impl WindowSource for BlinkSource {
    fn start(&mut self, _waker: Waker) -> Vec<SourceEvent> {
        vec![SourceEvent::Created { win: WIN, width: video::WIDTH, height: video::HEIGHT, title: "teste".into(), app_id: "teste".into() }]
    }

    fn poll(&mut self, now_ms: u64) -> Vec<SourceEvent> {
        if now_ms < self.next_ms {
            return Vec::new();
        }
        self.next_ms = now_ms + video::FRAME_MS;
        let anterior = self.tick;
        if self.started {
            self.tick += 1;
        }
        self.desenha();
        let rects = if !self.started {
            self.started = true;
            vec![Rect::new(0, 0, video::WIDTH, video::HEIGHT)]
        } else if Self::aceso(anterior) != Self::aceso(self.tick) {
            vec![Rect::new(0, 0, video::WIDTH, video::HEIGHT)]
        } else {
            vec![video::REGION]
        };
        vec![SourceEvent::Damage { win: WIN, rects }]
    }

    fn next_poll_in(&self, now_ms: u64) -> Option<Duration> {
        Some(Duration::from_millis(self.next_ms.saturating_sub(now_ms)))
    }

    fn with_pixels(&mut self, _win: u32, f: &mut dyn FnMut(u32, u32, &[u8])) {
        f(video::WIDTH, video::HEIGHT, &self.buf);
    }

    fn input(&mut self, _msg: &Message) {}

    fn frame_sent(&mut self, _win: u32) {}
}

#[test]
fn sequencia_video_start_quadros_video_stop_e_frame_sem_perdas() {
    let fab = FakeFactory::new();
    let h = Harness::start(Some(fab.clone()), Some(PAUSE));
    let mut c = h.client(CAP_BLOCK_CODEC | CAP_H264);
    assert_eq!(c.server_caps & CAP_H264, CAP_H264, "com fábrica, o servidor anuncia CAP_H264");
    c.until(20, |c| c.pos(|e| matches!(e, Ev::VideoStop { .. })).is_some_and(|i| c.evs[i..].iter().any(|(_, e)| matches!(e, Ev::Frame { .. }))));

    let i_start = c.pos(|e| matches!(e, Ev::VideoStart { .. })).unwrap();
    let i_vf = c.pos(|e| matches!(e, Ev::VideoFrame { .. })).unwrap();
    let i_stop = c.pos(|e| matches!(e, Ev::VideoStop { .. })).unwrap();
    let i_frame = i_stop + c.evs[i_stop..].iter().position(|(_, e)| matches!(e, Ev::Frame { .. })).unwrap();
    assert!(i_start < i_vf && i_vf < i_stop && i_stop < i_frame, "ordem: {}", c.resumo());
    assert_eq!(i_vf, i_start + 1, "o primeiro VIDEO_FRAME vem logo depois do VIDEO_START");
    assert_eq!(c.evs[i_start].1, Ev::VideoStart { stream: 1, region: video::REGION });
    assert!(c.evs[i_start + 1..i_stop].iter().all(|(_, e)| matches!(e, Ev::VideoFrame { stream: 1, .. })), "entre START e STOP só quadros de vídeo: {}", c.resumo());
    assert!(c.n_video_frames() >= 30, "poucos quadros de vídeo: {}", c.n_video_frames());
    assert_eq!(fab.opens.load(Ordering::SeqCst), 1);
    let cfg = fab.cfgs.lock().unwrap()[0];
    assert_eq!((cfg.width, cfg.height, cfg.fps, cfg.bitrate), (640, 384, 30, 5_000_000));
    let entrada = c.evs[i_start].0.duration_since(c.t0);
    assert!(entrada < Duration::from_secs(2), "entrou em vídeo só depois de {entrada:?}");

    let chaves: Vec<bool> = c.evs.iter().filter_map(|(_, e)| if let Ev::VideoFrame { key, .. } = e { Some(*key) } else { None }).collect();
    assert!(chaves[0] && chaves[1..].iter().all(|k| !k));

    assert_eq!(c.static_checks, c.n_video_frames());
    let ultimo = video::frame_at(PAUSE.as_millis() as u64, Some(PAUSE.as_millis() as u64));
    let origem = video::render(video::WIDTH, video::HEIGHT, ultimo);
    let r = video::REGION;
    let img = c.img_at_stop.as_ref().expect("quadro de vídeo no VIDEO_STOP");
    for row in 0..r.h as usize {
        let o = ((r.y as usize + row) * video::WIDTH as usize + r.x as usize) * 4;
        assert!(img[row * r.w as usize * 4..(row + 1) * r.w as usize * 4] == origem[o..o + r.w as usize * 4], "linha {row} da região no último quadro de vídeo");
    }

    let t_frame = c.evs[i_frame].0;
    let atraso = t_frame.duration_since(c.t0 + PAUSE);
    assert!(t_frame < c.t0 + PAUSE + Duration::from_millis(1500), "o FRAME sem perdas chegou {atraso:?} depois da pausa");
    let Ev::Frame { rects, .. } = &c.evs[i_frame].1 else { unreachable!() };
    assert!(cobre(rects, video::REGION), "o FRAME depois do VIDEO_STOP tem de codificar todos os blocos da região sem SKIP");
    assert!(c.composto() == origem, "a janela inteira difere da origem depois da pausa");
    assert!(c.dec.as_ref().unwrap().pixels() == origem.as_slice(), "o decodificador de blocos sozinho tem de ter a janela inteira");

    c.drop_connection();
    h.finish();
}

#[test]
fn cliente_sem_h264_nunca_recebe_mensagens_de_video() {
    let fab = FakeFactory::new();
    let h = Harness::start(Some(fab.clone()), None);
    let mut c = h.client(so_blocos(CAP_BLOCK_CODEC | CAP_H264));
    assert_eq!(c.server_caps & CAP_H264, CAP_H264);
    c.until(20, |c| c.evs.iter().filter(|(_, e)| matches!(e, Ev::Frame { .. })).count() >= 100);
    assert!(
        !c.kinds.iter().any(|k| [kind::VIDEO_START, kind::VIDEO_FRAME, kind::VIDEO_STOP].contains(k)),
        "cliente sem CAP_H264 recebeu mensagem de vídeo: {:?}",
        c.kinds.iter().filter(|k| (27..=29).contains(*k)).collect::<Vec<_>>()
    );
    assert_eq!(fab.opens.load(Ordering::SeqCst), 0, "nem o codificador chega a ser aberto");
    assert_eq!(fab.encodes.load(Ordering::SeqCst), 0);
    c.drop_connection();
    h.finish();
}

#[test]
fn servidor_sem_fabrica_nao_anuncia_h264_e_nada_muda() {
    let h = Harness::start(None, None);
    let mut c = h.client(CAP_BLOCK_CODEC | CAP_H264);
    assert_eq!(c.server_caps & CAP_H264, 0, "sem codificador, sem CAP_H264 no HELLO");
    c.until(20, |c| c.evs.iter().filter(|(_, e)| matches!(e, Ev::Frame { .. })).count() >= 100);
    assert!(!c.kinds.iter().any(|k| (27..=29).contains(k)));
    c.drop_connection();
    h.finish();
}

#[test]
fn fila_cheia_nao_alimenta_o_codificador() {
    let fab = FakeFactory::new();
    let h = Harness::start(Some(fab.clone()), None);
    let mut c = h.client(CAP_BLOCK_CODEC | CAP_H264);
    c.until(20, |c| c.n_video_frames() >= 1);
    c.auto_ack = false;
    let ate_aqui = c.n_video_frames();
    c.drain(Duration::from_millis(700));
    let recebidos = c.n_video_frames() - ate_aqui;
    assert!(recebidos >= 1, "o servidor deveria ter enviado ao menos mais um quadro");
    assert!(recebidos <= 2, "o limite de 2 quadros sem confirmação foi passado: {recebidos} quadros ({})", c.resumo());
    let chamadas = fab.encodes.load(Ordering::SeqCst);
    assert_eq!(chamadas, c.n_video_frames(), "todo quadro codificado foi enviado, e nenhum foi codificado a mais");
    c.drain(Duration::from_millis(700));
    assert_eq!(fab.encodes.load(Ordering::SeqCst), chamadas, "o codificador foi chamado com a fila cheia");
    assert_eq!(c.n_video_frames() - ate_aqui, recebidos);
    let Some((_, Ev::VideoFrame { seq, .. })) = c.evs.iter().rev().find(|(_, e)| matches!(e, Ev::VideoFrame { .. })).cloned() else { panic!() };
    let primeiro_sem_ack = seq - (recebidos as u32 - 1);
    c.send(Message::FrameAck { win: WIN, seq: primeiro_sem_ack });
    c.drain(Duration::from_millis(500));
    assert_eq!(c.n_video_frames() - ate_aqui, recebidos + 1, "uma confirmação libera um único quadro");
    assert_eq!(fab.encodes.load(Ordering::SeqCst), chamadas + 1);
    c.drop_connection();
    h.finish();
}

#[test]
fn win_resize_durante_o_video_encerra_o_fluxo_sem_video_stop() {
    let fab = FakeFactory::new();
    let h = Harness::start(Some(fab.clone()), None);
    let mut c = h.client(CAP_BLOCK_CODEC | CAP_H264);
    c.until(20, |c| c.n_video_frames() >= 5);
    assert_eq!(c.stream, Some(1));
    c.send(Message::ResizeReq { win: WIN, width: 800, height: 600 });
    c.until(10, |c| c.pos(|e| matches!(e, Ev::Resize(..))).is_some_and(|i| c.evs[i..].iter().any(|(_, e)| matches!(e, Ev::Frame { .. }))));

    let i_resize = c.pos(|e| matches!(e, Ev::Resize(800, 600))).unwrap();
    assert!(c.evs.iter().all(|(_, e)| !matches!(e, Ev::VideoStop { .. })), "WIN_RESIZE encerra o fluxo sem VIDEO_STOP: {}", c.resumo());
    assert!(c.evs[i_resize..].iter().all(|(_, e)| !matches!(e, Ev::VideoFrame { stream: 1, .. })));
    let (_, seguinte) = &c.evs[i_resize + 1];
    let Ev::Frame { rects, .. } = seguinte else { panic!("depois do WIN_RESIZE veio {seguinte:?}") };
    let area: u64 = rects.iter().map(|r| u64::from(r.w) * u64::from(r.h)).sum();
    assert_eq!(area, 800 * 600, "o primeiro quadro depois do WIN_RESIZE não pode ter SKIP");
    assert_eq!(c.stream, None);

    c.until(20, |c| c.n_video_frames() >= 8 && c.stream == Some(2));
    let i_start2 = c.pos(|e| matches!(e, Ev::VideoStart { stream: 2, .. })).unwrap();
    assert!(i_start2 > i_resize);
    assert!(matches!(c.evs[i_start2 + 1].1, Ev::VideoFrame { stream: 2, key: true, .. }));
    let Ev::VideoStart { region, .. } = c.evs[i_start2].1.clone() else { unreachable!() };
    assert_eq!(region, video::REGION);
    assert_eq!(fab.opens.load(Ordering::SeqCst), 2);
    c.drop_connection();
    h.finish();
}

#[test]
fn erro_do_codificador_encerra_o_video_e_volta_sem_perdas() {
    let fab = FakeFactory::with(false, Some(5));
    let h = Harness::start(Some(fab.clone()), None);
    let mut c = h.client(CAP_BLOCK_CODEC | CAP_H264);
    c.until(20, |c| c.pos(|e| matches!(e, Ev::VideoStop { .. })).is_some_and(|i| c.evs[i..].iter().any(|(_, e)| matches!(e, Ev::Frame { .. }))));
    assert_eq!(c.n_video_frames(), 5);
    let i_stop = c.pos(|e| matches!(e, Ev::VideoStop { .. })).unwrap();
    let (_, Ev::Frame { rects, .. }) = &c.evs[i_stop + 1] else { panic!("depois do VIDEO_STOP veio {:?}", c.evs[i_stop + 1].1) };
    assert!(cobre(rects, video::REGION), "a região volta sem SKIP");
    let antes = c.evs.len();
    c.drain(Duration::from_millis(2500));
    assert!(c.evs[antes..].iter().all(|(_, e)| matches!(e, Ev::Frame { .. })), "{}", c.resumo());
    assert!(c.evs.len() - antes >= 30, "o codec de blocos deveria seguir enviando quadros");
    assert_eq!(fab.opens.load(Ordering::SeqCst), 1);
    c.drop_connection();
    h.finish();
}

#[test]
fn codificador_que_nao_abre_deixa_tudo_no_codec_de_blocos() {
    let fab = FakeFactory::with(true, None);
    let h = Harness::start(Some(fab.clone()), None);
    let mut c = h.client(CAP_BLOCK_CODEC | CAP_H264);
    c.until(20, |c| c.evs.iter().filter(|(_, e)| matches!(e, Ev::Frame { .. })).count() >= 120);
    assert!(!c.kinds.iter().any(|k| (27..=29).contains(k)));
    assert_eq!(fab.opens.load(Ordering::SeqCst), 1);
    c.drop_connection();
    h.finish();
}

#[test]
fn dano_fora_da_regiao_durante_o_video_sai_pelo_codec_de_blocos_sem_tocar_a_regiao() {
    let fab = FakeFactory::new();
    let h = Harness::start_blink(Some(fab.clone()));
    let mut c = h.client(CAP_BLOCK_CODEC | CAP_H264);
    c.check_static = false;
    c.until(20, |c| {
        c.pos(|e| matches!(e, Ev::VideoStart { .. })).is_some_and(|i| c.evs[i..].iter().filter(|(_, e)| matches!(e, Ev::Frame { .. })).count() >= 4) && c.n_video_frames() >= 30
    });
    let i_start = c.pos(|e| matches!(e, Ev::VideoStart { .. })).unwrap();
    let durante: Vec<&Vec<Rect>> = c.evs[i_start..].iter().filter_map(|(_, e)| if let Ev::Frame { rects } = e { Some(rects) } else { None }).collect();
    assert!(durante.len() >= 4);
    for rects in durante {
        assert!(!rects.is_empty(), "um FRAME só de SKIP não deveria ser enviado");
        assert!(rects.iter().all(|r| r.intersect(video::REGION).is_none()), "durante o vídeo os blocos da região saem SKIP: {rects:?}");
        assert!(rects.iter().all(|r| r.intersect(BLINK).is_some()), "{rects:?}");
    }
    assert!(c.evs.iter().all(|(_, e)| !matches!(e, Ev::VideoStop { .. })));
    c.drop_connection();
    h.finish();
}

struct TwoSource {
    bufs: [Vec<u8>; 2],
    tick: u64,
    next_ms: u64,
    started: bool,
}

impl TwoSource {
    fn new() -> Self {
        TwoSource { bufs: [Vec::new(), Vec::new()], tick: 0, next_ms: 0, started: false }
    }
}

impl WindowSource for TwoSource {
    fn start(&mut self, _waker: Waker) -> Vec<SourceEvent> {
        (1..=2).map(|win| SourceEvent::Created { win, width: video::WIDTH, height: video::HEIGHT, title: "teste".into(), app_id: "teste".into() }).collect()
    }

    fn poll(&mut self, now_ms: u64) -> Vec<SourceEvent> {
        if now_ms < self.next_ms {
            return Vec::new();
        }
        self.next_ms = now_ms + video::FRAME_MS;
        let full = !self.started;
        self.started = true;
        self.tick += u64::from(!full);
        let parada = PAUSE.as_millis() as u64 / video::FRAME_MS;
        let mut out = Vec::new();
        for win in 1..=2u32 {
            let frame = if win == 1 { self.tick.min(parada) } else { self.tick };
            let antes = self.bufs[win as usize - 1].is_empty();
            if !antes && win == 1 && self.tick > parada {
                continue;
            }
            self.bufs[win as usize - 1] = video::render(video::WIDTH, video::HEIGHT, frame);
            let rects = if antes { vec![Rect::new(0, 0, video::WIDTH, video::HEIGHT)] } else { vec![video::REGION] };
            out.push(SourceEvent::Damage { win, rects });
        }
        out
    }

    fn next_poll_in(&self, now_ms: u64) -> Option<Duration> {
        Some(Duration::from_millis(self.next_ms.saturating_sub(now_ms)))
    }

    fn with_pixels(&mut self, win: u32, f: &mut dyn FnMut(u32, u32, &[u8])) {
        f(video::WIDTH, video::HEIGHT, &self.bufs[win as usize - 1]);
    }

    fn input(&mut self, _msg: &Message) {}

    fn frame_sent(&mut self, _win: u32) {}
}

#[test]
fn um_fluxo_de_video_por_vez_na_conexao_e_a_outra_janela_entra_depois() {
    let fab = FakeFactory::new();
    let acceptor = TcpAcceptor::bind("127.0.0.1:0").unwrap();
    let addr = acceptor.local_addr().unwrap();
    let mut opts = Harness::options(Some(fab.clone()), None, Hook::default());
    opts.frame_hook = None;
    let server = std::thread::spawn(move || serve_loop_with(&acceptor, &opts, Some(1), || Box::new(TwoSource::new()) as Box<dyn WindowSource>).unwrap());

    let Split { mut reader, mut writer, closer } = TcpTransport::connect(addr).unwrap().split().unwrap();
    write_message(&mut writer, &Message::hello_with_caps("cliente de teste de vídeo", CAP_BLOCK_CODEC | CAP_H264)).unwrap();
    read_hello(&mut reader).unwrap();

    let mut aberto: Option<(u32, u32)> = None;
    let mut inicios: Vec<(u32, u32)> = Vec::new();
    let mut frames_da_2_durante_o_1 = 0;
    let mut last_seq: HashMap<u32, u32> = HashMap::new();
    let limite = Instant::now() + Duration::from_secs(30);
    let mut fim = false;
    while !fim {
        assert!(Instant::now() < limite, "tempo esgotado; inícios: {inicios:?}");
        let m = read_message(&mut reader).expect("leitura");
        match &m {
            Message::VideoStart { win, stream, .. } => {
                assert_eq!(aberto, None, "VIDEO_START do fluxo {stream} (janela {win}) com o fluxo {aberto:?} ainda aberto");
                aberto = Some((*win, *stream));
                inicios.push((*win, *stream));
            }
            Message::VideoStop { win, stream } => {
                assert_eq!(aberto, Some((*win, *stream)));
                aberto = None;
            }
            Message::VideoFrame { win, stream, seq, .. } => {
                assert_eq!(aberto, Some((*win, *stream)), "VIDEO_FRAME fora de um fluxo aberto");
                if let Some(p) = last_seq.insert(*win, *seq) {
                    assert_eq!(*seq, p.wrapping_add(1));
                }
                write_message(&mut writer, &Message::FrameAck { win: *win, seq: *seq }).unwrap();
                if inicios.len() >= 2 && inicios[1].0 == 2 && *win == 2 {
                    fim = true;
                }
            }
            Message::Frame { win, seq, .. } => {
                if let Some(p) = last_seq.insert(*win, *seq) {
                    assert_eq!(*seq, p.wrapping_add(1));
                }
                if matches!(aberto, Some((1, _))) && *win == 2 {
                    frames_da_2_durante_o_1 += 1;
                }
                write_message(&mut writer, &Message::FrameAck { win: *win, seq: *seq }).unwrap();
            }
            _ => {}
        }
    }
    assert_eq!(inicios[0], (1, 1), "{inicios:?}");
    assert_eq!(inicios[1], (2, 2), "{inicios:?}");
    assert!(frames_da_2_durante_o_1 >= 10, "a janela 2 deveria seguir no codec de blocos enquanto a 1 tem o fluxo: {frames_da_2_durante_o_1}");
    assert_eq!(fab.opens.load(Ordering::SeqCst), 2, "o codificador só abre para quem entra");
    closer.close();
    server.join().unwrap();
}

struct FullSource {
    w: u32,
    h: u32,
    buf: Vec<u8>,
    tick: u64,
    next_ms: u64,
    started: bool,
}

const PARADA: u64 = 90;

fn pixel_cheio(w: u32, h: u32, n: u64) -> Vec<u8> {
    let mut v = vec![0u8; w as usize * h as usize * 4];
    for y in 0..h as usize {
        for x in 0..w as usize {
            let i = (y * w as usize + x) * 4;
            v[i] = ((x as u64 * 2 + n * 5) & 0xFF) as u8;
            v[i + 1] = ((y as u64 * 3 + n * 7) & 0xFF) as u8;
            v[i + 2] = (((x + y) as u64 + n * 3) & 0xFF) as u8;
            v[i + 3] = 0xFF;
        }
    }
    v
}

impl FullSource {
    fn new(w: u32, h: u32) -> Self {
        FullSource { w, h, buf: Vec::new(), tick: 0, next_ms: 0, started: false }
    }
}

impl WindowSource for FullSource {
    fn start(&mut self, _waker: Waker) -> Vec<SourceEvent> {
        vec![SourceEvent::Created { win: WIN, width: self.w, height: self.h, title: "teste".into(), app_id: "teste".into() }]
    }

    fn poll(&mut self, now_ms: u64) -> Vec<SourceEvent> {
        if now_ms < self.next_ms {
            return Vec::new();
        }
        self.next_ms = now_ms + video::FRAME_MS;
        let primeiro = !self.started;
        self.started = true;
        if !primeiro {
            if self.tick >= PARADA {
                return Vec::new();
            }
            self.tick += 1;
        }
        self.buf = pixel_cheio(self.w, self.h, self.tick);
        vec![SourceEvent::Damage { win: WIN, rects: vec![Rect::new(0, 0, self.w, self.h)] }]
    }

    fn next_poll_in(&self, now_ms: u64) -> Option<Duration> {
        Some(Duration::from_millis(self.next_ms.saturating_sub(now_ms)))
    }

    fn with_pixels(&mut self, _win: u32, f: &mut dyn FnMut(u32, u32, &[u8])) {
        f(self.w, self.h, &self.buf);
    }

    fn input(&mut self, _msg: &Message) {}

    fn frame_sent(&mut self, _win: u32) {}
}

fn roda_janela_cheia(w: u32, h: u32) -> (Client, Harness, Arc<FakeFactory>) {
    let fab = FakeFactory::new();
    let acceptor = TcpAcceptor::bind("127.0.0.1:0").unwrap();
    let addr = acceptor.local_addr().unwrap();
    let hook: Hook = Arc::default();
    let opts = Harness::options(Some(fab.clone()), None, hook.clone());
    let server = std::thread::spawn(move || serve_loop_with(&acceptor, &opts, Some(1), move || Box::new(FullSource::new(w, h)) as Box<dyn WindowSource>).unwrap());
    let h = Harness { addr, hook, server: Some(server), _serial: serial() };
    let mut c = h.client(CAP_BLOCK_CODEC | CAP_H264);
    c.check_static = false;
    c.until(30, |c| c.pos(|e| matches!(e, Ev::VideoStop { .. })).is_some_and(|i| c.evs[i..].iter().any(|(_, e)| matches!(e, Ev::Frame { .. }))));
    (c, h, fab)
}

#[test]
fn janela_inteira_animada_vira_regiao_do_tamanho_da_janela_e_volta_sem_perdas() {
    let (c, h, fab) = roda_janela_cheia(1280, 720);
    let i_start = c.pos(|e| matches!(e, Ev::VideoStart { .. })).unwrap();
    assert_eq!(c.evs[i_start].1, Ev::VideoStart { stream: 1, region: Rect::new(0, 0, 1280, 720) });
    let cfg = fab.cfgs.lock().unwrap()[0];
    assert_eq!((cfg.width, cfg.height), (1280, 720));
    assert!(c.img_at_stop.as_ref().unwrap() == &pixel_cheio(1280, 720, PARADA), "o recorte da região não bate com a origem");
    let i_stop = c.pos(|e| matches!(e, Ev::VideoStop { .. })).unwrap();
    let Ev::Frame { rects } = &c.evs[i_stop + 1].1 else { panic!("{}", c.resumo()) };
    assert!(cobre(rects, Rect::new(0, 0, 1280, 720)));
    assert!(c.dec.as_ref().unwrap().pixels() == pixel_cheio(1280, 720, PARADA).as_slice());
    assert!(c.evs[i_start + 1..i_stop].iter().all(|(_, e)| matches!(e, Ev::VideoFrame { .. })), "{}", c.resumo());
    c.drop_connection();
    h.finish();
}

#[test]
fn janela_de_largura_impar_deixa_a_ultima_coluna_de_blocos_fora_da_regiao() {
    let (c, h, _) = roda_janela_cheia(1281, 720);
    let i_start = c.pos(|e| matches!(e, Ev::VideoStart { .. })).unwrap();
    assert_eq!(c.evs[i_start].1, Ev::VideoStart { stream: 1, region: Rect::new(0, 0, 1280, 720) });
    let i_stop = c.pos(|e| matches!(e, Ev::VideoStop { .. })).unwrap();
    let durante: Vec<&Vec<Rect>> = c.evs[i_start..i_stop].iter().filter_map(|(_, e)| if let Ev::Frame { rects } = e { Some(rects) } else { None }).collect();
    assert!(durante.len() >= 10, "a coluna de fora segue pelo codec de blocos");
    for rects in durante {
        assert!(rects.iter().all(|r| r.x == 1280 && r.w == 1), "só a última coluna: {rects:?}");
    }
    let Ev::Frame { rects } = &c.evs[i_stop + 1].1 else { panic!() };
    assert!(cobre(rects, Rect::new(0, 0, 1280, 720)));
    assert!(c.dec.as_ref().unwrap().pixels() == pixel_cheio(1281, 720, PARADA).as_slice());
    c.drop_connection();
    h.finish();
}

struct DanoTotalSource {
    buf: Vec<u8>,
    tick: u64,
    next_ms: u64,
    started: bool,
    started_once: bool,
    modo: Modo,
}

#[derive(Clone, Copy, PartialEq)]
enum Modo {
    Regiao,
    SoPisca,
    Inteira,
    Rolagem,
    Ruido,
    Texto,
    Digitando,
    PartePequena,
}

fn pixel_texto(w: u32, h: u32, t: u64) -> Vec<u8> {
    let mut v = vec![0u8; w as usize * h as usize * 4];
    for i in (3..v.len()).step_by(4) {
        v[i] = 0xFF;
    }
    for y in 0..h as u64 {
        let gy = y + 16 * t;
        let (linha, ly) = (gy / 16, gy % 16);
        if !(2..14).contains(&ly) {
            continue;
        }
        for col in 0..(w as u64 / 8) {
            let mut x = linha.wrapping_mul(0x9E37_79B9_7F4A_7C15) ^ col.wrapping_mul(0xC2B2_AE3D_27D4_EB4F);
            x ^= x >> 31;
            x = x.wrapping_mul(0xBF58_476D_1CE4_E5B9);
            x ^= x >> 29;
            if x % 5 >= 3 || col >= 100 {
                continue;
            }
            for lx in 1..7u64 {
                if (x >> ((ly * 7 + lx) % 61)) & 1 == 1 {
                    let i = ((y * w as u64 + col * 8 + lx) * 4) as usize;
                    v[i..i + 3].fill(0xFF);
                }
            }
        }
    }
    v
}

fn pixel_ruido(w: u32, h: u32, n: u64) -> Vec<u8> {
    let mut v = vec![0u8; w as usize * h as usize * 4];
    for i in 0..(w as usize * h as usize) {
        let px = &mut v[i * 4..i * 4 + 4];
        let mut x = (i as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15) ^ n.wrapping_mul(0xC2B2_AE3D_27D4_EB4F);
        x ^= x >> 29;
        x = x.wrapping_mul(0xBF58_476D_1CE4_E5B9);
        x ^= x >> 32;
        if (x >> 40) & 3 == 0 {
            px.copy_from_slice(&[((x as u8) & 0x03) * 16, ((x >> 8) as u8 & 0x03) * 16, ((x >> 16) as u8 & 0x01) * 16, 0xFF]);
        } else {
            px.copy_from_slice(&[0, 0, 0, 0xFF]);
        }
    }
    v
}

impl DanoTotalSource {
    fn nova(modo: Modo) -> Self {
        DanoTotalSource { buf: Vec::new(), tick: 0, next_ms: 0, started: false, started_once: false, modo }
    }

    fn desenha(&mut self) {
        match self.modo {
            Modo::SoPisca => {
                self.buf = video::render(video::WIDTH, video::HEIGHT, 0);
                if self.tick.is_multiple_of(2) {
                    for y in BLINK.y..BLINK.y + BLINK.h {
                        let a = ((y * video::WIDTH + BLINK.x) * 4) as usize;
                        self.buf[a..a + BLINK.w as usize * 4].fill(0xFF);
                    }
                }
            }
            Modo::Regiao => self.buf = video::render(video::WIDTH, video::HEIGHT, self.tick),
            Modo::Inteira => self.buf = pixel_cheio(video::WIDTH, video::HEIGHT, self.tick),
            Modo::Ruido => self.buf = pixel_ruido(video::WIDTH, video::HEIGHT, self.tick),
            Modo::Texto => self.buf = pixel_texto(video::WIDTH, video::HEIGHT, self.tick),
            Modo::Digitando => {
                self.buf = pixel_texto(video::WIDTH, video::HEIGHT, 0);
                for k in 0..=self.tick / 3 {
                    let (x0, y0) = (8 * (k % 100) as usize, 20 * 16 + 2);
                    for y in y0..y0 + 12 {
                        let a = (y * video::WIDTH as usize + x0) * 4;
                        self.buf[a..a + 6 * 4].fill(0xFF);
                    }
                }
            }
            Modo::PartePequena => {
                self.buf = video::render(video::WIDTH, video::HEIGHT, 0);
                let r = pixel_ruido(video::WIDTH, video::HEIGHT, self.tick);
                for y in 100..228usize {
                    let a = (y * video::WIDTH as usize + 100) * 4;
                    self.buf[a..a + 128 * 4].copy_from_slice(&r[a..a + 128 * 4]);
                }
            }
            Modo::Rolagem => {
                self.buf = video::render(video::WIDTH, video::HEIGHT, 0);
                let t = self.tick.min(ROLAGEM_QUADROS);
                let cheio = pixel_cheio(video::WIDTH, video::HEIGHT, t);
                for y in 64..576usize {
                    let a = (y * video::WIDTH as usize + 64) * 4;
                    self.buf[a..a + 1152 * 4].copy_from_slice(&cheio[a..a + 1152 * 4]);
                }
            }
        }
    }
}

const ROLAGEM_QUADROS: u64 = 9;

impl WindowSource for DanoTotalSource {
    fn start(&mut self, _waker: Waker) -> Vec<SourceEvent> {
        vec![SourceEvent::Created { win: WIN, width: video::WIDTH, height: video::HEIGHT, title: "teste".into(), app_id: "teste".into() }]
    }

    fn poll(&mut self, now_ms: u64) -> Vec<SourceEvent> {
        if now_ms < self.next_ms {
            return Vec::new();
        }
        self.next_ms = now_ms + video::FRAME_MS;
        if self.started {
            self.tick += 1;
        }
        self.started = true;
        if self.modo == Modo::Rolagem && self.tick > ROLAGEM_QUADROS {
            return Vec::new();
        }
        if self.modo == Modo::Digitando {
            if !self.tick.is_multiple_of(3) && self.started_once {
                return Vec::new();
            }
            self.started_once = true;
            self.desenha();
            let k = self.tick / 3;
            return vec![SourceEvent::Damage { win: WIN, rects: vec![Rect::new(8 * (k % 100) as u32, 20 * 16, 8, 16)] }];
        }
        self.desenha();
        vec![SourceEvent::Damage { win: WIN, rects: vec![Rect::new(0, 0, video::WIDTH, video::HEIGHT)] }]
    }

    fn next_poll_in(&self, now_ms: u64) -> Option<Duration> {
        Some(Duration::from_millis(self.next_ms.saturating_sub(now_ms)))
    }

    fn with_pixels(&mut self, _win: u32, f: &mut dyn FnMut(u32, u32, &[u8])) {
        f(video::WIDTH, video::HEIGHT, &self.buf);
    }

    fn input(&mut self, _msg: &Message) {}

    fn frame_sent(&mut self, _win: u32) {}
}

#[derive(Clone, Copy)]
enum Fonte {
    Sintetica,
    DanoTotalMudaRegiao,
    DanoTotalSoPisca,
    JanelaInteira,
    Rolagem,
    Ruido,
    Texto,
    Digitando,
    PartePequena,
}

fn roda_fluxo_apertado(fonte: Fonte, throughput: f64, ack_ms: u64, espera_s: u64) -> Option<(Duration, Rect, usize)> {
    roda_fluxo(fonte, throughput, ack_ms, espera_s, false)
}

fn roda_fluxo(fonte: Fonte, throughput: f64, ack_ms: u64, espera_s: u64, ritmo: bool) -> Option<(Duration, Rect, usize)> {
    let fab = FakeFactory::new();
    let acceptor = TcpAcceptor::bind("127.0.0.1:0").unwrap();
    let addr = acceptor.local_addr().unwrap();
    let hook: Hook = Arc::default();
    let mut opts = Harness::options(Some(fab), None, hook.clone());
    opts.flow_initial_throughput = throughput;
    if ritmo {
        opts.max_rate = Some(throughput);
    }
    let server = std::thread::spawn(move || match fonte {
        Fonte::Sintetica => serve_loop(&acceptor, &opts, Some(1)).unwrap(),
        Fonte::DanoTotalMudaRegiao => serve_loop_with(&acceptor, &opts, Some(1), || Box::new(DanoTotalSource::nova(Modo::Regiao)) as Box<dyn WindowSource>).unwrap(),
        Fonte::DanoTotalSoPisca => serve_loop_with(&acceptor, &opts, Some(1), || Box::new(DanoTotalSource::nova(Modo::SoPisca)) as Box<dyn WindowSource>).unwrap(),
        Fonte::JanelaInteira => serve_loop_with(&acceptor, &opts, Some(1), || Box::new(DanoTotalSource::nova(Modo::Inteira)) as Box<dyn WindowSource>).unwrap(),
        Fonte::Rolagem => serve_loop_with(&acceptor, &opts, Some(1), || Box::new(DanoTotalSource::nova(Modo::Rolagem)) as Box<dyn WindowSource>).unwrap(),
        Fonte::Ruido => serve_loop_with(&acceptor, &opts, Some(1), || Box::new(DanoTotalSource::nova(Modo::Ruido)) as Box<dyn WindowSource>).unwrap(),
        Fonte::Texto => serve_loop_with(&acceptor, &opts, Some(1), || Box::new(DanoTotalSource::nova(Modo::Texto)) as Box<dyn WindowSource>).unwrap(),
        Fonte::Digitando => serve_loop_with(&acceptor, &opts, Some(1), || Box::new(DanoTotalSource::nova(Modo::Digitando)) as Box<dyn WindowSource>).unwrap(),
        Fonte::PartePequena => serve_loop_with(&acceptor, &opts, Some(1), || Box::new(DanoTotalSource::nova(Modo::PartePequena)) as Box<dyn WindowSource>).unwrap(),
    });
    let h = Harness { addr, hook, server: Some(server), _serial: serial() };
    let mut c = h.client(CAP_BLOCK_CODEC | CAP_H264);
    c.check_static = matches!(fonte, Fonte::Sintetica);
    c.ack_delay = Duration::from_millis(ack_ms);
    let limite = Instant::now() + Duration::from_secs(espera_s);
    while Instant::now() < limite && c.pos(|e| matches!(e, Ev::VideoStart { .. })).is_none() {
        c.next(Duration::from_millis(20));
    }
    let r = c.pos(|e| matches!(e, Ev::VideoStart { .. })).map(|i| {
        let Ev::VideoStart { region, .. } = c.evs[i].1 else { unreachable!() };
        (c.evs[i].0.duration_since(c.t0), region, c.evs[..i].iter().filter(|(_, e)| matches!(e, Ev::Frame { .. })).count())
    });
    c.drop_connection();
    h.finish();
    r
}

#[test]
fn quadro_fatiado_em_duas_fatias_entra_em_video_em_ate_2_s() {
    let (entrada, regiao, frames) = roda_fluxo_apertado(Fonte::Sintetica, 5.0 * 1024.0 * 1024.0, 10, 15).expect("o vídeo nunca começou");
    eprintln!("2 fatias: VIDEO_START em {entrada:?} depois de {frames} FRAME, região {}x{}", regiao.w, regiao.h);
    assert_eq!(regiao, video::REGION, "região detectada");
    let limite = if cfg!(debug_assertions) { Duration::from_secs(4) } else { Duration::from_secs(2) };
    assert!(entrada < limite, "entrou em vídeo só depois de {entrada:?} ({frames} FRAME)");
}

#[test]
fn quadro_fatiado_em_tres_ou_mais_fatias_tambem_entra_em_video() {
    let (entrada, regiao, frames) = roda_fluxo_apertado(Fonte::Sintetica, 2.0 * 1024.0 * 1024.0, 10, 15).expect("o vídeo nunca começou");
    eprintln!("3+ fatias: VIDEO_START em {entrada:?} depois de {frames} FRAME, região {}x{}", regiao.w, regiao.h);
    assert_eq!(regiao, video::REGION, "região detectada");
    let limite = if cfg!(debug_assertions) { Duration::from_secs(4) } else { Duration::from_secs(2) };
    assert!(entrada < limite, "entrou em vídeo só depois de {entrada:?} ({frames} FRAME)");
}

#[test]
fn dano_na_janela_inteira_com_so_a_regiao_mudando_vira_video_so_da_regiao() {
    for thr in [5.0 * 1024.0 * 1024.0, 2.0 * 1024.0 * 1024.0] {
        let (entrada, regiao, _) = roda_fluxo_apertado(Fonte::DanoTotalMudaRegiao, thr, 10, 15).expect("o vídeo nunca começou");
        eprintln!("dano total, só a região muda ({thr} B/s): VIDEO_START em {entrada:?}, região {}x{}+{}+{}", regiao.w, regiao.h, regiao.x, regiao.y);
        assert_eq!(regiao, video::REGION, "a região de vídeo tem de ser só a que muda");
    }
}

#[test]
fn janela_inteira_animada_com_fluxo_de_2_mib_entra_em_video_em_ate_3_s() {
    let (entrada, regiao, frames) = roda_fluxo_apertado(Fonte::JanelaInteira, 2.0 * 1024.0 * 1024.0, 10, 15).expect("o vídeo nunca começou");
    eprintln!("janela inteira, 2 MiB/s: VIDEO_START em {entrada:?} depois de {frames} FRAME, região {}x{}+{}+{}", regiao.w, regiao.h, regiao.x, regiao.y);
    assert_eq!(regiao, Rect::new(0, 0, video::WIDTH, video::HEIGHT), "região detectada");
    let limite = if cfg!(debug_assertions) { Duration::from_secs(6) } else { Duration::from_secs(3) };
    assert!(entrada < limite, "entrou em vídeo só depois de {entrada:?} ({frames} FRAME)");
}

#[test]
fn janela_inteira_com_ruido_entra_em_video_em_ate_2_s_com_vazao_baixa_e_alta() {
    let limite = if cfg!(debug_assertions) { Duration::from_secs(4) } else { Duration::from_secs(2) };
    for (mib, ack_ms) in [(1.5, 10), (2.5, 10), (2.5, 40), (3.0, 40), (11.0, 10)] {
        let r = roda_fluxo(Fonte::Ruido, mib * 1024.0 * 1024.0, ack_ms, 8, true);
        let (entrada, regiao, frames) = r.unwrap_or_else(|| panic!("{mib} MiB/s, confirmação {ack_ms} ms: o vídeo nunca começou em 8 s"));
        eprintln!("ruído, {mib} MiB/s, confirmação {ack_ms} ms: VIDEO_START em {entrada:?} depois de {frames} FRAME, região {}x{}", regiao.w, regiao.h);
        assert_eq!(regiao, Rect::new(0, 0, video::WIDTH, video::HEIGHT), "{mib} MiB/s: região detectada");
        assert!(entrada < limite, "{mib} MiB/s, confirmação {ack_ms} ms: entrou em vídeo só depois de {entrada:?} ({frames} FRAME)");
    }
}

#[test]
fn texto_digitacao_cursor_e_dano_total_com_pouca_mudanca_nao_viram_video_com_vazao_baixa() {
    for (nome, fonte) in [("rolagem de texto", Fonte::Texto), ("digitação", Fonte::Digitando), ("cursor piscando", Fonte::DanoTotalSoPisca), ("dano total, área pequena", Fonte::PartePequena)] {
        for mib in [1.5, 2.5, 3.0] {
            let r = roda_fluxo(fonte, mib * 1024.0 * 1024.0, 10, 6, true);
            assert!(r.is_none(), "{nome} a {mib} MiB/s entrou em vídeo: {r:?}");
        }
    }
}

#[test]
fn janela_inteira_animada_com_fluxo_de_1_mib_so_relata() {
    match roda_fluxo_apertado(Fonte::JanelaInteira, 1024.0 * 1024.0, 10, 15) {
        Some((entrada, regiao, frames)) => eprintln!("janela inteira, 1 MiB/s: VIDEO_START em {entrada:?} depois de {frames} FRAME, região {}x{}", regiao.w, regiao.h),
        None => eprintln!("janela inteira, 1 MiB/s: não entrou em vídeo em 15 s"),
    }
}

#[test]
fn rolagem_breve_com_fluxo_lento_nao_vira_video() {
    let r = roda_fluxo_apertado(Fonte::Rolagem, 2.0 * 1024.0 * 1024.0, 10, 5);
    assert!(r.is_none(), "entrou em vídeo com uma rolagem de 0,3 s: {r:?}");
}

#[test]
fn dano_na_janela_inteira_com_so_um_bloco_piscando_nunca_vira_video() {
    for thr in [5.0 * 1024.0 * 1024.0, 2.0 * 1024.0 * 1024.0] {
        let r = roda_fluxo_apertado(Fonte::DanoTotalSoPisca, thr, 10, 5);
        assert!(r.is_none(), "entrou em vídeo com dano total e só um bloco mudando: {r:?}");
    }
}

#[test]
fn dano_total_com_video_so_na_regiao_nao_manda_frame_de_blocos_vazio_e_o_video_flui() {
    for thr in [5.0 * 1024.0 * 1024.0, 2.0 * 1024.0 * 1024.0] {
        let fab = FakeFactory::new();
        let acceptor = TcpAcceptor::bind("127.0.0.1:0").unwrap();
        let addr = acceptor.local_addr().unwrap();
        let hook: Hook = Arc::default();
        let mut opts = Harness::options(Some(fab), None, hook.clone());
        opts.flow_initial_throughput = thr;
        let server = std::thread::spawn(move || serve_loop_with(&acceptor, &opts, Some(1), || Box::new(DanoTotalSource::nova(Modo::Regiao)) as Box<dyn WindowSource>).unwrap());
        let h = Harness { addr, hook, server: Some(server), _serial: serial() };
        let mut c = h.client(CAP_BLOCK_CODEC | CAP_H264);
        c.check_static = false;
        let limite = Instant::now() + Duration::from_secs(15);
        while Instant::now() < limite && c.pos(|e| matches!(e, Ev::VideoStart { .. })).is_none() {
            c.next(Duration::from_millis(20));
        }
        let i_start = c.pos(|e| matches!(e, Ev::VideoStart { .. })).expect("o vídeo nunca começou");
        let t_medida = Instant::now() + Duration::from_millis(500);
        while Instant::now() < t_medida {
            c.next(Duration::from_millis(20));
        }
        let (n_frames_0, n_video_0) = (c.evs.len(), c.n_video_frames());
        let fim = Instant::now() + Duration::from_secs(3);
        while Instant::now() < fim {
            c.next(Duration::from_millis(20));
        }
        let blocos: Vec<&Vec<Rect>> = c.evs[n_frames_0..].iter().filter_map(|(_, e)| if let Ev::Frame { rects } = e { Some(rects) } else { None }).collect();
        let video = c.n_video_frames() - n_video_0;
        eprintln!("dano total, vídeo só na região ({thr} B/s): em 3 s, {} FRAME de blocos e {video} VIDEO_FRAME (desde o VIDEO_START na posição {i_start})", blocos.len());
        assert!(blocos.iter().all(|r| !r.is_empty()), "FRAME só de SKIP durante o vídeo");
        assert!(blocos.len() <= 3, "{} FRAME de blocos em 3 s com só a região mudando", blocos.len());
        let minimo = if cfg!(debug_assertions) { 45 } else { 60 };
        assert!(video >= minimo, "só {video} VIDEO_FRAME em 3 s (mínimo {minimo})");
        assert!(c.evs.iter().all(|(_, e)| !matches!(e, Ev::VideoStop { .. })), "o vídeo não pode parar");
        c.drop_connection();
        h.finish();
    }
}
