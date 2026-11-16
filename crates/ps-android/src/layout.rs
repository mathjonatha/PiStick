//! Tamanho do pedido, vista (pinça e deslocamento locais), desenho em RGBA e acerto do toque (docs/26, decisões 6 e 7).
//! Lógica pura, sem nenhuma chamada do Android.
//!
//! Vocabulário:
//! - **tela**: a superfície do celular, em pixels físicos;
//! - **composto**: a janela em foco com os popups por cima (`ps_client::compose`), em pixels remotos; o canto superior
//!   esquerdo dele fica em `(origem_x, origem_y)` nas coordenadas da janela;
//! - **vista**: a região do composto que aparece na tela. Com ampliação 1 a vista tem o tamanho que o cliente pediu ao
//!   servidor (a janela inteira); a pinça a amplia e o arrasto de dois dedos a desloca, **só aqui**, sem novo
//!   `RESIZE_REQ`.

use ps_client::compose::{Composto, FUNDO_BGRX};

use crate::janelas::{Limites, PopupVisivel};

/// Densidade de referência: com ela, o fator padrão mostra cada pixel remoto como [`FATOR_PADRAO`] pixels do celular.
pub const DPI_REFERENCIA: f32 = 440.0;
/// A janela remota tem a largura da tela dividida por 2,5 (432 pixels remotos nos 1080 do celular de teste).
pub const FATOR_PADRAO: f32 = 2.5;
/// Menor e maior fator que a casca pode escolher, e o passo entre um e outro.
pub const FATOR_MIN: f32 = 1.0;
pub const FATOR_MAX: f32 = 4.0;
pub const FATOR_PASSO: f32 = 0.5;

/// Leva um fator qualquer ao passo mais próximo dentro de `[FATOR_MIN, FATOR_MAX]`; valor que não é número vira o padrão.
pub fn limitar_fator(f: f32) -> f32 {
    if !f.is_finite() {
        return FATOR_PADRAO;
    }
    ((f / FATOR_PASSO).round() * FATOR_PASSO).clamp(FATOR_MIN, FATOR_MAX)
}

/// O fator que a casca guarda em centésimos (250 = 2,5), validado e limitado.
pub fn fator_de_centesimos(centesimos: i32) -> f32 {
    limitar_fator(centesimos as f32 / 100.0)
}
/// Menor tamanho que se pede a uma janela, mesmo que a tela seja minúscula.
pub const MIN_PEDIDO: u32 = 64;
/// Ampliação máxima da pinça.
pub const ZOOM_MAX: f32 = 5.0;

/// A superfície em que o núcleo desenha.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Tela {
    pub largura: u32,
    pub altura: u32,
    /// Densidade em pontos por polegada (`DisplayMetrics.densityDpi`).
    pub dpi: u32,
}

/// Quantos pixels da tela cobrem um pixel remoto com ampliação 1: `fator * dpi / 440`.
pub fn px_por_pixel_remoto(dpi: u32, fator: f32) -> f32 {
    let dpi = if dpi == 0 { DPI_REFERENCIA } else { dpi as f32 };
    (fator * dpi / DPI_REFERENCIA).max(0.1)
}

/// O tamanho a pedir ao servidor (`RESIZE_REQ`) para a janela caber na tela: tamanho da tela dividido pelos pixels da
/// tela por pixel remoto, dentro dos limites que o servidor informou (zero = sem limite; o mínimo vence o máximo
/// absurdo). Nunca zero.
pub fn pedido_de_tamanho(tela: &Tela, fator: f32, lim: &Limites) -> (u32, u32) {
    let s = px_por_pixel_remoto(tela.dpi, fator);
    let eixo = |px: u32, min: u32, max: u32| {
        let mut v = ((px as f32 / s).round() as u32).max(MIN_PEDIDO);
        if max > 0 {
            v = v.min(max);
        }
        v.max(min).max(1)
    };
    (eixo(tela.largura, lim.min_w, lim.max_w), eixo(tela.altura, lim.min_h, lim.max_h))
}

/// A vista: ampliação e deslocamento locais do composto.
#[derive(Debug, Clone, PartialEq)]
pub struct Vista {
    tela: (u32, u32),
    /// Pixels da tela por pixel remoto com ampliação 1.
    s0: f32,
    zoom: f32,
    /// Canto superior esquerdo da região visível, em pixels do composto.
    cx: f32,
    cy: f32,
}

impl Vista {
    pub fn nova(tela: &Tela, fator: f32) -> Vista {
        Vista { tela: (tela.largura.max(1), tela.altura.max(1)), s0: px_por_pixel_remoto(tela.dpi, fator), zoom: 1.0, cx: 0.0, cy: 0.0 }
    }

    pub fn zoom(&self) -> f32 {
        self.zoom
    }

    /// Pixels da tela por pixel remoto agora (com a ampliação).
    pub fn escala(&self) -> f32 {
        self.s0 * self.zoom
    }

    /// Tamanho da região visível em pixels do composto.
    fn visivel(&self) -> (f32, f32) {
        (self.tela.0 as f32 / self.escala(), self.tela.1 as f32 / self.escala())
    }

    /// Mantém a vista dentro do composto: se o composto é maior que a região, a região não sai dele; se é menor, fica
    /// centrado.
    pub fn ajustar(&mut self, composto: (u32, u32)) {
        let (vw, vh) = self.visivel();
        let eixo = |c: f32, v: f32, atual: f32| if c >= v { atual.clamp(0.0, c - v) } else { (c - v) / 2.0 };
        self.cx = eixo(composto.0 as f32, vw, self.cx);
        self.cy = eixo(composto.1 as f32, vh, self.cy);
    }

    /// Volta ao ajuste: ampliação 1, canto em (0, 0).
    pub fn voltar_ao_ajuste(&mut self, composto: (u32, u32)) {
        self.zoom = 1.0;
        self.cx = 0.0;
        self.cy = 0.0;
        self.ajustar(composto);
    }

    /// Pinça: multiplica a ampliação por `razao` mantendo fixo o ponto do composto que está sob `foco` (em pixels da
    /// tela).
    pub fn pincar(&mut self, foco: (f32, f32), razao: f32, composto: (u32, u32)) {
        if !razao.is_finite() || razao <= 0.0 {
            return;
        }
        let ponto = self.tela_para_composto(foco.0, foco.1);
        self.zoom = (self.zoom * razao).clamp(1.0, ZOOM_MAX);
        let s = self.escala();
        self.cx = ponto.0 - foco.0 / s;
        self.cy = ponto.1 - foco.1 / s;
        self.ajustar(composto);
    }

    /// Arrasto de dois dedos com `dx, dy` pixels da tela: o conteúdo acompanha os dedos.
    pub fn deslocar(&mut self, dx: f32, dy: f32, composto: (u32, u32)) {
        let s = self.escala();
        self.cx -= dx / s;
        self.cy -= dy / s;
        self.ajustar(composto);
    }

    /// Ponto da tela (pixels) no composto (pixels remotos).
    pub fn tela_para_composto(&self, x: f32, y: f32) -> (f32, f32) {
        let s = self.escala();
        (self.cx + x / s, self.cy + y / s)
    }

    /// Retângulo do composto que o desenho cobre: canto inteiro e tamanho do buffer (nunca zero). O buffer tem o
    /// tamanho da região visível; a própria superfície o estica para a tela.
    pub fn regiao(&self) -> (i32, i32, u32, u32) {
        let (vw, vh) = self.visivel();
        (self.cx.round() as i32, self.cy.round() as i32, (vw.round() as u32).max(1), (vh.round() as u32).max(1))
    }
}

/// Desenha a região visível do composto em `saida` (RGBA, alfa 255; o fundo cinza onde não há janela) e devolve o
/// tamanho do buffer. Aproveita a capacidade de `saida`.
pub fn desenhar_rgba(c: &Composto, vista: &Vista, saida: &mut Vec<u8>) -> (u32, u32) {
    let (rx, ry, rw, rh) = vista.regiao();
    let (rw_us, rh_us) = (rw as usize, rh as usize);
    saida.clear();
    saida.resize(rw_us * rh_us * 4, 0);
    let fundo = [FUNDO_BGRX[2], FUNDO_BGRX[1], FUNDO_BGRX[0], 0xFF];
    let (cw, ch) = (c.largura as i32, c.altura as i32);
    // Colunas do destino que caem dentro do composto.
    let x0 = (-rx).clamp(0, rw as i32);
    let x1 = (cw - rx).clamp(x0, rw as i32);
    for y in 0..rh_us {
        let linha = &mut saida[y * rw_us * 4..(y + 1) * rw_us * 4];
        let sy = ry + y as i32;
        if sy < 0 || sy >= ch || x1 <= x0 {
            linha.as_chunks_mut::<4>().0.iter_mut().for_each(|p| *p = fundo);
            continue;
        }
        let (antes, resto) = linha.split_at_mut(x0 as usize * 4);
        let (meio, depois) = resto.split_at_mut((x1 - x0) as usize * 4);
        antes.as_chunks_mut::<4>().0.iter_mut().for_each(|p| *p = fundo);
        depois.as_chunks_mut::<4>().0.iter_mut().for_each(|p| *p = fundo);
        let s0 = (sy as usize * c.largura as usize + (rx + x0) as usize) * 4;
        let origem = &c.pixels[s0..s0 + meio.len()];
        for (d, s) in meio.as_chunks_mut::<4>().0.iter_mut().zip(origem.as_chunks::<4>().0) {
            *d = [s[2], s[1], s[0], 0xFF];
        }
    }
    (rw, rh)
}

/// Onde um toque caiu: a janela (a raiz ou um popup) e as coordenadas **locais** dela, em pixels remotos.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Alvo {
    pub win: u32,
    pub x: f32,
    pub y: f32,
}

/// Acha o alvo de um ponto do composto. `raiz` é a janela em foco com `(raiz_w, raiz_h)`; `popups` vêm na ordem de
/// empilhamento (o último está por cima) com posição absoluta. `origem` é a posição do composto nas coordenadas da
/// janela. Um popup vence a janela; entre popups vence o mais alto. Fora de tudo: `None`.
pub fn acertar(ponto_composto: (f32, f32), origem: (i32, i32), raiz: (u32, (u32, u32)), popups: &[PopupVisivel]) -> Option<Alvo> {
    let (px, py) = (ponto_composto.0 + origem.0 as f32, ponto_composto.1 + origem.1 as f32);
    for p in popups.iter().rev() {
        if px >= p.x as f32 && py >= p.y as f32 && px < (p.x + p.w as i32) as f32 && py < (p.y + p.h as i32) as f32 {
            return Some(Alvo { win: p.id, x: px - p.x as f32, y: py - p.y as f32 });
        }
    }
    let (id, (w, h)) = raiz;
    (px >= 0.0 && py >= 0.0 && px < w as f32 && py < h as f32).then_some(Alvo { win: id, x: px, y: py })
}

#[cfg(test)]
mod tests {
    use super::*;

    const CELULAR: Tela = Tela { largura: 1080, altura: 2400, dpi: 440 };

    fn composto(w: u32, h: u32, v: u8) -> Composto {
        Composto { largura: w, altura: h, pixels: [v, v / 2, v / 4, 0xFF].repeat((w * h) as usize), origem_x: 0, origem_y: 0 }
    }

    #[test]
    fn pedido_padrao_no_celular_de_teste() {
        assert_eq!(FATOR_PADRAO, 2.5);
        assert_eq!(pedido_de_tamanho(&CELULAR, FATOR_PADRAO, &Limites::default()), (432, 960));
    }

    #[test]
    fn pedido_muda_com_o_fator() {
        let lim = Limites::default();
        assert_eq!(pedido_de_tamanho(&CELULAR, 2.0, &lim).0, 540);
        assert_eq!(pedido_de_tamanho(&CELULAR, 2.5, &lim).0, 432);
        assert_eq!(pedido_de_tamanho(&CELULAR, 3.0, &lim).0, 360);
        assert_eq!(pedido_de_tamanho(&CELULAR, 1.0, &lim), (1080, 2400));
    }

    #[test]
    fn fator_tem_passo_e_limites() {
        assert_eq!(limitar_fator(2.5), 2.5);
        assert_eq!(limitar_fator(2.6), 2.5);
        assert_eq!(limitar_fator(2.8), 3.0);
        assert_eq!(limitar_fator(0.0), FATOR_MIN);
        assert_eq!(limitar_fator(-3.0), FATOR_MIN);
        assert_eq!(limitar_fator(9.0), FATOR_MAX);
        assert_eq!(limitar_fator(f32::NAN), FATOR_PADRAO);
        assert_eq!(limitar_fator(f32::INFINITY), FATOR_PADRAO);
        assert_eq!(fator_de_centesimos(250), 2.5);
        assert_eq!(fator_de_centesimos(100), 1.0);
        assert_eq!(fator_de_centesimos(400), 4.0);
        assert_eq!(fator_de_centesimos(-5), FATOR_MIN);
        assert_eq!(fator_de_centesimos(i32::MAX), FATOR_MAX);
        // Todo passo válido é fixo sob a limitação.
        let mut f = FATOR_MIN;
        while f <= FATOR_MAX {
            assert_eq!(limitar_fator(f), f);
            f += FATOR_PASSO;
        }
    }

    #[test]
    fn pedido_no_celular_de_teste_com_fator_2() {
        assert_eq!(pedido_de_tamanho(&CELULAR, 2.0, &Limites::default()), (540, 1200));
        // Densidade menor: cada pixel remoto cobre menos pixels da tela, a janela remota fica maior.
        let t = Tela { largura: 1080, altura: 1920, dpi: 320 };
        let (w, h) = pedido_de_tamanho(&t, 2.0, &Limites::default());
        assert!(w > 540 && h > 960, "{w}x{h}");
        // Densidade desconhecida (0) cai na de referência.
        assert_eq!(pedido_de_tamanho(&Tela { dpi: 0, ..CELULAR }, 2.0, &Limites::default()), (540, 1200));
        // Fator maior: janela menor.
        assert_eq!(pedido_de_tamanho(&CELULAR, 4.0, &Limites::default()), (270, 600));
    }

    #[test]
    fn pedido_respeita_os_limites_e_nunca_e_zero() {
        let lim = Limites { min_w: 600, min_h: 0, max_w: 0, max_h: 800 };
        assert_eq!(pedido_de_tamanho(&CELULAR, 2.0, &lim), (600, 800));
        // Mínimo maior que o máximo: o mínimo vence.
        let lim = Limites { min_w: 300, min_h: 0, max_w: 200, max_h: 0 };
        assert_eq!(pedido_de_tamanho(&CELULAR, 2.0, &lim).0, 300);
        // Tela minúscula ou degenerada.
        let (w, h) = pedido_de_tamanho(&Tela { largura: 0, altura: 1, dpi: 440 }, 2.0, &Limites::default());
        assert!(w >= MIN_PEDIDO && h >= MIN_PEDIDO);
        // Rotação: paisagem.
        assert_eq!(pedido_de_tamanho(&Tela { largura: 2400, altura: 1080, dpi: 440 }, 2.0, &Limites::default()), (1200, 540));
    }

    #[test]
    fn vista_inicial_cobre_exatamente_a_janela_pedida() {
        let v = Vista::nova(&CELULAR, 2.0);
        assert_eq!(v.regiao(), (0, 0, 540, 1200));
        assert!((v.escala() - 2.0).abs() < 1e-6);
        // Composto menor que a região (limite máximo da janela): fica centrado.
        let mut v = Vista::nova(&CELULAR, 2.0);
        v.ajustar((400, 1200));
        let (x, _, w, _) = v.regiao();
        assert_eq!((x, w), (-70, 540), "centrado: sobra 70 de cada lado");
    }

    #[test]
    fn pinca_mantem_o_ponto_sob_os_dedos_e_respeita_os_limites() {
        let mut v = Vista::nova(&CELULAR, 2.0);
        let c = (540, 1200);
        let foco = (540.0, 1200.0); // centro da tela
        let antes = v.tela_para_composto(foco.0, foco.1);
        v.pincar(foco, 2.0, c);
        let depois = v.tela_para_composto(foco.0, foco.1);
        assert!((antes.0 - depois.0).abs() < 0.01 && (antes.1 - depois.1).abs() < 0.01, "{antes:?} {depois:?}");
        assert!((v.zoom() - 2.0).abs() < 1e-6);
        assert_eq!(v.regiao().2, 270, "a região visível encolheu para a metade");
        // Não passa do máximo nem fica abaixo de 1.
        v.pincar(foco, 100.0, c);
        assert!((v.zoom() - ZOOM_MAX).abs() < 1e-6);
        v.pincar(foco, 0.0001, c);
        assert!((v.zoom() - 1.0).abs() < 1e-6);
        assert_eq!(v.regiao(), (0, 0, 540, 1200));
        // Razão inválida é ignorada.
        v.pincar(foco, f32::NAN, c);
        v.pincar(foco, -1.0, c);
        assert!((v.zoom() - 1.0).abs() < 1e-6);
    }

    #[test]
    fn deslocamento_acompanha_os_dedos_e_nao_sai_do_composto() {
        let mut v = Vista::nova(&CELULAR, 2.0);
        let c = (540, 1200);
        v.deslocar(100.0, 100.0, c);
        assert_eq!(v.regiao(), (0, 0, 540, 1200), "sem ampliação não há para onde deslocar");
        v.pincar((0.0, 0.0), 2.0, c);
        assert_eq!(v.regiao().0, 0);
        v.deslocar(-200.0, -400.0, c); // dedos para a esquerda e para cima: o conteúdo anda para eles
        let (x, y, w, h) = v.regiao();
        assert_eq!((x, y, w, h), (50, 100, 270, 600), "200 px da tela = 50 pixels remotos a 4 px por pixel");
        v.deslocar(-100000.0, -100000.0, c);
        let (x, y, w, h) = v.regiao();
        assert_eq!((x + w as i32, y + h as i32), (540, 1200), "parou na borda");
        v.voltar_ao_ajuste(c);
        assert_eq!(v.regiao(), (0, 0, 540, 1200));
    }

    #[test]
    fn desenho_converte_bgrx_em_rgba_e_pinta_o_fundo_fora_do_composto() {
        let t = Tela { largura: 8, altura: 4, dpi: 440 };
        let mut v = Vista::nova(&t, 1.0); // um pixel da tela por pixel remoto
        let c = composto(4, 2, 200); // B=200, G=100, R=50
        v.ajustar((4, 2));
        let mut out = Vec::new();
        let (w, h) = desenhar_rgba(&c, &v, &mut out);
        assert_eq!((w, h), (8, 4));
        assert_eq!(out.len(), 8 * 4 * 4);
        let px = |x: usize, y: usize| &out[(y * 8 + x) * 4..][..4];
        // Centralizado: sobram 2 colunas e 1 linha de cada lado.
        assert_eq!(px(2, 1), [50, 100, 200, 255], "RGBA a partir de BGRX");
        assert_eq!(px(5, 2), [50, 100, 200, 255]);
        assert_eq!(px(0, 0), [0x30, 0x30, 0x30, 255]);
        assert_eq!(px(1, 1), [0x30, 0x30, 0x30, 255]);
        assert_eq!(px(6, 1), [0x30, 0x30, 0x30, 255]);
        assert_eq!(px(2, 0), [0x30, 0x30, 0x30, 255]);
        assert_eq!(px(2, 3), [0x30, 0x30, 0x30, 255]);
    }

    #[test]
    fn desenho_com_ampliacao_recorta_a_regiao_visivel() {
        let t = Tela { largura: 4, altura: 4, dpi: 440 };
        let mut v = Vista::nova(&t, 1.0);
        // Composto 8x8 com um degradê em R: o pixel (x, y) tem R = y*8 + x.
        let mut px = Vec::new();
        for y in 0..8u8 {
            for x in 0..8u8 {
                px.extend_from_slice(&[0, 0, y * 8 + x, 0xFF]);
            }
        }
        let c = Composto { largura: 8, altura: 8, pixels: px, origem_x: 0, origem_y: 0 };
        v.pincar((0.0, 0.0), 1.0, (8, 8));
        let mut out = Vec::new();
        assert_eq!(desenhar_rgba(&c, &v, &mut out), (4, 4), "sem ampliação a região é do tamanho da tela");
        assert_eq!(out[0], 0);
        assert_eq!(out[(3 * 4 + 3) * 4], 3 * 8 + 3);
        v.deslocar(0.0, 0.0, (8, 8));
        v.pincar((0.0, 0.0), 2.0, (8, 8));
        assert_eq!(desenhar_rgba(&c, &v, &mut out), (2, 2), "ampliada 2x: região de 2x2 pixels remotos");
        v.deslocar(-3.0, -2.0, (8, 8)); // dedos para a esquerda/cima: 3/2 px da tela = 1,5 e 1 pixel remoto
        let (rx, ry, _, _) = v.regiao();
        assert_eq!((rx, ry), (2, 1));
        desenhar_rgba(&c, &v, &mut out);
        assert_eq!(out[0], (ry as u8) * 8 + rx as u8);
    }

    #[test]
    fn acerto_do_toque_prefere_o_popup_mais_alto_e_usa_coordenadas_locais() {
        let popups = [
            PopupVisivel { id: 10, parent: 1, x: 20, y: 30, w: 100, h: 80 },
            PopupVisivel { id: 11, parent: 10, x: 50, y: 40, w: 100, h: 80 }, // sobrepõe o 10 e sai um pouco dele
        ];
        let raiz = (1u32, (300u32, 400u32));
        // Em cima do popup 11 (e também do 10): vence o 11.
        let a = acertar((60.0, 75.0), (0, 0), raiz, &popups).unwrap();
        assert_eq!(a, Alvo { win: 11, x: 10.0, y: 35.0 });
        // Só no 10.
        let a = acertar((25.0, 35.0), (0, 0), raiz, &popups).unwrap();
        assert_eq!((a.win, a.x, a.y), (10, 5.0, 5.0));
        // Fora dos popups, na janela.
        let a = acertar((200.0, 300.0), (0, 0), raiz, &popups).unwrap();
        assert_eq!((a.win, a.x, a.y), (1, 200.0, 300.0));
        // Fora de tudo (o fundo cinza).
        assert!(acertar((350.0, 10.0), (0, 0), raiz, &popups).is_none());
        // Com a origem do composto negativa (popup que sai pela esquerda): o ponto é traduzido.
        let a = acertar((15.0, 15.0), (-10, -10), raiz, &popups).unwrap();
        assert_eq!((a.win, a.x, a.y), (1, 5.0, 5.0));
        assert!(acertar((5.0, 5.0), (-10, -10), raiz, &popups).is_none(), "cai à esquerda da janela e fora dos popups");
    }
}
