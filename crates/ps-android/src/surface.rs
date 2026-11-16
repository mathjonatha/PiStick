
use std::time::Instant;

use crate::ndk::{self, ANativeWindow, ANativeWindowBuffer, WINDOW_FORMAT_RGBA_8888, WINDOW_FORMAT_RGBX_8888};
use crate::registro;
use crate::tela::Tela;

const QUADROS_POR_MEDIDA: u64 = 120;

pub struct SuperficieAndroid {
    janela: *mut ANativeWindow,
    geometria: (u32, u32),
    soma_us: u64,
    quadros: u64,
}

unsafe impl Send for SuperficieAndroid {}

impl SuperficieAndroid {
    pub unsafe fn de_ponteiro(janela: *mut ANativeWindow) -> Option<SuperficieAndroid> {
        (!janela.is_null()).then_some(SuperficieAndroid { janela, geometria: (0, 0), soma_us: 0, quadros: 0 })
    }
}

impl Drop for SuperficieAndroid {
    fn drop(&mut self) {
        unsafe { ndk::ANativeWindow_release(self.janela) };
    }
}

impl Tela for SuperficieAndroid {
    fn publicar(&mut self, largura: u32, altura: u32, rgba: &[u8]) -> Result<(), String> {
        if largura == 0 || altura == 0 || rgba.len() < largura as usize * altura as usize * 4 {
            return Err(format!("quadro incoerente: {largura}x{altura} com {} bytes", rgba.len()));
        }
        let t0 = Instant::now();
        if self.geometria != (largura, altura) {
            let r = unsafe { ndk::ANativeWindow_setBuffersGeometry(self.janela, largura as i32, altura as i32, WINDOW_FORMAT_RGBX_8888) };
            if r != 0 {
                return Err(format!("ANativeWindow_setBuffersGeometry falhou ({r})"));
            }
            self.geometria = (largura, altura);
        }
        let mut buf = ANativeWindowBuffer { width: 0, height: 0, stride: 0, format: 0, bits: std::ptr::null_mut(), reserved: [0; 6] };
        let r = unsafe { ndk::ANativeWindow_lock(self.janela, &mut buf, std::ptr::null_mut()) };
        if r != 0 || buf.bits.is_null() {
            return Err(format!("ANativeWindow_lock falhou ({r})"));
        }
        let ok = buf.format == WINDOW_FORMAT_RGBX_8888 || buf.format == WINDOW_FORMAT_RGBA_8888;
        if ok && buf.stride > 0 && buf.width > 0 && buf.height > 0 {
            let largura_copia = (largura as usize).min(buf.width as usize);
            let altura_copia = (altura as usize).min(buf.height as usize);
            let passo = buf.stride as usize * 4;
            for y in 0..altura_copia {
                unsafe {
                    let dst = (buf.bits as *mut u8).add(y * passo);
                    let src = rgba.as_ptr().add(y * largura as usize * 4);
                    std::ptr::copy_nonoverlapping(src, dst, largura_copia * 4);
                }
            }
        }
        let r2 = unsafe { ndk::ANativeWindow_unlockAndPost(self.janela) };
        if !ok {
            return Err(format!("formato de buffer inesperado: {}", buf.format));
        }
        if r2 != 0 {
            return Err(format!("ANativeWindow_unlockAndPost falhou ({r2})"));
        }
        self.soma_us += t0.elapsed().as_micros() as u64;
        self.quadros += 1;
        if self.quadros.is_multiple_of(QUADROS_POR_MEDIDA) {
            registro::info(&format!("cópia para a Surface ({largura}x{altura}, stride {}): média de {} µs em {} quadros", buf.stride, self.soma_us / self.quadros, self.quadros));
        }
        Ok(())
    }
}
