
use std::io;
use std::sync::OnceLock;

pub mod nv12;

#[cfg(windows)]
pub mod mf;

pub trait VideoDecoder {
    fn decode(&mut self, data: &[u8], out: &mut Vec<u8>) -> io::Result<Option<(u32, u32)>>;
}

pub type DecoderFactory = fn(u32, u32) -> io::Result<Box<dyn VideoDecoder>>;

static FACTORY: OnceLock<DecoderFactory> = OnceLock::new();

pub fn register_decoder_factory(f: DecoderFactory) -> bool {
    FACTORY.set(f).is_ok()
}

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

pub fn probe() -> io::Result<()> {
    std::thread::Builder::new()
        .name("psc-sonda-video".into())
        .spawn(|| open_decoder(256, 256).map(|_| ()))
        .map_err(io::Error::other)?
        .join()
        .unwrap_or_else(|_| Err(io::Error::other("a sonda do decodificador de vídeo entrou em pânico")))
}
