//! Texto de versão que o núcleo devolve à casca (`Nucleo.versao()`); lógica pura, testada no computador.

/// Versão do núcleo e do `ps-client` que ele reaproveita, por exemplo `PiStick núcleo 0.1.0 (ps-client 0.1.0)`.
pub fn texto() -> String {
    format!("PiStick núcleo {} (ps-client {})", env!("CARGO_PKG_VERSION"), client_version())
}

/// O `ps-client` e o `ps-android` andam juntos no workspace; as capacidades anunciadas provam que o núcleo do
/// cliente foi de fato ligado a esta biblioteca (compilou para o alvo).
fn client_version() -> String {
    format!("{} caps={:#x}", env!("CARGO_PKG_VERSION"), ps_client::CLIENT_CAPS)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn o_texto_traz_a_versao_do_pacote_e_as_capacidades_do_cliente() {
        let t = texto();
        assert!(t.starts_with(&format!("PiStick núcleo {}", env!("CARGO_PKG_VERSION"))), "{t}");
        assert!(t.contains("ps-client"), "{t}");
        assert!(t.contains("caps=0x"), "{t}");
    }
}
