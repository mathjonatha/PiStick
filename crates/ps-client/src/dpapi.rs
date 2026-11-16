
use std::io;
use std::ptr;

use windows::core::w;
use windows::Win32::Foundation::{LocalFree, HLOCAL};
use windows::Win32::Security::Cryptography::{CryptProtectData, CryptUnprotectData, CRYPTPROTECT_UI_FORBIDDEN, CRYPT_INTEGER_BLOB};

const ENTROPY: &[u8] = b"PiStick chave-do-cliente v1";

fn blob(dados: &[u8]) -> io::Result<CRYPT_INTEGER_BLOB> {
    let n = u32::try_from(dados.len()).map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "dados grandes demais para a DPAPI"))?;
    Ok(CRYPT_INTEGER_BLOB { cbData: n, pbData: dados.as_ptr() as *mut u8 })
}

fn copiar_e_liberar(saida: CRYPT_INTEGER_BLOB, secreto: bool) -> Vec<u8> {
    if saida.pbData.is_null() {
        return Vec::new();
    }
    let n = saida.cbData as usize;
    let copia = unsafe { std::slice::from_raw_parts(saida.pbData, n) }.to_vec();
    if secreto {
        unsafe { ptr::write_bytes(saida.pbData, 0, n) };
    }
    unsafe {
        let _ = LocalFree(Some(HLOCAL(saida.pbData.cast())));
    }
    copia
}

pub fn protect(plano: &[u8]) -> io::Result<Vec<u8>> {
    let entrada = blob(plano)?;
    let entropia = blob(ENTROPY)?;
    let mut saida = CRYPT_INTEGER_BLOB { cbData: 0, pbData: ptr::null_mut() };
    let r = unsafe { CryptProtectData(&entrada, w!("PiStick"), Some(&entropia), None, None, CRYPTPROTECT_UI_FORBIDDEN, &mut saida) };
    r.map_err(|e| io::Error::other(format!("CryptProtectData falhou: {e}")))?;
    Ok(copiar_e_liberar(saida, false))
}

pub fn unprotect(cifrado: &[u8]) -> io::Result<Vec<u8>> {
    let entrada = blob(cifrado)?;
    let entropia = blob(ENTROPY)?;
    let mut saida = CRYPT_INTEGER_BLOB { cbData: 0, pbData: ptr::null_mut() };
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
