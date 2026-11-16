//! A sessão do cliente Android, sem aparelho: primeiro com eventos de rede fabricados (janelas, foco, popups, entrada,
//! área de transferência) e depois contra um `psd` de verdade no laço local (conexão cifrada, chave nova, pareamento).
//! A superfície é uma tela falsa que guarda os quadros recebidos.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use ps_android::eventos::{decodificar, fase, Evento, Fila, Segredo};
use ps_android::gestos::{Acao, Ponteiro, Toque};
use ps_android::layout::Tela as TelaInfo;
use ps_android::sessao::{Config, Controle, Sessao};
use ps_android::teclado::{Mod, Passo};
use ps_android::tela::Tela;
use ps_client::net::{SharedWindow, UiSink, UserEvent};
use ps_codec::Encoder;
use ps_proto::{CursorShape, Message};
use ps_secure::chaves::{format_code, PeerList};
use ps_secure::Keypair;
use ps_server::secure::{serve_secure, PairWait, Security, Timing};
use ps_server::{synthetic, ServerOptions, WindowSource};
use ps_transport::TcpAcceptor;

const CELULAR: TelaInfo = TelaInfo { largura: 1080, altura: 2400, dpi: 440 };

type Superficie = Arc<Mutex<Option<Box<dyn Tela>>>>;
type Quadros = Arc<Mutex<Vec<(u32, u32, Vec<u8>)>>>;

/// A tela falsa: guarda o tamanho e os pixels de cada quadro publicado.
struct Falsa(Quadros);

impl Tela for Falsa {
    fn publicar(&mut self, largura: u32, altura: u32, rgba: &[u8]) -> Result<(), String> {
        assert_eq!(rgba.len(), (largura * altura * 4) as usize, "o buffer tem de ser contíguo");
        self.0.lock().unwrap().push((largura, altura, rgba.to_vec()));
        Ok(())
    }
}

fn tela_falsa() -> (Superficie, Quadros) {
    let q: Quadros = Arc::default();
    (Arc::new(Mutex::new(Some(Box::new(Falsa(q.clone()))))), q)
}

/// Janela remota com o quadro todo da cor `v` (B = G = R = v) e um número de quadro pendente de confirmação.
fn janela(w: u32, h: u32, v: u8, seq: u32) -> Arc<SharedWindow> {
    let sh = SharedWindow::new(w, h);
    pintar(&sh, w, h, v, seq);
    sh
}

fn pintar(sh: &SharedWindow, w: u32, h: u32, v: u8, seq: u32) {
    let mut px = vec![v; (w * h * 4) as usize];
    px.as_chunks_mut::<4>().0.iter_mut().for_each(|p| p[3] = 0xFF);
    let mut o = Vec::new();
    Encoder::new(w, h).encode(&px, (w * 4) as usize, None, &mut o);
    let mut g = sh.inner.lock().unwrap();
    g.decoder.decode(&o).unwrap();
    g.acks.push(seq);
}

struct Banco {
    s: Arc<Sessao>,
    out: Receiver<Message>,
    entrada: Receiver<Vec<Passo>>,
    fila: Arc<Fila>,
    quadros: Quadros,
}

/// Os testes de conta usam o fator 2 explícito (540x1200 no celular de teste); o padrão tem teste próprio.
fn banco() -> Banco {
    banco_com_fator(2.0)
}

fn banco_com_fator(fator: f32) -> Banco {
    let fila = Arc::new(Fila::new());
    let (out_tx, out) = mpsc::channel();
    let (superficie, quadros) = tela_falsa();
    let (s, entrada) = Sessao::nova_com_fator(fila.clone(), out_tx, superficie, CELULAR, fator, None, None);
    Banco { s, out, entrada, fila, quadros }
}

impl Banco {
    fn ev(&self, e: UserEvent) {
        assert!(self.s.send(e));
    }

    fn saida(&self) -> Vec<Message> {
        self.out.try_iter().collect()
    }

    /// As mensagens da fila de entrada (a thread `psa-entrada` as mandaria depois das pausas).
    fn entrada_msgs(&self) -> Vec<Message> {
        self.entrada
            .try_iter()
            .flatten()
            .filter_map(|p| match p {
                Passo::Msg(m) => Some(m),
                Passo::PausaMs(_) => None,
            })
            .collect()
    }

    fn criar(&self, win: u32, w: u32, h: u32, titulo: &str, v: u8) -> Arc<SharedWindow> {
        let sh = janela(w, h, v, 100 + win);
        self.ev(UserEvent::WinCreate { win, width: w, height: h, title: titulo.into(), shared: sh.clone() });
        sh
    }
}

fn toque(acao: Acao, indice: usize, ps: &[(i32, f32, f32)], t_ms: u64) -> Toque {
    Toque { acao, indice, ponteiros: ps.iter().map(|&(id, x, y)| Ponteiro { id, x, y }).collect(), t_ms }
}

fn pixel(q: &(u32, u32, Vec<u8>), x: u32, y: u32) -> [u8; 4] {
    let i = ((y * q.0 + x) * 4) as usize;
    [q.2[i], q.2[i + 1], q.2[i + 2], q.2[i + 3]]
}

// ---------------------------------------------------------------------------------------------------------------
// Eventos de rede fabricados
// ---------------------------------------------------------------------------------------------------------------

#[test]
fn janela_nova_ganha_foco_pede_tamanho_desenha_e_confirma_o_quadro() {
    let b = banco();
    b.criar(1, 960, 600, "foot", 90);
    let saida = b.saida();
    assert!(saida.contains(&Message::Focus { win: 1, focused: true }), "{saida:?}");
    assert!(saida.contains(&Message::ResizeReq { win: 1, width: 540, height: 1200 }), "o tamanho do celular de teste: {saida:?}");
    assert!(saida.contains(&Message::FrameAck { win: 1, seq: 101 }), "o quadro apresentado é confirmado: {saida:?}");
    // A fila de eventos tem a lista de janelas com a aba e o foco.
    let ev = b.fila.drenar();
    assert!(ev.contains(&Evento::Janelas { foco: 1, abas: vec![(1, "foot".into())] }), "{ev:?}");
    // O que foi desenhado: buffer do tamanho da região visível (540x1200), cinza onde a janela (960x600) não cobre.
    let q = b.quadros.lock().unwrap();
    let ultimo = q.last().expect("desenhou");
    assert_eq!((ultimo.0, ultimo.1), (540, 1200));
    // A janela (960x600) é mais baixa que a região (540x1200): fica centrada na vertical (começa em y = 300).
    assert_eq!(pixel(ultimo, 10, 10), [48, 48, 48, 255], "acima da janela: fundo cinza");
    assert_eq!(pixel(ultimo, 10, 310), [90, 90, 90, 255], "o canto da janela");
}

#[test]
fn nova_janela_de_dialogo_ganha_o_foco_e_ao_fechar_o_foco_volta() {
    let b = banco();
    b.criar(1, 540, 1200, "editor", 50);
    b.saida();
    b.fila.drenar();
    b.criar(2, 400, 300, "Salvar como", 200);
    let saida = b.saida();
    assert!(saida.contains(&Message::Focus { win: 1, focused: false }) && saida.contains(&Message::Focus { win: 2, focused: true }), "{saida:?}");
    assert!(saida.contains(&Message::ResizeReq { win: 2, width: 540, height: 1200 }));
    let ev = b.fila.drenar();
    assert!(ev.contains(&Evento::Janelas { foco: 2, abas: vec![(1, "editor".into()), (2, "Salvar como".into())] }));
    // A janela de fundo recebe quadro: é confirmado na hora, sem desenhar.
    let n = b.quadros.lock().unwrap().len();
    let fundo = b.entrada_vazia_da_janela(1);
    b.ev(UserEvent::Frame { win: 1, shared: fundo });
    assert_eq!(b.quadros.lock().unwrap().len(), n, "janela escondida não é desenhada");
    assert!(b.saida().contains(&Message::FrameAck { win: 1, seq: 555 }));
    // Fechar o diálogo devolve o foco ao editor.
    b.ev(UserEvent::WinDestroy { win: 2 });
    let saida = b.saida();
    assert!(saida.contains(&Message::Focus { win: 2, focused: false }) && saida.contains(&Message::Focus { win: 1, focused: true }), "{saida:?}");
    let ev = b.fila.drenar();
    assert!(ev.contains(&Evento::Janelas { foco: 1, abas: vec![(1, "editor".into())] }), "{ev:?}");
    // Esc depois de fechar vai para o editor.
    b.s.tecla_extra(1);
    assert!(b.entrada_msgs().iter().all(|m| matches!(m, Message::Key { win: 1, .. })));
}

impl Banco {
    /// Uma janela cujo único quadro pendente tem `seq` 555.
    fn entrada_vazia_da_janela(&self, _win: u32) -> Arc<SharedWindow> {
        janela(8, 8, 1, 555)
    }
}

#[test]
fn tocar_numa_aba_troca_a_janela_mostrada_e_avisa_o_servidor() {
    let b = banco();
    b.criar(1, 540, 1200, "a", 40);
    b.criar(2, 540, 1200, "b", 220);
    b.saida();
    b.s.focar_aba(1);
    let saida = b.saida();
    assert!(saida.contains(&Message::Focus { win: 2, focused: false }) && saida.contains(&Message::Focus { win: 1, focused: true }), "{saida:?}");
    let q = b.quadros.lock().unwrap();
    assert_eq!(pixel(q.last().unwrap(), 100, 100), [40, 40, 40, 255], "agora a janela 1 aparece");
    drop(q);
    // Aba que não existe: nada muda.
    b.fila.drenar();
    b.s.focar_aba(99);
    assert!(b.saida().is_empty() && b.fila.is_empty());
}

#[test]
fn popup_e_composto_sobre_a_janela_na_posicao_certa_e_recebe_os_toques() {
    let b = banco();
    b.criar(1, 540, 1200, "mousepad", 50);
    let menu = janela(100, 200, 250, 300);
    b.ev(UserEvent::PopupCreate { win: 5, parent: 1, x: 20, y: 30, width: 100, height: 200, grab: true, shared: menu.clone() });
    b.saida();
    b.ev(UserEvent::Frame { win: 5, shared: menu });
    assert!(b.saida().contains(&Message::FrameAck { win: 5, seq: 300 }));
    {
        let q = b.quadros.lock().unwrap();
        let u = q.last().unwrap();
        assert_eq!(pixel(u, 40, 80), [250, 250, 250, 255], "dentro do popup (20..120 x 30..230, em pixels remotos)");
        assert_eq!(pixel(u, 10, 10), [50, 50, 50, 255], "fora do popup");
    }
    // Toque curto sobre o popup: vai ao popup, com coordenadas locais (remoto = tela / 2).
    // Tela (100, 140) = remoto (50, 70) = popup local (30, 40).
    b.s.toque(&toque(Acao::Baixo, 0, &[(0, 100.0, 140.0)], 0));
    b.s.toque(&toque(Acao::Cima, 0, &[(0, 100.0, 140.0)], 60));
    let m = b.entrada_msgs();
    assert_eq!(m[0], Message::PointerMove { win: 5, x: 30.0, y: 40.0 });
    assert!(matches!(m[1], Message::PointerButton { win: 5, pressed: true, .. }));
    assert!(matches!(m[2], Message::PointerButton { win: 5, pressed: false, .. }));
    // O popup some: o desenho volta a ser só a janela.
    b.ev(UserEvent::WinDestroy { win: 5 });
    let q = b.quadros.lock().unwrap();
    assert_eq!(pixel(q.last().unwrap(), 40, 80), [50, 50, 50, 255]);
}

#[test]
fn rotacao_pede_o_tamanho_novo_sem_derrubar_nada() {
    let b = banco();
    b.criar(1, 540, 1200, "a", 70);
    b.saida();
    b.s.definir_tela(TelaInfo { largura: 2400, altura: 1080, dpi: 440 });
    let saida = b.saida();
    assert!(saida.contains(&Message::ResizeReq { win: 1, width: 1200, height: 540 }), "{saida:?}");
    // O mesmo tamanho de novo não repete o pedido.
    b.s.definir_tela(TelaInfo { largura: 2400, altura: 1080, dpi: 440 });
    assert!(b.saida().is_empty());
    let q = b.quadros.lock().unwrap();
    assert_eq!((q.last().unwrap().0, q.last().unwrap().1), (1200, 540));
}

#[test]
fn sessao_nova_pede_o_tamanho_do_fator_padrao() {
    let fila = Arc::new(Fila::new());
    let (out_tx, out) = mpsc::channel();
    let (superficie, _q) = tela_falsa();
    let (s, _entrada) = Sessao::nova(fila, out_tx, superficie, CELULAR, None, None);
    s.send(UserEvent::WinCreate { win: 1, width: 960, height: 600, title: "a".into(), shared: janela(960, 600, 70, 101) });
    let saida: Vec<Message> = out.try_iter().collect();
    assert!(saida.contains(&Message::ResizeReq { win: 1, width: 432, height: 960 }), "{saida:?}");
}

#[test]
fn mudar_o_fator_refaz_o_pedido_de_todas_as_janelas_sem_derrubar_nada() {
    let b = banco();
    b.criar(1, 540, 1200, "a", 70);
    b.criar(2, 540, 1200, "b", 80);
    b.saida();
    b.s.definir_fator(3.0);
    let saida = b.saida();
    assert!(saida.contains(&Message::ResizeReq { win: 1, width: 360, height: 800 }), "{saida:?}");
    assert!(saida.contains(&Message::ResizeReq { win: 2, width: 360, height: 800 }), "{saida:?}");
    assert!(!b.s.parada());
    // O mesmo fator de novo não repete nada; valor fora do passo é levado ao passo (2,8 vira 3,0).
    b.s.definir_fator(2.8);
    assert!(b.saida().is_empty());
    b.s.definir_fator(2.0);
    let saida = b.saida();
    assert!(saida.contains(&Message::ResizeReq { win: 1, width: 540, height: 1200 }), "{saida:?}");
    assert!(saida.contains(&Message::ResizeReq { win: 2, width: 540, height: 1200 }), "{saida:?}");
}

#[test]
fn escala_fora_dos_limites_e_limitada() {
    let b = banco();
    b.criar(1, 540, 1200, "a", 70);
    b.saida();
    b.s.definir_fator(99.0);
    let saida = b.saida();
    assert!(saida.contains(&Message::ResizeReq { win: 1, width: 270, height: 600 }), "{saida:?}");
}

#[test]
fn limites_da_janela_entram_no_pedido_de_tamanho() {
    let b = banco();
    b.ev(UserEvent::WinLimits { win: 1, min_w: 0, min_h: 0, max_w: 400, max_h: 800 });
    b.criar(1, 540, 1200, "a", 70);
    let saida = b.saida();
    assert!(saida.contains(&Message::ResizeReq { win: 1, width: 400, height: 800 }), "{saida:?}");
}

#[test]
fn pinca_amplia_so_localmente_sem_novo_pedido_de_tamanho() {
    let b = banco();
    b.criar(1, 540, 1200, "a", 70);
    b.saida();
    let antes = b.quadros.lock().unwrap().len();
    // Dois dedos se afastando.
    b.s.toque(&toque(Acao::Baixo, 0, &[(0, 400.0, 1000.0)], 0));
    b.s.toque(&toque(Acao::PonteiroBaixo, 1, &[(0, 400.0, 1000.0), (1, 600.0, 1000.0)], 10));
    b.s.toque(&toque(Acao::Mover, 0, &[(0, 300.0, 1000.0), (1, 700.0, 1000.0)], 30));
    assert!(b.saida().iter().all(|m| !matches!(m, Message::ResizeReq { .. })), "a pinça é local");
    let q = b.quadros.lock().unwrap();
    assert!(q.len() > antes, "redesenhou");
    assert!(q.last().unwrap().0 < 540, "a região visível encolheu: {}", q.last().unwrap().0);
}

#[test]
fn texto_do_teclado_virtual_e_ctrl_grudento_viram_teclas_na_janela_em_foco() {
    let b = banco();
    b.criar(3, 540, 1200, "foot", 70);
    b.s.texto("ação");
    let m = b.entrada_msgs();
    assert!(!m.is_empty() && m.iter().all(|m| matches!(m, Message::Key { win: 3, .. })));
    // Ctrl + C.
    assert!(b.s.alternar_modificador(Mod::Ctrl));
    assert_eq!(b.s.modificadores(), (true, false));
    b.s.texto("c");
    let m = b.entrada_msgs();
    assert_eq!(m.first(), Some(&Message::Key { win: 3, keycode: 29, pressed: true }));
    assert_eq!(m.last(), Some(&Message::Key { win: 3, keycode: 29, pressed: false }));
    assert_eq!(b.s.modificadores(), (false, false));
    // Caractere sem tecla: aviso na fila.
    b.fila.drenar();
    b.s.texto("😀");
    assert!(matches!(b.fila.drenar().as_slice(), [Evento::Aviso(t)] if t.contains("descartado")));
    // Backspace e teclado físico.
    b.s.apagar(2, 0);
    assert_eq!(b.entrada_msgs().len(), 4);
    b.s.tecla_fisica(30, true);
    b.s.tecla_fisica(0, true);
    assert_eq!(b.entrada_msgs(), vec![Message::Key { win: 3, keycode: 30, pressed: true }]);
}

#[test]
fn sem_janela_em_foco_a_entrada_de_teclado_e_ignorada() {
    let b = banco();
    b.s.texto("abc");
    b.s.tecla_extra(1);
    b.s.tecla_fisica(30, true);
    assert!(b.entrada_msgs().is_empty());
}

#[test]
fn area_de_transferencia_nos_dois_sentidos_com_texto_acentuado() {
    let b = banco();
    b.criar(1, 540, 1200, "a", 70);
    b.saida();
    // Do Pi para o celular: vira evento para a casca gravar no ClipboardManager.
    assert_eq!(b.s.clipboard().from_server("text/plain;charset=utf-8", "do Pi: ação ç".as_bytes()).as_deref(), Some("do Pi: ação ç"));
    assert!(b.fila.drenar().contains(&Evento::AreaDeTransferencia("do Pi: ação ç".into())));
    // O texto que acabou de vir do Pi não volta como eco quando o app ganha o foco.
    b.s.foco_do_app(true, Some("do Pi: ação ç"));
    assert!(!b.saida().iter().any(|m| matches!(m, Message::ClipboardUp { .. })));
    // Do celular para o Pi: ao voltar ao foco com texto novo.
    b.s.foco_do_app(true, Some("do celular ÃÇ"));
    let saida = b.saida();
    assert!(saida.iter().any(|m| matches!(m, Message::ClipboardUp { data, .. } if data.as_slice() == "do celular ÃÇ".as_bytes())), "{saida:?}");
}

#[test]
fn perder_o_foco_do_app_avisa_o_servidor_e_a_desconexao_encerra_a_sessao() {
    let b = banco();
    b.criar(1, 540, 1200, "a", 70);
    b.saida();
    b.s.foco_do_app(false, None);
    assert!(b.saida().contains(&Message::Focus { win: 1, focused: false }));
    b.ev(UserEvent::Cursor { win: 1, shape: CursorShape::Hidden });
    b.ev(UserEvent::Disconnected("o servidor fechou a conexão".into()));
    assert!(b.fila.drenar().iter().any(|e| matches!(e, Evento::Estado { fase: f, texto } if *f == fase::DESCONECTADO && texto.contains("fechou"))));
    assert!(b.s.parada());
    assert!(!b.s.send(UserEvent::Quit), "depois de parar, a rede é mandada parar");
}

#[test]
fn quadro_de_janela_destruida_nao_trava_nem_entra_em_panico() {
    let b = banco();
    b.criar(1, 540, 1200, "a", 70);
    b.saida();
    let fantasma = janela(8, 8, 1, 777);
    b.ev(UserEvent::Frame { win: 42, shared: fantasma });
    assert!(b.saida().contains(&Message::FrameAck { win: 42, seq: 777 }), "janela desconhecida: confirma direto");
    b.ev(UserEvent::PopupMove { win: 99, x: 1, y: 1, width: 5, height: 5 });
    b.ev(UserEvent::WinResize { win: 77, width: 5, height: 5 });
    b.ev(UserEvent::WinTitle { win: 77, title: "x".into() });
    b.ev(UserEvent::WinDestroy { win: 77 });
}

#[test]
fn eventos_do_poll_saem_no_formato_documentado() {
    let b = banco();
    b.criar(1, 540, 1200, "a", 70);
    let bytes = b.fila.drenar_bytes();
    let ev = decodificar(&bytes).unwrap();
    assert!(ev.iter().any(|e| matches!(e, Evento::Janelas { foco: 1, .. })));
}

// ---------------------------------------------------------------------------------------------------------------
// Contra um psd de verdade
// ---------------------------------------------------------------------------------------------------------------

fn caixa(k: &Keypair) -> Box<Segredo> {
    Box::new(Segredo(*k.secret_bytes()))
}

fn tmpdir(nome: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("ps-android-sessao-{}-{nome}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

struct Pi {
    sec: Arc<Security>,
    addr: SocketAddr,
}

fn pi(nome: &str, preload: &[(&Keypair, &str)]) -> Pi {
    let dir = tmpdir(&format!("pi-{nome}"));
    if !preload.is_empty() {
        let mut l = PeerList::default();
        for (k, n) in preload {
            l.upsert(*k.public(), n, "2026-01-01");
        }
        ps_secure::chaves::save_list(&dir.join("pareados"), &l).unwrap();
    }
    let t = Timing { first_use_window: Duration::from_secs(60), pair_timeout: Duration::from_secs(10), handshake_timeout: Duration::from_secs(3), unknown_max: 50, unknown_per: Duration::from_secs(60) };
    let sec = Security::open_with(&dir, "pi-teste", false, t, |_| {}).unwrap();
    let acceptor = TcpAcceptor::bind("127.0.0.1:0").unwrap();
    let addr = acceptor.local_addr().unwrap();
    let opts = ServerOptions { fps: 30, quiet: true, ..ServerOptions::default() };
    let (s2, o2) = (sec.clone(), opts);
    std::thread::spawn(move || {
        let make = || Box::new(synthetic::SyntheticSource::new(o2.fps, o2.ui_size, o2.gradient_size)) as Box<dyn WindowSource>;
        let _ = serve_secure(&acceptor, &o2, s2, None, make);
    });
    Pi { sec, addr }
}

fn controle(nome: &str) -> (Controle, Quadros, PathBuf) {
    let dir = tmpdir(&format!("cli-{nome}"));
    let c = Controle::new(Config { pasta: dir.clone(), nome_cliente: "celular-teste".into() });
    let (sup, q) = tela_falsa();
    c.definir_tela(CELULAR);
    let boxed = sup.lock().unwrap().take();
    c.definir_superficie(boxed);
    (c, q, dir)
}

/// Esvazia a fila até `pronto` aceitar os eventos ou o prazo acabar; devolve tudo o que viu.
fn esperar(c: &Controle, prazo: Duration, mut pronto: impl FnMut(&[Evento]) -> bool) -> Vec<Evento> {
    let fim = Instant::now() + prazo;
    let mut todos = Vec::new();
    while Instant::now() < fim {
        todos.extend(decodificar(&c.poll(0)).unwrap());
        if pronto(&todos) {
            return todos;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    panic!("prazo: eventos vistos: {todos:?}");
}

fn conectou(ev: &[Evento]) -> bool {
    ev.iter().any(|e| matches!(e, Evento::Estado { fase: f, .. } if *f == fase::CONECTADO))
}

#[test]
fn conecta_ao_psd_pelo_canal_cifrado_com_a_chave_entregue_pela_casca_e_desenha() {
    let p = pi("conecta", &[]);
    let (c, quadros, dir) = controle("conecta");
    let chave = Keypair::generate().unwrap();
    c.conectar(&p.addr.to_string(), Some(caixa(&chave)));
    let ev = esperar(&c, Duration::from_secs(15), |ev| conectou(ev) && ev.iter().any(|e| matches!(e, Evento::Janelas { foco, .. } if *foco != 0)));
    // Aparelho registrado na lista de conhecidos (a pasta de dados do app), sem nenhum segredo nela.
    let conhecidos = std::fs::read_to_string(dir.join("aparelhos-conhecidos")).unwrap();
    assert!(conhecidos.contains("pi-teste"));
    assert!(!dir.join("chave-do-cliente").exists(), "a chave não é gravada pelo núcleo: quem guarda é a casca, no Keystore");
    assert_eq!(p.sec.list().len(), 1, "o aparelho pareou o celular no primeiro uso");
    let _ = ev;
    // Chegam quadros: o núcleo desenhou na tela falsa, no tamanho da região visível.
    let fim = Instant::now() + Duration::from_secs(10);
    while quadros.lock().unwrap().is_empty() && Instant::now() < fim {
        std::thread::sleep(Duration::from_millis(20));
    }
    {
        let q = quadros.lock().unwrap();
        let primeiro = q.first().expect("a tela recebeu quadros do psd");
        assert!(primeiro.0 > 0 && primeiro.1 > 0);
    }
    // Entrada: um toque e texto não derrubam a sessão.
    c.toque(&toque(Acao::Baixo, 0, &[(0, 300.0, 400.0)], 0));
    c.toque(&toque(Acao::Cima, 0, &[(0, 300.0, 400.0)], 50));
    c.texto("ação");
    std::thread::sleep(Duration::from_millis(500));
    // Desconectar: o psd vê a sessão acabar e a fila anuncia o estado.
    c.desconectar();
    assert!(decodificar(&c.poll(0)).unwrap().iter().all(|e| !matches!(e, Evento::Erro(_))));
}

#[test]
fn reabrir_com_a_mesma_chave_reconhece_o_aparelho_sem_parear_de_novo() {
    let p = pi("reconhece", &[]);
    let (c, _q, _dir) = controle("reconhece");
    let chave = Keypair::generate().unwrap();
    for _ in 0..2 {
        c.conectar(&p.addr.to_string(), Some(caixa(&chave)));
        esperar(&c, Duration::from_secs(15), conectou);
        c.desconectar();
        std::thread::sleep(Duration::from_millis(200));
    }
    assert_eq!(p.sec.list().len(), 1, "a segunda conexão não pareia de novo");
}

#[test]
fn sem_chave_o_nucleo_cria_uma_entrega_a_casca_e_so_conecta_depois_da_gravacao() {
    let p = pi("chave-nova", &[]);
    let (c, _q, _dir) = controle("chave-nova");
    c.conectar(&p.addr.to_string(), None);
    let ev = esperar(&c, Duration::from_secs(10), |ev| ev.iter().any(|e| matches!(e, Evento::ChaveNova(_))));
    assert!(!conectou(&ev), "não conecta antes de a casca gravar a chave");
    assert_eq!(p.sec.list().len(), 0);
    c.chave_gravada(true);
    esperar(&c, Duration::from_secs(15), conectou);
    assert_eq!(p.sec.list().len(), 1);
    c.desconectar();
}

#[test]
fn se_a_casca_nao_consegue_gravar_a_chave_nada_conecta_e_o_erro_e_claro() {
    let p = pi("chave-falha", &[]);
    let (c, _q, _dir) = controle("chave-falha");
    c.conectar(&p.addr.to_string(), None);
    esperar(&c, Duration::from_secs(10), |ev| ev.iter().any(|e| matches!(e, Evento::ChaveNova(_))));
    c.chave_gravada(false);
    let ev = esperar(&c, Duration::from_secs(10), |ev| ev.iter().any(|e| matches!(e, Evento::Erro(_))));
    assert!(ev.iter().any(|e| matches!(e, Evento::Erro(t) if t.contains("gravar a chave"))), "{ev:?}");
    assert_eq!(p.sec.list().len(), 0, "o aparelho nunca viu esse cliente");
}

#[test]
fn aparelho_com_outro_dono_mostra_o_codigo_de_pareamento_e_conecta_depois_da_aprovacao() {
    let dono = Keypair::generate().unwrap();
    let p = pi("aprova", &[(&dono, "dono")]);
    let (c, _q, dir) = controle("aprova");
    let chave = Keypair::generate().unwrap();
    let sec = p.sec.clone();
    let esperando = std::thread::spawn(move || sec.open_pair_window(Duration::from_secs(15), &|| true));
    while !p.sec.pair_window_open() {
        std::thread::sleep(Duration::from_millis(10));
    }
    c.conectar(&p.addr.to_string(), Some(caixa(&chave)));
    let PairWait::Request(req) = esperando.join().unwrap() else { panic!("sem pedido de pareamento") };
    // O celular mostra o mesmo código que o `psd pair` mostra ao operador.
    let ev = esperar(&c, Duration::from_secs(10), |ev| ev.iter().any(|e| matches!(e, Evento::Codigo(_))));
    let codigo = ev.iter().find_map(|e| if let Evento::Codigo(c) = e { Some(c.clone()) } else { None }).unwrap();
    assert_eq!(codigo, format_code(&req.code));
    assert!(ev.iter().any(|e| matches!(e, Evento::Estado { fase: f, .. } if *f == fase::PAREANDO)));
    p.sec.decide(req.id, true).unwrap();
    esperar(&c, Duration::from_secs(15), conectou);
    assert!(p.sec.list().len() == 2, "o dono e o celular");
    assert!(std::fs::read_to_string(dir.join("aparelhos-conhecidos")).unwrap().contains("pi-teste"));
    c.desconectar();
}

#[test]
fn aparelho_que_recusa_da_erro_claro_que_ensina_a_parear() {
    let dono = Keypair::generate().unwrap();
    let p = pi("recusa", &[(&dono, "dono")]);
    let (c, _q, _dir) = controle("recusa");
    let chave = Keypair::generate().unwrap();
    // Ninguém abriu o `psd pair`: o aparelho recusa na hora.
    c.conectar(&p.addr.to_string(), Some(caixa(&chave)));
    let ev = esperar(&c, Duration::from_secs(15), |ev| ev.iter().any(|e| matches!(e, Evento::Erro(_))));
    assert!(ev.iter().any(|e| matches!(e, Evento::Erro(t) if t.contains("psd pair"))), "{ev:?}");
    assert_eq!(p.sec.list().len(), 1);
}

#[test]
fn endereco_invalido_e_porta_fechada_dao_erro_sem_panico() {
    let (c, _q, _dir) = controle("invalido");
    let chave = Keypair::generate().unwrap();
    c.conectar("", Some(caixa(&chave)));
    esperar(&c, Duration::from_secs(5), |ev| ev.iter().any(|e| matches!(e, Evento::Erro(_))));
    // Porta onde ninguém escuta.
    let livre = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap();
    c.conectar(&livre.to_string(), Some(caixa(&chave)));
    esperar(&c, Duration::from_secs(10), |ev| ev.iter().any(|e| matches!(e, Evento::Erro(t) if t.contains("conectar"))));
}

#[test]
fn resolver_aceita_ip_com_e_sem_porta() {
    use ps_android::sessao::resolver;
    assert_eq!(resolver("192.168.0.13").unwrap().to_string(), "192.168.0.13:7700");
    assert_eq!(resolver(" 192.168.0.13:7701 ").unwrap().to_string(), "192.168.0.13:7701");
    assert!(resolver("").is_err());
    assert!(resolver("a\nb").is_err());
    assert!(resolver(&"x".repeat(300)).is_err());
}

// ---------------------------------------------------------------------------------------------------------------
// Som e estatísticas
// ---------------------------------------------------------------------------------------------------------------

static SAIDAS_ABERTAS: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
static SAIDAS_PARADAS: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

struct SaidaFalsa;

impl ps_client::audio::AudioSink for SaidaFalsa {
    fn name(&self) -> &'static str {
        "falsa"
    }

    fn stop(&mut self) {
        SAIDAS_PARADAS.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    }
}

fn abrir_saida_falsa(_e: Arc<ps_client::audio::AudioEngine>) -> Result<Box<dyn ps_client::audio::AudioSink>, String> {
    SAIDAS_ABERTAS.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    Ok(Box::new(SaidaFalsa))
}

#[test]
fn a_saida_de_som_abre_com_a_sessao_e_para_ao_desconectar() {
    use std::sync::atomic::Ordering::SeqCst;
    let p = pi("som", &[]);
    let (c, _q, _dir) = controle("som");
    c.definir_fabrica_de_saida(abrir_saida_falsa);
    let chave = Keypair::generate().unwrap();
    c.conectar(&p.addr.to_string(), Some(caixa(&chave)));
    esperar(&c, Duration::from_secs(15), conectou);
    assert_eq!(SAIDAS_ABERTAS.load(SeqCst), 1, "a saída é aberta antes do HELLO");
    assert_eq!(SAIDAS_PARADAS.load(SeqCst), 0);
    c.desconectar();
    let fim = Instant::now() + Duration::from_secs(5);
    while SAIDAS_PARADAS.load(SeqCst) == 0 && Instant::now() < fim {
        std::thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(SAIDAS_PARADAS.load(SeqCst), 1, "o motor parou a saída uma vez");
}

#[test]
fn as_linhas_de_estatisticas_tem_os_campos_do_psc() {
    let stats = ps_client::stats::Stats::default();
    stats.window_start();
    stats.record_rtt(11_000);
    let audio = ps_client::audio::AudioEngine::new(None);
    let l = ps_android::sessao::linhas_de_estatisticas(&stats, Some(&audio));
    assert_eq!(l.len(), 2, "{l:?}");
    assert!(l[0].starts_with("VIDEOSTATS fps=") && l[0].contains("rtt_mediano=11.00ms") && l[0].contains("decod_media=") && l[0].contains("rtt_amostras=1"), "{}", l[0]);
    assert!(l[1].starts_with("AUDIOSTATS pacotes=") && l[1].contains("esvaziamentos=") && l[1].contains("jitter="), "{}", l[1]);
    // Sem motor de áudio só sai a linha de vídeo; a janela seguinte já foi aberta.
    let l = ps_android::sessao::linhas_de_estatisticas(&stats, None);
    assert_eq!(l.len(), 1);
    assert!(l[0].contains("rtt_mediano=sem-amostras"));
}

/// Marco 8A: o `GOODBYE` do aparelho vira o evento `Encerrada` (com o texto) e o estado "desconectado", a sessão para, e o
/// `STATUS` vira `EstadoDoAparelho` para a casca mostrar. Sem aparelho de verdade: o que a casca faz com o texto não foi
/// verificado no celular.
#[test]
fn goodbye_e_status_viram_eventos_para_a_casca_e_a_sessao_para() {
    let b = banco();
    b.criar(1, 540, 1200, "a", 70);
    b.saida();
    b.ev(UserEvent::Status { flags: 0b1001, temp_dc: Some(512) });
    assert!(b.fila.drenar().contains(&Evento::EstadoDoAparelho { sinalizadores: 0b1001, temperatura_dc: Some(512) }));
    b.ev(UserEvent::Goodbye { reason: 1, text: "notebook de ação".into() });
    let ev = b.fila.drenar();
    assert!(ev.iter().any(|e| matches!(e, Evento::Encerrada { motivo: 1, texto } if texto.contains("notebook de ação"))), "{ev:?}");
    assert!(ev.iter().any(|e| matches!(e, Evento::Estado { fase: f, texto } if *f == fase::DESCONECTADO && texto.contains("assumiu"))), "{ev:?}");
    assert!(b.s.parada());
}

/// Marco 8A, ponta a ponta com um `psd` de verdade: dois celulares pareados; o segundo assume a sessão. O primeiro recebe
/// `Encerrada` (motivo 1) com o nome do segundo, a sessão dele para e **não reconecta sozinho**; o segundo desenha.
#[test]
fn outro_cliente_assume_a_sessao_e_o_primeiro_recebe_o_evento_e_nao_reconecta() {
    let (ka, kb) = (Keypair::generate().unwrap(), Keypair::generate().unwrap());
    let p = pi("troca", &[(&ka, "celular-a"), (&kb, "celular-b")]);
    let (a, _qa, _da) = controle("troca-a");
    let dir_b = tmpdir("cli-troca-b");
    let b = Controle::new(Config { pasta: dir_b, nome_cliente: "celular-b".into() });
    let (sup, qb) = tela_falsa();
    b.definir_tela(CELULAR);
    let boxed = sup.lock().unwrap().take();
    b.definir_superficie(boxed);
    a.conectar(&p.addr.to_string(), Some(caixa(&ka)));
    esperar(&a, Duration::from_secs(15), conectou);
    b.conectar(&p.addr.to_string(), Some(caixa(&kb)));
    let ev = esperar(&a, Duration::from_secs(15), |ev| ev.iter().any(|e| matches!(e, Evento::Encerrada { .. })));
    assert!(ev.iter().any(|e| matches!(e, Evento::Encerrada { motivo: 1, texto } if texto.contains("celular-teste") || texto.contains("celular-b"))), "{ev:?}");
    esperar(&b, Duration::from_secs(15), conectou);
    let fim = Instant::now() + Duration::from_secs(10);
    while qb.lock().unwrap().is_empty() && Instant::now() < fim {
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(!qb.lock().unwrap().is_empty(), "o segundo recebeu as janelas e desenhou");
    // O primeiro não volta sozinho: nenhum evento de conexão nos próximos segundos.
    std::thread::sleep(Duration::from_secs(2));
    let depois = decodificar(&a.poll(0)).unwrap();
    assert!(!depois.iter().any(|e| matches!(e, Evento::Estado { fase: f, .. } if *f == fase::CONECTANDO || *f == fase::CONECTADO)), "{depois:?}");
}
