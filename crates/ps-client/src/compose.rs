//! Composição de uma janela com os popups dela num só quadro BGRX: a captura "janela com os popups" do `psc` (comando
//! `shot`) e a tela do cliente Android, que mostra a janela em foco com os popups por cima.
//!
//! A parte pura ([`Camada`], [`compor`]) não conhece janelas nem travas; [`compose_with_popups`] a alimenta com o
//! registro de janelas do cliente.

use crate::registry::WinInfo;

/// Uma janela (a raiz ou um popup) com o quadro dela, para compor.
#[derive(Debug, Clone)]
pub struct Camada {
    pub id: u32,
    /// Janela mãe (a raiz tem 0). Popups ficam na posição `x, y` **relativa ao conteúdo da mãe**.
    pub parent: u32,
    pub x: i32,
    pub y: i32,
    pub w: u32,
    pub h: u32,
    /// BGRX, `w * h * 4` bytes.
    pub pixels: Vec<u8>,
}

/// O quadro composto: o menor retângulo que cobre a raiz e todos os popups.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Composto {
    pub largura: u32,
    pub altura: u32,
    /// BGRX, `largura * altura * 4` bytes (fundo cinza onde não há janela).
    pub pixels: Vec<u8>,
    /// Posição do canto superior esquerdo do quadro composto em coordenadas da janela raiz (zero ou negativa: um popup
    /// pode sair para cima ou para a esquerda da janela).
    pub origem_x: i32,
    pub origem_y: i32,
}

/// Cinza do fundo (o mesmo que o `psc` usa fora do quadro remoto).
pub const FUNDO_BGRX: [u8; 4] = [0x30, 0x30, 0x30, 0xFF];

/// Cola `src` (BGRX) em `dst` (BGRX, `dw` x `dh`) com o canto superior esquerdo em (ox, oy),
/// recortando o que sai do destino.
#[allow(clippy::too_many_arguments)]
pub fn paste_bgrx(dst: &mut [u8], dw: usize, dh: usize, src: &[u8], sw: usize, sh: usize, ox: i32, oy: i32) {
    for sy in 0..sh {
        let y = oy + sy as i32;
        if y < 0 || y >= dh as i32 {
            continue;
        }
        let x0 = ox.max(0);
        let x1 = (ox + sw as i32).min(dw as i32);
        if x1 <= x0 {
            continue;
        }
        let (sx0, n) = ((x0 - ox) as usize, (x1 - x0) as usize);
        let s = (sy * sw + sx0) * 4;
        let d = (y as usize * dw + x0 as usize) * 4;
        dst[d..d + n * 4].copy_from_slice(&src[s..s + n * 4]);
    }
}

/// Compõe `camadas` (a primeira é a raiz, em (0, 0); os popups vêm depois, o pai sempre antes do filho, na ordem de
/// empilhamento: o último fica por cima). Popup cujo pai não está na lista é ignorado. Camada com quadro de tamanho
/// incoerente (`pixels` menor que `w * h * 4`) é ignorada. `None` se a lista está vazia.
pub fn compor(camadas: &[Camada]) -> Option<Composto> {
    let raiz = camadas.first()?;
    let mut itens: Vec<(&Camada, i32, i32)> = vec![(raiz, 0, 0)];
    for c in &camadas[1..] {
        if let Some(&(_, px, py)) = itens.iter().find(|(p, _, _)| p.id == c.parent) {
            itens.push((c, px + c.x, py + c.y));
        }
    }
    itens.retain(|(c, _, _)| c.pixels.len() >= c.w as usize * c.h as usize * 4);
    if itens.is_empty() {
        return None;
    }
    let (mut x0, mut y0, mut x1, mut y1) = (0i32, 0i32, 0i32, 0i32);
    for (c, ax, ay) in &itens {
        x0 = x0.min(*ax);
        y0 = y0.min(*ay);
        x1 = x1.max(ax + c.w as i32);
        y1 = y1.max(ay + c.h as i32);
    }
    let (cw, ch) = ((x1 - x0) as usize, (y1 - y0) as usize);
    let mut pixels = FUNDO_BGRX.repeat(cw * ch);
    for (c, ax, ay) in &itens {
        paste_bgrx(&mut pixels, cw, ch, &c.pixels, c.w as usize, c.h as usize, ax - x0, ay - y0);
    }
    Some(Composto { largura: cw as u32, altura: ch as u32, pixels, origem_x: x0, origem_y: y0 })
}

/// Quadro atual de uma janela: largura, altura e pixels BGRX.
pub fn frame_of(w: &WinInfo) -> (u32, u32, Vec<u8>) {
    let g = w.shared.inner.lock().unwrap_or_else(|e| e.into_inner());
    (g.decoder.width(), g.decoder.height(), g.display_pixels().to_vec())
}

/// Janela `raiz` com os popups vivos sobre ela, nas posições em que o hospedeiro os mostra.
/// `None` se a raiz não tem popup.
pub fn compose_with_popups(root: &WinInfo, all: &[WinInfo]) -> Option<(u32, u32, Vec<u8>)> {
    // (janela, posição absoluta) em ordem de criação; o pai sempre vem antes do filho.
    let mut ids: Vec<&WinInfo> = vec![root];
    for w in all.iter().filter(|w| w.alive && w.popup) {
        if ids.iter().any(|p| p.id == w.parent) {
            ids.push(w);
        }
    }
    if ids.len() == 1 {
        return None;
    }
    let camadas: Vec<Camada> = ids
        .iter()
        .map(|w| {
            let (fw, fh, px) = frame_of(w);
            Camada { id: w.id, parent: w.parent, x: w.x, y: w.y, w: fw, h: fh, pixels: px }
        })
        .collect();
    let c = compor(&camadas)?;
    Some((c.largura, c.altura, c.pixels))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn camada(id: u32, parent: u32, x: i32, y: i32, w: u32, h: u32, v: u8) -> Camada {
        Camada { id, parent, x, y, w, h, pixels: [v, v, v, 0xFF].repeat((w * h) as usize) }
    }

    #[test]
    fn raiz_sozinha_tem_o_tamanho_dela_e_origem_zero() {
        let c = compor(&[camada(1, 0, 0, 0, 4, 3, 9)]).unwrap();
        assert_eq!((c.largura, c.altura, c.origem_x, c.origem_y), (4, 3, 0, 0));
        assert!(c.pixels.chunks(4).all(|p| p == [9, 9, 9, 0xFF]));
        assert!(compor(&[]).is_none());
    }

    #[test]
    fn popup_dentro_da_janela_cobre_a_raiz_na_posicao_certa() {
        let c = compor(&[camada(1, 0, 0, 0, 10, 10, 1), camada(2, 1, 2, 3, 3, 2, 7)]).unwrap();
        assert_eq!((c.largura, c.altura, c.origem_x, c.origem_y), (10, 10, 0, 0));
        let px = |x: usize, y: usize| c.pixels[(y * 10 + x) * 4];
        assert_eq!(px(1, 3), 1);
        assert_eq!(px(2, 3), 7);
        assert_eq!(px(4, 4), 7);
        assert_eq!(px(5, 4), 1);
        assert_eq!(px(3, 5), 1);
    }

    #[test]
    fn popup_que_sai_da_janela_amplia_o_quadro_e_a_origem_fica_negativa() {
        // Um popup de 4x4 em (-2, -1) e outro, filho dele, em (3, 5) relativo ao primeiro.
        let c = compor(&[camada(1, 0, 0, 0, 6, 6, 1), camada(2, 1, -2, -1, 4, 4, 7), camada(3, 2, 3, 5, 2, 2, 5)]).unwrap();
        assert_eq!((c.origem_x, c.origem_y), (-2, -1));
        // Largura: de -2 até max(6, -2+3+2=3, ...) = 6 => 8; altura: de -1 até max(6, -1+5+2=6) = 6 => 7.
        assert_eq!((c.largura, c.altura), (8, 7));
        let px = |x: i32, y: i32| c.pixels[(((y + 1) * 8 + (x + 2)) as usize) * 4];
        assert_eq!(px(-2, -1), 7);
        assert_eq!(px(1, 1), 7);
        assert_eq!(px(2, 2), 1);
        assert_eq!(px(1, 4), 5);
        // Fora de qualquer janela: fundo cinza.
        assert_eq!(&c.pixels[6 * 4..][..4], &FUNDO_BGRX);
    }

    #[test]
    fn popup_sem_pai_e_camada_incoerente_sao_ignorados() {
        let mut ruim = camada(3, 1, 0, 0, 5, 5, 8);
        ruim.pixels.truncate(10);
        let c = compor(&[camada(1, 0, 0, 0, 4, 4, 1), camada(2, 99, 1, 1, 2, 2, 7), ruim]).unwrap();
        assert_eq!((c.largura, c.altura), (4, 4));
        assert!(c.pixels.chunks(4).all(|p| p[0] == 1));
    }
}
