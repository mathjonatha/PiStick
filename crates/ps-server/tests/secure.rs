
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use ps_proto::{read_hello, read_message, write_message, Message, AUTH_PAIRED, AUTH_PAIRED_NOW, AUTH_PENDING, AUTH_REFUSED};
use ps_secure::canal::{client_handshake, PeerInfo, MAGIC};
use ps_secure::chaves::{fingerprint, pairing_code, PeerList};
use ps_secure::{Keypair, SecureConn};
use ps_server::secure::{serve_secure, PairWait, Security, Timing};
use ps_server::{synthetic, ServerOptions, WindowSource};
use ps_transport::{Split, TcpAcceptor, TcpReader, TcpTransport, Transport};

type Conn = SecureConn<TcpReader, TcpStream>;

struct Env {
    dir: PathBuf,
    sec: Arc<Security>,
    addr: SocketAddr,
    log: Arc<Mutex<Vec<String>>>,
}

fn tmpdir(nome: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("ps-server-secure-{}-{nome}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn start(nome: &str, timing: Timing, preload: &[(&Keypair, &str)]) -> Env {
    let dir = tmpdir(nome);
    if !preload.is_empty() {
        let mut l = PeerList::default();
        for (k, n) in preload {
            l.upsert(*k.public(), n, "2026-01-01");
        }
        ps_secure::chaves::save_list(&dir.join("pareados"), &l).unwrap();
    }
    let log: Arc<Mutex<Vec<String>>> = Arc::default();
    let l2 = log.clone();
    let sec = Security::open_with(&dir, "pi-teste", false, timing, move |m| l2.lock().unwrap().push(m.to_string())).unwrap();
    let acceptor = TcpAcceptor::bind("127.0.0.1:0").unwrap();
    let addr = acceptor.local_addr().unwrap();
    let opts = ServerOptions { fps: 30, hello_timeout: Duration::from_millis(1500), quiet: true, ..ServerOptions::default() };
    let s2 = sec.clone();
    let o2 = opts.clone();
    std::thread::spawn(move || {
        let make = || Box::new(synthetic::SyntheticSource::new(o2.fps, o2.ui_size, o2.gradient_size)) as Box<dyn WindowSource>;
        let _ = serve_secure(&acceptor, &o2, s2, None, make);
    });
    Env { dir, sec, addr, log }
}

fn timing() -> Timing {
    Timing { first_use_window: Duration::from_secs(60), pair_timeout: Duration::from_secs(5), handshake_timeout: Duration::from_secs(2), unknown_max: 5, unknown_per: Duration::from_secs(60) }
}

impl Env {
    fn log_has(&self, trecho: &str) -> bool {
        self.log.lock().unwrap().iter().any(|l| l.contains(trecho))
    }
}

fn noise(addr: SocketAddr, key: &Keypair, name: &str) -> Result<(Conn, PeerInfo), String> {
    let t = TcpTransport::connect(addr).map_err(|e| e.to_string())?;
    let peer = t.peer();
    let Split { mut reader, mut writer, closer } = t.split().map_err(|e| e.to_string())?;
    let (est, info) = client_handshake(&mut reader, &mut writer, key, name, |_, _| Ok(())).map_err(|e| e.to_string())?;
    Ok((SecureConn::new(reader, writer, closer, est.send, est.recv, peer), info))
}

fn status(c: &mut Conn) -> Result<(u8, [u8; 6]), String> {
    match read_message(&mut c.reader).map_err(|e| e.to_string())? {
        Message::AuthStatus { state, code } => Ok((state, code)),
        outra => Err(format!("esperava AUTH_STATUS, veio {outra:?}")),
    }
}

fn sessao_funciona(c: &mut Conn) {
    write_message(&mut c.writer, &Message::hello("cliente de teste")).unwrap();
    let (_, nome) = read_hello(&mut c.reader).unwrap();
    assert!(nome.contains("psd"));
    loop {
        if let Message::WinCreate { .. } = read_message(&mut c.reader).unwrap() {
            break;
        }
    }
}

fn ping_pong(c: &mut Conn, v: u64) {
    write_message(&mut c.writer, &Message::Ping { value: v }).unwrap();
    loop {
        if let Message::Pong { echo } = read_message(&mut c.reader).unwrap() {
            assert_eq!(echo, v);
            return;
        }
    }
}

fn espera(mut f: impl FnMut() -> bool, ate: Duration, msg: &str) {
    let fim = Instant::now() + ate;
    while !f() {
        assert!(Instant::now() < fim, "{msg}");
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn primeiro_uso_pareia_e_a_segunda_conexao_e_estado_zero() {
    let env = start("primeiro", timing(), &[]);
    assert!(env.sec.first_use_open());
    let key = Keypair::generate().unwrap();
    let (mut c, info) = noise(env.addr, &key, "notebook").unwrap();
    assert_eq!(info.static_key, env.sec.device_public());
    assert_eq!(info.name, "pi-teste");
    assert_eq!(status(&mut c).unwrap().0, AUTH_PAIRED_NOW);
    sessao_funciona(&mut c);
    drop(c);
    assert!(!env.sec.first_use_open(), "com um pareado, o primeiro uso fecha");
    let (lista, avisos) = ps_secure::chaves::load_list(&env.dir.join("pareados")).unwrap();
    assert!(avisos.is_empty());
    assert_eq!(lista.len(), 1);
    assert_eq!(lista.peers[0].name, "notebook");
    espera(|| env.sec.list().iter().all(|(_, on)| !on), Duration::from_secs(5), "a sessão anterior não terminou");
    let (mut c, _) = noise(env.addr, &key, "notebook").unwrap();
    assert_eq!(status(&mut c).unwrap().0, AUTH_PAIRED);
    sessao_funciona(&mut c);
}

#[test]
fn desconhecido_depois_do_primeiro_uso_e_recusado_e_o_log_tem_nome_e_impressao() {
    let dono = Keypair::generate().unwrap();
    let env = start("recusa", timing(), &[(&dono, "dono")]);
    assert!(!env.sec.first_use_open());
    let intruso = Keypair::generate().unwrap();
    let (mut c, _) = noise(env.addr, &intruso, "intruso").unwrap();
    assert_eq!(status(&mut c).unwrap().0, AUTH_REFUSED);
    assert!(read_message(&mut c.reader).is_err());
    let fp = fingerprint(intruso.public());
    espera(|| env.log_has("recusado"), Duration::from_secs(2), "o log não registrou a recusa");
    assert!(env.log_has("\"intruso\""), "{:?}", env.log.lock().unwrap());
    assert!(env.log_has(&fp), "o log precisa ter a impressão digital do recusado");
    let (mut c, _) = noise(env.addr, &dono, "dono").unwrap();
    assert_eq!(status(&mut c).unwrap().0, AUTH_PAIRED);
}

#[test]
fn aprovacao_com_codigo_igual_nos_dois_lados_sem_derrubar_a_sessao_em_curso() {
    let dono = Keypair::generate().unwrap();
    let env = start("aprova", timing(), &[(&dono, "dono")]);
    let (mut a, _) = noise(env.addr, &dono, "dono").unwrap();
    assert_eq!(status(&mut a).unwrap().0, AUTH_PAIRED);
    sessao_funciona(&mut a);
    ping_pong(&mut a, 1);
    let sec = env.sec.clone();
    let esperando = std::thread::spawn(move || sec.open_pair_window(Duration::from_secs(10), &|| true));
    espera(|| env.sec.pair_window_open(), Duration::from_secs(2), "a janela de pareamento não abriu");
    let novo = Keypair::generate().unwrap();
    let (mut b, info_b) = noise(env.addr, &novo, "celular").unwrap();
    let (estado, codigo) = status(&mut b).unwrap();
    assert_eq!(estado, AUTH_PENDING);
    let PairWait::Request(req) = esperando.join().unwrap() else { panic!("esperava um pedido") };
    let local = pairing_code(&info_b.handshake_hash);
    assert_eq!(local, codigo);
    assert_eq!(local, req.code);
    assert_eq!(req.name, "celular");
    assert_eq!(req.fingerprint, fingerprint(novo.public()));
    for v in 2..6 {
        ping_pong(&mut a, v);
        std::thread::sleep(Duration::from_millis(100));
    }
    env.sec.decide(req.id, true).unwrap();
    assert_eq!(status(&mut b).unwrap().0, AUTH_PAIRED_NOW);
    ping_pong(&mut a, 99);
    drop(a);
    sessao_funciona(&mut b);
    let nomes: Vec<String> = env.sec.list().into_iter().map(|(p, _)| p.name).collect();
    assert_eq!(nomes, vec!["dono".to_string(), "celular".to_string()]);
}

#[test]
fn pedido_negado_e_pedido_vencido_recusam_e_nao_pareiam() {
    let dono = Keypair::generate().unwrap();
    let mut t = timing();
    t.pair_timeout = Duration::from_millis(900);
    let env = start("nega", t, &[(&dono, "dono")]);
    let sec = env.sec.clone();
    let esperando = std::thread::spawn(move || sec.open_pair_window(Duration::from_secs(10), &|| true));
    espera(|| env.sec.pair_window_open(), Duration::from_secs(2), "janela");
    let k1 = Keypair::generate().unwrap();
    let (mut c, _) = noise(env.addr, &k1, "um").unwrap();
    assert_eq!(status(&mut c).unwrap().0, AUTH_PENDING);
    let PairWait::Request(req) = esperando.join().unwrap() else { panic!() };
    env.sec.decide(req.id, false).unwrap();
    assert_eq!(status(&mut c).unwrap().0, AUTH_REFUSED);
    let sec = env.sec.clone();
    let esperando = std::thread::spawn(move || sec.open_pair_window(Duration::from_secs(10), &|| true));
    espera(|| env.sec.pair_window_open(), Duration::from_secs(2), "janela");
    let k2 = Keypair::generate().unwrap();
    let (mut c, _) = noise(env.addr, &k2, "dois").unwrap();
    assert_eq!(status(&mut c).unwrap().0, AUTH_PENDING);
    let PairWait::Request(req) = esperando.join().unwrap() else { panic!() };
    assert_eq!(status(&mut c).unwrap().0, AUTH_REFUSED, "o prazo vence e o servidor recusa");
    assert!(env.sec.decide(req.id, true).is_err(), "aprovar depois de vencido não vale");
    assert_eq!(env.sec.list().len(), 1, "ninguém foi pareado");
    let (mut c, _) = noise(env.addr, &Keypair::generate().unwrap(), "tres").unwrap();
    assert_eq!(status(&mut c).unwrap().0, AUTH_REFUSED);
}

#[test]
fn cliente_que_some_com_o_pedido_pendente_libera_a_janela() {
    let dono = Keypair::generate().unwrap();
    let env = start("some", timing(), &[(&dono, "dono")]);
    let sec = env.sec.clone();
    let esperando = std::thread::spawn(move || sec.open_pair_window(Duration::from_secs(10), &|| true));
    espera(|| env.sec.pair_window_open(), Duration::from_secs(2), "janela");
    let (mut c, _) = noise(env.addr, &Keypair::generate().unwrap(), "foge").unwrap();
    assert_eq!(status(&mut c).unwrap().0, AUTH_PENDING);
    let PairWait::Request(req) = esperando.join().unwrap() else { panic!() };
    drop(c);
    espera(|| env.log_has("encerrado sem aprovação"), Duration::from_secs(3), "o pedido do cliente que saiu continuou valendo");
    assert!(env.sec.decide(req.id, true).is_err());
    assert_eq!(env.sec.list().len(), 1);
}

#[test]
fn remover_encerra_a_sessao_e_passa_a_recusar() {
    let dono = Keypair::generate().unwrap();
    let outro = Keypair::generate().unwrap();
    let env = start("remove", timing(), &[(&dono, "dono"), (&outro, "outro")]);
    let (mut c, _) = noise(env.addr, &outro, "outro").unwrap();
    assert_eq!(status(&mut c).unwrap().0, AUTH_PAIRED);
    sessao_funciona(&mut c);
    espera(|| env.sec.list().iter().any(|(_, on)| *on), Duration::from_secs(2), "sessão não ficou ativa");
    let fora = env.sec.remove(&fingerprint(outro.public())).unwrap();
    assert_eq!(fora.len(), 1);
    assert_eq!(fora[0].name, "outro");
    let fim = Instant::now() + Duration::from_secs(5);
    loop {
        if read_message(&mut c.reader).is_err() {
            break;
        }
        assert!(Instant::now() < fim, "a sessão do cliente removido não foi encerrada");
    }
    let (mut c2, _) = noise(env.addr, &outro, "outro").unwrap();
    assert_eq!(status(&mut c2).unwrap().0, AUTH_REFUSED);
    assert_eq!(env.sec.remove("nao-existe").unwrap().len(), 0);
    assert_eq!(ps_secure::chaves::load_list(&env.dir.join("pareados")).unwrap().0.len(), 1);
}

#[test]
fn reset_apaga_tudo_e_reabre_o_primeiro_uso() {
    let dono = Keypair::generate().unwrap();
    let env = start("reset", timing(), &[(&dono, "dono")]);
    assert!(!env.sec.first_use_open());
    assert_eq!(env.sec.reset().unwrap(), 1);
    assert!(env.sec.first_use_open());
    let novo = Keypair::generate().unwrap();
    let (mut c, _) = noise(env.addr, &novo, "novo").unwrap();
    assert_eq!(status(&mut c).unwrap().0, AUTH_PAIRED_NOW);
    let (mut d, _) = noise(env.addr, &dono, "dono").unwrap();
    assert_eq!(status(&mut d).unwrap().0, AUTH_REFUSED);
}

#[test]
fn primeiro_uso_fecha_depois_da_janela_de_tempo() {
    let mut t = timing();
    t.first_use_window = Duration::from_millis(300);
    let env = start("janela", t, &[]);
    std::thread::sleep(Duration::from_millis(500));
    assert!(!env.sec.first_use_open());
    let (mut c, _) = noise(env.addr, &Keypair::generate().unwrap(), "tarde").unwrap();
    assert_eq!(status(&mut c).unwrap().0, AUTH_REFUSED);
}

#[test]
fn limite_de_cinco_desconhecidos_por_minuto_o_sexto_fecha_sem_resposta() {
    let dono = Keypair::generate().unwrap();
    let env = start("limite", timing(), &[(&dono, "dono")]);
    for i in 0..5 {
        let (mut c, _) = noise(env.addr, &Keypair::generate().unwrap(), &format!("x{i}")).unwrap();
        assert_eq!(status(&mut c).unwrap().0, AUTH_REFUSED, "tentativa {i}");
    }
    let (mut c, _) = noise(env.addr, &Keypair::generate().unwrap(), "x6").unwrap();
    assert!(status(&mut c).is_err(), "a sexta tentativa não pode receber resposta");
    espera(|| env.log_has("muitas tentativas"), Duration::from_secs(2), "o log não registrou o limite");
    let (mut c, _) = noise(env.addr, &dono, "dono").unwrap();
    assert_eq!(status(&mut c).unwrap().0, AUTH_PAIRED);
}

#[test]
fn texto_claro_do_laco_local_continua_funcionando_com_a_seguranca_ligada() {
    let env = start("claro", timing(), &[]);
    let Split { mut reader, mut writer, .. } = TcpTransport::connect(env.addr).unwrap().split().unwrap();
    write_message(&mut writer, &Message::hello("claro")).unwrap();
    let (_, nome) = read_hello(&mut reader).unwrap();
    assert!(nome.contains("psd"));
    assert!(env.sec.first_use_open());
}

#[test]
fn lixo_na_abertura_e_aperto_de_mao_parado_nao_derrubam_o_servidor() {
    let mut t = timing();
    t.handshake_timeout = Duration::from_millis(400);
    let env = start("lixo", t, &[]);
    let mut s = TcpStream::connect(env.addr).unwrap();
    s.write_all(&[0xFF, 0xFF, 0xFF, 0xFF, 1, 2, 3, 4]).unwrap();
    drop(s);
    let mut s = TcpStream::connect(env.addr).unwrap();
    s.write_all(MAGIC).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
    let t0 = Instant::now();
    let mut b = [0u8; 1];
    let r = s.read(&mut b);
    assert!(matches!(r, Ok(0) | Err(_)));
    assert!(t0.elapsed() < Duration::from_millis(2500), "o prazo do aperto de mão não fechou a conexão");
    let mut s = TcpStream::connect(env.addr).unwrap();
    s.write_all(MAGIC).unwrap();
    s.write_all(&[0x00, 0x20]).unwrap();
    s.write_all(&[0xAB; 32]).unwrap();
    drop(s);
    let (mut c, _) = noise(env.addr, &Keypair::generate().unwrap(), "bom").unwrap();
    assert_eq!(status(&mut c).unwrap().0, AUTH_PAIRED_NOW);
    sessao_funciona(&mut c);
}

#[cfg(unix)]
mod controle {
    use super::*;
    use std::io::{BufRead, BufReader};
    use std::os::unix::net::UnixStream;

    fn comando(path: &std::path::Path, linha: &str) -> UnixStream {
        let mut s = UnixStream::connect(path).unwrap();
        s.write_all(format!("{linha}\n").as_bytes()).unwrap();
        s
    }

    fn linhas(s: UnixStream) -> Vec<String> {
        BufReader::new(s).lines().map_while(Result::ok).collect()
    }

    #[test]
    fn soquete_de_controle_lista_pareia_remove_e_reseta() {
        use std::os::unix::fs::PermissionsExt;
        let dono = Keypair::generate().unwrap();
        let env = start("ctl", timing(), &[(&dono, "dono")]);
        let sock = env.dir.join("ctl.sock");
        ps_server::ctl::serve(env.sec.clone(), &sock).unwrap();
        assert_eq!(std::fs::metadata(&sock).unwrap().permissions().mode() & 0o777, 0o600);
        let l = linhas(comando(&sock, "LIST"));
        assert_eq!(l.len(), 2);
        assert!(l[0].starts_with("C\t") && l[0].contains("dono") && l[1] == "END");
        let mut s = comando(&sock, "PAIR");
        let mut r = BufReader::new(s.try_clone().unwrap());
        let mut linha = String::new();
        r.read_line(&mut linha).unwrap();
        assert!(linha.starts_with("WAIT "), "{linha}");
        espera(|| env.sec.pair_window_open(), Duration::from_secs(2), "janela");
        let novo = Keypair::generate().unwrap();
        let (mut c, info) = noise(env.addr, &novo, "celular").unwrap();
        assert_eq!(status(&mut c).unwrap().0, AUTH_PENDING);
        linha.clear();
        r.read_line(&mut linha).unwrap();
        let partes: Vec<&str> = linha.trim_end().strip_prefix("REQ ").unwrap().split('\t').collect();
        assert_eq!(partes[1], "celular");
        assert_eq!(partes[2], fingerprint(novo.public()));
        let local = ps_secure::chaves::format_code(&pairing_code(&info.handshake_hash));
        assert_eq!(partes[3], local, "o código do `psd pair` e o do cliente têm de ser iguais");
        s.write_all(b"YES\n").unwrap();
        linha.clear();
        r.read_line(&mut linha).unwrap();
        assert_eq!(linha.trim_end(), "OK celular");
        assert_eq!(status(&mut c).unwrap().0, AUTH_PAIRED_NOW);
        assert_eq!(linhas(comando(&sock, "LIST")).len(), 3);
        let s1 = comando(&sock, "PAIR");
        espera(|| env.sec.pair_window_open(), Duration::from_secs(2), "janela 2");
        let l = linhas(comando(&sock, "PAIR"));
        assert_eq!(l, vec!["WAIT 5".to_string(), "BUSY".to_string()]);
        drop(s1);
        espera(|| !env.sec.pair_window_open(), Duration::from_secs(3), "a janela continuou aberta depois que o comando saiu");
        assert_eq!(linhas(comando(&sock, "REMOVE celular")), vec!["OK 1".to_string()]);
        assert!(linhas(comando(&sock, "REMOVE celular"))[0].starts_with("ERR"));
        assert_eq!(linhas(comando(&sock, "RESET")), vec!["OK 1".to_string()]);
        assert!(env.sec.first_use_open());
        assert!(linhas(comando(&sock, "LIXO"))[0].starts_with("ERR"));
    }

    #[test]
    fn soquete_velho_e_apagado_e_soquete_vivo_e_recusado() {
        let env = start("ctl2", timing(), &[]);
        let sock = env.dir.join("c.sock");
        drop(std::os::unix::net::UnixListener::bind(&sock).unwrap());
        ps_server::ctl::serve(env.sec.clone(), &sock).unwrap();
        assert!(!linhas(comando(&sock, "LIST")).is_empty());
        assert!(ps_server::ctl::serve(env.sec.clone(), &sock).is_err());
        assert!(!linhas(comando(&sock, "LIST")).is_empty());
    }
}
