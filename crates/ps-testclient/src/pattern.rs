
pub const BAND: u32 = 32;

pub fn color(x: u32, y: u32, f: u32) -> u32 {
    let r = x.wrapping_mul(7).wrapping_add(f.wrapping_mul(13)) & 0xFF;
    let g = y.wrapping_mul(5).wrapping_add(f.wrapping_mul(3)) & 0xFF;
    let b = (x ^ y).wrapping_add(f.wrapping_mul(29)) & 0xFF;
    (r << 16) | (g << 8) | b
}

pub fn bands(w: u32) -> u32 {
    w.div_ceil(BAND)
}

pub fn band_rect(w: u32, h: u32, band: u32) -> (u32, u32, u32, u32) {
    let x = band * BAND;
    (x, 0, BAND.min(w - x), h)
}

pub const MARKER_RECT: (u32, u32, u32, u32) = (0, 0, 2, 1);

pub fn paint_full(canvas: &mut [u32], w: u32, h: u32, m: u32) {
    for y in 0..h {
        for x in 0..w {
            canvas[(y * w + x) as usize] = color(x, y, m);
        }
    }
    set_marker(canvas, m, m);
}

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
        let a = expected(100, 40, 7, 3);
        assert_eq!(read_marker(&a), (7, 3));
        assert_eq!(px(&a, 100, 40, 5), color(40, 5, 5));
        assert_eq!(px(&a, 100, 70, 9), color(70, 9, 6));
        assert_eq!(px(&a, 100, 10, 9), color(10, 9, 4));
        let b = expected(100, 40, 4, 3);
        assert_eq!(px(&b, 100, 70, 9), color(70, 9, 3));
    }
}
