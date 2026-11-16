//! DPAPI do Windows (marco 9B, `docs/25`, decisão 4): cifra e decifra bytes com a chave do usuário do Windows
//! (`CryptProtectData` e `CryptUnprotectData`, escopo do usuário, sem interface).
//!
//! **Todo o `unsafe` da DPAPI fica neste arquivo**, e só aqui: as duas chamadas à DPAPI e o tratamento do
//! bloco que ela devolve (leitura, zeragem e liberação). O resto do cliente chama [`protect`] e [`unprotect`], que são seguras.
//!
//! Uma entropia fixa e própria do PiStick vai junto: outro programa do mesmo usuário que chame a DPAPI sem
//! ela não decifra o arquivo por acidente (não é segredo, só separa os contextos).

use std::io;
use std::ptr;

use windows::core::w;
use windows::Win32::Foundation::{LocalFree, HLOCAL};
use windows::Win32::Security::Cryptography::{CryptProtectData, CryptUnprotectData, CRYPTPROTECT_UI_FORBIDDEN, CRYPT_INTEGER_BLOB};

const ENTROPY: &[u8] = b"PiStick chave-do-cliente v1";

/// Descreve um trecho de memória para a API. Só cria o ponteiro; quem o usa é o chamador, dentro de `unsafe`.
fn blob(dados: &[u8]) -> io::Result<CRYPT_INTEGER_BLOB> {
    let n = u32::try_from(dados.len()).map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "dados grandes demais para a DPAPI"))?;
    // O ponteiro é `*mut` só porque a estrutura do sistema é assim; a DPAPI não escreve na entrada.
    Ok(CRYPT_INTEGER_BLOB { cbData: n, pbData: dados.as_ptr() as *mut u8 })
}

/// Copia o bloco de saída do sistema para um `Vec`, zera o original se `secreto` e o devolve ao sistema.
fn copiar_e_liberar(saida: CRYPT_INTEGER_BLOB, secreto: bool) -> Vec<u8> {
    if saida.pbData.is_null() {
        return Vec::new();
    }
    let n = saida.cbData as usize;
    // SAFETY: a chamada que preencheu `saida` teve sucesso, então `pbData` aponta para `cbData` bytes válidos e
    // inicializados, alocados pelo sistema (LocalAlloc) e que ninguém mais usa; a cópia termina antes da liberação.
    let copia = unsafe { std::slice::from_raw_parts(saida.pbData, n) }.to_vec();
    if secreto {
        // SAFETY: mesma região de `n` bytes, ainda alocada e de escrita (é o buffer de saída do sistema).
        unsafe { ptr::write_bytes(saida.pbData, 0, n) };
    }
    // SAFETY: `pbData` veio do LocalAlloc do sistema, é liberado uma única vez e não é usado depois daqui.
    unsafe {
        let _ = LocalFree(Some(HLOCAL(saida.pbData.cast())));
    }
    copia
}

/// Cifra `plano` para o usuário atual do Windows.
pub fn protect(plano: &[u8]) -> io::Result<Vec<u8>> {
    let entrada = blob(plano)?;
    let entropia = blob(ENTROPY)?;
    let mut saida = CRYPT_INTEGER_BLOB { cbData: 0, pbData: ptr::null_mut() };
    // SAFETY: `entrada` e `entropia` apontam para fatias vivas durante a chamada (a API só lê delas); `saida` é um
    // bloco vazio válido para a API preencher; a descrição é um literal largo terminado em zero; sem interface
    // (CRYPTPROTECT_UI_FORBIDDEN), então não há estrutura de prompt.
    let r = unsafe { CryptProtectData(&entrada, w!("PiStick"), Some(&entropia), None, None, CRYPTPROTECT_UI_FORBIDDEN, &mut saida) };
    r.map_err(|e| io::Error::other(format!("CryptProtectData falhou: {e}")))?;
    Ok(copiar_e_liberar(saida, false))
}

/// Decifra o que [`protect`] gerou, para o mesmo usuário do Windows. Erro se o dado foi adulterado, truncado ou
/// cifrado para outro usuário ou outra máquina.
pub fn unprotect(cifrado: &[u8]) -> io::Result<Vec<u8>> {
    let entrada = blob(cifrado)?;
    let entropia = blob(ENTROPY)?;
    let mut saida = CRYPT_INTEGER_BLOB { cbData: 0, pbData: ptr::null_mut() };
    // SAFETY: igual a `protect`: as entradas são fatias vivas só lidas pela API, `saida` é um bloco vazio válido,
    // não pedimos a descrição (None) e a interface é proibida. Em falha a API não aloca saída.
    let r = unsafe { CryptUnprotectData(&entrada, None, Some(&entropia), None, None, CRYPTPROTECT_UI_FORBIDDEN, &mut saida) };
    r.map_err(|e| io::Error::new(io::ErrorKind::InvalidData, format!("CryptUnprotectData falhou: {e}")))?;
    Ok(copiar_e_liberar(saida, true))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ida_e_volta() {
        let plano = [7u8; 32];
        let c = protect(&plano).unwrap();
        assert!(c.len() > 32);
        assert!(!c.windows(32).any(|w| w == plano));
        assert_eq!(unprotect(&c).unwrap(), plano);
    }

    #[test]
    fn adulterado_ou_truncado_da_erro_sem_panico() {
        let c = protect(&[9u8; 32]).unwrap();
        let mut ruim = c.clone();
        let meio = ruim.len() / 2;
        ruim[meio] ^= 1;
        assert!(unprotect(&ruim).is_err());
        assert!(unprotect(&c[..c.len() - 1]).is_err());
        assert!(unprotect(&[]).is_err());
        assert!(unprotect(b"lixo qualquer que nao e um bloco da DPAPI").is_err());
    }
}
