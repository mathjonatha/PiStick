//! Pareamento e canal cifrado vistos do cliente (`ps_client::secure::open`) contra um `psd` de verdade
//! (`serve_secure`) no laço local, e a prova do intermediário (aceite 5 do marco 9A). Chaves geradas a cada
//! teste, em pastas temporárias; `force_noise` faz o laço local usar o canal cifrado.

use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use ps_client::secure::{forget, known_devices, open, ConnectError, Connected, SecurityOptions};
use ps_proto::{read_hello, read_message, write_message, Message, AUTH_PENDING};
use ps_secure::canal::{client_handshake, server_handshake};
use ps_secure::chaves::{fingerprint, format_code, pairing_code, PeerList};
use ps_secure::{Keypair, SecureConn};
use ps_server::secure::{serve_secure, PairWait, Security, Timing};
use ps_server::{synthetic, ServerOptions, WindowSource};
use ps_transport::{Split, TcpAcceptor, TcpReader, TcpTransport, Transport};

fn tmpdir(nome: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("ps-client-pareamento-{}-{nome}", std::process::id()));
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
    let t = Timing { first_use_window: Duration::from_secs(60), pair_timeout: Duration::from_secs(5), handshake_timeout: Duration::from_secs(2), unknown_max: 50, unknown_per: Duration::from_secs(60) };
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

fn opcoes(nome: &str, dir: &Path) -> SecurityOptions {
    SecurityOptions { insecure: false, force_noise: true, client_name: nome.into(), keys_dir: dir.to_path_buf(), on_code: None }
}

fn abrir(addr: SocketAddr, so: &SecurityOptions) -> Result<Connected, ConnectError> {
    open(TcpTransport::connect(addr).unwrap(), &addr.to_string(), so)
}

fn secure(c: Connected) -> SecureConn<TcpReader, TcpStream> {
    match c {
        Connected::Secure(s) => *s,
        Connected::Plain(_) => panic!("esperava canal cifrado"),
    }
}

fn sessao(c: &mut SecureConn<TcpReader, TcpStream>) {
    write_message(&mut c.writer, &Message::hello("psc de teste")).unwrap();
    let (_, nome) = read_hello(&mut c.reader).unwrap();
    assert!(nome.contains("psd"));
}

fn espera_janela(p: &Pi) {
    while !p.sec.pair_window_open() {
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn espera_codigo(visto: &Arc<Mutex<Option<String>>>) -> String {
    for _ in 0..500 {
        if let Some(c) = visto.lock().unwrap().clone() {
            return c;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    panic!("o cliente não mostrou código de pareamento");
}

#[test]
fn primeiro_uso_guarda_o_aparelho_e_a_segunda_conexao_reconhece() {
    let p = pi("primeiro", &[]);
    let dir = tmpdir("c-primeiro");
    let so = opcoes("notebook", &dir);
    let mut c = secure(abrir(p.addr, &so).unwrap());
    sessao(&mut c);
    drop(c);
    let conhecidos = known_devices(&dir).unwrap();
    assert_eq!(conhecidos.len(), 1);
    assert_eq!(conhecidos[0].0, "pi-teste");
    assert_eq!(conhecidos[0].1, fingerprint(&p.sec.device_public()));
    assert_eq!(conhecidos[0].2, p.addr.to_string());
    // A chave do cliente persistiu (mesma identidade na segunda vez: continua pareado).
    let arq = std::fs::read(dir.join("chave-do-cliente")).unwrap();
    #[cfg(windows)]
    {
        // Marco 9B: no Windows o arquivo é protegido pela DPAPI e não contém a chave em claro.
        let (kp, nova) = ps_client::keystore::load_or_create_client_key(&dir.join("chave-do-cliente")).unwrap();
        assert!(!nova);
        assert!(arq.len() > 32 && arq.starts_with(ps_client::keystore::MAGIC), "o arquivo deveria estar protegido");
        assert!(!arq.windows(32).any(|j| j == kp.secret_bytes()), "a chave não pode estar em claro no arquivo");
    }
    #[cfg(not(windows))]
    assert_eq!(arq.len(), 32);
    let mut c = secure(abrir(p.addr, &so).unwrap());
    sessao(&mut c);
    assert_eq!(p.sec.list().len(), 1, "a segunda conexão não pareia de novo");
}

#[test]
fn laco_local_usa_texto_claro_salvo_com_force_noise() {
    let p = pi("claro", &[]);
    let dir = tmpdir("c-claro");
    let mut so = opcoes("notebook", &dir);
    so.force_noise = false;
    assert!(matches!(abrir(p.addr, &so).unwrap(), Connected::Plain(_)));
    assert!(known_devices(&dir).unwrap().is_empty());
    assert!(!dir.join("chave-do-cliente").exists(), "texto claro não cria chave");
    // `--insecure` também é texto claro, mesmo com Noise forçado.
    so.insecure = true;
    so.force_noise = true;
    assert!(matches!(abrir(p.addr, &so).unwrap(), Connected::Plain(_)));
}

#[test]
fn cliente_desconhecido_depois_do_primeiro_uso_e_recusado_com_saida_4() {
    let dono = Keypair::generate().unwrap();
    let p = pi("recusa", &[(&dono, "dono")]);
    let dir = tmpdir("c-recusa");
    let e = abrir(p.addr, &opcoes("intruso", &dir)).err().expect("recusado");
    assert!(matches!(e, ConnectError::Refused(_)), "{e:?}");
    assert_eq!(e.exit_code(), 4);
    assert!(e.message().contains("psd pair"), "a mensagem precisa ensinar a parear: {e}");
    assert!(known_devices(&dir).unwrap().is_empty(), "recusado não guarda o aparelho");
}

#[test]
fn aprovacao_no_aparelho_mostra_o_mesmo_codigo_nos_dois_lados() {
    let dono = Keypair::generate().unwrap();
    let p = pi("aprova", &[(&dono, "dono")]);
    let dir = tmpdir("c-aprova");
    let visto: Arc<Mutex<Option<String>>> = Arc::default();
    let v2 = visto.clone();
    let mut so = opcoes("celular", &dir);
    so.on_code = Some(Arc::new(move |c| *v2.lock().unwrap() = Some(c.to_string())));
    let sec = p.sec.clone();
    let esperando = std::thread::spawn(move || sec.open_pair_window(Duration::from_secs(10), &|| true));
    espera_janela(&p);
    let addr = p.addr;
    let cliente = std::thread::spawn(move || abrir(addr, &so));
    let PairWait::Request(req) = esperando.join().unwrap() else { panic!("sem pedido") };
    // O que o `psd pair` mostra ao operador e o que o cliente mostra ao usuário são o mesmo código.
    assert_eq!(espera_codigo(&visto), format_code(&req.code));
    p.sec.decide(req.id, true).unwrap();
    let mut c = secure(cliente.join().unwrap().unwrap());
    sessao(&mut c);
    assert_eq!(known_devices(&dir).unwrap().len(), 1);
}

// ---- intermediário ----

/// Um proxy que termina o Noise dos dois lados com chave própria, fazendo-se passar pelo aparelho (mesmo
/// nome). Com `reescreve`, troca o código do `AUTH_STATUS` pelo que o cliente calcula (o melhor que um
/// intermediário pode fazer para disfarçar). Devolve o endereço dele e o resultado da thread.
fn proxy(real: SocketAddr, reescreve: bool) -> (SocketAddr, JoinHandle<Result<(), String>>) {
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = l.local_addr().unwrap();
    let h = std::thread::spawn(move || {
        let (s, _) = l.accept().map_err(|e| e.to_string())?;
        let t = TcpTransport::from_stream(s).map_err(|e| e.to_string())?;
        let peer = t.peer();
        let Split { mut reader, mut writer, closer } = t.split().map_err(|e| e.to_string())?;
        let mut magic = [0u8; 4];
        std::io::Read::read_exact(&mut reader, &mut magic).map_err(|e| e.to_string())?;
        let chave_do_proxy = Keypair::generate().unwrap();
        let (est, info_c) = server_handshake(&mut reader, &mut writer, &chave_do_proxy, "pi-teste").map_err(|e| format!("lado do cliente: {e}"))?;
        let mut down = SecureConn::new(reader, writer, closer, est.send, est.recv, peer);
        // Do outro lado, o proxy é um cliente desconhecido do aparelho de verdade.
        let t = TcpTransport::connect(real).map_err(|e| e.to_string())?;
        let peer = t.peer();
        let Split { mut reader, mut writer, closer } = t.split().map_err(|e| e.to_string())?;
        let (est, _) = client_handshake(&mut reader, &mut writer, &Keypair::generate().unwrap(), "proxy", |_, _| Ok(())).map_err(|e| format!("lado do aparelho: {e}"))?;
        let mut up = SecureConn::new(reader, writer, closer, est.send, est.recv, peer);
        loop {
            let m = read_message(&mut up.reader).map_err(|e| e.to_string())?;
            let m = match m {
                Message::AuthStatus { state: AUTH_PENDING, .. } if reescreve => Message::AuthStatus { state: AUTH_PENDING, code: pairing_code(&info_c.handshake_hash) },
                outra => outra,
            };
            let fim = !matches!(m, Message::AuthStatus { state: AUTH_PENDING, .. });
            write_message(&mut down.writer, &m).map_err(|e| e.to_string())?;
            if fim {
                return Ok(());
            }
        }
    });
    (addr, h)
}

#[test]
fn intermediario_que_termina_o_noise_dos_dois_lados_tem_codigo_diferente() {
    let dono = Keypair::generate().unwrap();
    let p = pi("mitm-codigo", &[(&dono, "dono")]);
    let sec = p.sec.clone();
    let esperando = std::thread::spawn(move || sec.open_pair_window(Duration::from_secs(10), &|| true));
    espera_janela(&p);
    let (paddr, ph) = proxy(p.addr, true);
    let dir = tmpdir("c-mitm-codigo");
    let visto: Arc<Mutex<Option<String>>> = Arc::default();
    let v2 = visto.clone();
    let mut so = opcoes("notebook", &dir);
    so.on_code = Some(Arc::new(move |c| *v2.lock().unwrap() = Some(c.to_string())));
    let cliente = std::thread::spawn(move || abrir(paddr, &so));
    let PairWait::Request(req) = esperando.join().unwrap() else { panic!("o aparelho não recebeu pedido") };
    let no_pi = format_code(&req.code);
    let no_cliente = espera_codigo(&visto);
    assert_ne!(no_cliente, no_pi, "com um intermediário, os códigos de 6 dígitos têm de diferir");
    // O operador vê que não bate e nega.
    p.sec.decide(req.id, false).unwrap();
    let r = cliente.join().unwrap();
    assert!(matches!(r, Err(ConnectError::Refused(_))), "a conexão negada não pode virar sessão");
    ph.join().unwrap().unwrap();
    assert_eq!(p.sec.list().len(), 1, "o aparelho não pareou o intermediário");
}

#[test]
fn intermediario_que_repassa_o_codigo_do_aparelho_e_denunciado_pelo_cliente() {
    let dono = Keypair::generate().unwrap();
    let p = pi("mitm-repassa", &[(&dono, "dono")]);
    let sec = p.sec.clone();
    let esperando = std::thread::spawn(move || sec.open_pair_window(Duration::from_secs(10), &|| true));
    espera_janela(&p);
    let (paddr, ph) = proxy(p.addr, false);
    let dir = tmpdir("c-mitm-repassa");
    let so = opcoes("notebook", &dir);
    let cliente = std::thread::spawn(move || abrir(paddr, &so));
    let PairWait::Request(req) = esperando.join().unwrap() else { panic!() };
    // O cliente calcula o código sozinho; o do aparelho (repassado) não bate: aborta.
    let r = cliente.join().unwrap();
    assert!(matches!(r, Err(ConnectError::KeyMismatch(ref m)) if m.contains("no meio")), "{:?}", r.err());
    let _ = p.sec.decide(req.id, false);
    let _ = ph.join();
}

#[test]
fn cliente_que_ja_conhece_o_aparelho_recusa_a_chave_do_intermediario() {
    let p = pi("mitm-chave", &[]);
    let dir = tmpdir("c-mitm-chave");
    let so = opcoes("notebook", &dir);
    // Primeira conexão, direto: o cliente guarda a chave verdadeira do aparelho (e é pareado).
    drop(secure(abrir(p.addr, &so).unwrap()));
    let guardada = known_devices(&dir).unwrap();
    assert_eq!(guardada.len(), 1);
    // Agora pelo proxy, que apresenta o mesmo nome com chave própria.
    let (paddr, ph) = proxy(p.addr, true);
    let e = abrir(paddr, &so).err().expect("o cliente tem de recusar");
    assert!(matches!(e, ConnectError::KeyMismatch(_)), "{e:?}");
    assert_eq!(e.exit_code(), 5);
    assert!(e.message().contains("diferente") && e.message().contains("--forget"), "{e}");
    // O cliente não chegou a enviar o terceiro recado: o intermediário nem soube quem era o cliente.
    assert!(ph.join().unwrap().is_err(), "o proxy não pode ter concluído o aperto de mão com o cliente");
    // E a lista não mudou.
    assert_eq!(known_devices(&dir).unwrap(), guardada);
}

#[test]
fn esquecer_o_aparelho_permite_aceitar_a_chave_nova_com_o_mesmo_nome() {
    let a = pi("esquece-a", &[]);
    let dir = tmpdir("c-esquece");
    let so = opcoes("notebook", &dir);
    drop(secure(abrir(a.addr, &so).unwrap()));
    // Outro aparelho com o mesmo nome (outra chave): recusado até esquecer o anterior.
    let b = pi("esquece-b", &[]);
    assert!(matches!(abrir(b.addr, &so), Err(ConnectError::KeyMismatch(_))));
    assert_eq!(forget(&dir, "nao-existe").unwrap(), 0);
    assert_eq!(forget(&dir, "PI-TESTE").unwrap(), 1);
    assert!(known_devices(&dir).unwrap().is_empty());
    drop(secure(abrir(b.addr, &so).unwrap()));
    assert_eq!(known_devices(&dir).unwrap()[0].1, fingerprint(&b.sec.device_public()));
}

#[test]
fn chave_antiga_em_claro_migra_sozinha_e_o_cliente_continua_pareado() {
    // Marco 9B: um arquivo do formato antigo (32 bytes em claro) de um cliente já pareado.
    let kp = Keypair::generate().unwrap();
    let p = pi("migra", &[(&kp, "notebook")]);
    let dir = tmpdir("c-migra");
    std::fs::write(dir.join("chave-do-cliente"), kp.secret_bytes()).unwrap();
    let so = opcoes("notebook", &dir);
    for _ in 0..2 {
        let mut c = secure(abrir(p.addr, &so).expect("continua pareado, sem novo pareamento"));
        sessao(&mut c);
    }
    assert_eq!(p.sec.list().len(), 1, "nenhum pareamento novo");
    let arq = std::fs::read(dir.join("chave-do-cliente")).unwrap();
    #[cfg(windows)]
    {
        assert!(arq.starts_with(ps_client::keystore::MAGIC), "migrou para o formato protegido");
        assert!(!arq.windows(32).any(|j| j == kp.secret_bytes()), "a chave não pode ficar em claro");
    }
    #[cfg(not(windows))]
    assert_eq!(arq.as_slice(), kp.secret_bytes());
}

#[cfg(windows)]
#[test]
fn arquivo_protegido_adulterado_nao_conecta_e_nao_gera_chave_nova() {
    let p = pi("adulterado", &[]);
    let dir = tmpdir("c-adulterado");
    let so = opcoes("notebook", &dir);
    let mut c = secure(abrir(p.addr, &so).unwrap());
    sessao(&mut c);
    drop(c);
    let arq = dir.join("chave-do-cliente");
    let mut ruim = std::fs::read(&arq).unwrap();
    let meio = ruim.len() / 2 + 4;
    ruim[meio] ^= 0x10;
    std::fs::write(&arq, &ruim).unwrap();
    let e = abrir(p.addr, &so).err().expect("deveria recusar o arquivo adulterado");
    assert!(matches!(e, ConnectError::Other(_)), "{e}");
    assert!(e.message().contains("protegido pelo Windows") && e.message().contains("Nada foi sobrescrito"), "{e}");
    assert_eq!(std::fs::read(&arq).unwrap(), ruim, "o arquivo ruim continua onde estava");
    assert_eq!(std::fs::read(dir.join("chave-do-cliente.ilegivel")).unwrap(), ruim, "e uma cópia ficou ao lado");
    assert_eq!(p.sec.list().len(), 1, "nenhuma chave nova, nenhum pareamento novo");
}
