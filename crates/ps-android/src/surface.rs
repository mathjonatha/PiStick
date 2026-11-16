//! A superfície da casca como [`Tela`]: `ANativeWindow` (docs/26, decisão 7). Desenho por software: a cada quadro
//! `ANativeWindow_lock`, cópia linha a linha respeitando o `stride` do buffer, `ANativeWindow_unlockAndPost`. A escala
//! para a tela fica por conta da própria superfície (`ANativeWindow_setBuffersGeometry` com o tamanho do quadro).
//!
//! Todo o `unsafe` da superfície está aqui; as declarações do NDK, em `ndk.rs`.

use std::time::Instant;

use crate::ndk::{self, ANativeWindow, ANativeWindowBuffer, WINDOW_FORMAT_RGBA_8888, WINDOW_FORMAT_RGBX_8888};
use crate::registro;
use crate::tela::Tela;

/// A cada quantos quadros a medida da cópia vai para o `logcat`.
const QUADROS_POR_MEDIDA: u64 = 120;

pub struct SuperficieAndroid {
    janela: *mut ANativeWindow,
    geometria: (u32, u32),
    soma_us: u64,
    quadros: u64,
}

// SAFETY: a `ANativeWindow` é um objeto com contagem de referências do sistema, seguro para usar de qualquer thread
// (as chamadas do NDK que usamos são thread-safe); esta estrutura é a dona da referência e só a usa por `&mut self`.
unsafe impl Send for SuperficieAndroid {}

impl SuperficieAndroid {
    /// Toma posse de uma referência já adquirida (`ANativeWindow_fromSurface`). `None` se o ponteiro é nulo.
    ///
    /// # Safety
    /// `janela` tem de ser nula ou uma `ANativeWindow*` válida cuja referência passa a pertencer a esta estrutura.
    pub unsafe fn de_ponteiro(janela: *mut ANativeWindow) -> Option<SuperficieAndroid> {
        (!janela.is_null()).then_some(SuperficieAndroid { janela, geometria: (0, 0), soma_us: 0, quadros: 0 })
    }
}

impl Drop for SuperficieAndroid {
    fn drop(&mut self) {
        // SAFETY: `janela` é uma referência válida que esta estrutura possui; é solta uma única vez, aqui.
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
            // SAFETY: `janela` é válida (posse nossa); os argumentos são inteiros e o formato é RGBX_8888.
            let r = unsafe { ndk::ANativeWindow_setBuffersGeometry(self.janela, largura as i32, altura as i32, WINDOW_FORMAT_RGBX_8888) };
            if r != 0 {
                return Err(format!("ANativeWindow_setBuffersGeometry falhou ({r})"));
            }
            self.geometria = (largura, altura);
        }
        let mut buf = ANativeWindowBuffer { width: 0, height: 0, stride: 0, format: 0, bits: std::ptr::null_mut(), reserved: [0; 6] };
        // SAFETY: `janela` é válida; `buf` é uma estrutura local com o mesmo layout de `ANativeWindow_Buffer`; o
        // retângulo sujo nulo pede o buffer inteiro.
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
                // SAFETY: o buffer travado tem `buf.height` linhas de `stride` pixels de 4 bytes a partir de `bits`;
                // `y < buf.height` e `largura_copia <= buf.width <= stride`, logo a escrita de `largura_copia * 4`
                // bytes na linha `y` fica dentro do buffer; a origem tem `largura * altura * 4` bytes (conferido acima)
                // e a leitura da linha `y` fica dentro dela; os dois intervalos não se sobrepõem.
                unsafe {
                    let dst = (buf.bits as *mut u8).add(y * passo);
                    let src = rgba.as_ptr().add(y * largura as usize * 4);
                    std::ptr::copy_nonoverlapping(src, dst, largura_copia * 4);
                }
            }
        }
        // SAFETY: `janela` é válida e está travada por nós; solta o buffer e o envia à tela (sempre, mesmo se o
        // formato era inesperado, para não deixar a superfície travada).
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
