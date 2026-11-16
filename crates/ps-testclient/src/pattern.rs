//! O padrão desenhado pelo cliente de teste e a função que o reconstrói para conferência.
//!
//! O quadro 0 pinta a superfície inteira com [`color`]. Cada quadro `n >= 1` repinta só a
//! faixa vertical `n % faixas` (largura [`BAND`]) com `color(x, y, n)` e o marcador, e declara
//! dano só nesses dois retângulos. Depois de um redimensionamento o quadro `m` é uma nova base
//! (pintura inteira). O marcador são os dois primeiros pixels da linha 0: o número do quadro
//! `n` e o da base `m` (24 bits cada), o que permite reconstruir o conteúdo esperado só a
//! partir dos pixels recebidos.

/// Largura de cada faixa repintada.
pub const BAND: u32 = 32;

/// Cor 0x00RRGGBB do pixel `(x, y)` no quadro `f`.
pub fn color(x: u32, y: u32, f: u32) -> u32 {
    let r = x.wrapping_mul(7).wrapping_add(f.wrapping_mul(13)) & 0xFF;
    let g = y.wrapping_mul(5).wrapping_add(f.wrapping_mul(3)) & 0xFF;
    let b = (x ^ y).wrapping_add(f.wrapping_mul(29)) & 0xFF;
    (r << 16) | (g << 8) | b
}

/// Número de faixas numa superfície de largura `w`.
pub fn bands(w: u32) -> u32 {
    w.div_ceil(BAND)
}

/// Retângulo `(x, y, largura, altura)` da faixa `band`.
pub fn band_rect(w: u32, h: u32, band: u32) -> (u32, u32, u32, u32) {
    let x = band * BAND;
    (x, 0, BAND.min(w - x), h)
}

/// Retângulo do marcador.
pub const MARKER_RECT: (u32, u32, u32, u32) = (0, 0, 2, 1);

/// Pinta o quadro-base `m` inteiro.
pub fn paint_full(canvas: &mut [u32], w: u32, h: u32, m: u32) {
    for y in 0..h {
        for x in 0..w {
            canvas[(y * w + x) as usize] = color(x, y, m);
        }
    }
    set_marker(canvas, m, m);
}

/// Repinta a faixa do quadro `n` (o marcador também) e devolve os retângulos de dano.
pub fn paint_band(canvas: &mut [u32], w: u32, h: u32, n: u32, m: u32) -> [(u32, u32, u32, u32); 2] {
    let rect = band_rect(w, h, n % bands(w));
    for y in 0..rect.3 {
        for x in rect.0..rect.0 + rect.2 {
            canvas[(y * w + x) as usize] = color(x, y, n);
        }
    }
    set_marker(canvas, n, m);
    [rect, MARKER_RECT]
}

fn set_marker(canvas: &mut [u32], n: u32, m: u32) {
    canvas[0] = n & 0xFF_FFFF;
    canvas[1] = m & 0xFF_FFFF;
}

/// Conteúdo esperado depois do quadro `n`, com base `m`, em BGRX (4 bytes por pixel, X = 0xFF).
pub fn expected(w: u32, h: u32, n: u32, m: u32) -> Vec<u8> {
    let mut canvas = vec![0u32; (w * h) as usize];
    paint_full(&mut canvas, w, h, m);
    for k in m + 1..=n {
        paint_band(&mut canvas, w, h, k, m);
    }
    set_marker(&mut canvas, n, m);
    let mut out = Vec::with_capacity(canvas.len() * 4);
    for c in canvas {
        out.extend_from_slice(&[c as u8, (c >> 8) as u8, (c >> 16) as u8, 0xFF]);
    }
    out
}

/// Lê o marcador `(n, m)` de pixels BGRX.
pub fn read_marker(bgrx: &[u8]) -> (u32, u32) {
    let px = |i: usize| u32::from(bgrx[i * 4]) | u32::from(bgrx[i * 4 + 1]) << 8 | u32::from(bgrx[i * 4 + 2]) << 16;
    (px(0), px(1))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn px(img: &[u8], w: u32, x: u32, y: u32) -> u32 {
        let i = ((y * w + x) * 4) as usize;
        u32::from(img[i]) | u32::from(img[i + 1]) << 8 | u32::from(img[i + 2]) << 16
    }

    #[test]
    fn reconstrucao_segue_as_faixas() {
        // 100 px de largura = 4 faixas; base 3, depois os quadros 4..=7 repintam as faixas 0..=3.
        let a = expected(100, 40, 7, 3);
        assert_eq!(read_marker(&a), (7, 3));
        assert_eq!(px(&a, 100, 40, 5), color(40, 5, 5)); // faixa 1 -> quadro 5
        assert_eq!(px(&a, 100, 70, 9), color(70, 9, 6)); // faixa 2 -> quadro 6
        assert_eq!(px(&a, 100, 10, 9), color(10, 9, 4)); // faixa 0 -> quadro 4
        // Sem repintura posterior, a base permanece.
        let b = expected(100, 40, 4, 3);
        assert_eq!(px(&b, 100, 70, 9), color(70, 9, 3));
    }
}
