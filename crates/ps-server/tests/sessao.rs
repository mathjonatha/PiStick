
use std::io::Write;
use std::net::{SocketAddr, TcpStream};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use ps_proto::{read_hello, read_message, write_message, Message, ProtoError, AUTH_PAIRED, AUTH_REFUSED, CAPS_MARCO3, CAP_BLOCK_CODEC, CAP_SESSION, GOODBYE_CLIENT, GOODBYE_REPLACED};
use ps_secure::canal::{client_handshake, MAGIC};
use ps_secure::chaves::PeerList;
use ps_secure::{Keypair, SecureConn};
use ps_server::secure::{serve_secure, Security, Timing};
use ps_server::{ServerOptions, SourceEvent, Waker, WindowSource};
use ps_transport::{Split, TcpAcceptor, TcpReader, TcpTransport, Transport};

type Conn = SecureConn<TcpReader, TcpStream>;
type Log = Arc<Mutex<Vec<(Instant, String)>>>;

struct Rec {
    log: Log,
    pixels: Vec<u8>,
}

impl Rec {
    fn push(&self, s: String) {
        self.log.lock().unwrap().push((Instant::now(), s));
    }
}

impl WindowSource for Rec {
    fn start(&mut self, _waker: Waker) -> Vec<SourceEvent> {
        self.push("start".into());
        vec![SourceEvent::Created { win: 1, width: 128, height: 96, title: "rec".into(), app_id: "rec".into() }]
    }
    fn poll(&mut self, _now_ms: u64) -> Vec<SourceEvent> {
        Vec::new()
    }
    fn next_poll_in(&self, _now_ms: u64) -> Option<Duration> {
        None
    }
    fn with_pixels(&mut self, _win: u32, f: &mut dyn FnMut(u32, u32, &[u8])) {
        f(128, 96, &self.pixels);
    }
    fn input(&mut self, msg: &Message) {
        match msg {
            Message::Key { keycode, pressed, .. } => self.push(format!("tecla {keycode} {}", if *pressed { "baixo" } else { "cima" })),
            Message::PointerButton { button, pressed, .. } => self.push(format!("botao {button:#x} {}", if *pressed { "baixo" } else { "cima" })),
            _ => {}
        }
    }
    fn frame_sent(&mut self, _win: u32) {}
    fn stop(&mut self) {
        self.push("stop".into());
    }
}

struct Env {
    sec: Arc<Security>,
    addr: SocketAddr,
    log: Log,
    slog: Arc<Mutex<Vec<String>>>,
}

fn tmpdir(nome: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("ps-server-sessao-{}-{nome}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn timing() -> Timing {
    Timing { first_use_window: Duration::from_secs(60), pair_timeout: Duration::from_secs(5), handshake_timeout: Duration::from_secs(2), unknown_max: 50, unknown_per: Duration::from_secs(60) }
}

fn start(nome: &str, preload: &[(&Keypair, &str)]) -> Env {
    let dir = tmpdir(nome);
    if !preload.is_empty() {
        let mut l = PeerList::default();
        for (k, n) in preload {
            l.upsert(*k.public(), n, "2026-01-01");
        }
        ps_secure::chaves::save_list(&dir.join("pareados"), &l).unwrap();
    }
    let slog: Arc<Mutex<Vec<String>>> = Arc::default();
    let l2 = slog.clone();
    let sec = Security::open_with(&dir, "pi-teste", false, timing(), move |m| l2.lock().unwrap().push(m.to_string())).unwrap();
    let acceptor = TcpAcceptor::bind("127.0.0.1:0").unwrap();
    let addr = acceptor.local_addr().unwrap();
    let opts = ServerOptions { hello_timeout: Duration::from_millis(1500), quiet: true, ..ServerOptions::default() };
    let log: Log = Arc::default();
    let (s2, l3) = (sec.clone(), log.clone());
    std::thread::spawn(move || {
        let make = || Box::new(Rec { log: l3.clone(), pixels: vec![0x40; 128 * 96 * 4] }) as Box<dyn WindowSource>;
        let _ = serve_secure(&acceptor, &opts, s2, None, make);
    });
    Env { sec, addr, log, slog }
}

impl Env {
    fn eventos(&self) -> Vec<String> {
        self.log.lock().unwrap().iter().map(|(_, s)| s.clone()).collect()
    }

    fn instante(&self, evento: &str) -> Option<Instant> {
        self.log.lock().unwrap().iter().rev().find(|(_, s)| s == evento).map(|(t, _)| *t)
    }

    fn slog_has(&self, trecho: &str) -> bool {
        self.slog.lock().unwrap().iter().any(|l| l.contains(trecho))
    }
}

fn espera(mut f: impl FnMut() -> bool, ate: Duration, msg: &str) {
    let fim = Instant::now() + ate;
    while !f() {
        assert!(Instant::now() < fim, "{msg}");
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn noise(addr: SocketAddr, key: &Keypair, name: &str) -> Conn {
    let t = TcpTransport::connect(addr).unwrap();
    let peer = t.peer();
    let Split { mut reader, mut writer, closer } = t.split().unwrap();
    let (est, _) = client_handshake(&mut reader, &mut writer, key, name, |_, _| Ok(())).unwrap();
    SecureConn::new(reader, writer, closer, est.send, est.recv, peer)
}

fn estado(c: &mut Conn) -> u8 {
    match read_message(&mut c.reader).unwrap() {
        Message::AuthStatus { state, .. } => state,
        outra => panic!("esperava AUTH_STATUS, veio {outra:?}"),
    }
}

fn hello<R: std::io::Read, W: Write>(r: &mut R, w: &mut W, caps: u32) -> u32 {
    write_message(w, &Message::hello_with_caps("cliente de teste", caps)).unwrap();
    read_hello(r).unwrap().0
}

fn espera_janela<R: std::io::Read>(r: &mut R) {
    loop {
        if let Message::WinCreate { .. } = read_message(r).unwrap() {
            return;
        }
    }
}

fn espera_goodbye<R: std::io::Read>(r: &mut R, ate: Duration) -> Result<(u8, String), ProtoError> {
    let fim = Instant::now() + ate;
    loop {
        match read_message(r)? {
            Message::Goodbye { reason, text } => return Ok((reason, text)),
            _ => assert!(Instant::now() < fim, "o GOODBYE não chegou em {ate:?}"),
        }
    }
}

fn ping_pong<R: std::io::Read, W: Write>(r: &mut R, w: &mut W, v: u64) {
    write_message(w, &Message::Ping { value: v }).unwrap();
    loop {
        if let Message::Pong { echo } = read_message(r).unwrap() {
            assert_eq!(echo, v);
            return;
        }
    }
}

const CAPS: u32 = CAP_BLOCK_CODEC | CAPS_MARCO3 | CAP_SESSION;

#[test]
fn cliente_pareado_assume_a_sessao_e_o_antigo_recebe_goodbye_com_o_nome_do_novo() {
    let (ka, kb) = (Keypair::generate().unwrap(), Keypair::generate().unwrap());
    let env = start("troca", &[(&ka, "notebook-a"), (&kb, "notebook-b")]);
    let mut a = noise(env.addr, &ka, "notebook-a");
    assert_eq!(estado(&mut a), AUTH_PAIRED);
    assert_ne!(hello(&mut a.reader, &mut a.writer, CAPS) & CAP_SESSION, 0, "o servidor anuncia CAP_SESSION");
    espera_janela(&mut a.reader);
    ping_pong(&mut a.reader, &mut a.writer, 1);

    let t0 = Instant::now();
    let mut b = noise(env.addr, &kb, "notebook-b");
    assert_eq!(estado(&mut b), AUTH_PAIRED);
    let (motivo, texto) = espera_goodbye(&mut a.reader, Duration::from_secs(3)).expect("GOODBYE");
    assert_eq!((motivo, texto.as_str()), (GOODBYE_REPLACED, "notebook-b"));
    assert!(read_message(&mut a.reader).is_err(), "depois do GOODBYE a conexão do antigo fecha");
    hello(&mut b.reader, &mut b.writer, CAPS);
    espera_janela(&mut b.reader);
    ping_pong(&mut b.reader, &mut b.writer, 2);
    assert!(t0.elapsed() < Duration::from_secs(2), "a troca demorou {:?}", t0.elapsed());
    espera(|| env.slog_has("assumiu a sessão"), Duration::from_secs(2), "o registro do servidor não anotou a troca");
    let ev = env.eventos();
    assert_eq!(ev.iter().filter(|e| *e == "start").count(), 2, "{ev:?}");
    assert!(env.instante("stop").is_some());
}

#[test]
fn desconhecido_lixo_e_pedido_pendente_nao_derrubam_a_sessao_em_curso() {
    let dono = Keypair::generate().unwrap();
    let env = start("naoderruba", &[(&dono, "dono")]);
    let mut a = noise(env.addr, &dono, "dono");
    assert_eq!(estado(&mut a), AUTH_PAIRED);
    hello(&mut a.reader, &mut a.writer, CAPS);
    espera_janela(&mut a.reader);
    ping_pong(&mut a.reader, &mut a.writer, 1);

    let mut intruso = noise(env.addr, &Keypair::generate().unwrap(), "intruso");
    assert_eq!(estado(&mut intruso), AUTH_REFUSED);
    ping_pong(&mut a.reader, &mut a.writer, 2);
    let sec = env.sec.clone();
    let esperando = std::thread::spawn(move || sec.open_pair_window(Duration::from_secs(10), &|| true));
    espera(|| env.sec.pair_window_open(), Duration::from_secs(2), "a janela de pareamento não abriu");
    let mut novo = noise(env.addr, &Keypair::generate().unwrap(), "celular");
    assert_eq!(estado(&mut novo), ps_proto::AUTH_PENDING);
    let ps_server::secure::PairWait::Request(req) = esperando.join().unwrap() else { panic!("esperava um pedido") };
    ping_pong(&mut a.reader, &mut a.writer, 3);
    let mut s = TcpStream::connect(env.addr).unwrap();
    s.write_all(&[0xFF, 0xFF, 0xFF, 0xFF, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12]).unwrap();
    let mut s2 = TcpStream::connect(env.addr).unwrap();
    s2.write_all(MAGIC).unwrap();
    s2.write_all(&[0x00, 0x20]).unwrap();
    s2.write_all(&[0xAB; 32]).unwrap();
    std::thread::sleep(Duration::from_millis(700));
    ping_pong(&mut a.reader, &mut a.writer, 4);
    env.sec.decide(req.id, true).unwrap();
    assert_eq!(estado(&mut novo), ps_proto::AUTH_PAIRED_NOW);
    std::thread::sleep(Duration::from_millis(500));
    ping_pong(&mut a.reader, &mut a.writer, 5);
    assert!(!env.slog_has("assumiu a sessão"), "{:?}", env.slog.lock().unwrap());
    assert_eq!(env.eventos().iter().filter(|e| *e == "start").count(), 1, "nenhuma sessão nova começou: {:?}", env.eventos());
    drop(a);
    hello(&mut novo.reader, &mut novo.writer, CAPS);
    espera_janela(&mut novo.reader);
}

#[test]
fn o_mesmo_cliente_reconectando_assume_em_menos_de_um_segundo() {
    let k = Keypair::generate().unwrap();
    let env = start("mesmo", &[(&k, "notebook")]);
    let mut velha = noise(env.addr, &k, "notebook");
    assert_eq!(estado(&mut velha), AUTH_PAIRED);
    hello(&mut velha.reader, &mut velha.writer, CAPS);
    espera_janela(&mut velha.reader);
    let t0 = Instant::now();
    let mut nova = noise(env.addr, &k, "notebook");
    assert_eq!(estado(&mut nova), AUTH_PAIRED);
    hello(&mut nova.reader, &mut nova.writer, CAPS);
    espera_janela(&mut nova.reader);
    ping_pong(&mut nova.reader, &mut nova.writer, 7);
    let dt = t0.elapsed();
    assert!(dt < Duration::from_secs(1), "a nova conexão levou {dt:?} para assumir");
    println!("TROCA do mesmo cliente: {dt:?}");
    let (motivo, texto) = espera_goodbye(&mut velha.reader, Duration::from_secs(2)).expect("GOODBYE");
    assert_eq!((motivo, texto.as_str()), (GOODBYE_REPLACED, "notebook"));
}

#[test]
fn teclas_e_botoes_apertados_sao_soltos_no_fim_de_toda_sessao() {
    let (ka, kb) = (Keypair::generate().unwrap(), Keypair::generate().unwrap());
    let env = start("presas", &[(&ka, "a"), (&kb, "b")]);
    let mut a = noise(env.addr, &ka, "a");
    assert_eq!(estado(&mut a), AUTH_PAIRED);
    hello(&mut a.reader, &mut a.writer, CAPS);
    espera_janela(&mut a.reader);
    write_message(&mut a.writer, &Message::Key { win: 1, keycode: 42, pressed: true }).unwrap();
    write_message(&mut a.writer, &Message::Key { win: 1, keycode: 30, pressed: true }).unwrap();
    write_message(&mut a.writer, &Message::Key { win: 1, keycode: 30, pressed: false }).unwrap();
    write_message(&mut a.writer, &Message::PointerButton { win: 1, button: 0x110, pressed: true }).unwrap();
    ping_pong(&mut a.reader, &mut a.writer, 1);
    let mut b = noise(env.addr, &kb, "b");
    assert_eq!(estado(&mut b), AUTH_PAIRED);
    espera_goodbye(&mut a.reader, Duration::from_secs(3)).expect("GOODBYE");
    hello(&mut b.reader, &mut b.writer, CAPS);
    espera_janela(&mut b.reader);
    let ev = env.eventos();
    let ate_o_stop: Vec<&String> = ev.iter().take_while(|e| *e != "stop").collect();
    assert!(ate_o_stop.iter().any(|e| *e == "tecla 42 cima"), "Shift não foi solto antes do fim: {ev:?}");
    assert!(ate_o_stop.iter().any(|e| *e == "botao 0x110 cima"), "o botão não foi solto antes do fim: {ev:?}");
    assert_eq!(ev.iter().filter(|e| *e == "tecla 30 cima").count(), 1, "{ev:?}");
    write_message(&mut b.writer, &Message::Key { win: 1, keycode: 29, pressed: true }).unwrap();
    ping_pong(&mut b.reader, &mut b.writer, 2);
    drop(b);
    espera(|| env.eventos().iter().any(|e| e == "tecla 29 cima"), Duration::from_secs(3), "a tecla Ctrl não foi solta quando a conexão caiu");
}

#[test]
fn goodbye_do_cliente_encerra_a_sessao_em_menos_de_200_ms() {
    let env = start("adeus", &[]);
    let Split { mut reader, mut writer, closer: _closer } = TcpTransport::connect(env.addr).unwrap().split().unwrap();
    hello(&mut reader, &mut writer, CAPS);
    espera_janela(&mut reader);
    write_message(&mut writer, &Message::Key { win: 1, keycode: 42, pressed: true }).unwrap();
    ping_pong(&mut reader, &mut writer, 1);
    let t0 = Instant::now();
    write_message(&mut writer, &Message::Goodbye { reason: GOODBYE_CLIENT, text: String::new() }).unwrap();
    espera(|| env.instante("stop").is_some(), Duration::from_secs(2), "a sessão não terminou");
    let dt = env.instante("stop").unwrap().saturating_duration_since(t0);
    println!("GOODBYE do cliente até o fim da sessão: {dt:?}");
    assert!(dt < Duration::from_millis(200), "a sessão levou {dt:?} para terminar depois do GOODBYE");
    assert!(env.eventos().iter().any(|e| e == "tecla 42 cima"), "o Shift apertado também é solto");
    let Split { reader: mut r2, writer: mut w2, .. } = TcpTransport::connect(env.addr).unwrap().split().unwrap();
    hello(&mut r2, &mut w2, CAPS);
    espera_janela(&mut r2);
}

#[test]
fn cliente_sem_cap_session_e_apenas_desconectado() {
    let (ka, kb) = (Keypair::generate().unwrap(), Keypair::generate().unwrap());
    let env = start("antigo", &[(&ka, "antigo"), (&kb, "novo")]);
    let mut a = noise(env.addr, &ka, "antigo");
    assert_eq!(estado(&mut a), AUTH_PAIRED);
    let caps_servidor = hello(&mut a.reader, &mut a.writer, CAP_BLOCK_CODEC | CAPS_MARCO3);
    assert_ne!(caps_servidor & CAP_SESSION, 0, "o servidor anuncia, mas só vale com os dois lados");
    espera_janela(&mut a.reader);
    let mut b = noise(env.addr, &kb, "novo");
    assert_eq!(estado(&mut b), AUTH_PAIRED);
    let r = espera_goodbye(&mut a.reader, Duration::from_secs(3));
    assert!(r.is_err(), "um cliente sem CAP_SESSION não pode receber GOODBYE: {r:?}");
    hello(&mut b.reader, &mut b.writer, CAPS);
    espera_janela(&mut b.reader);
}
