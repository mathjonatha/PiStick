//! Conversão NV12 → BGRX, pura e própria (sem GPU, sem dependências).
//!
//! NV12: plano Y (um byte por pixel) e depois um plano UV intercalado com metade da largura e da altura
//! (U, V, U, V, ...). A cor de cada par UV vale para o bloco de 2×2 pixels (replicação, sem filtro).
//! Matriz BT.601 ou BT.709, faixa limitada (Y 16..235, UV 16..240) ou completa, em ponto fixo de 16 bits.

/// Matriz de cor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Matrix {
    Bt601,
    Bt709,
}

/// Faixa de valores.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Range {
    /// Y de 16 a 235, UV de 16 a 240 (TV).
    Limited,
    /// 0 a 255.
    Full,
}

/// Parâmetros de cor de um fluxo.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ColorSpec {
    pub matrix: Matrix,
    pub range: Range,
}

impl ColorSpec {
    /// Na dúvida (o decodificador não disse): BT.709 limitada para altura de 720 ou mais, e BT.601
    /// limitada abaixo disso. É o que o codificador do Pi sinaliza no VUI.
    pub fn guess(height: u32) -> ColorSpec {
        ColorSpec { matrix: if height >= 720 { Matrix::Bt709 } else { Matrix::Bt601 }, range: Range::Limited }
    }
}

/// Coeficientes em ponto fixo Q16: (escala de Y, R de V, G de U, G de V, B de U).
fn coefficients(c: ColorSpec) -> (i32, i32, i32, i32, i32) {
    let q = |x: f64| (x * 65536.0).round() as i32;
    let (rv, gu, gv, bu) = match c.matrix {
        Matrix::Bt601 => (1.402, 0.344136, 0.714136, 1.772),
        Matrix::Bt709 => (1.5748, 0.187324, 0.468124, 1.8556),
    };
    match c.range {
        Range::Full => (q(1.0), q(rv), q(gu), q(gv), q(bu)),
        // Y' = (Y - 16) * 255 / 219 e UV' = (UV - 128) * 255 / 224.
        Range::Limited => (q(255.0 / 219.0), q(rv * 255.0 / 224.0), q(gu * 255.0 / 224.0), q(gv * 255.0 / 224.0), q(bu * 255.0 / 224.0)),
    }
}

#[inline]
fn clamp8(v: i32) -> u8 {
    v.clamp(0, 255) as u8
}

/// Converte um quadro NV12 de `width`×`height` (pares) para BGRX (X = 0xFF).
///
/// `y_plane`: linhas de `y_stride` bytes. `uv_plane`: `height / 2` linhas de `uv_stride` bytes.
/// `out`: `width * height * 4` bytes, linhas contíguas. Devolve `false` sem escrever se algum
/// tamanho não cabe.
#[allow(clippy::too_many_arguments)]
pub fn nv12_to_bgrx(y_plane: &[u8], y_stride: usize, uv_plane: &[u8], uv_stride: usize, width: usize, height: usize, color: ColorSpec, out: &mut [u8]) -> bool {
    if width == 0 || height == 0 || !width.is_multiple_of(2) || !height.is_multiple_of(2) || y_stride < width || uv_stride < width {
        return false;
    }
    if y_plane.len() < y_stride * (height - 1) + width || uv_plane.len() < uv_stride * (height / 2 - 1) + width || out.len() < width * height * 4 {
        return false;
    }
    let (ky, krv, kgu, kgv, kbu) = coefficients(color);
    let (y_off, uv_off) = match color.range {
        Range::Limited => (16, 128),
        Range::Full => (0, 128),
    };
    for row in 0..height {
        let yrow = &y_plane[row * y_stride..row * y_stride + width];
        let uvrow = &uv_plane[(row / 2) * uv_stride..(row / 2) * uv_stride + width];
        let orow = &mut out[row * width * 4..(row + 1) * width * 4];
        for x in 0..width {
            let (u, v) = (i32::from(uvrow[x & !1]) - uv_off, i32::from(uvrow[x | 1]) - uv_off);
            let yy = (i32::from(yrow[x]) - y_off) * ky + 32768;
            let o = &mut orow[x * 4..x * 4 + 4];
            o[0] = clamp8((yy + kbu * u) >> 16);
            o[1] = clamp8((yy - kgu * u - kgv * v) >> 16);
            o[2] = clamp8((yy + krv * v) >> 16);
            o[3] = 0xFF;
        }
    }
    true
}

/// Converte um quadro I420 (planar: Y, depois U, depois V, os dois últimos com metade da largura e da altura) de
/// `width`×`height` (pares) para BGRX (X = 0xFF). `u_plane` e `v_plane` têm `height / 2` linhas de `u_stride` e
/// `v_stride` bytes (cada um pelo menos `width / 2`). Devolve `false` sem escrever se algum tamanho não cabe.
#[allow(clippy::too_many_arguments)]
pub fn i420_to_bgrx(y_plane: &[u8], y_stride: usize, u_plane: &[u8], u_stride: usize, v_plane: &[u8], v_stride: usize, width: usize, height: usize, color: ColorSpec, out: &mut [u8]) -> bool {
    if width == 0 || height == 0 || !width.is_multiple_of(2) || !height.is_multiple_of(2) || y_stride < width || u_stride < width / 2 || v_stride < width / 2 {
        return false;
    }
    let (cw, ch) = (width / 2, height / 2);
    if y_plane.len() < y_stride * (height - 1) + width || u_plane.len() < u_stride * (ch - 1) + cw || v_plane.len() < v_stride * (ch - 1) + cw || out.len() < width * height * 4 {
        return false;
    }
    let (ky, krv, kgu, kgv, kbu) = coefficients(color);
    let (y_off, uv_off) = match color.range {
        Range::Limited => (16, 128),
        Range::Full => (0, 128),
    };
    for row in 0..height {
        let yrow = &y_plane[row * y_stride..row * y_stride + width];
        let urow = &u_plane[(row / 2) * u_stride..(row / 2) * u_stride + cw];
        let vrow = &v_plane[(row / 2) * v_stride..(row / 2) * v_stride + cw];
        let orow = &mut out[row * width * 4..(row + 1) * width * 4];
        for x in 0..width {
            let (u, v) = (i32::from(urow[x / 2]) - uv_off, i32::from(vrow[x / 2]) - uv_off);
            let yy = (i32::from(yrow[x]) - y_off) * ky + 32768;
            let o = &mut orow[x * 4..x * 4 + 4];
            o[0] = clamp8((yy + kbu * u) >> 16);
            o[1] = clamp8((yy - kgu * u - kgv * v) >> 16);
            o[2] = clamp8((yy + krv * v) >> 16);
            o[3] = 0xFF;
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    /// BGRX -> NV12 (só para os testes): cada par UV é a média do bloco 2×2.
    fn bgrx_to_nv12(bgrx: &[u8], w: usize, h: usize, c: ColorSpec) -> (Vec<u8>, Vec<u8>) {
        let (kr, kb) = match c.matrix {
            Matrix::Bt601 => (0.299, 0.114),
            Matrix::Bt709 => (0.2126, 0.0722),
        };
        let kg = 1.0 - kr - kb;
        let yuv = |px: &[u8]| {
            let (b, g, r) = (f64::from(px[0]), f64::from(px[1]), f64::from(px[2]));
            let y = kr * r + kg * g + kb * b;
            let u = (b - y) / (2.0 * (1.0 - kb));
            let v = (r - y) / (2.0 * (1.0 - kr));
            (y, u, v)
        };
        let (ys, us, vs) = match c.range {
            Range::Limited => (219.0 / 255.0, 224.0 / 255.0, 16.0),
            Range::Full => (1.0, 1.0, 0.0),
        };
        let mut y_plane = vec![0u8; w * h];
        let mut uv_plane = vec![0u8; w * h / 2];
        for row in 0..h {
            for x in 0..w {
                let (y, _, _) = yuv(&bgrx[(row * w + x) * 4..]);
                y_plane[row * w + x] = (y * ys + vs).round().clamp(0.0, 255.0) as u8;
            }
        }
        for by in 0..h / 2 {
            for bx in 0..w / 2 {
                let (mut u, mut v) = (0.0, 0.0);
                for (dy, dx) in [(0, 0), (0, 1), (1, 0), (1, 1)] {
                    let (_, uu, vv) = yuv(&bgrx[((by * 2 + dy) * w + bx * 2 + dx) * 4..]);
                    u += uu / 4.0;
                    v += vv / 4.0;
                }
                uv_plane[by * w + bx * 2] = (128.0 + u * us).round().clamp(0.0, 255.0) as u8;
                uv_plane[by * w + bx * 2 + 1] = (128.0 + v * us).round().clamp(0.0, 255.0) as u8;
            }
        }
        (y_plane, uv_plane)
    }

    fn cor_chapada(b: u8, g: u8, r: u8, w: usize, h: usize) -> Vec<u8> {
        [b, g, r, 0xFF].repeat(w * h)
    }

    #[test]
    fn cores_chapadas_ida_e_volta_em_todas_as_combinacoes() {
        let cores: [[u8; 3]; 9] = [[0, 0, 0], [255, 255, 255], [0, 0, 191], [0, 191, 0], [191, 0, 0], [0, 191, 191], [191, 191, 0], [191, 0, 191], [16, 16, 16]];
        for matrix in [Matrix::Bt601, Matrix::Bt709] {
            for range in [Range::Limited, Range::Full] {
                let c = ColorSpec { matrix, range };
                for cor in cores {
                    let src = cor_chapada(cor[0], cor[1], cor[2], 16, 16);
                    let (y, uv) = bgrx_to_nv12(&src, 16, 16, c);
                    let mut out = vec![0u8; 16 * 16 * 4];
                    assert!(nv12_to_bgrx(&y, 16, &uv, 16, 16, 16, c, &mut out));
                    for px in out.as_chunks::<4>().0.iter() {
                        for k in 0..3 {
                            assert!((i32::from(px[k]) - i32::from(cor[k])).abs() <= 2, "{c:?} cor {cor:?} canal {k}: {px:?}");
                        }
                        assert_eq!(px[3], 0xFF);
                    }
                }
            }
        }
    }

    #[test]
    fn valores_conhecidos_da_faixa_limitada() {
        let c = ColorSpec { matrix: Matrix::Bt709, range: Range::Limited };
        let mut out = vec![0u8; 2 * 2 * 4];
        // Preto (Y 16), branco (Y 235), neutro (UV 128).
        for (y, esperado) in [(16u8, 0u8), (235, 255), (126, 128)] {
            assert!(nv12_to_bgrx(&[y; 4], 2, &[128, 128], 2, 2, 2, c, &mut out));
            assert!((i32::from(out[0]) - i32::from(esperado)).abs() <= 1, "Y {y}: {out:?}");
        }
        // Vermelho BT.709 limitado: Y 63, U 102, V 240.
        assert!(nv12_to_bgrx(&[63; 4], 2, &[102, 240], 2, 2, 2, c, &mut out));
        assert!(out[2] >= 250 && out[1] <= 6 && out[0] <= 6, "{out:?}");
        // A mesma entrada em BT.601 dá outro resultado (a matriz errada aparece como erro grande).
        let c601 = ColorSpec { matrix: Matrix::Bt601, range: Range::Limited };
        let mut out601 = vec![0u8; 16];
        assert!(nv12_to_bgrx(&[63; 4], 2, &[102, 240], 2, 2, 2, c601, &mut out601));
        assert!((i32::from(out[1]) - i32::from(out601[1])).abs() > 10 || (i32::from(out[2]) - i32::from(out601[2])).abs() > 5);
    }

    #[test]
    fn faixa_errada_da_erro_grande_em_branco_e_preto() {
        let limitada = ColorSpec { matrix: Matrix::Bt709, range: Range::Limited };
        let completa = ColorSpec { matrix: Matrix::Bt709, range: Range::Full };
        let src = cor_chapada(255, 255, 255, 4, 4);
        let (y, uv) = bgrx_to_nv12(&src, 4, 4, limitada);
        let mut out = vec![0u8; 64];
        assert!(nv12_to_bgrx(&y, 4, &uv, 4, 4, 4, completa, &mut out));
        assert!(out[0] < 245, "branco limitado lido como faixa completa fica {}", out[0]);
    }

    #[test]
    fn stride_maior_que_a_largura_e_planos_separados() {
        let c = ColorSpec::guess(1080);
        assert_eq!(c.matrix, Matrix::Bt709);
        assert_eq!(ColorSpec::guess(384).matrix, Matrix::Bt601);
        let (w, h) = (6usize, 4usize);
        let src = cor_chapada(10, 200, 30, w, h);
        let (y, uv) = bgrx_to_nv12(&src, w, h, c);
        // Mesmo conteúdo com 10 bytes de preenchimento por linha.
        let (ystr, uvstr) = (w + 10, w + 10);
        let mut yp = vec![0xAAu8; ystr * h];
        let mut uvp = vec![0xAAu8; uvstr * h / 2];
        for r in 0..h {
            yp[r * ystr..r * ystr + w].copy_from_slice(&y[r * w..(r + 1) * w]);
        }
        for r in 0..h / 2 {
            uvp[r * uvstr..r * uvstr + w].copy_from_slice(&uv[r * w..(r + 1) * w]);
        }
        let (mut a, mut b) = (vec![0u8; w * h * 4], vec![0u8; w * h * 4]);
        assert!(nv12_to_bgrx(&y, w, &uv, w, w, h, c, &mut a));
        assert!(nv12_to_bgrx(&yp, ystr, &uvp, uvstr, w, h, c, &mut b));
        assert_eq!(a, b);
    }

    #[test]
    fn entradas_invalidas_nao_escrevem_nem_dao_panico() {
        let c = ColorSpec::guess(100);
        let mut out = vec![7u8; 64];
        assert!(!nv12_to_bgrx(&[0; 4], 2, &[0; 2], 2, 3, 2, c, &mut out), "largura ímpar");
        assert!(!nv12_to_bgrx(&[0; 3], 2, &[0; 2], 2, 2, 2, c, &mut out), "plano Y curto");
        assert!(!nv12_to_bgrx(&[0; 4], 2, &[0; 1], 2, 2, 2, c, &mut out), "plano UV curto");
        assert!(!nv12_to_bgrx(&[0; 4], 2, &[0; 2], 2, 2, 2, c, &mut out[..8]), "saída curta");
        assert!(!nv12_to_bgrx(&[], 0, &[], 0, 0, 0, c, &mut out));
        assert!(out.iter().all(|&b| b == 7));
    }

    /// I420 com os mesmos pixels do NV12: o resultado é idêntico, com passos maiores que a largura.
    #[test]
    fn i420_da_o_mesmo_resultado_que_nv12_e_respeita_os_passos() {
        let (w, h) = (8usize, 4usize);
        let mut bgrx = Vec::new();
        for i in 0..w * h {
            bgrx.extend_from_slice(&[(i * 7) as u8, (i * 13) as u8, (i * 29) as u8, 0xFF]);
        }
        let c = ColorSpec { matrix: Matrix::Bt709, range: Range::Limited };
        let (y, uv) = bgrx_to_nv12(&bgrx, w, h, c);
        let mut esperado = vec![0u8; w * h * 4];
        assert!(nv12_to_bgrx(&y, w, &uv, w, w, h, c, &mut esperado));
        // Separa U e V e os coloca em planos com passo maior (e lixo no resto de cada linha).
        let (cw, ch) = (w / 2, h / 2);
        let (us, vs) = (cw + 3, cw + 5);
        let mut u = vec![0xEEu8; us * ch];
        let mut v = vec![0xEEu8; vs * ch];
        for r in 0..ch {
            for x in 0..cw {
                u[r * us + x] = uv[r * w + x * 2];
                v[r * vs + x] = uv[r * w + x * 2 + 1];
            }
        }
        let mut ys = vec![0xEEu8; (w + 6) * h];
        for r in 0..h {
            ys[r * (w + 6)..r * (w + 6) + w].copy_from_slice(&y[r * w..(r + 1) * w]);
        }
        let mut out = vec![0u8; w * h * 4];
        assert!(i420_to_bgrx(&ys, w + 6, &u, us, &v, vs, w, h, c, &mut out));
        assert_eq!(out, esperado);
        // Tamanhos que não cabem ou passos curtos: falso, sem escrever nem entrar em pânico.
        let mut nada = vec![7u8; w * h * 4];
        assert!(!i420_to_bgrx(&ys[..10], w + 6, &u, us, &v, vs, w, h, c, &mut nada));
        assert!(!i420_to_bgrx(&ys, w + 6, &u[..3], us, &v, vs, w, h, c, &mut nada));
        assert!(!i420_to_bgrx(&ys, w - 1, &u, us, &v, vs, w, h, c, &mut nada));
        assert!(!i420_to_bgrx(&ys, w + 6, &u, 1, &v, vs, w, h, c, &mut nada));
        assert!(!i420_to_bgrx(&ys, w + 6, &u, us, &v, vs, 7, h, c, &mut nada));
        assert!(!i420_to_bgrx(&ys, w + 6, &u, us, &v, vs, w, h, c, &mut nada[..8]));
        assert!(nada.iter().all(|&b| b == 7));
    }
}
