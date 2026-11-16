//! Teste de ponta a ponta do compositor (só Linux): cliente Wayland (`ps-testclient`) -> `psd`
//! em modo Wayland -> protocolo -> decodificador, comparando pixels com o padrão conhecido.
#![cfg(target_os = "linux")]

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use ps_codec::testutil::normalize_x;
use ps_codec::Decoder;
use ps_proto::{read_hello, read_message, write_message, CursorShape, Message, ProtoError, CAP_BLOCK_CODEC, CAPS_MARCO3, MAX_CLIPBOARD, MIME_TEXT};
use ps_server::wayland::{SocketSpec, WaylandCompositor};
use ps_server::{serve_loop_with, ServerOptions};
use ps_testclient::{pattern, CursorMode, Options, IMAGE_HOTSPOT, IMAGE_PIXEL, IMAGE_SIDE};
use ps_transport::{Closer, Split, TcpAcceptor, TcpTransport, Transport};

/// Quadros de faixa que o cliente de teste desenha depois de cada pintura-base.
const BAND_FRAMES: u32 = 40;

struct Rig {
    dir: PathBuf,
    comp: Option<WaylandCompositor>,
    addr: std::net::SocketAddr,
    server: Option<JoinHandle<()>>,
}

fn rig(max_sessions: usize) -> Rig {
    let dir = std::env::temp_dir().join(format!("pistick-e2e-{}-{}", std::process::id(), unique()));
    std::fs::create_dir_all(&dir).unwrap();
    let comp = WaylandCompositor::start(SocketSpec::Path(dir.join("wl-teste"))).expect("compositor");
    let acceptor = TcpAcceptor::bind("127.0.0.1:0").unwrap();
    let addr = acceptor.local_addr().unwrap();
    let opts = ServerOptions {
        hello_timeout: Duration::from_millis(800),
        quiet: std::env::var("PS_LOG").is_err(),
        // Sem fatiamento nem limite de vazão: o teste confere pixels, não o controle de fluxo.
        flow_initial_throughput: 1e10,
        flow_adaptive: false,
        ..ServerOptions::default()
    };
    // `make_source` é chamada uma vez por sessão.
    let fontes: Vec<_> = (0..max_sessions).map(|_| comp.source()).collect();
    let server = std::thread::spawn(move || {
        let mut it = fontes.into_iter();
        serve_loop_with(&acceptor, &opts, Some(max_sessions), move || it.next().expect("fonte")).unwrap();
    });
    Rig { dir, comp: Some(comp), addr, server: Some(server) }
}

fn unique() -> u64 {
    static N: AtomicU32 = AtomicU32::new(0);
    u64::from(N.fetch_add(1, Ordering::Relaxed))
}

impl Rig {
    fn socket(&self) -> PathBuf {
        self.dir.join("wl-teste")
    }

    fn client(&self, auto_ack: bool) -> Proto {
        Proto::connect(self.addr, auto_ack, CAP_BLOCK_CODEC)
    }

    /// Cliente que anuncia as extensões do marco 3 (popups, cursor, área de transferência).
    fn client_marco3(&self) -> Proto {
        Proto::connect(self.addr, true, CAP_BLOCK_CODEC | CAPS_MARCO3)
    }

    fn finish(mut self) {
        if let Some(s) = self.server.take() {
            s.join().unwrap();
        }
        if let Some(c) = self.comp.take() {
            c.shutdown();
        }
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn spawn_testclient(socket: &Path, log: &Path, counter: Option<Arc<AtomicU32>>) -> JoinHandle<Result<(), String>> {
    spawn_testclient_with(socket, log, |o| o.counter = counter)
}

/// Como [`spawn_testclient`], com ajuste das opções do cliente de teste.
fn spawn_testclient_with(socket: &Path, log: &Path, ajusta: impl FnOnce(&mut Options)) -> JoinHandle<Result<(), String>> {
    let mut opts = Options {
        socket: Some(socket.to_path_buf()),
        title: "e2e-teste ação".into(),
        log: Some(log.to_path_buf()),
        band_frames: BAND_FRAMES,
        ..Options::default()
    };
    ajusta(&mut opts);
    std::thread::spawn(move || ps_testclient::run(opts))
}

/// Cliente do protocolo sem janela: decodifica e confirma tudo.
struct Proto {
    rx: Receiver<Result<Message, ProtoError>>,
    writer: std::net::TcpStream,
    closer: Closer,
    auto_ack: bool,
    decoders: HashMap<u32, Decoder>,
    titles: HashMap<u32, String>,
    frames: usize,
    log: Vec<String>,
    /// Quadros recebidos e ainda não confirmados (quando `auto_ack` está desligado).
    unacked: Vec<(u32, u32)>,
    /// Todas as mensagens que não são quadro, na ordem em que chegaram.
    seen: Vec<Message>,
    /// Retângulos que cada quadro decodificou (janela, regiões), na ordem de chegada.
    rects: Vec<(u32, Vec<ps_codec::Rect>)>,
}

impl Proto {
    fn connect(addr: std::net::SocketAddr, auto_ack: bool, caps: u32) -> Proto {
        let Split { mut reader, mut writer, closer } = TcpTransport::connect(addr).unwrap().split().unwrap();
        write_message(&mut writer, &Message::hello_with_caps("e2e", caps)).unwrap();
        let (_, name) = read_hello(&mut reader).expect("HELLO do servidor");
        assert!(name.contains("psd"), "nome: {name}");
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || loop {
            let r = read_message(&mut reader);
            let fim = r.is_err();
            if tx.send(r).is_err() || fim {
                break;
            }
        });
        Proto { rx, writer, closer, auto_ack, decoders: HashMap::new(), titles: HashMap::new(), frames: 0, log: Vec::new(), unacked: Vec::new(), seen: Vec::new(), rects: Vec::new() }
    }

    fn send(&mut self, m: Message) {
        write_message(&mut self.writer, &m).unwrap();
    }

    fn next(&mut self, dur: Duration) -> Option<Message> {
        match self.rx.recv_timeout(dur) {
            Ok(Ok(m)) => {
                if !matches!(m, Message::Frame { .. }) {
                    self.seen.push(m.clone());
                }
                match &m {
                    Message::WinCreate { win, width, height, title, .. } => {
                        self.decoders.insert(*win, Decoder::new(*width, *height));
                        self.titles.insert(*win, title.clone());
                        self.log.push(format!("create {win} {width}x{height}"));
                    }
                    Message::PopupCreate { win, width, height, .. } => {
                        self.decoders.insert(*win, Decoder::new(*width, *height));
                        self.log.push(format!("popup {win} {width}x{height}"));
                    }
                    Message::WinResize { win, width, height } => {
                        self.decoders.get_mut(win).expect("janela existe").reset(*width, *height);
                        self.log.push(format!("resize {win} {width}x{height}"));
                    }
                    Message::WinDestroy { win } => {
                        self.decoders.remove(win);
                        self.log.push(format!("destroy {win}"));
                    }
                    Message::WinTitle { win, title } => {
                        self.titles.insert(*win, title.clone());
                    }
                    Message::Frame { win, seq, width, height, data, .. } => {
                        let dec = self.decoders.get_mut(win).expect("quadro de janela desconhecida");
                        assert_eq!((dec.width(), dec.height()), (*width, *height));
                        let regioes = dec.decode(data).expect("decodificação");
                        self.rects.push((*win, regioes));
                        self.frames += 1;
                        if self.auto_ack {
                            let ack = Message::FrameAck { win: *win, seq: *seq };
                            self.send(ack);
                        } else {
                            self.unacked.push((*win, *seq));
                        }
                    }
                    _ => {}
                }
                Some(m)
            }
            Ok(Err(e)) => panic!("erro de leitura: {e}"),
            Err(RecvTimeoutError::Timeout) => None,
            Err(RecvTimeoutError::Disconnected) => panic!("leitor terminou"),
        }
    }

    /// Passa a confirmar tudo, inclusive o que ficou pendente.
    fn start_acking(&mut self) {
        self.auto_ack = true;
        for (win, seq) in std::mem::take(&mut self.unacked) {
            self.send(Message::FrameAck { win, seq });
        }
    }

    fn until(&mut self, secs: u64, what: &str, mut cond: impl FnMut(&Proto) -> bool) {
        let limite = Instant::now() + Duration::from_secs(secs);
        while !cond(self) {
            assert!(Instant::now() < limite, "tempo esgotado esperando: {what}\nregistro: {:?}", self.log);
            self.next(Duration::from_millis(20));
        }
    }

    /// Espera a janela `win` assentar no quadro final de `BAND_FRAMES` faixas e confere cada pixel.
    fn expect_settled(&mut self, win: u32, size: (u32, u32)) {
        let (w, h) = size;
        let mut ultimo = String::new();
        let limite = Instant::now() + Duration::from_secs(20);
        loop {
            self.next(Duration::from_millis(20));
            if let Some(d) = self.decoders.get(&win) {
                if (d.width(), d.height()) == (w, h) {
                    let mut px = d.pixels().to_vec();
                    normalize_x(&mut px);
                    let (n, m) = pattern::read_marker(&px);
                    if n > m && n - m == BAND_FRAMES {
                        let esp = pattern::expected(w, h, n, m);
                        if px == esp {
                            return;
                        }
                        let onde = px.iter().zip(&esp).position(|(a, b)| a != b).unwrap();
                        ultimo = format!("n={n} m={m}: primeiro byte diferente em {onde} (pixel {}, {})", (onde / 4) % w as usize, (onde / 4) / w as usize);
                    }
                }
            }
            assert!(Instant::now() < limite, "janela {win} não assentou no padrão esperado ({w}x{h}); {ultimo}\nregistro: {:?}", self.log);
        }
    }

    fn drop_connection(self) {
        self.closer.close();
    }
}

fn read_log(path: &Path) -> String {
    std::fs::read_to_string(path).unwrap_or_default()
}

fn wait_log(path: &Path, proto: &mut Proto, needle: &str) {
    let limite = Instant::now() + Duration::from_secs(10);
    while !read_log(path).contains(needle) {
        proto.next(Duration::from_millis(20));
        assert!(Instant::now() < limite, "o cliente de teste não registrou {needle:?}; registro:\n{}", read_log(path));
    }
}

fn fluxo_completo(cliente_wayland_primeiro: bool) {
    let rig = rig(1);
    let log = rig.dir.join("eventos.log");
    let mut proto;
    let cliente;
    if cliente_wayland_primeiro {
        cliente = spawn_testclient(&rig.socket(), &log, None);
        // Espera o toplevel existir (arquivo de registro com o configure) antes de conectar.
        let limite = Instant::now() + Duration::from_secs(10);
        while !read_log(&log).contains("configure 960x600") {
            assert!(Instant::now() < limite, "sem configure: {}", read_log(&log));
            std::thread::sleep(Duration::from_millis(20));
        }
        std::thread::sleep(Duration::from_millis(300));
        proto = rig.client(true);
    } else {
        proto = rig.client(true);
        cliente = spawn_testclient(&rig.socket(), &log, None);
    }

    // 1. WIN_CREATE com título e tamanho do configure inicial (960x600).
    proto.until(10, "WIN_CREATE", |p| !p.titles.is_empty());
    let win = *proto.titles.keys().next().unwrap();
    assert_eq!(proto.titles[&win], "e2e-teste ação");
    assert_eq!((proto.decoders[&win].width(), proto.decoders[&win].height()), (960, 600));

    // 2. Pixels idênticos ao padrão, depois que o desenho assenta.
    proto.expect_settled(win, (960, 600));

    // 3. RESIZE_REQ -> WIN_RESIZE e pixels corretos no tamanho novo.
    proto.send(Message::ResizeReq { win, width: 500, height: 300 });
    let mut visto = false;
    let limite = Instant::now() + Duration::from_secs(10);
    while !visto {
        assert!(Instant::now() < limite, "WIN_RESIZE não chegou: {:?}", proto.log);
        if let Some(Message::WinResize { win: w, width, height }) = proto.next(Duration::from_millis(20)) {
            assert_eq!((w, width, height), (win, 500, 300));
            visto = true;
        }
    }
    proto.expect_settled(win, (500, 300));
    assert!(read_log(&log).contains("configure 500x300"));

    // 4. Entrada: tecla simples, uma que depende do layout br (ç), modificador, ponteiro.
    proto.send(Message::Focus { win, focused: true });
    let teclas: [(u32, bool); 8] = [
        (30, true),  // a
        (30, false),
        (39, true), // ; no US, ç no layout br
        (39, false),
        (42, true), // shift esquerdo
        (39, true), // Ç
        (39, false),
        (42, false),
    ];
    for (k, p) in teclas {
        proto.send(Message::Key { win, keycode: k, pressed: p });
    }
    proto.send(Message::Key { win, keycode: 53, pressed: true }); // '/' no US, ';' no layout br
    proto.send(Message::Key { win, keycode: 53, pressed: false });
    proto.send(Message::PointerMove { win, x: 10.5, y: 20.25 });
    proto.send(Message::PointerButton { win, button: 0x110, pressed: true });
    proto.send(Message::PointerButton { win, button: 0x110, pressed: false });
    proto.send(Message::PointerAxis { win, dx: 0.0, dy: 15.0 });
    wait_log(&log, &mut proto, "axis vertical 15.00");
    let l = read_log(&log);
    assert!(l.contains("keymap ok"), "{l}");
    // O Android do Waydroid escolhe o layout do teclado pela primeira palavra deste nome (docs/23).
    assert!(l.contains("keymap layout \"Brazilian (ABNT2)\""), "nome do layout do keymap: {l}");
    assert!(l.contains("kbd-enter"), "{l}");
    assert!(l.contains("key 30 pressed sym=a utf8=\"a\""), "{l}");
    assert!(l.contains("key 39 pressed sym=ccedilla utf8=\"ç\""), "tecla do ç (layout br): {l}");
    assert!(l.contains("key 39 pressed sym=Ccedilla utf8=\"Ç\""), "Shift+ç: {l}");
    assert!(l.contains("key 53 pressed sym=semicolon utf8=\";\""), "tecla do ';' (layout br): {l}");
    assert!(l.contains("ptr-enter 10.5 20.2"), "{l}");
    assert!(l.contains("button 272 pressed") && l.contains("button 272 released"), "{l}");

    // 5. CLOSE_REQ -> WIN_DESTROY e o cliente de teste termina.
    proto.send(Message::CloseReq { win });
    proto.until(10, "WIN_DESTROY", |p| !p.decoders.contains_key(&win));
    let r = cliente.join().expect("thread do cliente");
    assert_eq!(r, Ok(()));
    assert!(read_log(&log).contains("close"));

    proto.drop_connection();
    rig.finish();
}

/// Linhas `toplevel-states ...` do registro do cliente de teste, como listas de nomes.
fn estados(log: &Path) -> Vec<Vec<String>> {
    read_log(log)
        .lines()
        .filter_map(|l| l.strip_prefix("toplevel-states "))
        .map(|l| l.split(',').filter(|s| !s.is_empty()).map(str::to_string).collect())
        .collect()
}

fn espera_estados(log: &Path, proto: &mut Proto, cond: impl Fn(&[String]) -> bool, o_que: &str) {
    let limite = Instant::now() + Duration::from_secs(10);
    loop {
        if estados(log).last().is_some_and(|e| cond(e)) {
            return;
        }
        proto.next(Duration::from_millis(20));
        assert!(Instant::now() < limite, "estados esperados ({o_que}) não vistos; registro:\n{}", read_log(log));
    }
}

#[test]
fn geometria_da_janela_recorta_a_margem_e_desloca_o_ponteiro() {
    let rig = rig(1);
    let log = rig.dir.join("geometria.log");
    // Buffer de 1000x640 com 20 px de margem magenta; a geometria declarada é 960x600.
    let cliente = spawn_testclient_with(&rig.socket(), &log, |o| o.margin = 20);
    let mut proto = rig.client(true);
    proto.until(10, "WIN_CREATE", |p| !p.titles.is_empty());
    let win = *proto.titles.keys().next().unwrap();
    // O tamanho transmitido é o da geometria, não o do buffer.
    assert_eq!((proto.decoders[&win].width(), proto.decoders[&win].height()), (960, 600));
    // Os pixels são só os da área útil: o padrão exato, sem nenhum pixel da margem.
    proto.expect_settled(win, (960, 600));

    // Ponteiro: (10.5, 20.25) na área transmitida é (30.5, 40.25) na superfície do app.
    proto.send(Message::Focus { win, focused: true });
    proto.send(Message::PointerMove { win, x: 10.5, y: 20.25 });
    wait_log(&log, &mut proto, "ptr-enter 30.5 40.2");

    // Redimensionar: nova geometria, novo buffer com margem.
    proto.send(Message::ResizeReq { win, width: 500, height: 300 });
    proto.until(10, "WIN_RESIZE", |p| p.log.iter().any(|l| l == &format!("resize {win} 500x300")));
    proto.expect_settled(win, (500, 300));

    proto.send(Message::CloseReq { win });
    proto.until(10, "WIN_DESTROY", |p| !p.decoders.contains_key(&win));
    assert_eq!(cliente.join().unwrap(), Ok(()));
    proto.drop_connection();
    rig.finish();
}

#[test]
fn estados_ativado_e_ladrilhado_nas_quatro_bordas() {
    let rig = rig(1);
    let log = rig.dir.join("estados.log");
    let cliente = spawn_testclient_with(&rig.socket(), &log, |_| {});
    let mut proto = rig.client(true);
    proto.until(10, "WIN_CREATE", |p| !p.titles.is_empty());
    let win = *proto.titles.keys().next().unwrap();

    let ladrilhada = |e: &[String]| ["TiledLeft", "TiledRight", "TiledTop", "TiledBottom"].iter().all(|n| e.iter().any(|x| x == n));
    espera_estados(&log, &mut proto, |e| ladrilhada(e) && e.iter().any(|x| x == "Activated"), "ativada e ladrilhada");

    // Perdeu o foco: sai "ativado", continua ladrilhada.
    proto.send(Message::Focus { win, focused: false });
    espera_estados(&log, &mut proto, |e| ladrilhada(e) && !e.iter().any(|x| x == "Activated"), "sem ativação");
    // Ganhou o foco de novo.
    proto.send(Message::Focus { win, focused: true });
    espera_estados(&log, &mut proto, |e| ladrilhada(e) && e.iter().any(|x| x == "Activated"), "ativada de novo");

    proto.send(Message::CloseReq { win });
    proto.until(10, "WIN_DESTROY", |p| !p.decoders.contains_key(&win));
    assert_eq!(cliente.join().unwrap(), Ok(()));
    proto.drop_connection();
    rig.finish();
}

#[test]
fn limites_de_tamanho_viram_win_limits() {
    let rig = rig(1);
    let log = rig.dir.join("limites.log");
    let cliente = spawn_testclient_with(&rig.socket(), &log, |o| {
        o.min_size = Some((200, 150));
        o.max_size = Some((1000, 800));
    });
    let mut proto = rig.client_marco3();
    proto.until(10, "WIN_LIMITS", |p| p.seen.iter().any(|m| matches!(m, Message::WinLimits { .. })));
    let win = *proto.titles.keys().next().unwrap();
    let lim = proto.seen.iter().find_map(|m| if let Message::WinLimits { win, min_w, min_h, max_w, max_h } = m { Some((*win, *min_w, *min_h, *max_w, *max_h)) } else { None });
    assert_eq!(lim, Some((win, 200, 150, 1000, 800)));

    proto.send(Message::CloseReq { win });
    proto.until(10, "WIN_DESTROY", |p| !p.decoders.contains_key(&win));
    assert_eq!(cliente.join().unwrap(), Ok(()));
    proto.drop_connection();
    rig.finish();
}

#[test]
fn redimensionamento_pedido_pelo_app_chega_como_win_resize() {
    let rig = rig(1);
    let log = rig.dir.join("autoresize.log");
    let cliente = spawn_testclient_with(&rig.socket(), &log, |o| o.self_resize = Some((700, 500)));
    let mut proto = rig.client(true);
    proto.until(10, "WIN_CREATE", |p| !p.titles.is_empty());
    let win = *proto.titles.keys().next().unwrap();
    proto.expect_settled(win, (960, 600));

    // A tecla "r" faz o app trocar de tamanho sozinho, sem nenhum RESIZE_REQ.
    proto.send(Message::Focus { win, focused: true });
    wait_log(&log, &mut proto, "kbd-enter");
    proto.send(Message::Key { win, keycode: 19, pressed: true });
    proto.send(Message::Key { win, keycode: 19, pressed: false });
    proto.until(10, "WIN_RESIZE do app", |p| p.log.iter().any(|l| l == &format!("resize {win} 700x500")));
    proto.expect_settled(win, (700, 500));
    assert!(!proto.seen.iter().any(|m| matches!(m, Message::ResizeReq { .. })));

    proto.send(Message::CloseReq { win });
    proto.until(10, "WIN_DESTROY", |p| !p.decoders.contains_key(&win));
    assert_eq!(cliente.join().unwrap(), Ok(()));
    proto.drop_connection();
    rig.finish();
}

// ---------------------------------------------------------------- popups

/// Manda uma tecla (pressiona e solta) pelo protocolo.
fn tecla(proto: &mut Proto, win: u32, code: u32) {
    proto.send(Message::Key { win, keycode: code, pressed: true });
    proto.send(Message::Key { win, keycode: code, pressed: false });
}

fn ocorrencias(log: &Path, needle: &str) -> usize {
    read_log(log).matches(needle).count()
}

/// Espera o registro do cliente de teste ter `needle` pelo menos `n` vezes.
fn wait_log_n(log: &Path, proto: &mut Proto, needle: &str, n: usize) {
    let limite = Instant::now() + Duration::from_secs(10);
    while ocorrencias(log, needle) < n {
        proto.next(Duration::from_millis(20));
        assert!(Instant::now() < limite, "o cliente de teste não registrou {needle:?} {n} vez(es); registro:\n{}", read_log(log));
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
struct PopupMsg {
    win: u32,
    parent: u32,
    x: i32,
    y: i32,
    w: u32,
    h: u32,
    grab: bool,
}

/// O `n`-ésimo (a partir de 0) `POPUP_CREATE` recebido.
fn popup_create(p: &Proto, n: usize) -> Option<PopupMsg> {
    p.seen
        .iter()
        .filter_map(|m| match m {
            Message::PopupCreate { win, parent, x, y, width, height, grab } => Some(PopupMsg { win: *win, parent: *parent, x: *x, y: *y, w: *width, h: *height, grab: *grab }),
            _ => None,
        })
        .nth(n)
}

fn espera_popup(proto: &mut Proto, n: usize) -> PopupMsg {
    proto.until(10, "POPUP_CREATE", |p| popup_create(p, n).is_some());
    popup_create(proto, n).unwrap()
}

/// Espera o quadro do popup ser decodificado com a cor sólida `rgb` no centro.
fn espera_cor(proto: &mut Proto, win: u32, rgb: u32) {
    proto.until(10, "quadro do popup com a cor esperada", |p| {
        p.decoders.get(&win).is_some_and(|d| {
            let (w, h) = (d.width() as usize, d.height() as usize);
            let i = ((h / 2) * w + w / 2) * 4;
            let px = &d.pixels()[i..i + 3];
            (px[2], px[1], px[0]) == ((rgb >> 16) as u8, (rgb >> 8) as u8, rgb as u8)
        })
    });
}

fn destruido(p: &Proto, win: u32) -> bool {
    p.log.iter().any(|l| l == &format!("destroy {win}"))
}

fn posicao_do_destroy(p: &Proto, win: u32) -> usize {
    p.log.iter().position(|l| l == &format!("destroy {win}")).expect("destroy no registro")
}

fn abre_janela_com_popups(rig: &Rig, nome: &str) -> (Proto, u32, PathBuf, JoinHandle<Result<(), String>>) {
    let log = rig.dir.join(format!("{nome}.log"));
    let cliente = spawn_testclient_with(&rig.socket(), &log, |o| o.popups = true);
    let mut proto = rig.client_marco3();
    proto.until(10, "WIN_CREATE", |p| !p.titles.is_empty());
    let win = *proto.titles.keys().next().unwrap();
    proto.send(Message::Focus { win, focused: true });
    wait_log_n(&log, &mut proto, "kbd-enter main", 1);
    (proto, win, log, cliente)
}

fn fecha_janela(mut proto: Proto, win: u32, cliente: JoinHandle<Result<(), String>>, rig: Rig) {
    proto.send(Message::CloseReq { win });
    proto.until(10, "WIN_DESTROY da janela", |p| !p.decoders.contains_key(&win));
    assert_eq!(cliente.join().unwrap(), Ok(()));
    proto.drop_connection();
    rig.finish();
}

#[test]
fn menu_com_captura_submenu_e_dispensa_em_cadeia() {
    let rig = rig(1);
    let (mut proto, win, log, cliente) = abre_janela_com_popups(&rig, "menu");

    // `p` abre o menu: POPUP_CREATE na posição calculada do positioner (anchor BottomLeft de (100,50,40,20)).
    tecla(&mut proto, win, 25);
    let menu = espera_popup(&mut proto, 0);
    assert_eq!((menu.parent, menu.x, menu.y, menu.w, menu.h, menu.grab), (win, 100, 70, 200, 150, true));
    // O conteúdo chega por FRAME com o id do popup.
    espera_cor(&mut proto, menu.win, 0x2040C0);
    // O teclado passa ao popup com captura.
    wait_log_n(&log, &mut proto, "kbd-enter popup1", 1);

    // Submenu: filho do menu, em (190, 10) relativo a ele.
    tecla(&mut proto, win, 46);
    let sub = espera_popup(&mut proto, 1);
    assert_eq!((sub.parent, sub.x, sub.y, sub.w, sub.h, sub.grab), (menu.win, 190, 10, 120, 80, true));
    espera_cor(&mut proto, sub.win, 0xC04020);
    wait_log_n(&log, &mut proto, "kbd-enter popup2", 1);

    // As teclas vão para o submenu (o mais fundo da cadeia).
    tecla(&mut proto, win, 30);
    wait_log_n(&log, &mut proto, "key 30 pressed sym=a utf8=\"a\" focus=popup2", 1);

    // O cliente dispensa a cadeia (clique fora): o filho sai antes do pai, `popup_done` nos dois.
    proto.send(Message::CloseReq { win: menu.win });
    proto.until(10, "WIN_DESTROY dos dois popups", |p| destruido(p, menu.win) && destruido(p, sub.win));
    assert!(posicao_do_destroy(&proto, sub.win) < posicao_do_destroy(&proto, menu.win), "o filho deve sair antes do pai: {:?}", proto.log);
    wait_log_n(&log, &mut proto, "popup-done popup2", 1);
    wait_log_n(&log, &mut proto, "popup-done popup1", 1);
    // O teclado volta para a janela.
    wait_log_n(&log, &mut proto, "kbd-enter main", 2);

    // Clique na janela com o menu aberto: dispensa o menu e o clique é consumido.
    tecla(&mut proto, win, 25);
    let m2 = espera_popup(&mut proto, 2);
    proto.send(Message::PointerMove { win, x: 400.0, y: 400.0 });
    proto.send(Message::PointerButton { win, button: 0x110, pressed: true });
    proto.send(Message::PointerButton { win, button: 0x110, pressed: false });
    proto.until(10, "menu dispensado pelo clique", |p| destruido(p, m2.win));
    // Outro clique, agora sem menu, chega ao app (prova que o laço de consumo terminou).
    proto.send(Message::PointerButton { win, button: 0x110, pressed: true });
    proto.send(Message::PointerButton { win, button: 0x110, pressed: false });
    wait_log(&log, &mut proto, "button 272 released");
    assert_eq!(ocorrencias(&log, "button 272 pressed"), 1, "só o segundo clique chega ao app:\n{}", read_log(&log));
    assert_eq!(ocorrencias(&log, "button 272 released"), 1, "{}", read_log(&log));

    // FOCUS 0 da raiz dispensa o menu.
    wait_log_n(&log, &mut proto, "kbd-enter main", 3);
    tecla(&mut proto, win, 25);
    let m3 = espera_popup(&mut proto, 3);
    proto.send(Message::Focus { win, focused: false });
    proto.until(10, "menu dispensado pela perda de foco", |p| destruido(p, m3.win));

    fecha_janela(proto, win, cliente, rig);
}

#[test]
fn dica_sem_captura_nao_recebe_teclado_e_some_quando_o_app_a_destroi() {
    let rig = rig(1);
    let (mut proto, win, log, cliente) = abre_janela_com_popups(&rig, "dica");

    tecla(&mut proto, win, 20); // t
    let dica = espera_popup(&mut proto, 0);
    assert_eq!((dica.parent, dica.x, dica.y, dica.w, dica.h, dica.grab), (win, 300, 300, 120, 40, false));
    espera_cor(&mut proto, dica.win, 0x20A040);

    // O teclado continua na janela principal.
    tecla(&mut proto, win, 30);
    wait_log_n(&log, &mut proto, "key 30 pressed sym=a utf8=\"a\" focus=main", 1);
    assert_eq!(ocorrencias(&log, "kbd-enter tip"), 0, "{}", read_log(&log));

    // Um clique na janela não dispensa a dica (não há captura): ela só some quando o app a destrói.
    proto.send(Message::PointerMove { win, x: 50.0, y: 50.0 });
    proto.send(Message::PointerButton { win, button: 0x110, pressed: true });
    proto.send(Message::PointerButton { win, button: 0x110, pressed: false });
    wait_log(&log, &mut proto, "button 272 released");
    assert!(!destruido(&proto, dica.win));
    tecla(&mut proto, win, 22); // u
    proto.until(10, "WIN_DESTROY da dica", |p| destruido(p, dica.win));

    fecha_janela(proto, win, cliente, rig);
}

#[test]
fn reposicionar_o_popup_vira_popup_move() {
    let rig = rig(1);
    let (mut proto, win, log, cliente) = abre_janela_com_popups(&rig, "reposicao");

    tecla(&mut proto, win, 25);
    let menu = espera_popup(&mut proto, 0);
    tecla(&mut proto, win, 47); // v
    proto.until(10, "POPUP_MOVE", |p| p.seen.iter().any(|m| matches!(m, Message::PopupMove { .. })));
    let mv = proto.seen.iter().find_map(|m| if let Message::PopupMove { win, x, y, width, height } = m { Some((*win, *x, *y, *width, *height)) } else { None });
    assert_eq!(mv, Some((menu.win, 300, 210, 200, 150)));
    wait_log_n(&log, &mut proto, "popup-repositioned popup1 7", 1);

    fecha_janela(proto, win, cliente, rig);
}

#[test]
fn popup_e_restrito_ao_monitor_virtual() {
    let rig = rig(1);
    let (mut proto, win, _log, cliente) = abre_janela_com_popups(&rig, "monitor");

    // Janela grande (1900x1000): o popup 300x200 pedido no canto passaria do monitor de 1920x1080 e
    // é deslizado para dentro dele.
    proto.send(Message::ResizeReq { win, width: 1900, height: 1000 });
    proto.until(10, "WIN_RESIZE", |p| p.log.iter().any(|l| l == &format!("resize {win} 1900x1000")));
    tecla(&mut proto, win, 48); // b
    let grande = espera_popup(&mut proto, 0);
    assert_eq!((grande.x, grande.y, grande.w, grande.h), (1620, 880, 300, 200));

    fecha_janela(proto, win, cliente, rig);
}

#[test]
fn cliente_sem_a_extensao_de_popups_tem_o_menu_dispensado_na_hora() {
    let rig = rig(1);
    let log = rig.dir.join("sem-extensao.log");
    let cliente = spawn_testclient_with(&rig.socket(), &log, |o| o.popups = true);
    let mut proto = rig.client(true); // só o codec de blocos
    proto.until(10, "WIN_CREATE", |p| !p.titles.is_empty());
    let win = *proto.titles.keys().next().unwrap();
    proto.send(Message::Focus { win, focused: true });
    wait_log_n(&log, &mut proto, "kbd-enter main", 1);
    tecla(&mut proto, win, 25);
    wait_log_n(&log, &mut proto, "popup-done popup1", 1);
    assert!(popup_create(&proto, 0).is_none());
    fecha_janela(proto, win, cliente, rig);
}

// ---------------------------------------------------------------- cursor

/// Abre a janela com o cursor pedido, leva o ponteiro para dentro dela e devolve o primeiro `CURSOR`.
fn cursor_recebido(modo: CursorMode, nome: &str) -> CursorShape {
    let rig = rig(1);
    let log = rig.dir.join(format!("{nome}.log"));
    let cliente = spawn_testclient_with(&rig.socket(), &log, |o| o.cursor = Some(modo));
    let mut proto = rig.client_marco3();
    proto.until(10, "WIN_CREATE", |p| !p.titles.is_empty());
    let win = *proto.titles.keys().next().unwrap();
    proto.send(Message::Focus { win, focused: true });
    wait_log_n(&log, &mut proto, "kbd-enter main", 1);
    // O ponteiro entra na janela; o app responde com o cursor dele.
    proto.send(Message::PointerMove { win, x: 30.0, y: 40.0 });
    proto.until(10, "CURSOR do app", |p| p.seen.iter().any(|m| matches!(m, Message::Cursor { shape, .. } if *shape != CursorShape::Named("default".into()))));
    let achado = proto.seen.iter().find_map(|m| match m {
        Message::Cursor { win: w, shape } if *shape != CursorShape::Named("default".into()) => Some((*w, shape.clone())),
        _ => None,
    });
    let (w, shape) = achado.unwrap();
    assert_eq!(w, win, "o cursor vale para a janela sob o ponteiro");
    fecha_janela(proto, win, cliente, rig);
    shape
}

#[test]
fn cursor_nomeado_do_cursor_shape_chega_como_nome_css() {
    assert_eq!(cursor_recebido(CursorMode::Named("text".into()), "cursor-nome"), CursorShape::Named("text".into()));
}

#[test]
fn cursor_por_imagem_com_commit_antes_do_set_cursor() {
    cursor_de_imagem(CursorMode::Image, "cursor-imagem");
}

#[test]
fn cursor_por_imagem_com_commit_depois_do_set_cursor() {
    cursor_de_imagem(CursorMode::ImageLate, "cursor-imagem-tarde");
}

fn cursor_de_imagem(modo: CursorMode, nome: &str) {
    match cursor_recebido(modo, nome) {
        CursorShape::Image { width, height, hot_x, hot_y, bgra } => {
            assert_eq!((u32::from(width), u32::from(height)), (IMAGE_SIDE, IMAGE_SIDE));
            assert_eq!((i32::from(hot_x), i32::from(hot_y)), IMAGE_HOTSPOT);
            assert_eq!(bgra.len(), (IMAGE_SIDE * IMAGE_SIDE * 4) as usize);
            assert!(bgra.chunks(4).all(|p| p == IMAGE_PIXEL), "pixels do cursor");
        }
        outro => panic!("esperava imagem, veio {outro:?}"),
    }
}

#[test]
fn cursor_oculto() {
    assert_eq!(cursor_recebido(CursorMode::Hidden, "cursor-oculto"), CursorShape::Hidden);
}

#[test]
fn sem_a_extensao_de_cursor_nada_e_enviado() {
    let rig = rig(1);
    let log = rig.dir.join("cursor-sem-ext.log");
    let cliente = spawn_testclient_with(&rig.socket(), &log, |o| o.cursor = Some(CursorMode::Named("text".into())));
    let mut proto = rig.client(true); // só o codec de blocos
    proto.until(10, "WIN_CREATE", |p| !p.titles.is_empty());
    let win = *proto.titles.keys().next().unwrap();
    proto.send(Message::Focus { win, focused: true });
    wait_log_n(&log, &mut proto, "kbd-enter main", 1);
    proto.send(Message::PointerMove { win, x: 30.0, y: 40.0 });
    wait_log_n(&log, &mut proto, "cursor-set named text", 1);
    let fim = Instant::now() + Duration::from_millis(400);
    while Instant::now() < fim {
        proto.next(Duration::from_millis(20));
    }
    assert!(!proto.seen.iter().any(|m| matches!(m, Message::Cursor { .. })), "{:?}", proto.seen);
    fecha_janela(proto, win, cliente, rig);
}

// ---------------------------------------------------------------- área de transferência

/// Textos de todas as mensagens `CLIPBOARD` recebidas do servidor.
fn textos_da_area(p: &Proto) -> Vec<String> {
    p.seen
        .iter()
        .filter_map(|m| match m {
            Message::Clipboard { mime, data } => {
                assert_eq!(mime, MIME_TEXT);
                Some(String::from_utf8(data.clone()).unwrap())
            }
            _ => None,
        })
        .collect()
}

/// Abre o cliente de teste (com `ajusta`), conecta um cliente de protocolo com `caps` e dá o foco à janela.
fn abre_com_area(rig: &Rig, nome: &str, caps: u32, ajusta: impl FnOnce(&mut Options)) -> (Proto, u32, PathBuf, JoinHandle<Result<(), String>>) {
    let log = rig.dir.join(format!("{nome}.log"));
    let cliente = spawn_testclient_with(&rig.socket(), &log, ajusta);
    let mut proto = Proto::connect(rig.addr, true, caps);
    proto.until(10, "WIN_CREATE", |p| !p.titles.is_empty());
    let win = *proto.titles.keys().next().unwrap();
    proto.send(Message::Focus { win, focused: true });
    wait_log_n(&log, &mut proto, "kbd-enter main", 1);
    (proto, win, log, cliente)
}

/// Deixa passar um tempo lendo mensagens do servidor.
fn espera(proto: &mut Proto, ms: u64) {
    let fim = Instant::now() + Duration::from_millis(ms);
    while Instant::now() < fim {
        proto.next(Duration::from_millis(20));
    }
}

#[test]
fn selecao_do_pi_chega_ao_hospedeiro_e_nao_se_repete() {
    let rig = rig(1);
    let texto = "pistick-teste-123 ação ç";
    let (mut proto, win, _log, cliente) = abre_com_area(&rig, "area-pi", CAP_BLOCK_CODEC | CAPS_MARCO3, |o| o.clipboard = Some(texto.into()));

    // O app do Pi define a seleção (tecla y): o servidor lê o texto na hora e envia.
    tecla(&mut proto, win, 21);
    proto.until(10, "CLIPBOARD", |p| !textos_da_area(p).is_empty());
    assert_eq!(textos_da_area(&proto), vec![texto.to_string()]);

    // Mesma seleção de novo: conteúdo idêntico ao último, não é reenviado.
    tecla(&mut proto, win, 21);
    espera(&mut proto, 600);
    assert_eq!(textos_da_area(&proto).len(), 1, "{:?}", proto.seen.len());

    fecha_janela(proto, win, cliente, rig);
}

#[test]
fn texto_do_hospedeiro_vira_a_selecao_dos_apps_sem_eco() {
    let rig = rig(1);
    let (mut proto, win, log, cliente) = abre_com_area(&rig, "area-host", CAP_BLOCK_CODEC | CAPS_MARCO3, |o| o.clipboard = Some("vindo-do-hospedeiro ação".into()));

    // O hospedeiro define o texto: o app (com foco) recebe uma seleção nova que ele consegue ler.
    proto.send(Message::ClipboardUp { mime: MIME_TEXT.into(), data: "do Windows ç 123".as_bytes().to_vec() });
    wait_log_n(&log, &mut proto, "selection-received \"do Windows ç 123\"", 1);
    // Outro texto: outra seleção.
    proto.send(Message::ClipboardUp { mime: MIME_TEXT.into(), data: "segundo".as_bytes().to_vec() });
    wait_log_n(&log, &mut proto, "selection-received \"segundo\"", 1);

    // O app define a própria seleção (tecla y): como o texto é novo, vai ao hospedeiro...
    tecla(&mut proto, win, 21);
    proto.until(10, "CLIPBOARD", |p| !textos_da_area(p).is_empty());
    assert_eq!(textos_da_area(&proto), vec!["vindo-do-hospedeiro ação".to_string()]);
    // ...e o hospedeiro mandar de volta o mesmo texto não vira seleção de novo (sem eco).
    let antes = ocorrencias(&log, "selection-received");
    proto.send(Message::ClipboardUp { mime: MIME_TEXT.into(), data: "vindo-do-hospedeiro ação".as_bytes().to_vec() });
    espera(&mut proto, 600);
    assert_eq!(ocorrencias(&log, "selection-received"), antes, "{}", read_log(&log));

    fecha_janela(proto, win, cliente, rig);
}

#[test]
fn texto_acima_de_1_mib_nao_e_enviado() {
    let rig = rig(1);
    let grande = "a".repeat(MAX_CLIPBOARD + 100);
    let (mut proto, win, log, cliente) = abre_com_area(&rig, "area-grande", CAP_BLOCK_CODEC | CAPS_MARCO3, |o| o.clipboard = Some(grande));
    tecla(&mut proto, win, 21);
    wait_log_n(&log, &mut proto, "selection-sent", 1);
    espera(&mut proto, 1000);
    assert!(textos_da_area(&proto).is_empty(), "texto acima de 1 MiB não deve ser enviado");
    fecha_janela(proto, win, cliente, rig);
}

#[test]
fn sem_a_extensao_de_area_de_transferencia_nada_e_enviado() {
    let rig = rig(1);
    let (mut proto, win, log, cliente) = abre_com_area(&rig, "area-sem-ext", CAP_BLOCK_CODEC, |o| o.clipboard = Some("texto".into()));
    tecla(&mut proto, win, 21);
    wait_log_n(&log, &mut proto, "selection-set", 1);
    espera(&mut proto, 500);
    // Ninguém pediu o texto ao app: sem a extensão, o servidor nem tenta lê-lo.
    assert_eq!(ocorrencias(&log, "selection-sent"), 0);
    assert!(textos_da_area(&proto).is_empty());
    fecha_janela(proto, win, cliente, rig);
}

#[test]
fn decoracao_do_kde_responde_servidor_para_o_gtk3() {
    // O GTK 3 só abandona a barra de título própria se o compositor anuncia o protocolo do KDE com modo servidor.
    let rig = rig(1);
    let log = rig.dir.join("kde.log");
    let cliente = spawn_testclient_with(&rig.socket(), &log, |o| o.kde_decoration = true);
    let mut proto = rig.client(true);
    proto.until(10, "WIN_CREATE", |p| !p.titles.is_empty());
    let win = *proto.titles.keys().next().unwrap();
    wait_log(&log, &mut proto, "kde-default-mode Server");
    wait_log(&log, &mut proto, "kde-mode Server");
    assert!(!read_log(&log).contains("kde-mode Client"), "{}", read_log(&log));
    fecha_janela(proto, win, cliente, rig);
}

#[test]
fn ponta_a_ponta_com_janela_ja_existente_na_conexao() {
    fluxo_completo(true);
}

#[test]
fn ponta_a_ponta_com_janela_criada_depois_da_conexao() {
    fluxo_completo(false);
}

#[test]
fn retorno_de_quadro_segue_o_ritmo_da_sessao() {
    // Sem sessão: o app desenha a no máximo 30 quadros por segundo.
    let rig = rig(1);
    let log = rig.dir.join("ritmo.log");
    let contador = Arc::new(AtomicU32::new(0));
    let opts = Options {
        socket: Some(rig.socket()),
        log: Some(log.clone()),
        counter: Some(contador.clone()),
        ..Options::default() // sem limite de quadros
    };
    let cliente = std::thread::spawn(move || ps_testclient::run(opts));
    std::thread::sleep(Duration::from_millis(500));
    let a = contador.load(Ordering::Relaxed);
    std::thread::sleep(Duration::from_millis(1000));
    let sem_sessao = contador.load(Ordering::Relaxed) - a;
    assert!((15..=36).contains(&sem_sessao), "sem sessão esperava ~30 quadros/s, vi {sem_sessao}");

    // Com sessão que não confirma nada: o app para depois de poucos quadros (2 em voo).
    let mut proto = rig.client(false);
    proto.until(10, "WIN_CREATE", |p| !p.titles.is_empty());
    let win = *proto.titles.keys().next().unwrap();
    std::thread::sleep(Duration::from_millis(100));
    let a = contador.load(Ordering::Relaxed);
    let t_fim = Instant::now() + Duration::from_millis(600);
    while Instant::now() < t_fim {
        proto.next(Duration::from_millis(20));
    }
    let sem_ack = contador.load(Ordering::Relaxed) - a;
    assert!(sem_ack <= 5, "sem confirmações o app deveria parar, vi {sem_ack} quadros em 600 ms");

    // Confirmando tudo, o app anda de novo no ritmo do enlace.
    proto.start_acking();
    let a = contador.load(Ordering::Relaxed);
    let t_fim = Instant::now() + Duration::from_millis(1000);
    while Instant::now() < t_fim {
        proto.next(Duration::from_millis(5));
    }
    let com_ack = contador.load(Ordering::Relaxed) - a;
    assert!(com_ack > 30, "com confirmações o app deveria passar de 30 quadros/s, vi {com_ack}");

    proto.send(Message::CloseReq { win });
    proto.until(10, "WIN_DESTROY", |p| !p.decoders.contains_key(&win));
    assert_eq!(cliente.join().unwrap(), Ok(()));
    proto.drop_connection();
    rig.finish();
}

// ---------------------------------------------------------------- DMABUF (marco 4.5)

/// O teste precisa do heap do sistema (alocar um DMABUF sem GPU) e do nó de renderização (sem ele o `psd`
/// não anuncia o global).
fn dmabuf_disponivel() -> bool {
    let heap = std::fs::OpenOptions::new().read(true).write(true).open("/dev/dma_heap/system").is_ok();
    let render = Path::new("/dev/dri/renderD128").exists();
    if !(heap && render) {
        eprintln!("DMABUF indisponível (heap: {heap}, renderD128: {render}); teste pulado");
    }
    heap && render
}

fn spawn_dmabuf(socket: &Path, defeito: ps_testclient::Defeito, burst: u32, hold: Duration) -> JoinHandle<Result<(), String>> {
    let opts = ps_testclient::DmabufOptions { socket: socket.to_path_buf(), defeito, burst, hold, ..Default::default() };
    std::thread::spawn(move || ps_testclient::run_dmabuf(opts))
}

fn spawn_dmabuf_formato(socket: &Path, formato: ps_testclient::FormatoDmabuf) -> JoinHandle<Result<(), String>> {
    let opts = ps_testclient::DmabufOptions { socket: socket.to_path_buf(), formato, hold: Duration::from_secs(2), ..Default::default() };
    std::thread::spawn(move || ps_testclient::run_dmabuf(opts))
}

/// Espera a janela `win` ficar com o padrão-base `n` (320x200), bit a bit.
fn espera_padrao_dmabuf(proto: &mut Proto, win: u32, n: u32) {
    let (w, h) = (320u32, 200u32);
    let limite = Instant::now() + Duration::from_secs(10);
    loop {
        proto.next(Duration::from_millis(20));
        if let Some(d) = proto.decoders.get(&win) {
            if (d.width(), d.height()) == (w, h) {
                let mut px = d.pixels().to_vec();
                normalize_x(&mut px);
                if px == pattern::expected(w, h, n, n) {
                    return;
                }
            }
        }
        assert!(Instant::now() < limite, "a janela {win} não ficou com o padrão {n} (DMABUF); registro: {:?}", proto.log);
    }
}

#[test]
fn dmabuf_quadro_identico_bit_a_bit() {
    if !dmabuf_disponivel() {
        return;
    }
    let rig = rig(1);
    let mut proto = rig.client(true);
    let cliente = spawn_dmabuf(&rig.socket(), ps_testclient::Defeito::Nenhum, 1, Duration::from_secs(2));
    proto.until(10, "WIN_CREATE", |p| !p.titles.is_empty());
    let win = *proto.titles.keys().next().unwrap();
    espera_padrao_dmabuf(&mut proto, win, 0);
    assert_eq!(cliente.join().unwrap(), Ok(()));
    proto.drop_connection();
    rig.finish();
}

#[test]
fn dmabuf_rajada_de_commits_termina_no_ultimo_quadro() {
    if !dmabuf_disponivel() {
        return;
    }
    let rig = rig(1);
    let mut proto = rig.client(true);
    // 40 commits a cada ~3 ms: a maioria cai dentro do intervalo mínimo e não é lida; o último tem de chegar.
    let cliente = spawn_dmabuf(&rig.socket(), ps_testclient::Defeito::Nenhum, 40, Duration::from_secs(2));
    proto.until(10, "WIN_CREATE", |p| !p.titles.is_empty());
    let win = *proto.titles.keys().next().unwrap();
    espera_padrao_dmabuf(&mut proto, win, 39);
    assert_eq!(cliente.join().unwrap(), Ok(()));
    proto.drop_connection();
    rig.finish();
}

#[test]
fn dmabuf_parametros_invalidos_recebem_erro_e_o_psd_segue() {
    if !dmabuf_disponivel() {
        return;
    }
    use ps_testclient::Defeito::*;
    let rig = rig(1);
    let mut proto = rig.client(true);
    for d in [FormatoNaoAnunciado, ModificadorTiling, DoisPlanos, LarguraZero, LarguraAcimaDoLimite, DescritorCurto] {
        let r = spawn_dmabuf(&rig.socket(), d, 1, Duration::from_millis(200)).join().unwrap();
        let e = r.expect_err(&format!("{d:?} deveria ser recusado"));
        assert!(e.contains("erro do compositor"), "{d:?}: {e}");
    }
    // O compositor segue atendendo: um cliente correto aparece com o padrão certo.
    let bom = spawn_dmabuf(&rig.socket(), Nenhum, 1, Duration::from_secs(2));
    proto.until(10, "WIN_CREATE do cliente correto", |p| p.titles.values().any(|t| t == "dmabuf-teste"));
    let win = *proto.titles.iter().find(|(_, t)| *t == "dmabuf-teste").unwrap().0;
    espera_padrao_dmabuf(&mut proto, win, 0);
    assert_eq!(bom.join().unwrap(), Ok(()));
    proto.drop_connection();
    rig.finish();
}

#[test]
fn dmabuf_abgr_e_xbgr_chegam_com_r_e_b_nos_lugares_certos() {
    if !dmabuf_disponivel() {
        return;
    }
    use ps_testclient::FormatoDmabuf::*;
    // O cliente escreve R, G, B, A (ou X) na memória; o `psd` troca R e B na cópia e o quadro que sai
    // é igual, bit a bit, ao do formato XRGB (que o Mesa do Pi usa).
    for formato in [Abgr8888, Xbgr8888, Xrgb8888] {
        let rig = rig(1);
        let mut proto = rig.client(true);
        let cliente = spawn_dmabuf_formato(&rig.socket(), formato);
        proto.until(10, "WIN_CREATE", |p| !p.titles.is_empty());
        let win = *proto.titles.keys().next().unwrap();
        espera_padrao_dmabuf(&mut proto, win, 0);
        assert_eq!(cliente.join().unwrap(), Ok(()), "{formato:?}");
        proto.drop_connection();
        rig.finish();
    }
}

// ---------------------------------------------------------------- subsuperfícies e viewport (marco 5)

fn spawn_sub(socket: &Path, cenario: ps_testclient::Cenario, log: &Path) -> JoinHandle<Result<(), String>> {
    let opts = ps_testclient::SubOptions { socket: socket.to_path_buf(), cenario, log: Some(log.to_path_buf()), ..Default::default() };
    std::thread::spawn(move || ps_testclient::run_sub(opts))
}

/// Cor 0x00RRGGBB do pixel `(x, y)` da janela, se ela existe e o pixel cabe.
fn rgb_em(proto: &Proto, win: u32, (x, y): (u32, u32)) -> Option<u32> {
    let d = proto.decoders.get(&win)?;
    if x >= d.width() || y >= d.height() {
        return None;
    }
    let i = ((y * d.width() + x) * 4) as usize;
    let p = d.pixels();
    Some(u32::from(p[i + 2]) << 16 | u32::from(p[i + 1]) << 8 | u32::from(p[i]))
}

fn espera_pixel(proto: &mut Proto, win: u32, pos: (u32, u32), rgb: u32) {
    let limite = Instant::now() + Duration::from_secs(10);
    while rgb_em(proto, win, pos) != Some(rgb) {
        proto.next(Duration::from_millis(20));
        assert!(Instant::now() < limite, "o pixel {pos:?} da janela {win} não ficou {rgb:06X} (está {:?}); registro: {:?}", rgb_em(proto, win, pos).map(|c| format!("{c:06X}")), proto.log);
    }
}

/// Confere já que o pixel tem a cor.
fn confere_pixel(proto: &Proto, win: u32, pos: (u32, u32), rgb: u32) {
    assert_eq!(rgb_em(proto, win, pos).map(|c| format!("{c:06X}")), Some(format!("{rgb:06X}")), "pixel {pos:?} da janela {win}; registro: {:?}", proto.log);
}

fn janela_sub(proto: &mut Proto) -> u32 {
    proto.until(10, "WIN_CREATE de sub-teste", |p| p.titles.values().any(|t| t == "sub-teste"));
    *proto.titles.iter().find(|(_, t)| *t == "sub-teste").unwrap().0
}

fn tamanho(proto: &Proto, win: u32) -> (u32, u32) {
    let d = &proto.decoders[&win];
    (d.width(), d.height())
}

/// Espera o cliente do cenário terminar sem erro e encerra a sessão.
fn fim_do_cenario(proto: Proto, cliente: JoinHandle<Result<(), String>>, rig: Rig) {
    assert_eq!(cliente.join().unwrap(), Ok(()));
    proto.drop_connection();
    rig.finish();
}

#[test]
fn subsuperficie_shm_sobre_raiz_shm_e_dano_so_da_regiao() {
    use ps_testclient::subsurface::*;
    let rig = rig(1);
    let log = rig.dir.join("sub.log");
    let mut proto = rig.client(true);
    let cliente = spawn_sub(&rig.socket(), Cenario::Dessincronizada, &log);
    wait_log(&log, &mut proto, "pronto");
    let win = janela_sub(&mut proto);
    espera_pixel(&mut proto, win, (60, 40), COR_SUB_A);
    assert_eq!(tamanho(&proto, win), RAIZ);
    // A subsuperfície cobre (50, 30) a (110, 70) e só ela; a raiz aparece em volta.
    for (pos, cor) in [((5, 5), COR_RAIZ), ((150, 100), COR_RAIZ), ((49, 30), COR_RAIZ), ((50, 29), COR_RAIZ), ((50, 30), COR_SUB_A), ((109, 69), COR_SUB_A), ((110, 70), COR_RAIZ), ((110, 50), COR_RAIZ)] {
        confere_pixel(&proto, win, pos, cor);
    }
    // Troca de cor com dano só em (10, 10, 20, 20) da subsuperfície = (60, 40, 20, 20) da janela.
    espera(&mut proto, 300);
    let antes = proto.rects.len();
    wait_log(&log, &mut proto, "dano");
    espera_pixel(&mut proto, win, (65, 45), COR_SUB_B);
    espera(&mut proto, 300);
    confere_pixel(&proto, win, (59, 45), COR_SUB_A);
    confere_pixel(&proto, win, (80, 45), COR_SUB_A);
    confere_pixel(&proto, win, (52, 32), COR_SUB_A);
    confere_pixel(&proto, win, (105, 65), COR_SUB_A);
    confere_pixel(&proto, win, (5, 5), COR_RAIZ);
    // O quadro cobre só os blocos de 64x64 que o dano toca: (0, 0) a (128, 64), não a janela de 200x120.
    let novos: Vec<_> = proto.rects[antes..].iter().filter(|(w, _)| *w == win).collect();
    assert!(!novos.is_empty(), "nenhum quadro depois do dano");
    for (_, rs) in novos {
        for r in rs {
            assert!(r.x + r.w <= 128 && r.y + r.h <= 64, "região fora do esperado: {r:?}");
        }
    }
    fim_do_cenario(proto, cliente, rig);
}

#[test]
fn subsuperficie_sincronizada_so_aparece_no_commit_do_pai() {
    use ps_testclient::subsurface::*;
    let rig = rig(1);
    let log = rig.dir.join("sub.log");
    let mut proto = rig.client(true);
    let cliente = spawn_sub(&rig.socket(), Cenario::Sincronizada, &log);
    wait_log(&log, &mut proto, "raiz");
    let win = janela_sub(&mut proto);
    espera_pixel(&mut proto, win, (60, 40), COR_RAIZ);
    wait_log(&log, &mut proto, "sub-commit");
    // O commit da subsuperfície sincronizada fica em cache até o pai commitar.
    espera(&mut proto, 400);
    confere_pixel(&proto, win, (60, 40), COR_RAIZ);
    wait_log(&log, &mut proto, "pai-commit");
    espera_pixel(&mut proto, win, (60, 40), COR_SUB_A);
    confere_pixel(&proto, win, (49, 30), COR_RAIZ);
    confere_pixel(&proto, win, (110, 70), COR_RAIZ);
    fim_do_cenario(proto, cliente, rig);
}

#[test]
fn place_above_e_place_below_mudam_quem_fica_por_cima() {
    use ps_testclient::subsurface::*;
    let rig = rig(1);
    let log = rig.dir.join("sub.log");
    let mut proto = rig.client(true);
    let cliente = spawn_sub(&rig.socket(), Cenario::Ordem, &log);
    wait_log(&log, &mut proto, "ordem0");
    let win = janela_sub(&mut proto);
    // A em (20, 20) 100x60; B em (60, 40) 100x60; o ponto (80, 50) é dos dois, (30, 30) só de A, (140, 90) só de B.
    espera_pixel(&mut proto, win, (80, 50), COR_SUB_B);
    confere_pixel(&proto, win, (30, 30), COR_SUB_A);
    confere_pixel(&proto, win, (140, 90), COR_SUB_B);
    confere_pixel(&proto, win, (10, 10), COR_RAIZ);
    wait_log(&log, &mut proto, "ordem1");
    espera_pixel(&mut proto, win, (80, 50), COR_SUB_A);
    confere_pixel(&proto, win, (140, 90), COR_SUB_B);
    wait_log(&log, &mut proto, "ordem2");
    espera_pixel(&mut proto, win, (80, 50), COR_SUB_B);
    confere_pixel(&proto, win, (30, 30), COR_SUB_A);
    fim_do_cenario(proto, cliente, rig);
}

#[test]
fn remover_a_subsuperficie_redesenha_a_raiz() {
    use ps_testclient::subsurface::*;
    let rig = rig(1);
    let log = rig.dir.join("sub.log");
    let mut proto = rig.client(true);
    let cliente = spawn_sub(&rig.socket(), Cenario::Remover, &log);
    wait_log(&log, &mut proto, "pronto");
    let win = janela_sub(&mut proto);
    espera_pixel(&mut proto, win, (60, 40), COR_SUB_A);
    wait_log(&log, &mut proto, "removida");
    espera_pixel(&mut proto, win, (60, 40), COR_RAIZ);
    espera_pixel(&mut proto, win, (105, 65), COR_RAIZ);
    assert_eq!(tamanho(&proto, win), RAIZ);
    fim_do_cenario(proto, cliente, rig);
}

#[test]
fn raiz_de_1x1_esticada_pelo_viewport_com_subsuperficie_dmabuf_abgr() {
    // O caso do Waydroid: a raiz é um buffer shm de 1x1 com `set_destination`, o conteúdo vai numa
    // subsuperfície DMABUF ABGR8888.
    if !dmabuf_disponivel() {
        return;
    }
    use ps_testclient::subsurface::*;
    let rig = rig(1);
    let log = rig.dir.join("sub.log");
    let mut proto = rig.client(true);
    let cliente = spawn_sub(&rig.socket(), Cenario::Dmabuf1x1, &log);
    wait_log(&log, &mut proto, "pronto");
    let win = janela_sub(&mut proto);
    espera_padrao_dmabuf(&mut proto, win, 0);
    assert_eq!(tamanho(&proto, win), DMABUF_TAM);
    // A janela nasceu com o tamanho do destino do viewport, nunca com 1x1.
    assert!(proto.log.iter().any(|l| *l == format!("create {win} {}x{}", DMABUF_TAM.0, DMABUF_TAM.1)), "{:?}", proto.log);
    assert!(!proto.log.iter().any(|l| l.contains(" 1x1")), "{:?}", proto.log);
    // A rajada de commits (leituras adiadas pelo intervalo mínimo) termina no último quadro.
    wait_log(&log, &mut proto, "rajada");
    espera_padrao_dmabuf(&mut proto, win, DMABUF_RAJADA);
    fim_do_cenario(proto, cliente, rig);
}

#[test]
fn raiz_esticada_pelo_viewport_e_refeita_quando_so_o_destino_muda() {
    use ps_testclient::subsurface::*;
    let rig = rig(1);
    let log = rig.dir.join("sub.log");
    let mut proto = rig.client(true);
    let cliente = spawn_sub(&rig.socket(), Cenario::RaizEsticada, &log);
    wait_log(&log, &mut proto, "pronto");
    let win = janela_sub(&mut proto);
    espera_pixel(&mut proto, win, (10, 10), COR_ESQUERDA);
    assert_eq!(tamanho(&proto, win), ESTICADA_TAM);
    confere_pixel(&proto, win, (49, 25), COR_ESQUERDA);
    confere_pixel(&proto, win, (50, 25), COR_DIREITA);
    confere_pixel(&proto, win, (99, 49), COR_DIREITA);
    // O app só muda o destino para 50x25, sem buffer novo.
    wait_log(&log, &mut proto, "menor");
    proto.until(10, "WIN_RESIZE para 50x25", |p| p.log.iter().any(|l| l.starts_with("resize") && l.ends_with("50x25")));
    espera_pixel(&mut proto, win, (10, 10), COR_ESQUERDA);
    espera_pixel(&mut proto, win, (30, 10), COR_DIREITA);
    assert_eq!(tamanho(&proto, win), (50, 25));
    fim_do_cenario(proto, cliente, rig);
}

#[test]
fn janela_de_1x1_so_aparece_quando_ganha_tamanho_de_verdade() {
    use ps_testclient::subsurface::*;
    let rig = rig(1);
    let log = rig.dir.join("sub.log");
    let mut proto = rig.client(true);
    let cliente = spawn_sub(&rig.socket(), Cenario::Fantasma, &log);
    // Com 1x1 a janela não é anunciada.
    wait_log(&log, &mut proto, "1x1");
    espera(&mut proto, 400);
    assert!(proto.titles.is_empty() && proto.log.is_empty(), "janela de 1x1 anunciada: {:?}", proto.log);
    // Com tamanho de verdade, aparece.
    wait_log(&log, &mut proto, "real");
    let win = janela_sub(&mut proto);
    espera_pixel(&mut proto, win, (10, 10), COR_RAIZ);
    assert_eq!(tamanho(&proto, win), RAIZ);
    // Depois de anunciada, encolher para 1x1 não faz nada: nem WIN_RESIZE, nem a imagem muda.
    wait_log(&log, &mut proto, "1x1-de-novo");
    espera(&mut proto, 400);
    assert_eq!(tamanho(&proto, win), RAIZ);
    confere_pixel(&proto, win, (10, 10), COR_RAIZ);
    // Voltando ao tamanho de verdade, o conteúdo novo chega.
    wait_log(&log, &mut proto, "real-2");
    espera_pixel(&mut proto, win, (10, 10), COR_RAIZ_2);
    assert!(!proto.log.iter().any(|l| l.contains(" 1x1")), "{:?}", proto.log);
    fim_do_cenario(proto, cliente, rig);
}

// ---------------------------------------------------------------- ponteiro e cursor (marco 5)

#[test]
fn set_cursor_repetido_do_app_chega_ao_hospedeiro_uma_vez_so() {
    let rig = rig(1);
    let log = rig.dir.join("cursor-repete.log");
    // O app repete o `set_cursor` nulo 30 vezes ao entrar (o Android faz isso umas 60 vezes por segundo).
    let cliente = spawn_testclient_with(&rig.socket(), &log, |o| {
        o.cursor = Some(CursorMode::Hidden);
        o.cursor_repeat = 30;
    });
    let mut proto = rig.client_marco3();
    proto.until(10, "WIN_CREATE", |p| !p.titles.is_empty());
    let win = *proto.titles.keys().next().unwrap();
    proto.send(Message::Focus { win, focused: true });
    wait_log_n(&log, &mut proto, "kbd-enter main", 1);
    proto.send(Message::PointerMove { win, x: 30.0, y: 40.0 });
    wait_log_n(&log, &mut proto, "cursor-set hidden", 31);
    espera(&mut proto, 500);
    let ocultos = proto.seen.iter().filter(|m| matches!(m, Message::Cursor { shape, .. } if *shape == CursorShape::Hidden)).count();
    assert_eq!(ocultos, 1, "o mesmo set_cursor repetido deve chegar uma vez só: {:?}", proto.seen);
    fecha_janela(proto, win, cliente, rig);
}

/// Posição `(x, y)` do `ptr-enter` de número `n` (a partir de 1) no registro do cliente de teste.
fn posicao_do_enter(log: &Path, n: usize) -> Option<(String, String)> {
    let texto = read_log(log);
    let l = texto.lines().filter(|l| l.starts_with("ptr-enter ")).nth(n - 1)?;
    let mut it = l.split_whitespace().skip(1);
    Some((it.next()?.to_string(), it.next()?.to_string()))
}

#[test]
fn clique_em_janela_sem_movimento_previo_nao_usa_a_posicao_de_outra_janela() {
    let rig = rig(1);
    let log_a = rig.dir.join("clique-a.log");
    let log_b = rig.dir.join("clique-b.log");
    let ca = spawn_testclient_with(&rig.socket(), &log_a, |o| o.title = "janela-a".into());
    let cb = spawn_testclient_with(&rig.socket(), &log_b, |o| o.title = "janela-b".into());
    let mut proto = rig.client(true);
    proto.until(10, "as duas janelas", |p| p.titles.values().any(|t| t == "janela-a") && p.titles.values().any(|t| t == "janela-b"));
    let id = |p: &Proto, t: &str| *p.titles.iter().find(|(_, x)| x.as_str() == t).unwrap().0;
    let (a, b) = (id(&proto, "janela-a"), id(&proto, "janela-b"));

    // O ponteiro anda só na janela A.
    proto.send(Message::PointerMove { win: a, x: 120.0, y: 60.0 });
    wait_log_n(&log_a, &mut proto, "ptr-enter 120.0 60.0", 1);
    // O `enter` vem seguido de um `motion` na mesma posição (o Android só atualiza o ponteiro em `motion`).
    wait_log_n(&log_a, &mut proto, "motion 120.0 60.0", 1);
    // Clique na B, nova, sem nenhum movimento nela: entra no canto (0, 0), nunca em (120, 60) da A, e o app
    // recebe `enter`, o movimento na mesma posição e só então o botão.
    proto.send(Message::PointerButton { win: b, button: 0x110, pressed: true });
    wait_log_n(&log_b, &mut proto, "button 272 pressed", 1);
    assert_eq!(posicao_do_enter(&log_b, 1), Some(("0.0".into(), "0.0".into())), "{}", read_log(&log_b));
    let t = read_log(&log_b);
    let (e, m, bt) = (t.find("ptr-enter 0.0 0.0").unwrap(), t.find("motion 0.0 0.0").expect("motion depois do enter"), t.find("button 272 pressed").unwrap());
    assert!(e < m && m < bt, "ordem enter, motion, button: {t}");
    proto.send(Message::PointerButton { win: b, button: 0x110, pressed: false });
    wait_log_n(&log_b, &mut proto, "button 272 released", 1);

    // Agora a B tem uma posição conhecida; o ponteiro vai para a A e volta por um botão, sem movimento:
    // a B entra na posição dela (30, 40), não na (120, 60) do último movimento da A.
    proto.send(Message::PointerMove { win: b, x: 30.0, y: 40.0 });
    wait_log_n(&log_b, &mut proto, "motion 30.0 40.0", 1);
    proto.send(Message::PointerMove { win: a, x: 120.0, y: 60.0 });
    wait_log_n(&log_a, &mut proto, "ptr-enter 120.0 60.0", 2);
    proto.send(Message::PointerButton { win: b, button: 0x110, pressed: true });
    wait_log_n(&log_b, &mut proto, "button 272 pressed", 2);
    let enters_b = ocorrencias(&log_b, "ptr-enter ");
    assert_eq!(posicao_do_enter(&log_b, enters_b), Some(("30.0".into(), "40.0".into())), "{}", read_log(&log_b));
    proto.send(Message::PointerButton { win: b, button: 0x110, pressed: false });

    proto.send(Message::CloseReq { win: a });
    proto.send(Message::CloseReq { win: b });
    assert_eq!(ca.join().unwrap(), Ok(()));
    assert_eq!(cb.join().unwrap(), Ok(()));
    proto.drop_connection();
    rig.finish();
}
