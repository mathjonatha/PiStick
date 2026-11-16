
pub const ETIQUETA: &str = "PiStick";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Nivel {
    Info = 4,
    Aviso = 5,
    Erro = 6,
}

pub fn escrever(nivel: Nivel, texto: &str) {
    let linha = sem_nul(texto);
    #[cfg(target_os = "android")]
    crate::ndk::log_write(nivel as i32, ETIQUETA, &linha);
    #[cfg(not(target_os = "android"))]
    eprintln!("[{ETIQUETA}] {nivel:?}: {linha}");
}

pub fn info(texto: &str) {
    escrever(Nivel::Info, texto);
}

pub fn erro(texto: &str) {
    escrever(Nivel::Erro, texto);
}

fn sem_nul(texto: &str) -> String {
    texto.split('\0').next().unwrap_or("").to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nul_corta_a_linha() {
        assert_eq!(sem_nul("abc\0def"), "abc");
        assert_eq!(sem_nul("sem nul ação"), "sem nul ação");
        assert_eq!(sem_nul("\0x"), "");
    }

    #[test]
    fn os_niveis_sao_os_do_android() {
        assert_eq!((Nivel::Info as i32, Nivel::Aviso as i32, Nivel::Erro as i32), (4, 5, 6));
    }

    #[test]
    fn escrever_nao_entra_em_panico_fora_do_android() {
        info("linha de teste");
        erro("linha de erro de teste");
    }
}
