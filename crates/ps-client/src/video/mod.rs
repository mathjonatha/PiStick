//! Vídeo por região (marco 4): decodifica o H.264 que o servidor manda para a região de uma janela
//! e escreve o resultado no mesmo buffer BGRX que a janela apresenta.
//!
//! - [`nv12`]: conversão NV12 → BGRX própria e pura.
//! - `mf` (só Windows): decodificador H.264 do Media Foundation. Todo o `unsafe` de COM fica lá.
//! - [`VideoDecoder`]: o que a rede precisa de um decodificador; [`open_decoder`] abre o do sistema.

use std::io;
use std::sync::OnceLock;

pub mod nv12;

#[cfg(windows)]
pub mod mf;

/// Decodificador de um fluxo H.264 (Annex B, uma unidade de acesso por chamada).
pub trait VideoDecoder {
    /// Decodifica uma unidade de acesso. Se saiu um quadro, escreve o retângulo de exibição (já
    /// recortado) em `out` como BGRX com linhas contíguas (`largura * altura * 4` bytes) e devolve o
    /// tamanho `(largura, altura)`; `None` se o decodificador ainda precisa de mais entrada.
    fn decode(&mut self, data: &[u8], out: &mut Vec<u8>) -> io::Result<Option<(u32, u32)>>;
}

/// Como abrir o decodificador de um sistema que o `ps-client` não conhece (o cliente Android registra a do
/// `AMediaCodec`): recebe a largura e a altura da região.
pub type DecoderFactory = fn(u32, u32) -> io::Result<Box<dyn VideoDecoder>>;

static FACTORY: OnceLock<DecoderFactory> = OnceLock::new();

/// Registra a fábrica de decodificadores do sistema (uma vez por processo; `false` se já havia uma). No `psc` do
/// Windows nada é registrado e vale o Media Foundation.
pub fn register_decoder_factory(f: DecoderFactory) -> bool {
    FACTORY.set(f).is_ok()
}

/// Abre o decodificador do sistema para um fluxo com a região de `width`×`height`.
pub fn open_decoder(width: u32, height: u32) -> io::Result<Box<dyn VideoDecoder>> {
    if let Some(f) = FACTORY.get() {
        return f(width, height);
    }
    #[cfg(windows)]
    {
        Ok(Box::new(mf::MfDecoder::open(width, height)?))
    }
    #[cfg(not(windows))]
    {
        let _ = (width, height);
        Err(io::Error::new(io::ErrorKind::Unsupported, "este sistema não tem decodificador H.264 no psc"))
    }
}

/// O sistema consegue decodificar H.264? Só então o `psc` anuncia `CAP_H264`.
///
/// Roda numa thread própria e já terminada: o Media Foundation inicializa o COM da thread que o usa
/// (multithread), e a thread principal do `psc` precisa do COM em modo de apartamento para o `winit`
/// (arrastar e soltar), então ela nunca pode ser a que abre o decodificador.
pub fn probe() -> io::Result<()> {
    std::thread::Builder::new()
        .name("psc-sonda-video".into())
        .spawn(|| open_decoder(256, 256).map(|_| ()))
        .map_err(io::Error::other)?
        .join()
        .unwrap_or_else(|_| Err(io::Error::other("a sonda do decodificador de vídeo entrou em pânico")))
}
