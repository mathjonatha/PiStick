//! Teste de integração do anunciante mDNS: sobe numa porta UDP livre, no laço, pergunta por
//! unicast (porta de origem diferente de 5353) e confere a resposta.

#![cfg(target_os = "linux")]

use std::net::{Ipv4Addr, UdpSocket};
use std::time::{Duration, Instant};

use ps_discovery::{build_query, parse_response};
use ps_server::discovery::{Announcer, Config};

fn config(name: &str, tcp_port: u16) -> Config {
    Config { name: Some(name.into()), port: tcp_port, mdns_port: 0, include_loopback: true, pair: None }
}

/// Pergunta por unicast ao anunciante e espera a resposta por até 3 s.
fn perguntar(porta_udp: u16, id: u16) -> Option<Vec<u8>> {
    let s = UdpSocket::bind("127.0.0.1:0").unwrap();
    s.set_read_timeout(Some(Duration::from_millis(200))).unwrap();
    let fim = Instant::now() + Duration::from_secs(3);
    let mut buf = [0u8; 9100];
    while Instant::now() < fim {
        s.send_to(&build_query(id), (Ipv4Addr::LOCALHOST, porta_udp)).unwrap();
        if let Ok((n, _)) = s.recv_from(&mut buf) {
            return Some(buf[..n].to_vec());
        }
    }
    None
}

#[test]
fn responde_pergunta_unicast_com_o_servico() {
    let a = Announcer::start(config("pi-teste", 7711)).expect("o soquete abre");
    let pkt = perguntar(a.local_port(), 0xABCD).expect("resposta unicast");
    assert_eq!(u16::from_be_bytes([pkt[0], pkt[1]]), 0xABCD, "a resposta unicast repete o ID");
    let achados = parse_response(&pkt);
    assert_eq!(achados.len(), 1, "{achados:?}");
    let f = &achados[0];
    assert_eq!(f.instance, "pi-teste");
    assert_eq!(f.port, 7711);
    // Com `include_loopback` entram também as interfaces reais da máquina; o laço está entre elas.
    assert!(f.addrs.contains(&Ipv4Addr::LOCALHOST), "{:?}", f.addrs);
    assert_eq!(f.proto_version, Some(ps_proto::VERSION));
}

#[test]
fn ignora_lixo_e_pergunta_de_outro_servico() {
    let a = Announcer::start(config("pi-lixo", 7712)).expect("o soquete abre");
    let s = UdpSocket::bind("127.0.0.1:0").unwrap();
    s.set_read_timeout(Some(Duration::from_millis(400))).unwrap();
    // Espera o anunciante listar as interfaces (a primeira volta do laço).
    assert!(perguntar(a.local_port(), 1).is_some());
    s.send_to(&[1, 2, 3], (Ipv4Addr::LOCALHOST, a.local_port())).unwrap();
    s.send_to(&[0xFF; 600], (Ipv4Addr::LOCALHOST, a.local_port())).unwrap();
    let mut buf = [0u8; 9100];
    assert!(s.recv_from(&mut buf).is_err(), "lixo não pode ter resposta");
    // E o anunciante continua respondendo.
    assert!(perguntar(a.local_port(), 2).is_some());
}

#[test]
fn encerrar_o_anunciante_nao_trava() {
    let t0 = Instant::now();
    drop(Announcer::start(config("pi-fim", 7713)).unwrap());
    assert!(t0.elapsed() < Duration::from_secs(2));
}

#[test]
fn anuncia_sec_e_pair_conforme_a_consulta_e_reanuncia_quando_muda() {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;
    let aberto = Arc::new(AtomicBool::new(true));
    let a2 = aberto.clone();
    let mut cfg = config("pi-sec", 7714);
    cfg.pair = Some(ps_server::discovery::PairFlag(Arc::new(move || a2.load(Ordering::SeqCst))));
    let a = Announcer::start(cfg).expect("o soquete abre");
    let f = &parse_response(&perguntar(a.local_port(), 7).expect("resposta"))[0].clone();
    assert_eq!(f.secure, Some(true));
    assert_eq!(f.pair_open, Some(true));
    // O primeiro uso fecha; no máximo 5 s depois (a volta de atualização) a resposta já diz `closed`.
    aberto.store(false, Ordering::SeqCst);
    let fim = Instant::now() + Duration::from_secs(8);
    loop {
        let f = parse_response(&perguntar(a.local_port(), 8).expect("resposta")).remove(0);
        if f.pair_open == Some(false) {
            break;
        }
        assert!(Instant::now() < fim, "o TXT não passou a dizer pair=closed");
        std::thread::sleep(Duration::from_millis(300));
    }
}
