//! Transportes: um fluxo de bytes confiável, ordenado e bidirecional.
//!
//! O protocolo (`ps-proto`) só precisa de `Read` e `Write`. Este crate define o
//! trait [`Transport`], que entrega as duas metades separadas (uma para a thread
//! que lê, outra para a que escreve) e um [`Closer`] para derrubar a conexão de
//! qualquer thread. Hoje há TCP; depois haverá USB bulk com o mesmo trait.

use std::io::{self, Read, Write};
use std::net::{Shutdown, SocketAddr, TcpListener, TcpStream, ToSocketAddrs};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

/// Fecha a conexão de qualquer thread; desbloqueia leituras e escritas pendentes.
#[derive(Clone)]
pub struct Closer(Arc<dyn Fn() + Send + Sync>);

impl Closer {
    pub fn new(f: impl Fn() + Send + Sync + 'static) -> Self {
        Closer(Arc::new(f))
    }

    pub fn close(&self) {
        (self.0)();
    }
}

/// Roda `f` e, se ela passar de `timeout`, fecha a conexão por `closer` (o que desbloqueia leituras e escritas
/// pendentes de `f`). Devolve o resultado de `f` e se o prazo estourou. Serve para o limite de tempo do
/// aperto de mão e da abertura.
pub fn with_deadline<T>(closer: &Closer, timeout: Duration, f: impl FnOnce() -> T) -> (T, bool) {
    let (done_tx, done_rx) = std::sync::mpsc::channel::<()>();
    let c = closer.clone();
    let vigia = std::thread::spawn(move || {
        if let Err(std::sync::mpsc::RecvTimeoutError::Timeout) = done_rx.recv_timeout(timeout) {
            c.close();
            return true;
        }
        false
    });
    let r = f();
    let _ = done_tx.send(());
    let expirou = vigia.join().unwrap_or(false);
    (r, expirou)
}

/// As duas metades de uma conexão, mais o meio de fechá-la.
pub struct Split<R, W> {
    pub reader: R,
    pub writer: W,
    pub closer: Closer,
}

/// Fluxo de bytes confiável e bidirecional.
pub trait Transport: Send + 'static {
    type Reader: Read + Send + 'static;
    type Writer: Write + Send + 'static;

    /// Separa em leitor e escritor, que podem viver em threads diferentes.
    fn split(self) -> io::Result<Split<Self::Reader, Self::Writer>>;

    /// Descrição legível da outra ponta, para logs.
    fn peer(&self) -> String;

    /// Limita a fila de envio do sistema, para o atraso não se esconder no kernel.
    /// Transportes sem esse conceito ignoram.
    fn set_send_queue_limit(&self, _bytes: usize) -> io::Result<()> {
        Ok(())
    }
}

/// Quem aceita conexões de entrada.
pub trait Listener {
    type Conn: Transport;

    /// Bloqueia até chegar uma conexão.
    fn accept(&self) -> io::Result<Self::Conn>;

    /// Descrição legível do endereço em que escuta.
    fn local(&self) -> String;
}

/// Transporte TCP com `TCP_NODELAY`.
pub struct TcpTransport {
    stream: TcpStream,
}

impl TcpTransport {
    pub fn from_stream(stream: TcpStream) -> io::Result<Self> {
        stream.set_nodelay(true)?;
        Ok(TcpTransport { stream })
    }

    /// O soquete por baixo (para `peek`, endereço do par e clones).
    pub fn stream(&self) -> &TcpStream {
        &self.stream
    }

    /// Endereço IP da outra ponta, se conhecido.
    pub fn peer_ip(&self) -> Option<std::net::IpAddr> {
        self.stream.peer_addr().ok().map(|a| a.ip())
    }

    /// Conecta a `addr` (por exemplo `"127.0.0.1:7700"`).
    pub fn connect(addr: impl ToSocketAddrs) -> io::Result<Self> {
        Self::from_stream(TcpStream::connect(addr)?)
    }
}

/// Leitor TCP que acorda periodicamente para ver se a conexão foi fechada por um [`Closer`].
/// No Windows, `shutdown` feito por outra thread não desbloqueia um `recv` em andamento
/// (verificado), então a leitura usa um tempo limite curto e repete enquanto não fechou.
pub struct TcpReader {
    stream: TcpStream,
    closed: Arc<AtomicBool>,
}

impl Read for TcpReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        loop {
            if self.closed.load(Ordering::Acquire) {
                return Ok(0);
            }
            match self.stream.read(buf) {
                Err(e) if matches!(e.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut) => continue,
                r => return r,
            }
        }
    }
}

impl Transport for TcpTransport {
    type Reader = TcpReader;
    type Writer = TcpStream;

    fn split(self) -> io::Result<Split<TcpReader, TcpStream>> {
        let writer = self.stream.try_clone()?;
        let for_close = self.stream.try_clone()?;
        self.stream.set_read_timeout(Some(Duration::from_millis(100)))?;
        let closed = Arc::new(AtomicBool::new(false));
        let flag = closed.clone();
        Ok(Split {
            reader: TcpReader { stream: self.stream, closed },
            writer,
            closer: Closer::new(move || {
                flag.store(true, Ordering::Release);
                let _ = for_close.shutdown(Shutdown::Both);
            }),
        })
    }

    /// `SO_SNDBUF`. (`TCP_NOTSENT_LOWAT` não existe em `std` nem em `socket2`; ficou de fora.)
    fn set_send_queue_limit(&self, bytes: usize) -> io::Result<()> {
        socket2::SockRef::from(&self.stream).set_send_buffer_size(bytes)
    }

    fn peer(&self) -> String {
        match self.stream.peer_addr() {
            Ok(a) => a.to_string(),
            Err(_) => "desconhecido".to_string(),
        }
    }
}

/// Listener TCP.
pub struct TcpAcceptor {
    listener: TcpListener,
}

impl TcpAcceptor {
    pub fn bind(addr: impl ToSocketAddrs) -> io::Result<Self> {
        Ok(TcpAcceptor { listener: TcpListener::bind(addr)? })
    }

    pub fn from_listener(listener: TcpListener) -> Self {
        TcpAcceptor { listener }
    }

    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        self.listener.local_addr()
    }

    /// Outra alça para o mesmo soquete de escuta (para uma thread que só aceita conexões).
    pub fn try_clone_listener(&self) -> io::Result<TcpListener> {
        self.listener.try_clone()
    }
}

impl Listener for TcpAcceptor {
    type Conn = TcpTransport;

    fn accept(&self) -> io::Result<TcpTransport> {
        let (stream, _) = self.listener.accept()?;
        TcpTransport::from_stream(stream)
    }

    fn local(&self) -> String {
        match self.listener.local_addr() {
            Ok(a) => a.to_string(),
            Err(_) => "desconhecido".to_string(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tcp_ida_e_volta_com_nodelay() {
        let acc = TcpAcceptor::bind("127.0.0.1:0").unwrap();
        let addr = acc.local_addr().unwrap();
        let servidor = std::thread::spawn(move || {
            let conn = acc.accept().unwrap();
            assert!(conn.stream.nodelay().unwrap());
            let Split { mut reader, mut writer, .. } = conn.split().unwrap();
            let mut buf = [0u8; 5];
            reader.read_exact(&mut buf).unwrap();
            writer.write_all(&buf).unwrap();
        });
        let cli = TcpTransport::connect(addr).unwrap();
        assert!(cli.stream.nodelay().unwrap());
        let Split { mut reader, mut writer, .. } = cli.split().unwrap();
        writer.write_all(b"ola!!").unwrap();
        let mut eco = [0u8; 5];
        reader.read_exact(&mut eco).unwrap();
        assert_eq!(&eco, b"ola!!");
        servidor.join().unwrap();
    }

    #[test]
    fn prazo_fecha_a_conexao_e_avisa() {
        let acc = TcpAcceptor::bind("127.0.0.1:0").unwrap();
        let addr = acc.local_addr().unwrap();
        let t = std::thread::spawn(move || {
            let c = acc.accept().unwrap();
            std::thread::sleep(Duration::from_secs(2));
            drop(c);
        });
        let Split { mut reader, closer, .. } = TcpTransport::connect(addr).unwrap().split().unwrap();
        let t0 = std::time::Instant::now();
        let (r, expirou) = with_deadline(&closer, Duration::from_millis(200), || reader.read(&mut [0u8; 1]));
        assert!(expirou);
        assert!(matches!(r, Ok(0) | Err(_)));
        assert!(t0.elapsed() < Duration::from_secs(1));
        // Dentro do prazo: não expira.
        let (_, expirou) = with_deadline(&closer, Duration::from_secs(5), || 1 + 1);
        assert!(!expirou);
        t.join().unwrap();
    }

    #[test]
    fn limite_da_fila_de_envio_e_aceito() {
        let acc = TcpAcceptor::bind("127.0.0.1:0").unwrap();
        let addr = acc.local_addr().unwrap();
        let t = std::thread::spawn(move || {
            let c = acc.accept().unwrap();
            c.set_send_queue_limit(256 * 1024).unwrap();
            let sz = socket2::SockRef::from(&c.stream).send_buffer_size().unwrap();
            // O Linux dobra o valor pedido; o Windows usa o pedido. Em todo caso, bem abaixo de 1 MiB.
            assert!((64 * 1024..=1024 * 1024).contains(&sz), "SO_SNDBUF = {sz}");
        });
        let _c = TcpTransport::connect(addr).unwrap();
        t.join().unwrap();
    }

    #[test]
    fn closer_desbloqueia_a_leitura() {
        let acc = TcpAcceptor::bind("127.0.0.1:0").unwrap();
        let addr = acc.local_addr().unwrap();
        let t = std::thread::spawn(move || {
            let c = acc.accept().unwrap();
            std::thread::sleep(std::time::Duration::from_secs(3));
            drop(c);
        });
        let Split { mut reader, closer, .. } = TcpTransport::connect(addr).unwrap().split().unwrap();
        let t2 = std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(50));
            closer.close();
        });
        let mut b = [0u8; 1];
        let t0 = std::time::Instant::now();
        // Retorna (0 bytes ou erro) quando fechado, em vez de bloquear.
        let r = reader.read(&mut b);
        assert!(matches!(r, Ok(0) | Err(_)));
        assert!(t0.elapsed() < std::time::Duration::from_secs(2), "o Closer não desbloqueou a leitura");
        t2.join().unwrap();
        t.join().unwrap();
    }
}
