//! A superfície em que o núcleo desenha (docs/26, decisão 7). No Android é a `ANativeWindow` (`surface.rs`); nos testes,
//! uma tela falsa que guarda o que recebeu.

/// Quem recebe um quadro pronto.
pub trait Tela: Send {
    /// Mostra um quadro RGBA (alfa 255) de `largura` x `altura` pixels, linhas contíguas (`largura * 4` bytes por
    /// linha). O destino se encarrega do passo (`stride`) do buffer dele e de esticar o quadro para a tela.
    fn publicar(&mut self, largura: u32, altura: u32, rgba: &[u8]) -> Result<(), String>;
}
