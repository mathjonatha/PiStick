//! Teste de integração sem interface gráfica: sobe o servidor sintético numa porta livre
//! e conecta um cliente "sem janela" (só protocolo + Decoder).

use std::collections::HashMap;
use std::io::Write;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use ps_codec::testutil::normalize_x;
use ps_codec::Decoder;
use ps_proto::{read_hello, read_message, write_message, Message, ProtoError};
use ps_server::{serve_loop, ServerOptions};
use ps_transport::{Closer, Split, TcpAcceptor, TcpTransport, Transport};

type Pixels = Arc<Mutex<HashMap<(u32, u32), (Vec<u8>, std::ops::Range<usize>)>>>;
type Inputs = Arc<Mutex<Vec<Message>>>;

struct Harness {
    addr: std::net::SocketAddr,
    pixels: Pixels,
    inputs: Inputs,
    server: Option<JoinHandle<()>>,
}

impl Harness {
    fn start(max_sessions: usize) -> Harness {
        Self::start_with(max_sessions, |_| {})
    }

    fn start_with(max_sessions: usize, ajusta: impl FnOnce(&mut ServerOptions)) -> Harness {
        let acceptor = TcpAcceptor::bind("127.0.0.1:0").unwrap();
        let addr = acceptor.local_addr().unwrap();
        let pixels: Pixels = Arc::default();
        let inputs: Inputs = Arc::default();
        let (p, i) = (pixels.clone(), inputs.clone());
        let mut opts = ServerOptions {
            fps: 60,
            hello_timeout: Duration::from_millis(400),
            quiet: std::env::var("PS_LOG").is_err(),
            frame_hook: Some(Arc::new(move |f| {
                p.lock().unwrap().insert((f.win, f.seq), (f.pixels.to_vec(), f.strips.clone()));
            })),
            input_hook: Some(Arc::new(move |m| i.lock().unwrap().push(m.clone()))),
            ..ServerOptions::default()
        };
        ajusta(&mut opts);
        let server = std::thread::spawn(move || {
            serve_loop(&acceptor, &opts, Some(max_sessions)).unwrap();
        });
        Harness { addr, pixels, inputs, server: Some(server) }
    }

    fn client(&self) -> TestClient {
        TestClient::connect(self.addr, self.pixels.clone())
    }

    fn finish(mut self) {
        // Todas as sessões já terminaram quando o chamador soltou os clientes.
        self.server.take().unwrap().join().unwrap();
    }
}

struct TestClient {
    rx: Receiver<Result<Message, ProtoError>>,
    writer: std::net::TcpStream,
    closer: Closer,
    pixels: Pixels,
    decoders: HashMap<u32, Decoder>,
    titles: HashMap<u32, String>,
    last_seq: HashMap<u32, u32>,
    /// quadros decodificados e verificados, por janela
    frames: HashMap<u32, Vec<FrameInfo>>,
    auto_ack: bool,
}

#[derive(Clone, Copy)]
struct FrameInfo {
    seq: u32,
    /// faixas de blocos que o quadro cobre (0..n = inteiro)
    strips: (usize, usize),
    n_strips: usize,
    bytes: usize,
    width: u32,
    height: u32,
}

impl TestClient {
    fn connect(addr: std::net::SocketAddr, pixels: Pixels) -> TestClient {
        let Split { mut reader, mut writer, closer } = TcpTransport::connect(addr).unwrap().split().unwrap();
        write_message(&mut writer, &Message::hello("cliente de teste")).unwrap();
        let (caps, name) = read_hello(&mut reader).expect("HELLO do servidor");
        assert_eq!(caps & ps_proto::CAP_BLOCK_CODEC, ps_proto::CAP_BLOCK_CODEC);
        assert!(name.starts_with("psd"), "nome: {name}");
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || loop {
            let r = read_message(&mut reader);
            let fim = r.is_err();
            if tx.send(r).is_err() || fim {
                break;
            }
        });
        TestClient {
            rx,
            writer,
            closer,
            pixels,
            decoders: HashMap::new(),
            titles: HashMap::new(),
            last_seq: HashMap::new(),
            frames: HashMap::new(),
            auto_ack: true,
        }
    }

    fn send(&mut self, m: Message) {
        write_message(&mut self.writer, &m).unwrap();
    }

    /// Próxima mensagem já processada (quadros decodificados e conferidos), ou `None` no tempo limite.
    fn next(&mut self, dur: Duration) -> Option<Message> {
        match self.rx.recv_timeout(dur) {
            Ok(Ok(m)) => {
                self.process(&m);
                Some(m)
            }
            Ok(Err(e)) => panic!("erro de leitura no cliente de teste: {e}"),
            Err(RecvTimeoutError::Timeout) => None,
            Err(RecvTimeoutError::Disconnected) => panic!("leitor do cliente terminou"),
        }
    }

    fn process(&mut self, m: &Message) {
        match m {
            Message::WinCreate { win, width, height, title, .. } => {
                self.decoders.insert(*win, Decoder::new(*width, *height));
                self.titles.insert(*win, title.clone());
            }
            Message::WinResize { win, width, height } => {
                self.decoders.get_mut(win).expect("janela existe").reset(*width, *height);
            }
            Message::WinDestroy { win } => {
                self.decoders.remove(win);
            }
            Message::Frame { win, seq, width, height, data, .. } => {
                let dec = self.decoders.get_mut(win).expect("quadro de janela desconhecida");
                assert_eq!((dec.width(), dec.height()), (*width, *height), "tamanho do quadro difere do da janela");
                if let Some(prev) = self.last_seq.insert(*win, *seq) {
                    assert_eq!(*seq, prev + 1, "seq deve crescer de 1 em 1 por janela");
                }
                dec.decode(data).expect("decodificação do quadro");
                let (esperado, strips) = self.pixels.lock().unwrap().get(&(*win, *seq)).cloned().expect("servidor registrou o quadro");
                let mut esperado = esperado;
                normalize_x(&mut esperado);
                // Um quadro fatiado só atualiza as faixas que cobre: compara só essas linhas.
                let n_strips = (*height as usize).div_ceil(64);
                let (y0, y1) = (strips.start * 64, (strips.end * 64).min(*height as usize));
                let row = *width as usize * 4;
                assert_eq!(
                    &dec.pixels()[y0 * row..y1 * row],
                    &esperado[y0 * row..y1 * row],
                    "pixels da janela {win} seq {seq} (faixas {strips:?}) diferem do que o servidor gerou"
                );
                self.frames.entry(*win).or_default().push(FrameInfo { seq: *seq, strips: (strips.start, strips.end), n_strips, bytes: data.len(), width: *width, height: *height });
                if self.auto_ack {
                    self.send(Message::FrameAck { win: *win, seq: *seq });
                }
            }
            _ => {}
        }
    }

    /// Processa mensagens até `cond` ser verdadeira ou estourar o tempo.
    fn until(&mut self, secs: u64, mut cond: impl FnMut(&TestClient) -> bool) {
        let limite = Instant::now() + Duration::from_secs(secs);
        while !cond(self) {
            assert!(Instant::now() < limite, "tempo esgotado esperando a condição");
            self.next(Duration::from_millis(50));
        }
    }

    fn n_frames(&self, win: u32) -> usize {
        self.frames.get(&win).map_or(0, Vec::len)
    }

    fn drop_connection(self) {
        self.closer.close();
    }
}

#[test]
fn quadros_decodificados_batem_com_o_que_o_servidor_gerou() {
    let h = Harness::start(1);
    let mut c = h.client();
    c.until(10, |c| c.n_frames(1) >= 8 && c.n_frames(2) >= 8);

    assert_eq!(c.titles.len(), 2);
    assert!(c.titles[&1].contains("interface"));
    assert!(c.titles[&2].contains("degradê"));
    assert_eq!(c.decoders[&1].width(), 640);

    // O primeiro quadro é inteiro; os da interface seguintes devem ser muito menores (dano parcial).
    let ui = &c.frames[&1];
    let menor = ui.iter().skip(1).map(|f| f.bytes).min().unwrap();
    assert!(menor * 10 < ui[0].bytes, "quadro de interface com pouco dano deveria ser <10% do inteiro ({menor} vs {})", ui[0].bytes);
    // O degradê muda tudo a cada quadro: quadros grandes.
    let gr = &c.frames[&2];
    assert!(gr.iter().skip(1).all(|f| f.bytes > 1000));

    c.drop_connection();
    h.finish();
}

#[test]
fn controle_de_fluxo_dois_quadros_sem_confirmacao_e_dano_acumulado() {
    // Orçamento de bytes enorme e fixo: aqui só vale o limite de 2 quadros por janela.
    let h = Harness::start_with(1, |o| {
        o.flow_initial_throughput = 1e10;
        o.flow_adaptive = false;
    });
    let mut c = h.client();
    c.auto_ack = false;

    // Sem confirmar nada: exatamente 2 quadros por janela e depois silêncio.
    c.until(10, |c| c.n_frames(1) >= 2 && c.n_frames(2) >= 2);
    let t_fim = Instant::now() + Duration::from_millis(600);
    while Instant::now() < t_fim {
        c.next(Duration::from_millis(50));
    }
    assert_eq!(c.n_frames(1), 2, "janela 1 não pode passar de 2 quadros sem confirmação");
    assert_eq!(c.n_frames(2), 2, "janela 2 não pode passar de 2 quadros sem confirmação");

    // Uma confirmação libera exatamente mais um quadro, com o dano acumulado e o conteúdo mais recente.
    c.send(Message::FrameAck { win: 1, seq: 0 });
    c.until(5, |c| c.n_frames(1) == 3);
    let t_fim = Instant::now() + Duration::from_millis(400);
    while Instant::now() < t_fim {
        c.next(Duration::from_millis(50));
    }
    assert_eq!(c.n_frames(1), 3, "uma confirmação libera um único quadro");
    assert_eq!(c.n_frames(2), 2, "a janela 2 continua bloqueada");
    // `process` já comparou os pixels do 3º quadro com o que o servidor tinha naquele instante.

    c.drop_connection();
    h.finish();
}

#[test]
fn ping_pong_e_kind_desconhecido_pulado() {
    let h = Harness::start(1);
    let mut c = h.client();
    // Mensagem de kind desconhecido enviada pelo cliente: o servidor deve pulá-la.
    let mut cru = Vec::new();
    cru.extend_from_slice(&5u32.to_le_bytes());
    cru.extend_from_slice(&999u16.to_le_bytes());
    cru.extend_from_slice(&0u16.to_le_bytes());
    cru.extend_from_slice(&[1, 2, 3, 4, 5]);
    c.writer.write_all(&cru).unwrap();
    c.send(Message::Ping { value: 0xDEAD_BEEF_1234 });
    let mut pong = None;
    let limite = Instant::now() + Duration::from_secs(5);
    while pong.is_none() && Instant::now() < limite {
        if let Some(Message::Pong { echo }) = c.next(Duration::from_millis(50)) {
            pong = Some(echo);
        }
    }
    assert_eq!(pong, Some(0xDEAD_BEEF_1234));
    c.drop_connection();
    h.finish();
}

#[test]
fn redimensionar_muda_o_tamanho_do_conteudo() {
    let h = Harness::start(1);
    let mut c = h.client();
    c.until(10, |c| c.n_frames(1) >= 2);

    c.send(Message::ResizeReq { win: 1, width: 320, height: 240 });
    let mut visto = false;
    let limite = Instant::now() + Duration::from_secs(5);
    while !visto && Instant::now() < limite {
        if let Some(Message::WinResize { win: 1, width, height }) = c.next(Duration::from_millis(50)) {
            assert_eq!((width, height), (320, 240));
            visto = true;
        }
    }
    assert!(visto, "WIN_RESIZE não chegou");
    let antes = c.n_frames(1);
    c.until(10, |c| c.n_frames(1) >= antes + 3);
    // Depois do WIN_RESIZE todos os quadros têm o novo tamanho (o `process` já exige que
    // combine com o decodificador) e o primeiro não tem SKIP: decodificou num Decoder zerado.
    let novos = &c.frames[&1][antes..];
    assert!(novos.iter().all(|f| (f.width, f.height) == (320, 240)));
    // A primeira FRAME depois do WIN_RESIZE cobre o quadro inteiro (nunca é fatiada).
    assert_eq!(novos[0].strips, (0, novos[0].n_strips));
    assert_eq!(c.decoders[&1].pixels().len(), 320 * 240 * 4);

    // Tamanho absurdamente pequeno é limitado ao mínimo.
    c.send(Message::ResizeReq { win: 1, width: 10, height: 10 });
    let mut minimo = false;
    let limite = Instant::now() + Duration::from_secs(5);
    while !minimo && Instant::now() < limite {
        if let Some(Message::WinResize { win: 1, width, height }) = c.next(Duration::from_millis(50)) {
            assert_eq!((width, height), (64, 64));
            minimo = true;
        }
    }
    assert!(minimo);
    c.drop_connection();
    h.finish();
}

#[test]
fn fechar_janela_envia_win_destroy_e_para_os_quadros() {
    let h = Harness::start(1);
    let mut c = h.client();
    c.until(10, |c| c.n_frames(2) >= 2);
    c.send(Message::CloseReq { win: 2 });
    let mut destruida = false;
    let limite = Instant::now() + Duration::from_secs(5);
    while !destruida && Instant::now() < limite {
        if let Some(Message::WinDestroy { win }) = c.next(Duration::from_millis(50)) {
            assert_eq!(win, 2);
            destruida = true;
        }
    }
    assert!(destruida, "WIN_DESTROY não chegou");
    // Depois do WIN_DESTROY nenhum quadro da janela 2 (o `process` falharia com "janela desconhecida").
    let n1 = c.n_frames(1);
    c.until(10, |c| c.n_frames(1) >= n1 + 3);
    c.drop_connection();
    h.finish();
}

#[test]
fn eventos_de_entrada_chegam_ao_servidor() {
    let h = Harness::start(1);
    let mut c = h.client();
    c.until(10, |c| c.n_frames(1) >= 1);
    let enviados = vec![
        Message::PointerMove { win: 1, x: 10.5, y: 20.25 },
        Message::PointerButton { win: 1, button: 0x110, pressed: true },
        Message::PointerButton { win: 1, button: 0x110, pressed: false },
        Message::PointerAxis { win: 1, dx: 0.0, dy: -15.0 },
        Message::Key { win: 1, keycode: 30, pressed: true },
        Message::Key { win: 1, keycode: 30, pressed: false },
        Message::Focus { win: 1, focused: true },
    ];
    for m in &enviados {
        c.send(m.clone());
    }
    let limite = Instant::now() + Duration::from_secs(5);
    loop {
        c.next(Duration::from_millis(20));
        let recebidos = h.inputs.lock().unwrap().clone();
        if enviados.iter().all(|m| recebidos.contains(m)) {
            break;
        }
        assert!(Instant::now() < limite, "entradas não chegaram: {recebidos:?}");
    }
    c.drop_connection();
    h.finish();
}

#[test]
fn reconexao_volta_a_escutar_com_quadro_inteiro_novo() {
    let h = Harness::start(3);

    // Sessão 1: cai abruptamente no meio do fluxo.
    let mut c1 = h.client();
    c1.until(10, |c| c.n_frames(1) >= 3 && c.n_frames(2) >= 3);
    c1.drop_connection();

    // Sessão 2: HELLO com versão errada é recusado e o servidor continua escutando.
    {
        let Split { mut reader, mut writer, .. } = TcpTransport::connect(h.addr).unwrap().split().unwrap();
        write_message(&mut writer, &Message::Hello { version: 99, caps: 0, name: "ruim".into() }).unwrap();
        // O servidor fecha: a leitura termina com Closed ou erro de E/S.
        assert!(read_message(&mut reader).is_err());
    }

    // Sessão 3: tudo recomeça do zero (seq 0, quadro inteiro) e os pixels conferem.
    let mut c2 = h.client();
    c2.until(10, |c| c.n_frames(1) >= 3 && c.n_frames(2) >= 3);
    assert_eq!(c2.frames[&1][0].seq, 0);
    assert_eq!(c2.titles.len(), 2);
    c2.drop_connection();
    h.finish();
}

#[test]
fn cliente_mudo_nao_impede_o_proximo_depois_do_prazo() {
    let h = Harness::start(2);
    // Conecta e não diz nada.
    let mudo = std::net::TcpStream::connect(h.addr).unwrap();
    let t0 = Instant::now();
    // O segundo cliente entra na fila do accept e só é atendido depois do prazo (400 ms).
    let mut c = h.client();
    assert!(t0.elapsed() >= Duration::from_millis(300), "atendido cedo demais: {:?}", t0.elapsed());
    c.until(10, |c| c.n_frames(1) >= 2);
    drop(mudo);
    c.drop_connection();
    h.finish();
}

#[test]
fn quadro_grande_e_fatiado_e_cada_fatia_confere() {
    // Vazão fixa e baixa: orçamento de 64 KiB; o degradê 1280x800 (centenas de KiB) tem de sair em fatias.
    let h = Harness::start_with(1, |o| {
        o.flow_initial_throughput = 100.0 * 1024.0;
        o.flow_adaptive = false;
        o.gradient_size = (1280, 800);
    });
    let mut c = h.client();
    c.until(20, |c| c.n_frames(2) >= 30 && c.n_frames(1) >= 5);
    let g = &c.frames[&2];
    // A primeira nunca é fatiada; depois aparecem fatias (menos que o quadro inteiro).
    assert_eq!(g[0].strips, (0, g[0].n_strips));
    let fatias = g.iter().skip(1).filter(|f| f.strips != (0, f.n_strips)).count();
    assert!(fatias >= 10, "esperava fatias, vi {fatias} de {}", g.len());
    // Todas as faixas acabam recebendo vez (nenhuma morre de fome).
    let mut cobertas = vec![false; g[0].n_strips];
    for f in g.iter().skip(1) {
        cobertas[f.strips.0..f.strips.1].fill(true);
    }
    assert!(cobertas.iter().all(|&x| x), "faixas sem vez: {cobertas:?}");
    c.drop_connection();
    h.finish();
}

#[test]
fn bytes_em_voo_respeitam_o_orcamento_sem_confirmacoes() {
    let h = Harness::start_with(1, |o| {
        o.flow_initial_throughput = 100.0 * 1024.0; // orçamento mínimo: 64 KiB
        o.flow_adaptive = false;
        o.gradient_size = (1280, 800);
    });
    let mut c = h.client();
    c.auto_ack = false;
    let t_fim = Instant::now() + Duration::from_millis(1200);
    while Instant::now() < t_fim {
        c.next(Duration::from_millis(50));
    }
    let orcamento = ps_server::flow::MIN_BUDGET;
    let (mut total, mut maior) = (0usize, 0usize);
    for f in c.frames.values().flatten() {
        total += f.bytes;
        maior = maior.max(f.bytes);
    }
    // Sem confirmação nada sai além do orçamento, salvo a exceção de progresso de UM quadro.
    assert!(total <= orcamento + maior, "em voo {total} B > orçamento {orcamento} + maior quadro {maior}");
    assert!(c.n_frames(1) + c.n_frames(2) >= 1);
    c.drop_connection();
    h.finish();
}

#[test]
fn janela_com_foco_e_atendida_primeiro() {
    // Orçamento minúsculo e fixo; a janela de interface recebe o foco e continua fluindo
    // enquanto o degradê pesado divide a conexão.
    let h = Harness::start_with(1, |o| {
        o.flow_initial_throughput = 100.0 * 1024.0;
        o.flow_adaptive = false;
        o.gradient_size = (1280, 800);
    });
    let mut c = h.client();
    c.send(Message::Focus { win: 1, focused: true });
    let t_fim = Instant::now() + Duration::from_millis(1500);
    while Instant::now() < t_fim {
        c.next(Duration::from_millis(5));
    }
    let (ui, gr) = (c.n_frames(1), c.n_frames(2));
    assert!(ui >= 10, "interface com foco deveria fluir, vi {ui} quadros (degradê: {gr})");
    c.drop_connection();
    h.finish();
}

// ---------------------------------------------------------------- apresentação atômica (docs/20)

/// Resultado de ler `quadros` quadros fatiados da janela de listras.
struct Listras {
    /// (fatias do quadro, número do quadro nos marcadores ao fim, imagem igual ao gerador para esse número)
    grupos: Vec<(usize, u32, bool)>,
    /// Alguma fatia com `MORE` apareceu.
    viu_more: bool,
}

/// Conecta como cliente (`atomic` = anuncia `CAP_ATOMIC`), confirma cada `FRAME` na hora e agrupa as fatias
/// pelo bit `MORE`. Sem `MORE` (cliente sem a capacidade) cada `FRAME` é um grupo de uma fatia.
fn le_listras(h: &Harness, atomic: bool, quadros: usize) -> Listras {
    use ps_codec::testutil::stripes;
    let Split { mut reader, mut writer, closer } = TcpTransport::connect(h.addr).unwrap().split().unwrap();
    let caps = ps_proto::CAP_BLOCK_CODEC | if atomic { ps_proto::CAP_ATOMIC } else { 0 };
    write_message(&mut writer, &Message::hello_with_caps("cliente de listras", caps)).unwrap();
    let (server_caps, _) = read_hello(&mut reader).expect("HELLO do servidor");
    assert_eq!(server_caps & ps_proto::CAP_ATOMIC, ps_proto::CAP_ATOMIC, "o servidor sempre anuncia CAP_ATOMIC");
    let mut dec = Decoder::new(stripes::WIDTH, stripes::HEIGHT);
    let mut r = Listras { grupos: Vec::new(), viu_more: false };
    let mut fatias = 0usize;
    let mut ultimo_seq = None::<u32>;
    let fim = Instant::now() + Duration::from_secs(60);
    while r.grupos.len() < quadros {
        assert!(Instant::now() < fim, "tempo esgotado: {} grupos", r.grupos.len());
        match read_message(&mut reader).expect("leitura") {
            Message::WinCreate { width, height, .. } => assert_eq!((width, height), (stripes::WIDTH, stripes::HEIGHT)),
            Message::Frame { win, seq, data, more, .. } => {
                if let Some(a) = ultimo_seq {
                    assert_eq!(seq, a + 1);
                }
                ultimo_seq = Some(seq);
                dec.decode(&data).expect("decodificação");
                write_message(&mut writer, &Message::FrameAck { win, seq }).unwrap();
                if !atomic {
                    assert!(!more, "sem CAP_ATOMIC nenhuma fatia leva MORE");
                }
                r.viu_more |= more;
                fatias += 1;
                if !more {
                    let marcas = stripes::read_markers(dec.pixels(), stripes::WIDTH, stripes::HEIGHT);
                    let n = marcas[0];
                    let mut esperado = vec![0u8; (stripes::WIDTH * stripes::HEIGHT * 4) as usize];
                    stripes::render_into(&mut esperado, stripes::WIDTH, stripes::HEIGHT, n);
                    let igual = marcas.iter().all(|&m| m == n) && dec.pixels() == esperado.as_slice();
                    r.grupos.push((fatias, n, igual));
                    fatias = 0;
                }
            }
            _ => {}
        }
    }
    closer.close();
    r
}

fn servidor_de_listras(max_sessions: usize) -> Harness {
    // Orçamento fixo de 64 KiB: o quadro de ruído (1,7 MB) sai em dezenas de fatias de 32 KiB.
    Harness::start_with(max_sessions, |o| {
        o.synthetic_stripes = true;
        o.flow_initial_throughput = 100.0 * 1024.0;
        o.flow_adaptive = false;
    })
}

#[test]
fn quadro_fatiado_chega_com_more_e_todas_as_fatias_vem_do_mesmo_instante() {
    let h = servidor_de_listras(1);
    let r = le_listras(&h, true, 6);
    assert!(r.viu_more, "o quadro de ruído tem de ser fatiado e as fatias levam MORE");
    // O primeiro quadro (inteiro, sem SKIP) não é fatiado; os demais têm várias fatias.
    assert_eq!(r.grupos[0].0, 1, "o primeiro quadro não pode ser fatiado");
    assert!(r.grupos.iter().skip(1).all(|g| g.0 > 5), "esperava dezenas de fatias por quadro: {:?}", r.grupos);
    // Ao fim de cada quadro (a última fatia, sem MORE) a imagem é exatamente o quadro do gerador de um só instante:
    // todos os marcadores iguais e os pixels de ruído idênticos ao que o gerador desenha para esse número.
    assert!(r.grupos.iter().all(|g| g.2), "algum quadro fatiado saiu de instantes diferentes: {:?}", r.grupos);
    // Os quadros avançam (o dano novo vira o quadro seguinte), nunca voltam.
    assert!(r.grupos.windows(2).all(|p| p[1].1 > p[0].1), "números de quadro: {:?}", r.grupos);
    h.finish();
}

#[test]
fn sem_cap_atomic_nada_muda_no_fio_mas_o_quadro_ainda_sai_de_um_instante() {
    let h = servidor_de_listras(1);
    // O cliente sem a capacidade nunca vê MORE (a asserção está em `le_listras`); cada FRAME é um grupo.
    let r = le_listras(&h, false, 12);
    assert!(!r.viu_more);
    // Cada fatia, isolada, vem do instantâneo do seu quadro: depois de aplicadas todas as fatias de um quadro
    // a imagem é um quadro inteiro; como o cliente sem a capacidade não sabe onde o quadro acaba, aqui só
    // conferimos que o fio é idêntico ao de antes (FRAME sem flags) e que há fatias.
    assert!(r.grupos.len() >= 12);
    h.finish();
}
