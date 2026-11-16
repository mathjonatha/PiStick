//! Canal cifrado sobre qualquer `Read + Write`: abertura, aperto de mão e registros de transporte
//! (formato no fio em `docs/21` e `docs/05`).
//!
//! ```text
//! Abertura (cliente -> servidor, em claro): "PSN1"
//! Aperto de mão: três recados, cada um  u16 len (big-endian) + recado Noise
//! Transporte: registros  u16 len (big-endian, 16..=65535) + texto cifrado (dados + 16 bytes de etiqueta)
//! ```

use std::io::{self, Read, Write};

use crate::noise::{CipherState, Established, HandshakeState, Keypair, NoiseError, Role, MAX_MESSAGE, TAG_LEN};

/// Os 4 bytes de abertura, também usados como prólogo do Noise.
pub const MAGIC: &[u8; 4] = b"PSN1";
/// Recado de aperto de mão maior que isto: fecha a conexão.
pub const MAX_HANDSHAKE_MSG: usize = 1024;
/// Nome (de aparelho ou cliente) na carga do aperto de mão.
pub const MAX_NAME: usize = 63;
/// Dados em claro por registro: 65535 menos a etiqueta.
pub const MAX_PLAIN: usize = MAX_MESSAGE - TAG_LEN;

/// Falha do canal.
#[derive(Debug)]
pub enum ChannelError {
    Io(io::Error),
    Noise(NoiseError),
    /// A outra ponta violou o formato (tamanho fora do limite, nome inválido...).
    Protocol(&'static str),
    /// O chamador recusou a outra ponta (por exemplo, chave de aparelho diferente da conhecida).
    Rejected(String),
}

impl std::fmt::Display for ChannelError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ChannelError::Io(e) => write!(f, "erro de E/S: {e}"),
            ChannelError::Noise(e) => write!(f, "{e}"),
            ChannelError::Protocol(m) => write!(f, "violação do formato: {m}"),
            ChannelError::Rejected(m) => write!(f, "{m}"),
        }
    }
}

impl std::error::Error for ChannelError {}

impl From<io::Error> for ChannelError {
    fn from(e: io::Error) -> Self {
        ChannelError::Io(e)
    }
}

impl From<NoiseError> for ChannelError {
    fn from(e: NoiseError) -> Self {
        ChannelError::Noise(e)
    }
}

/// O que o aperto de mão revela da outra ponta.
pub struct PeerInfo {
    pub static_key: [u8; 32],
    pub name: String,
    pub handshake_hash: [u8; 32],
}

fn check_name(bytes: Vec<u8>) -> Result<String, ChannelError> {
    if bytes.len() > MAX_NAME {
        return Err(ChannelError::Protocol("nome longo demais"));
    }
    String::from_utf8(bytes).map_err(|_| ChannelError::Protocol("nome não é UTF-8"))
}

fn write_hs<W: Write>(w: &mut W, msg: &[u8], prefix: &[u8]) -> Result<(), ChannelError> {
    if msg.len() > MAX_HANDSHAKE_MSG {
        return Err(ChannelError::Protocol("recado de aperto de mão grande demais"));
    }
    let mut buf = Vec::with_capacity(prefix.len() + 2 + msg.len());
    buf.extend_from_slice(prefix);
    buf.extend_from_slice(&(msg.len() as u16).to_be_bytes());
    buf.extend_from_slice(msg);
    w.write_all(&buf)?;
    w.flush()?;
    Ok(())
}

fn read_hs<R: Read>(r: &mut R) -> Result<Vec<u8>, ChannelError> {
    let mut l = [0u8; 2];
    r.read_exact(&mut l)?;
    let n = u16::from_be_bytes(l) as usize;
    if n == 0 || n > MAX_HANDSHAKE_MSG {
        return Err(ChannelError::Protocol("tamanho de recado de aperto de mão inválido"));
    }
    let mut b = vec![0u8; n];
    r.read_exact(&mut b)?;
    Ok(b)
}

/// Aperto de mão do **cliente** (iniciador). Escreve `"PSN1"` e o primeiro recado, lê o segundo e chama
/// `verify(chave_do_aparelho, nome_do_aparelho)`; se ela recusar, **não** envia o terceiro recado (o cliente
/// não revela a identidade dele a um aparelho que não reconhece). Depois envia o terceiro.
pub fn client_handshake<R: Read, W: Write>(
    r: &mut R,
    w: &mut W,
    key: &Keypair,
    my_name: &str,
    verify: impl FnOnce(&[u8; 32], &str) -> Result<(), String>,
) -> Result<(Established, PeerInfo), ChannelError> {
    run_client(r, w, HandshakeState::new(Role::Initiator, MAGIC, key.clone()), my_name, verify)
}

/// Como [`client_handshake`], com a efêmera fixa (**só testes**).
#[doc(hidden)]
pub fn client_handshake_with_ephemeral<R: Read, W: Write>(
    r: &mut R,
    w: &mut W,
    key: &Keypair,
    ephemeral: [u8; 32],
    my_name: &str,
    verify: impl FnOnce(&[u8; 32], &str) -> Result<(), String>,
) -> Result<(Established, PeerInfo), ChannelError> {
    run_client(r, w, HandshakeState::new(Role::Initiator, MAGIC, key.clone()).with_ephemeral_for_test(ephemeral), my_name, verify)
}

fn run_client<R: Read, W: Write>(
    r: &mut R,
    w: &mut W,
    mut hs: HandshakeState,
    my_name: &str,
    verify: impl FnOnce(&[u8; 32], &str) -> Result<(), String>,
) -> Result<(Established, PeerInfo), ChannelError> {
    if my_name.len() > MAX_NAME {
        return Err(ChannelError::Protocol("nome do cliente longo demais"));
    }
    let mut m1 = Vec::new();
    hs.write_message(b"", &mut m1)?;
    write_hs(w, &m1, MAGIC)?;
    let m2 = read_hs(r)?;
    let device_name = check_name(hs.read_message(&m2)?)?;
    let device_key = hs.remote_static().ok_or(ChannelError::Protocol("sem chave estática"))?;
    verify(&device_key, &device_name).map_err(ChannelError::Rejected)?;
    let mut m3 = Vec::new();
    hs.write_message(my_name.as_bytes(), &mut m3)?;
    write_hs(w, &m3, &[])?;
    let hash = hs.handshake_hash();
    let est = hs.finish()?;
    Ok((est, PeerInfo { static_key: device_key, name: device_name, handshake_hash: hash }))
}

/// Aperto de mão do **servidor** (respondedor). Os 4 bytes `"PSN1"` já foram consumidos por quem decidiu
/// entre Noise e texto claro.
pub fn server_handshake<R: Read, W: Write>(r: &mut R, w: &mut W, key: &Keypair, my_name: &str) -> Result<(Established, PeerInfo), ChannelError> {
    run_server(r, w, HandshakeState::new(Role::Responder, MAGIC, key.clone()), my_name)
}

/// Como [`server_handshake`], com a efêmera fixa (**só testes**).
#[doc(hidden)]
pub fn server_handshake_with_ephemeral<R: Read, W: Write>(r: &mut R, w: &mut W, key: &Keypair, ephemeral: [u8; 32], my_name: &str) -> Result<(Established, PeerInfo), ChannelError> {
    run_server(r, w, HandshakeState::new(Role::Responder, MAGIC, key.clone()).with_ephemeral_for_test(ephemeral), my_name)
}

fn run_server<R: Read, W: Write>(r: &mut R, w: &mut W, mut hs: HandshakeState, my_name: &str) -> Result<(Established, PeerInfo), ChannelError> {
    if my_name.len() > MAX_NAME {
        return Err(ChannelError::Protocol("nome do aparelho longo demais"));
    }
    let m1 = read_hs(r)?;
    hs.read_message(&m1)?;
    let mut m2 = Vec::new();
    hs.write_message(my_name.as_bytes(), &mut m2)?;
    write_hs(w, &m2, &[])?;
    let m3 = read_hs(r)?;
    let client_name = check_name(hs.read_message(&m3)?)?;
    let client_key = hs.remote_static().ok_or(ChannelError::Protocol("sem chave estática"))?;
    let hash = hs.handshake_hash();
    let est = hs.finish()?;
    Ok((est, PeerInfo { static_key: client_key, name: client_name, handshake_hash: hash }))
}

// ---------------------------------------------------------------------------------------------
// Registros de transporte
// ---------------------------------------------------------------------------------------------

fn dead_err(msg: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg)
}

/// Metade de leitura: junta os registros e entrega o fluxo de bytes decifrado. Um registro que não
/// autentica encerra o fluxo com erro, sem resposta, e todas as leituras seguintes também falham.
pub struct SecureReader<R> {
    inner: R,
    cs: CipherState,
    hdr: [u8; 2],
    hdr_have: usize,
    body: Vec<u8>,
    body_have: usize,
    plain: Vec<u8>,
    pos: usize,
    dead: bool,
}

impl<R: Read> SecureReader<R> {
    pub fn new(inner: R, recv: CipherState) -> Self {
        SecureReader { inner, cs: recv, hdr: [0; 2], hdr_have: 0, body: Vec::new(), body_have: 0, plain: Vec::new(), pos: 0, dead: false }
    }

    /// Lê (aos poucos, tolerando `WouldBlock`/`TimedOut` no meio sem perder o ponto) o próximo registro.
    /// `Ok(false)` = fim limpo da conexão entre dois registros.
    fn next_record(&mut self) -> io::Result<bool> {
        while self.hdr_have < 2 {
            match self.inner.read(&mut self.hdr[self.hdr_have..]) {
                Ok(0) if self.hdr_have == 0 => return Ok(false),
                Ok(0) => return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "conexão cortada no meio de um registro")),
                Ok(n) => self.hdr_have += n,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => return Err(e),
            }
        }
        let len = u16::from_be_bytes(self.hdr) as usize;
        if len < TAG_LEN {
            self.dead = true;
            return Err(dead_err("registro curto demais"));
        }
        if self.body.len() != len {
            self.body.resize(len, 0);
        }
        while self.body_have < len {
            match self.inner.read(&mut self.body[self.body_have..]) {
                Ok(0) => return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "conexão cortada no meio de um registro")),
                Ok(n) => self.body_have += n,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => return Err(e),
            }
        }
        self.hdr_have = 0;
        self.body_have = 0;
        self.plain.clear();
        std::mem::swap(&mut self.plain, &mut self.body);
        if self.cs.decrypt_in_place(&[], &mut self.plain).is_err() {
            self.dead = true;
            self.plain.clear();
            return Err(dead_err("registro não autentica"));
        }
        self.pos = 0;
        Ok(true)
    }
}

impl<R: Read> Read for SecureReader<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        loop {
            if self.pos < self.plain.len() {
                let n = buf.len().min(self.plain.len() - self.pos);
                buf[..n].copy_from_slice(&self.plain[self.pos..self.pos + n]);
                self.pos += n;
                return Ok(n);
            }
            if self.dead {
                return Err(dead_err("canal encerrado por registro inválido"));
            }
            if !self.next_record()? {
                return Ok(0);
            }
        }
    }
}

/// Metade de escrita: cada `write` vira um ou mais registros (até [`MAX_PLAIN`] bytes cada), enviados na
/// hora. `write_message` do protocolo escreve a mensagem inteira num `write_all`, então cada mensagem do
/// protocolo vai num registro (ou em vários, se passar de 65519 bytes), sem atraso de agrupamento.
pub struct SecureWriter<W> {
    inner: W,
    cs: CipherState,
    dead: bool,
    out: Vec<u8>,
}

impl<W: Write> SecureWriter<W> {
    pub fn new(inner: W, send: CipherState) -> Self {
        SecureWriter { inner, cs: send, dead: false, out: Vec::new() }
    }

    /// Devolve o escritor de baixo (para os testes recuperarem os bytes dos registros).
    pub fn into_inner(self) -> W {
        self.inner
    }
}

impl<W: Write> Write for SecureWriter<W> {
    fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        if self.dead {
            return Err(io::Error::new(io::ErrorKind::BrokenPipe, "canal cifrado já falhou"));
        }
        if data.is_empty() {
            return Ok(0);
        }
        let n = data.len().min(MAX_PLAIN);
        let total = n + TAG_LEN;
        self.out.clear();
        self.out.reserve(2 + total);
        self.out.extend_from_slice(&(total as u16).to_be_bytes());
        self.out.extend_from_slice(&data[..n]);
        let tag = match self.cs.encrypt_detached(&[], &mut self.out[2..]) {
            Ok(t) => t,
            Err(_) => {
                self.dead = true;
                return Err(io::Error::other("falha ao cifrar o registro"));
            }
        };
        self.out.extend_from_slice(&tag);
        if let Err(e) = self.inner.write_all(&self.out) {
            self.dead = true;
            return Err(e);
        }
        Ok(n)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}
