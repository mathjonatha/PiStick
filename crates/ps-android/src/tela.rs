
pub trait Tela: Send {
    fn publicar(&mut self, largura: u32, altura: u32, rgba: &[u8]) -> Result<(), String>;
}
