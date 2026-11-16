
use std::io::{Cursor, Read, Write};
use std::sync::mpsc::{channel, Receiver, Sender};

use ps_secure::canal::{client_handshake, server_handshake, ChannelError, SecureReader, SecureWriter, MAX_PLAIN};
use ps_secure::noise::{CipherState, HandshakeState, Keypair, NoiseError, Role};

struct PipeR {
    rx: Receiver<Vec<u8>>,
    buf: Vec<u8>,
    pos: usize,
}
struct PipeW(Sender<Vec<u8>>);

fn pipe() -> (PipeR, PipeW) {
    let (tx, rx) = channel();
    (PipeR { rx, buf: Vec::new(), pos: 0 }, PipeW(tx))
}

impl Read for PipeR {
    fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
        while self.pos >= self.buf.len() {
            match self.rx.recv() {
                Ok(b) => {
                    self.buf = b;
                    self.pos = 0;
                }
                Err(_) => return Ok(0),
            }
        }
        let n = out.len().min(self.buf.len() - self.pos);
        out[..n].copy_from_slice(&self.buf[self.pos..self.pos + n]);
        self.pos += n;
        Ok(n)
    }
}

impl Write for PipeW {
    fn write(&mut self, d: &[u8]) -> std::io::Result<usize> {
        self.0.send(d.to_vec()).map_err(|_| std::io::Error::from(std::io::ErrorKind::BrokenPipe))?;
        Ok(d.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

type Ponta = (SecureReader<PipeR>, SecureWriter<PipeW>);

fn conectar() -> (Ponta, Ponta) {
    let (c2s_r, c2s_w) = pipe();
    let (s2c_r, s2c_w) = pipe();
    let skey = Keypair::generate().unwrap();
    let ckey = Keypair::generate().unwrap();
    let (spub, cpub) = (*skey.public(), *ckey.public());
    let srv = std::thread::spawn(move || {
        let (mut r, mut w) = (c2s_r, s2c_w);
        let mut magic = [0u8; 4];
        r.read_exact(&mut magic).unwrap();
        assert_eq!(&magic, b"PSN1");
        let (est, info) = server_handshake(&mut r, &mut w, &skey, "aparelho").unwrap();
        assert_eq!(info.name, "cliente");
        assert_eq!(info.static_key, cpub);
        (r, w, est)
    });
    let (mut r, mut w) = (s2c_r, c2s_w);
    let (est, info) = client_handshake(&mut r, &mut w, &ckey, "cliente", |k, n| {
        assert_eq!(k, &spub);
        assert_eq!(n, "aparelho");
        Ok(())
    })
    .unwrap();
    assert_eq!(info.static_key, spub);
    let (sr, sw, sest) = srv.join().unwrap();
    assert_eq!(est.handshake_hash, sest.handshake_hash);
    let cliente = (SecureReader::new(r, est.recv), SecureWriter::new(w, est.send));
    let servidor = (SecureReader::new(sr, sest.recv), SecureWriter::new(sw, sest.send));
    (cliente, servidor)
}

#[test]
fn ida_e_volta_pelo_canal_com_mensagens_de_varios_tamanhos() {
    let ((mut cr, mut cw), (mut sr, mut sw)) = conectar();
    for tam in [1usize, 15, 16, 1000, MAX_PLAIN - 1, MAX_PLAIN, MAX_PLAIN + 1, 3 * MAX_PLAIN + 7] {
        let dados: Vec<u8> = (0..tam).map(|i| (i * 31 % 251) as u8).collect();
        cw.write_all(&dados).unwrap();
        let mut volta = vec![0u8; tam];
        sr.read_exact(&mut volta).unwrap();
        assert_eq!(volta, dados, "cliente -> servidor, {tam} bytes");
        sw.write_all(&dados).unwrap();
        let mut volta = vec![0u8; tam];
        cr.read_exact(&mut volta).unwrap();
        assert_eq!(volta, dados, "servidor -> cliente, {tam} bytes");
    }
    drop(cw);
    let mut b = [0u8; 1];
    assert_eq!(sr.read(&mut b).unwrap(), 0);
}

#[test]
fn verificacao_do_cliente_recusa_e_nao_envia_o_terceiro_recado() {
    let (c2s_r, c2s_w) = pipe();
    let (s2c_r, s2c_w) = pipe();
    let skey = Keypair::generate().unwrap();
    let srv = std::thread::spawn(move || {
        let (mut r, mut w) = (c2s_r, s2c_w);
        let mut magic = [0u8; 4];
        r.read_exact(&mut magic).unwrap();
        server_handshake(&mut r, &mut w, &skey, "aparelho").map(|_| ())
    });
    let (mut r, mut w) = (s2c_r, c2s_w);
    let res = client_handshake(&mut r, &mut w, &Keypair::generate().unwrap(), "cliente", |_, _| Err("chave diferente da guardada".into()));
    assert!(matches!(res, Err(ChannelError::Rejected(m)) if m.contains("diferente")));
    drop((r, w));
    assert!(srv.join().unwrap().is_err());
}

fn aperto_adulterado(a: &Keypair, b: &Keypair, alvo: usize, bit: Option<usize>) -> bool {
    let mut i = HandshakeState::new(Role::Initiator, b"PSN1", a.clone());
    let mut r = HandshakeState::new(Role::Responder, b"PSN1", b.clone());
    for n in 0..3 {
        let (w, rd) = if n % 2 == 0 { (&mut i, &mut r) } else { (&mut r, &mut i) };
        let mut m = Vec::new();
        w.write_message(if n == 0 { b"" } else { b"nome" }, &mut m).unwrap();
        if let (true, Some(bit)) = (n == alvo, bit) {
            m[bit / 8] ^= 1 << (bit % 8);
        }
        if rd.read_message(&m).is_err() {
            return true;
        }
    }
    i.handshake_hash() != r.handshake_hash()
}

#[test]
fn um_bit_trocado_em_qualquer_recado_do_aperto_de_mao_e_detectado() {
    let a = Keypair::generate().unwrap();
    let b = Keypair::generate().unwrap();
    for (n, tam) in [(0usize, 32usize), (1, 32 + 48 + 4 + 16), (2, 48 + 4 + 16)] {
        assert!(!aperto_adulterado(&a, &b, n, None), "sem adulteração tem de passar");
        for bit in 0..tam * 8 {
            assert!(aperto_adulterado(&a, &b, n, Some(bit)), "bit {bit} do recado {n} trocado e não detectado");
        }
    }
}

#[test]
fn recado_truncado_ou_com_bytes_a_mais_e_recusado() {
    let mut i = HandshakeState::new(Role::Initiator, b"PSN1", Keypair::generate().unwrap());
    let mut m1 = Vec::new();
    i.write_message(b"", &mut m1).unwrap();
    for n in 0..32 {
        let mut r2 = HandshakeState::new(Role::Responder, b"PSN1", Keypair::generate().unwrap());
        assert_eq!(r2.read_message(&m1[..n]).unwrap_err(), NoiseError::BadLength);
    }
    let mut r = HandshakeState::new(Role::Responder, b"PSN1", Keypair::generate().unwrap());
    r.read_message(&m1).unwrap();
    let mut m2 = Vec::new();
    r.write_message(b"x", &mut m2).unwrap();
    for n in 0..m2.len() {
        let mut i2 = HandshakeState::new(Role::Initiator, b"PSN1", Keypair::generate().unwrap());
        let mut tmp = Vec::new();
        i2.write_message(b"", &mut tmp).unwrap();
        assert!(i2.read_message(&m2[..n]).is_err(), "corte em {n}");
        assert_eq!(i2.read_message(&m2).unwrap_err(), NoiseError::BadState);
    }
    let mut r3 = HandshakeState::new(Role::Responder, b"PSN1", Keypair::generate().unwrap());
    let mut longo = m1.clone();
    longo.extend_from_slice(b"sobra");
    let carga = r3.read_message(&longo).unwrap();
    assert_eq!(carga, b"sobra", "o primeiro recado leva carga em claro; bytes extras viram carga e ficam no hash");
}

fn par_de_cifras() -> (CipherState, CipherState) {
    let mut i = HandshakeState::new(Role::Initiator, b"t", Keypair::generate().unwrap());
    let mut r = HandshakeState::new(Role::Responder, b"t", Keypair::generate().unwrap());
    for n in 0..3 {
        let (w, rd) = if n % 2 == 0 { (&mut i, &mut r) } else { (&mut r, &mut i) };
        let mut m = Vec::new();
        w.write_message(b"", &mut m).unwrap();
        rd.read_message(&m).unwrap();
    }
    (i.finish().unwrap().send, r.finish().unwrap().recv)
}

fn registros(textos: &[&[u8]]) -> (Vec<Vec<u8>>, CipherState) {
    let (send, recv) = par_de_cifras();
    let mut w = SecureWriter::new(Vec::new(), send);
    let mut limites = Vec::new();
    let mut antes = 0;
    for t in textos {
        w.write_all(t).unwrap();
        antes += 2 + t.len() + 16;
        limites.push(antes);
    }
    let bytes = w.into_inner();
    let mut out = Vec::new();
    let mut ini = 0;
    for l in limites {
        out.push(bytes[ini..l].to_vec());
        ini = l;
    }
    (out, recv)
}

fn ler_tudo(recv: CipherState, bytes: Vec<u8>) -> (Vec<u8>, Option<std::io::Error>) {
    let mut r = SecureReader::new(Cursor::new(bytes), recv);
    let mut out = Vec::new();
    let mut buf = [0u8; 512];
    loop {
        match r.read(&mut buf) {
            Ok(0) => return (out, None),
            Ok(n) => out.extend_from_slice(&buf[..n]),
            Err(e) => return (out, Some(e)),
        }
    }
}

#[test]
fn registros_sem_adulteracao_chegam_inteiros() {
    let (regs, recv) = registros(&[b"primeiro", b"segundo registro", b"3"]);
    let (lido, erro) = ler_tudo(recv, regs.concat());
    assert!(erro.is_none());
    assert_eq!(lido, b"primeirosegundo registro3");
}

#[test]
fn um_bit_trocado_em_qualquer_posicao_de_um_registro_e_rejeitado() {
    let (regs, recv0) = registros(&[b"mensagem do protocolo", b"outra"]);
    let _ = recv0;
    for alvo in 0..regs.len() {
        let total_bits = regs[alvo].len() * 8;
        for bit in 0..total_bits {
            let (regs, recv) = registros(&[b"mensagem do protocolo", b"outra"]);
            let mut todos = regs.clone();
            todos[alvo][bit / 8] ^= 1 << (bit % 8);
            let esperado: Vec<u8> = regs.concat();
            let (lido, erro) = ler_tudo(recv, todos.concat());
            assert!(erro.is_some(), "registro {alvo}, bit {bit}: adulteração aceita");
            let _ = esperado;
            let inicio_do_alvo: usize = if alvo == 0 { 0 } else { b"mensagem do protocolo".len() };
            assert_eq!(lido.len(), inicio_do_alvo, "registro {alvo}, bit {bit}: entregou dados do registro adulterado");
        }
    }
}

#[test]
fn registro_repetido_ou_fora_de_ordem_e_rejeitado() {
    let (regs, recv) = registros(&[b"aaaa", b"bbbb", b"cccc"]);
    let (lido, erro) = ler_tudo(recv, [regs[0].clone(), regs[0].clone()].concat());
    assert_eq!(lido, b"aaaa");
    assert!(erro.is_some());
    let (regs, recv) = registros(&[b"aaaa", b"bbbb", b"cccc"]);
    let (lido, erro) = ler_tudo(recv, [regs[1].clone(), regs[0].clone()].concat());
    assert!(lido.is_empty());
    assert!(erro.is_some());
    let (regs, recv) = registros(&[b"aaaa", b"bbbb", b"cccc"]);
    let (lido, erro) = ler_tudo(recv, [regs[0].clone(), regs[2].clone()].concat());
    assert_eq!(lido, b"aaaa");
    assert!(erro.is_some());
}

#[test]
fn truncamento_nunca_entrega_registro_incompleto() {
    let (regs, _) = registros(&[b"aaaa", b"bbbbbbbb"]);
    let total: usize = regs.iter().map(|r| r.len()).sum();
    for corte in 0..total {
        let (regs, recv) = registros(&[b"aaaa", b"bbbbbbbb"]);
        let bytes: Vec<u8> = regs.concat()[..corte].to_vec();
        let (lido, erro) = ler_tudo(recv, bytes);
        let completos = if corte >= regs[0].len() { b"aaaa".len() } else { 0 };
        assert_eq!(lido.len(), completos, "corte em {corte}: entregou texto de registro incompleto");
        let na_fronteira = corte == 0 || corte == regs[0].len();
        assert_eq!(erro.is_none(), na_fronteira, "corte em {corte}");
    }
}

#[test]
fn comprimento_invalido_no_cabecalho_do_registro_encerra_o_canal() {
    let (_, recv) = registros(&[b"x"]);
    let (_, e) = ler_tudo(recv, vec![0, 15, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15]);
    assert!(e.is_some());
}

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn bytes(&mut self, n: usize) -> Vec<u8> {
        (0..n).map(|_| self.next() as u8).collect()
    }
}

#[test]
fn dez_mil_entradas_aleatorias_no_leitor_de_handshake_nao_causam_panico() {
    let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
    let key = Keypair::generate().unwrap();
    for _ in 0..10_000 {
        let n = (rng.next() % 1300) as usize;
        let mut entrada = rng.bytes(n);
        if rng.next().is_multiple_of(2) && entrada.len() >= 2 {
            let l = (entrada.len() - 2).min(1024) as u16;
            entrada[..2].copy_from_slice(&l.to_be_bytes());
        }
        let mut w = Vec::new();
        let r = server_handshake(&mut Cursor::new(entrada), &mut w, &key, "aparelho");
        assert!(r.is_err());
    }
}

#[test]
fn dez_mil_entradas_aleatorias_no_leitor_de_registros_nao_causam_panico() {
    let mut rng = Rng(0xD1B5_4A32_D192_ED03);
    for _ in 0..10_000 {
        let (_, recv) = par_de_cifras();
        let n = (rng.next() % 300) as usize;
        let mut entrada = rng.bytes(n);
        if rng.next().is_multiple_of(2) && entrada.len() >= 2 {
            let l = (entrada.len() - 2).max(16) as u16;
            entrada[..2].copy_from_slice(&l.to_be_bytes());
        }
        let (lido, erro) = ler_tudo(recv, entrada);
        assert!(lido.is_empty());
        let _ = erro;
    }
}

#[test]
fn nome_longo_demais_no_aperto_de_mao_e_recusado_antes_de_enviar() {
    let (mut r, mut w) = (Cursor::new(Vec::new()), Vec::new());
    let res = client_handshake(&mut r, &mut w, &Keypair::generate().unwrap(), &"x".repeat(64), |_, _| Ok(()));
    assert!(matches!(res, Err(ChannelError::Protocol(_))));
    assert!(w.is_empty());
}

#[test]
#[ignore = "medição; rodar com --release e --ignored"]
fn vazao_do_canal_em_laco_local() {
    use std::net::{TcpListener, TcpStream};
    let total: usize = 256 * 1024 * 1024;
    let tam = 64 * 1024;
    let lis = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = lis.local_addr().unwrap();
    let (send, recv) = par_de_cifras();
    let srv = std::thread::spawn(move || {
        let (s, _) = lis.accept().unwrap();
        let mut r = SecureReader::new(s, recv);
        let mut buf = vec![0u8; tam];
        let mut n = 0usize;
        while n < total {
            let k = r.read(&mut buf).unwrap();
            assert!(k > 0);
            n += k;
        }
        n
    });
    let s = TcpStream::connect(addr).unwrap();
    s.set_nodelay(true).unwrap();
    let mut w = SecureWriter::new(s, send);
    let dados = vec![0x5au8; tam];
    let t0 = std::time::Instant::now();
    for _ in 0..total / tam {
        w.write_all(&dados).unwrap();
    }
    let lido = srv.join().unwrap();
    let dt = t0.elapsed().as_secs_f64();
    println!("vazão do canal cifrado: {:.1} MB/s ({} MiB em {:.2} s)", lido as f64 / dt / 1e6, lido / (1024 * 1024), dt);
}
