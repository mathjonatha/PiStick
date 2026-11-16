//! Utilidades de teste e de benchmark: gerador pseudoaleatório simples (xorshift64*)
//! para não adicionar dependências.

pub struct Rng(u64);

impl Rng {
    pub fn new(seed: u64) -> Self {
        Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1)
    }

    pub fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    /// Inteiro em `0..n` (n > 0).
    pub fn below(&mut self, n: u32) -> u32 {
        ((self.next_u64() >> 32) % u64::from(n)) as u32
    }

    pub fn fill(&mut self, buf: &mut [u8]) {
        for chunk in buf.chunks_mut(8) {
            let v = self.next_u64().to_le_bytes();
            chunk.copy_from_slice(&v[..chunk.len()]);
        }
    }
}

/// Coloca o byte X de cada pixel BGRX em 0xFF.
pub fn normalize_x(pixels: &mut [u8]) {
    for px in pixels.chunks_exact_mut(4) {
        px[3] = 0xFF;
    }
}

/// Gerador da janela de vídeo sintética (marco 4): `psd --synthetic-video` e os testes que
/// comparam com a origem usam este mesmo código. Só aritmética inteira, então o resultado é o
/// mesmo em qualquer máquina.
///
/// A janela tem 1280×720 por padrão. Uma região animada de 640×384 em (64, 64) muda por completo
/// a cada quadro (rampas suaves em movimento e um quadrado que quica: padrão que o H.264 comprime
/// bem). O resto da janela é parado: texto, linhas finas de 1 px e retângulos.
pub mod video {
    use crate::Rect;

    pub const WIDTH: u32 = 1280;
    pub const HEIGHT: u32 = 720;
    /// Região animada, em pixels (múltiplos de 64: blocos inteiros do codec).
    pub const REGION: Rect = Rect::new(64, 64, 640, 384);
    /// Duração de um quadro da animação (30 por segundo).
    pub const FRAME_MS: u64 = 33;

    const BG: [u8; 4] = [0x2A, 0x22, 0x20, 0xFF];
    const BAR: [u8; 4] = [0x3A, 0x30, 0x2C, 0xFF];
    const TEXT: [u8; 4] = [0xE6, 0xE6, 0xE6, 0xFF];
    const LINE_A: [u8; 4] = [0xC0, 0x90, 0x40, 0xFF];
    const LINE_B: [u8; 4] = [0x50, 0xC0, 0x70, 0xFF];
    const FRAME_BOX: [u8; 4] = [0x60, 0x60, 0xE0, 0xFF];

    /// Número do quadro da animação em `now_ms` (milissegundos desde o início da sessão); depois de
    /// `pause_after_ms` a animação para no último quadro.
    pub fn frame_at(now_ms: u64, pause_after_ms: Option<u64>) -> u64 {
        pause_after_ms.map_or(now_ms, |p| now_ms.min(p)) / FRAME_MS
    }

    /// A parte da região animada que cabe numa janela de `w`×`h`.
    pub fn region_in(w: u32, h: u32) -> Option<Rect> {
        REGION.intersect(Rect::new(0, 0, w, h))
    }

    /// Janela inteira no quadro `frame`: parte parada mais região animada. BGRX, stride = `w * 4`.
    pub fn render(w: u32, h: u32, frame: u64) -> Vec<u8> {
        let mut buf = vec![0u8; w as usize * h as usize * 4];
        draw_static(&mut buf, w, h);
        draw_region(&mut buf, w, h, frame);
        buf
    }

    #[allow(clippy::too_many_arguments)]
    fn fill(buf: &mut [u8], w: u32, h: u32, x: i32, y: i32, rw: i32, rh: i32, c: [u8; 4]) {
        let x0 = x.clamp(0, w as i32) as usize;
        let x1 = x.saturating_add(rw).clamp(0, w as i32) as usize;
        let y0 = y.clamp(0, h as i32) as usize;
        let y1 = y.saturating_add(rh).clamp(0, h as i32) as usize;
        if x1 <= x0 {
            return;
        }
        for yy in y0..y1 {
            for px in buf[(yy * w as usize + x0) * 4..(yy * w as usize + x1) * 4].chunks_exact_mut(4) {
                px.copy_from_slice(&c);
            }
        }
    }

    /// Dígitos em fonte 5×7 (cada linha são 5 bits; o bit 4 é a coluna da esquerda).
    fn glyph(ch: char) -> [u8; 7] {
        match ch {
            '0' => [0b01110, 0b10001, 0b10011, 0b10101, 0b11001, 0b10001, 0b01110],
            '1' => [0b00100, 0b01100, 0b00100, 0b00100, 0b00100, 0b00100, 0b01110],
            '2' => [0b01110, 0b10001, 0b00001, 0b00010, 0b00100, 0b01000, 0b11111],
            '3' => [0b11110, 0b00001, 0b00001, 0b01110, 0b00001, 0b00001, 0b11110],
            '4' => [0b00010, 0b00110, 0b01010, 0b10010, 0b11111, 0b00010, 0b00010],
            '5' => [0b11111, 0b10000, 0b11110, 0b00001, 0b00001, 0b10001, 0b01110],
            '6' => [0b00110, 0b01000, 0b10000, 0b11110, 0b10001, 0b10001, 0b01110],
            '7' => [0b11111, 0b00001, 0b00010, 0b00100, 0b01000, 0b01000, 0b01000],
            '8' => [0b01110, 0b10001, 0b10001, 0b01110, 0b10001, 0b10001, 0b01110],
            '9' => [0b01110, 0b10001, 0b10001, 0b01111, 0b00001, 0b00010, 0b01100],
            _ => [0; 7],
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn text(buf: &mut [u8], w: u32, h: u32, x: i32, y: i32, scale: i32, s: &str, c: [u8; 4]) {
        let mut cx = x;
        for ch in s.chars() {
            for (row, bits) in glyph(ch).iter().enumerate() {
                for col in 0..5 {
                    if bits & (0b10000 >> col) != 0 {
                        fill(buf, w, h, cx + col * scale, y + row as i32 * scale, scale, scale, c);
                    }
                }
            }
            cx += 6 * scale;
        }
    }

    /// Desenha a parte parada (a região animada fica com a cor de fundo até `draw_region`).
    pub fn draw_static(buf: &mut [u8], w: u32, h: u32) {
        let (wi, hi) = (w as i32, h as i32);
        fill(buf, w, h, 0, 0, wi, hi, BG);
        fill(buf, w, h, 0, 0, wi, 48, BAR);
        text(buf, w, h, 12, 12, 3, "0123456789", TEXT);
        // Texto em três tamanhos, à direita da região.
        text(buf, w, h, 724, 72, 1, "3141592653589793238462643383279", TEXT);
        text(buf, w, h, 724, 96, 2, "2718281828459045", TEXT);
        text(buf, w, h, 724, 132, 3, "1414213562", TEXT);
        // Linhas finas de 1 px: grade à direita e régua embaixo.
        for i in 0..10 {
            fill(buf, w, h, 724, 180 + i * 16, 520, 1, if i % 2 == 0 { LINE_A } else { LINE_B });
        }
        for i in 0..34 {
            fill(buf, w, h, 724 + i * 16, 180, 1, 160, if i % 2 == 0 { LINE_B } else { LINE_A });
        }
        for i in 0..40 {
            fill(buf, w, h, 64 + i * 16, 464, 1, 24 + (i % 4) * 8, LINE_A);
        }
        fill(buf, w, h, 64, 464, 640, 1, TEXT);
        text(buf, w, h, 64, 504, 2, "0123456789 0123456789 0123456789", TEXT);
        text(buf, w, h, 64, 532, 1, "9876543210987654321098765432109876543210", TEXT);
        // Diagonal fina e retângulos vazados.
        for i in 0..200 {
            fill(buf, w, h, 64 + i * 3, 580 + i / 2, 1, 1, LINE_B);
        }
        for k in 0..4 {
            let (x, y) = (740 + k * 130, 400 + k * 6);
            fill(buf, w, h, x, y, 112, 1, FRAME_BOX);
            fill(buf, w, h, x, y + 90, 112, 1, FRAME_BOX);
            fill(buf, w, h, x, y, 1, 91, FRAME_BOX);
            fill(buf, w, h, x + 111, y, 1, 91, FRAME_BOX);
        }
    }

    /// Onda triangular de 0 a 255 com o período dado.
    fn tri(v: i64, period: i64) -> u8 {
        let m = v.rem_euclid(period);
        let half = period / 2;
        let d = if m < half { m } else { period - m };
        (d * 255 / half) as u8
    }

    /// Vai e volta entre 0 e `span`.
    fn bounce(v: i64, span: i64) -> i64 {
        let m = v.rem_euclid(2 * span);
        if m <= span {
            m
        } else {
            2 * span - m
        }
    }

    /// O quadrado cinza chapado (0xF0) que quica na região, em coordenadas da própria região.
    pub fn box_rect(frame: u64) -> Rect {
        let n = (frame % 1_000_000) as i64;
        Rect::new(bounce(n * 5, i64::from(REGION.w) - 96) as u32, bounce(n * 3, i64::from(REGION.h) - 96) as u32, 96, 96)
    }

    /// Desenha a região animada do quadro `frame`, recortada à janela.
    pub fn draw_region(buf: &mut [u8], w: u32, h: u32, frame: u64) {
        let n = (frame % 1_000_000) as i64;
        let Some(vis) = region_in(w, h) else { return };
        let (ox, oy) = (i64::from(REGION.x), i64::from(REGION.y));
        let (bx, by) = (bounce(n * 5, i64::from(REGION.w) - 96), bounce(n * 3, i64::from(REGION.h) - 96));
        for y in vis.y..vis.y + vis.h {
            let v = i64::from(y) - oy;
            let g = tri(v + 2 * n, 256);
            let row = &mut buf[(y as usize * w as usize + vis.x as usize) * 4..(y as usize * w as usize + (vis.x + vis.w) as usize) * 4];
            for (i, px) in row.chunks_exact_mut(4).enumerate() {
                let u = i64::from(vis.x) + i as i64 - ox;
                if (bx..bx + 96).contains(&u) && (by..by + 96).contains(&v) {
                    px.copy_from_slice(&[0xF0, 0xF0, 0xF0, 0xFF]);
                } else {
                    px[0] = tri(u + 3 * n, 320);
                    px[1] = g;
                    px[2] = tri(u + v - 4 * n, 512);
                    px[3] = 0xFF;
                }
            }
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn quadro_depende_so_do_numero_e_a_pausa_congela() {
            assert_eq!(render(WIDTH, HEIGHT, 7), render(WIDTH, HEIGHT, 7));
            assert_eq!(frame_at(0, None), 0);
            assert_eq!(frame_at(32, None), 0);
            assert_eq!(frame_at(33, None), 1);
            assert_eq!(frame_at(10_000, Some(4_000)), 4_000 / FRAME_MS);
            assert_eq!(frame_at(3_000, Some(4_000)), 3_000 / FRAME_MS);
        }

        #[test]
        fn so_a_regiao_muda_e_ela_muda_em_todos_os_blocos_a_cada_quadro() {
            let a = render(WIDTH, HEIGHT, 100);
            let b = render(WIDTH, HEIGHT, 101);
            let stride = WIDTH as usize * 4;
            for y in 0..HEIGHT as usize {
                for x in 0..WIDTH as usize {
                    let dentro = (64..704).contains(&x) && (64..448).contains(&y);
                    let igual = a[y * stride + x * 4..y * stride + x * 4 + 4] == b[y * stride + x * 4..y * stride + x * 4 + 4];
                    assert!(dentro || igual, "pixel ({x}, {y}) fora da região mudou");
                }
            }
            // Cada bloco de 64×64 da região tem algum pixel diferente (é isso que o torna "quente").
            for by in 1..7usize {
                for bx in 1..11usize {
                    let mut mudou = false;
                    for y in by * 64..by * 64 + 64 {
                        mudou |= a[y * stride + bx * 256..y * stride + bx * 256 + 256] != b[y * stride + bx * 256..y * stride + bx * 256 + 256];
                    }
                    assert!(mudou, "bloco ({bx}, {by}) igual nos quadros 100 e 101");
                }
            }
        }

        #[test]
        fn a_parte_parada_tem_detalhe_fino_e_a_regiao_e_suave() {
            let img = render(WIDTH, HEIGHT, 0);
            let stride = WIDTH as usize * 4;
            // Linhas de 1 px: numa linha vertical da grade, o vizinho da direita tem outra cor...
            let (x, y) = (724 + 16 * 3, 190);
            assert_ne!(img[y * stride + x * 4..y * stride + x * 4 + 3], img[y * stride + (x + 1) * 4..y * stride + (x + 1) * 4 + 3]);
            // ... e numa linha horizontal, o vizinho de baixo.
            let (x, y) = (730, 180 + 16);
            assert_ne!(img[y * stride + x * 4..y * stride + x * 4 + 3], img[(y + 1) * stride + x * 4..(y + 1) * stride + x * 4 + 3]);
            // Região: vizinhos na horizontal diferem pouco (fora do quadrado que quica).
            let (x, y) = (600usize, 400usize);
            for c in 0..3 {
                let d = (i32::from(img[y * stride + x * 4 + c]) - i32::from(img[y * stride + (x + 1) * 4 + c])).abs();
                assert!(d <= 4, "canal {c}: diferença {d} entre vizinhos");
            }
        }

        #[test]
        fn janela_menor_que_a_regiao_recorta_sem_panico() {
            for (w, h) in [(64u32, 64u32), (100, 90), (300, 200), (1280, 720), (4096, 4096)] {
                let img = render(w, h, 5);
                assert_eq!(img.len(), w as usize * h as usize * 4);
            }
            assert_eq!(region_in(64, 64), None);
            assert_eq!(region_in(300, 200), Some(Rect::new(64, 64, 236, 136)));
        }
    }
}

/// Barras de cor para conferir matriz e faixa de cor do decodificador: oito barras verticais chapadas
/// (branco, amarelo, ciano, verde, magenta, vermelho, azul, preto, nesta ordem, com 75% de
/// intensidade nas cores, como nas barras SMPTE) nas linhas de cima, e embaixo uma faixa de rampa que
/// anda a cada quadro (para haver movimento e quadros P de verdade). Qualquer tamanho a partir de 64×64.
pub mod bars {
    /// Cores das barras, BGR.
    pub const COLORS: [[u8; 3]; 8] = [
        [0xBF, 0xBF, 0xBF], // branco 75%
        [0x00, 0xBF, 0xBF], // amarelo
        [0xBF, 0xBF, 0x00], // ciano
        [0x00, 0xBF, 0x00], // verde
        [0xBF, 0x00, 0xBF], // magenta
        [0x00, 0x00, 0xBF], // vermelho
        [0xBF, 0x00, 0x00], // azul
        [0x10, 0x10, 0x10], // preto
    ];

    /// Altura da faixa animada embaixo.
    pub const STRIP_H: u32 = 40;

    /// Retângulo (x, y, largura, altura) do miolo chapado de cada barra, afastado 8 px das bordas
    /// (a subamostragem de cor borra as bordas) e da faixa animada.
    pub fn flat_areas(w: u32, h: u32) -> Vec<[u32; 4]> {
        let bar = w / 8;
        (0..8).map(|i| [i * bar + 8, 8, bar.saturating_sub(16), h.saturating_sub(STRIP_H + 16)]).collect()
    }

    /// Quadro `frame`, BGRX com stride `w * 4`.
    pub fn render(w: u32, h: u32, frame: u64) -> Vec<u8> {
        let mut buf = vec![0xFFu8; w as usize * h as usize * 4];
        let bar = (w / 8).max(1);
        let top = h.saturating_sub(STRIP_H);
        for y in 0..h {
            for x in 0..w {
                let i = (y as usize * w as usize + x as usize) * 4;
                if y < top {
                    buf[i..i + 3].copy_from_slice(&COLORS[((x / bar) as usize).min(7)]);
                } else {
                    let v = ((x as u64 * 3 + frame * 5) % 510) as i64;
                    let v = if v < 255 { v } else { 510 - v } as u8;
                    buf[i] = v;
                    buf[i + 1] = 255 - v;
                    buf[i + 2] = ((y - top) * 6 % 256) as u8;
                }
            }
        }
        buf
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn barras_chapadas_e_faixa_animada() {
            let (w, h) = (320u32, 200u32);
            let a = render(w, h, 3);
            let b = render(w, h, 4);
            for r in flat_areas(w, h) {
                for y in r[1]..r[1] + r[3] {
                    for x in r[0]..r[0] + r[2] {
                        let i = (y as usize * w as usize + x as usize) * 4;
                        assert_eq!(a[i..i + 3], b[i..i + 3]);
                        assert_eq!(a[i..i + 3], a[(r[1] as usize * w as usize + r[0] as usize) * 4..][..3]);
                    }
                }
            }
            let i = ((h - 5) as usize * w as usize + 7) * 4;
            assert_ne!(a[i..i + 3], b[i..i + 3], "a faixa de baixo anda");
            assert_eq!(a[3], 0xFF);
        }
    }
}

/// Gerador da janela de listras sintética (`docs/20`): `psd --synthetic --synthetic-stripes` e a medição
/// `atomic` do `psc`. Cada quadro é ruído que não comprime (para forçar o fatiamento) e, no começo de cada
/// faixa de [`MARKER_EVERY`] linhas, um bloco de [`MARKER_W`]×[`MARKER_H`] pixels de cor sólida que
/// codifica o número do quadro (24 bits em B, G, R). Dois marcadores com números diferentes numa mesma imagem
/// apresentada denunciam um quadro feito de instantes diferentes.
pub mod stripes {
    use super::Rng;

    pub const WIDTH: u32 = 960;
    pub const HEIGHT: u32 = 600;
    /// Quadros por segundo (um a cada [`FRAME_MS`]).
    pub const FPS: u64 = 30;
    pub const FRAME_MS: u64 = 33;
    /// Distância entre marcadores, em linhas.
    pub const MARKER_EVERY: u32 = 32;
    pub const MARKER_W: u32 = 64;
    pub const MARKER_H: u32 = 8;

    /// Quantos marcadores cabem numa janela de altura `h`.
    pub fn marker_count(h: u32) -> usize {
        h.div_ceil(MARKER_EVERY) as usize
    }

    /// Desenha o quadro `n` em `buf` (BGRX, `w`×`h`, stride = `w` × 4).
    pub fn render_into(buf: &mut [u8], w: u32, h: u32, n: u32) {
        let mut rng = Rng::new(0x5EED_0000 + u64::from(n));
        rng.fill(buf);
        for px in buf.chunks_exact_mut(4) {
            px[3] = 0xFF;
        }
        let cor = [n as u8, (n >> 8) as u8, (n >> 16) as u8, 0xFF];
        for k in 0..marker_count(h) as u32 {
            for y in (k * MARKER_EVERY)..(k * MARKER_EVERY + MARKER_H).min(h) {
                for x in 0..MARKER_W.min(w) {
                    let i = (y as usize * w as usize + x as usize) * 4;
                    buf[i..i + 4].copy_from_slice(&cor);
                }
            }
        }
    }

    /// Número do quadro codificado em cada marcador, de cima para baixo. Um marcador que não é de cor sólida
    /// (imagem que não vem do gerador) conta como `u32::MAX`.
    pub fn read_markers(pixels: &[u8], w: u32, h: u32) -> Vec<u32> {
        (0..marker_count(h) as u32)
            .map(|k| {
                let mut valor = None;
                for y in (k * MARKER_EVERY)..(k * MARKER_EVERY + MARKER_H).min(h) {
                    for x in 0..MARKER_W.min(w) {
                        let i = (y as usize * w as usize + x as usize) * 4;
                        let v = u32::from(pixels[i]) | u32::from(pixels[i + 1]) << 8 | u32::from(pixels[i + 2]) << 16;
                        match valor {
                            None => valor = Some(v),
                            Some(anterior) if anterior != v => return u32::MAX,
                            _ => {}
                        }
                    }
                }
                valor.unwrap_or(u32::MAX)
            })
            .collect()
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn marcadores_ida_e_volta_e_ruido_diferente() {
            let (w, h) = (WIDTH, HEIGHT);
            let mut a = vec![0u8; (w * h * 4) as usize];
            let mut b = a.clone();
            render_into(&mut a, w, h, 0x0001_02FF);
            render_into(&mut b, w, h, 0x0001_0300);
            assert_eq!(read_markers(&a, w, h), vec![0x0001_02FF; 19]);
            assert_eq!(read_markers(&b, w, h), vec![0x0001_0300; 19]);
            assert_ne!(a[1000 * 4..1100 * 4], b[1000 * 4..1100 * 4], "ruído de quadros diferentes difere");
            // Misturando faixas de dois quadros, os marcadores acusam.
            let mut mista = a.clone();
            let metade = (h / 2 * w * 4) as usize;
            mista[metade..].copy_from_slice(&b[metade..]);
            let m = read_markers(&mista, w, h);
            assert!(m.contains(&0x0001_02FF) && m.contains(&0x0001_0300));
            // Fora dos marcadores, o X é 0xFF.
            assert!(a.chunks_exact(4).all(|p| p[3] == 0xFF));
        }
    }
}
