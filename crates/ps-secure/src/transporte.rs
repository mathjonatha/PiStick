//! O canal cifrado como [`Transport`] do PiStick: o servidor e o cliente passam a conexão cifrada para o
//! mesmo código de sessão que já usava TCP puro.

use std::io::{self, Read, Write};

use ps_transport::{Closer, Split, Transport};

use crate::canal::{SecureReader, SecureWriter};
use crate::noise::CipherState;

/// Conexão cifrada pronta para virar sessão.
pub struct SecureConn<R, W> {
    pub reader: SecureReader<R>,
    pub writer: SecureWriter<W>,
    pub closer: Closer,
    pub peer: String,
}

impl<R: Read + Send + 'static, W: Write + Send + 'static> SecureConn<R, W> {
    /// Monta a conexão a partir das metades cruas e das cifras do aperto de mão concluído.
    pub fn new(reader: R, writer: W, closer: Closer, send: CipherState, recv: CipherState, peer: String) -> Self {
        SecureConn { reader: SecureReader::new(reader, recv), writer: SecureWriter::new(writer, send), closer, peer }
    }
}

impl<R: Read + Send + 'static, W: Write + Send + 'static> Transport for SecureConn<R, W> {
    type Reader = SecureReader<R>;
    type Writer = SecureWriter<W>;

    fn split(self) -> io::Result<Split<Self::Reader, Self::Writer>> {
        Ok(Split { reader: self.reader, writer: self.writer, closer: self.closer })
    }

    fn peer(&self) -> String {
        self.peer.clone()
    }
}
