//! Das ações do gesto (em pixels da tela) às mensagens do protocolo (em pixels da janela remota): desfaz a escala, a
//! pinça e o deslocamento da [`Vista`] e escolhe a janela alvo (a raiz ou um popup, com coordenadas locais). Lógica pura.
//!
//! Como o `psc` faz: um toque que cai num popup vai ao popup, com coordenadas relativas a ele; enquanto um botão está
//! apertado o ponteiro fica "preso" à janela em que o aperto começou (o arrasto de uma seleção continua nela mesmo que
//! o dedo saia da janela).

use ps_proto::Message;

use crate::gestos::{Botao, Saida};
use crate::janelas::PopupVisivel;
use crate::layout::{acertar, Vista};

/// Botão esquerdo e direito no código do Linux (`linux/input-event-codes.h`), como o `psc` os manda.
pub const BTN_LEFT: u32 = 0x110;
pub const BTN_RIGHT: u32 = 0x111;

/// O que o mapeador precisa saber da cena atual.
pub struct Cena<'a> {
    /// A janela de nível superior em foco e o tamanho dela.
    pub raiz: Option<(u32, (u32, u32))>,
    /// Os popups dela, na ordem de empilhamento, com posição absoluta.
    pub popups: &'a [PopupVisivel],
    /// Posição do composto nas coordenadas da janela (zero ou negativa).
    pub origem: (i32, i32),
    /// Tamanho do composto.
    pub composto: (u32, u32),
}

/// O efeito de uma ação.
#[derive(Debug, Default, PartialEq)]
pub struct Efeito {
    pub mensagens: Vec<Message>,
    /// A vista mudou: redesenhar.
    pub redesenhar: bool,
    pub vibrar: bool,
}

/// Guarda a janela em que o botão foi apertado e a última janela tocada.
#[derive(Debug, Default)]
pub struct Mapeador {
    /// (janela, posição absoluta dela nas coordenadas da janela raiz).
    preso: Option<(u32, (i32, i32))>,
    ultima: Option<u32>,
}

fn posicao_absoluta(win: u32, cena: &Cena) -> (i32, i32) {
    cena.popups.iter().find(|p| p.id == win).map_or((0, 0), |p| (p.x, p.y))
}

impl Mapeador {
    pub fn new() -> Mapeador {
        Mapeador::default()
    }

    /// Esquece a janela presa (a janela em foco mudou, ou a conexão caiu).
    pub fn reiniciar(&mut self) {
        *self = Mapeador::default();
    }

    /// Mensagens para a janela presa, ou para a janela sob `(x, y)` (tela).
    fn alvo(&self, x: f32, y: f32, vista: &Vista, cena: &Cena) -> Option<(u32, f32, f32)> {
        let ponto = vista.tela_para_composto(x, y);
        if let Some((win, (ox, oy))) = self.preso {
            return Some((win, ponto.0 + cena.origem.0 as f32 - ox as f32, ponto.1 + cena.origem.1 as f32 - oy as f32));
        }
        let raiz = cena.raiz?;
        acertar(ponto, cena.origem, raiz, cena.popups).map(|a| (a.win, a.x, a.y))
    }

    pub fn aplicar(&mut self, s: Saida, vista: &mut Vista, cena: &Cena) -> Efeito {
        let mut ef = Efeito::default();
        match s {
            Saida::Mover { x, y } => {
                if let Some((win, lx, ly)) = self.alvo(x, y, vista, cena) {
                    self.ultima = Some(win);
                    ef.mensagens.push(Message::PointerMove { win, x: lx, y: ly });
                }
            }
            Saida::Botao { botao, pressionado, x, y } => {
                let button = match botao {
                    Botao::Esquerdo => BTN_LEFT,
                    Botao::Direito => BTN_RIGHT,
                };
                if pressionado {
                    // O aperto vai para a janela sob o dedo e a prende até soltar.
                    let ponto = vista.tela_para_composto(x, y);
                    let alvo = cena.raiz.and_then(|r| acertar(ponto, cena.origem, r, cena.popups));
                    if let Some(a) = alvo {
                        if self.preso.is_none() {
                            self.preso = Some((a.win, posicao_absoluta(a.win, cena)));
                        }
                        self.ultima = Some(a.win);
                        ef.mensagens.push(Message::PointerButton { win: a.win, button, pressed: true });
                    }
                } else if let Some((win, _)) = self.preso.take() {
                    ef.mensagens.push(Message::PointerButton { win, button, pressed: false });
                }
            }
            Saida::Rolar { dx, dy } => {
                let win = self.ultima.or(cena.raiz.map(|r| r.0));
                let s = vista.escala();
                if let (Some(win), true) = (win, s > 0.0) {
                    // O conteúdo acompanha os dedos; o eixo do protocolo é o oposto (como a roda do `psc`).
                    ef.mensagens.push(Message::PointerAxis { win, dx: -dx / s, dy: -dy / s });
                }
            }
            Saida::Pinca { x, y, razao } => {
                vista.pincar((x, y), razao, cena.composto);
                ef.redesenhar = true;
            }
            Saida::Deslocar { dx, dy } => {
                vista.deslocar(dx, dy, cena.composto);
                ef.redesenhar = true;
            }
            Saida::VoltarAoAjuste => {
                vista.voltar_ao_ajuste(cena.composto);
                ef.redesenhar = true;
            }
            Saida::Vibrar => ef.vibrar = true,
        }
        ef
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gestos::{Acao, Gestos, Ponteiro, Toque};
    use crate::layout::Tela;

    const TELA: Tela = Tela { largura: 1080, altura: 2400, dpi: 440 };

    fn cena<'a>(popups: &'a [PopupVisivel]) -> Cena<'a> {
        Cena { raiz: Some((1, (540, 1200))), popups, origem: (0, 0), composto: (540, 1200) }
    }

    fn msgs(v: Vec<Saida>, m: &mut Mapeador, vista: &mut Vista, c: &Cena) -> Vec<Message> {
        v.into_iter().flat_map(|s| m.aplicar(s, vista, c).mensagens).collect()
    }

    #[test]
    fn mover_desfaz_a_escala_da_tela_para_a_janela() {
        let mut v = Vista::nova(&TELA, 2.0);
        let c = cena(&[]);
        let ef = Mapeador::new().aplicar(Saida::Mover { x: 540.0, y: 1000.0 }, &mut v, &c);
        assert_eq!(ef.mensagens, vec![Message::PointerMove { win: 1, x: 270.0, y: 500.0 }]);
    }

    #[test]
    fn com_pinca_e_deslocamento_o_ponto_continua_certo() {
        let mut v = Vista::nova(&TELA, 2.0);
        let c = cena(&[]);
        let mut m = Mapeador::new();
        // Amplia 2x em volta do canto superior esquerdo, depois desloca: a janela anda 100 px remotos.
        assert!(m.aplicar(Saida::Pinca { x: 0.0, y: 0.0, razao: 2.0 }, &mut v, &c).redesenhar);
        m.aplicar(Saida::Deslocar { dx: -400.0, dy: -800.0 }, &mut v, &c); // 4 px da tela por pixel remoto
        let ef = m.aplicar(Saida::Mover { x: 40.0, y: 80.0 }, &mut v, &c);
        assert_eq!(ef.mensagens, vec![Message::PointerMove { win: 1, x: 110.0, y: 220.0 }]);
        assert!(m.aplicar(Saida::VoltarAoAjuste, &mut v, &c).redesenhar);
        let ef = m.aplicar(Saida::Mover { x: 40.0, y: 80.0 }, &mut v, &c);
        assert_eq!(ef.mensagens, vec![Message::PointerMove { win: 1, x: 20.0, y: 40.0 }]);
    }

    #[test]
    fn toque_em_popup_vai_ao_popup_com_coordenadas_locais() {
        let popups = [PopupVisivel { id: 10, parent: 1, x: 100, y: 50, w: 120, h: 200 }];
        let mut v = Vista::nova(&TELA, 2.0);
        let c = cena(&popups);
        let mut m = Mapeador::new();
        // Tela (300, 200) = remoto (150, 100) = dentro do popup, local (50, 50).
        let ef = m.aplicar(Saida::Mover { x: 300.0, y: 200.0 }, &mut v, &c);
        assert_eq!(ef.mensagens, vec![Message::PointerMove { win: 10, x: 50.0, y: 50.0 }]);
        // Fora do popup: na janela.
        let ef = m.aplicar(Saida::Mover { x: 800.0, y: 1000.0 }, &mut v, &c);
        assert_eq!(ef.mensagens, vec![Message::PointerMove { win: 1, x: 400.0, y: 500.0 }]);
    }

    #[test]
    fn botao_prende_o_ponteiro_na_janela_do_aperto_ate_soltar() {
        let popups = [PopupVisivel { id: 10, parent: 1, x: 100, y: 50, w: 120, h: 200 }];
        let mut v = Vista::nova(&TELA, 2.0);
        let c = cena(&popups);
        let mut m = Mapeador::new();
        let ms = msgs(
            vec![
                Saida::Mover { x: 300.0, y: 200.0 },
                Saida::Botao { botao: Botao::Esquerdo, pressionado: true, x: 300.0, y: 200.0 },
                // O dedo sai do popup, mas o arrasto continua nele.
                Saida::Mover { x: 800.0, y: 1000.0 },
                Saida::Botao { botao: Botao::Esquerdo, pressionado: false, x: 800.0, y: 1000.0 },
                Saida::Mover { x: 800.0, y: 1000.0 },
            ],
            &mut m,
            &mut v,
            &c,
        );
        assert_eq!(
            ms,
            vec![
                Message::PointerMove { win: 10, x: 50.0, y: 50.0 },
                Message::PointerButton { win: 10, button: BTN_LEFT, pressed: true },
                Message::PointerMove { win: 10, x: 300.0, y: 450.0 },
                Message::PointerButton { win: 10, button: BTN_LEFT, pressed: false },
                Message::PointerMove { win: 1, x: 400.0, y: 500.0 },
            ]
        );
    }

    #[test]
    fn fora_de_qualquer_janela_nada_e_enviado_e_soltar_sem_apertar_tambem_nao() {
        let mut v = Vista::nova(&TELA, 2.0);
        let c = Cena { raiz: Some((1, (300, 300))), popups: &[], origem: (0, 0), composto: (300, 300) };
        let mut m = Mapeador::new();
        assert!(m.aplicar(Saida::Mover { x: 900.0, y: 900.0 }, &mut v, &c).mensagens.is_empty());
        assert!(m.aplicar(Saida::Botao { botao: Botao::Esquerdo, pressionado: true, x: 900.0, y: 900.0 }, &mut v, &c).mensagens.is_empty());
        assert!(m.aplicar(Saida::Botao { botao: Botao::Esquerdo, pressionado: false, x: 900.0, y: 900.0 }, &mut v, &c).mensagens.is_empty());
        // Sem janela em foco, nada.
        let sem = Cena { raiz: None, popups: &[], origem: (0, 0), composto: (0, 0) };
        assert!(m.aplicar(Saida::Mover { x: 1.0, y: 1.0 }, &mut v, &sem).mensagens.is_empty());
        assert!(m.aplicar(Saida::Rolar { dx: 0.0, dy: -10.0 }, &mut v, &sem).mensagens.is_empty());
    }

    #[test]
    fn rolagem_tem_o_sinal_do_psc_e_a_escala_da_vista() {
        let mut v = Vista::nova(&TELA, 2.0);
        let c = cena(&[]);
        let mut m = Mapeador::new();
        // Dedos para cima (dy negativo na tela) = conteúdo sobe = eixo positivo no protocolo; 2 px da tela por pixel remoto.
        let ef = m.aplicar(Saida::Rolar { dx: 20.0, dy: -30.0 }, &mut v, &c);
        assert_eq!(ef.mensagens, vec![Message::PointerAxis { win: 1, dx: -10.0, dy: 15.0 }]);
        // A rolagem vai à última janela tocada.
        let popups = [PopupVisivel { id: 10, parent: 1, x: 0, y: 0, w: 400, h: 400 }];
        let c = cena(&popups);
        m.aplicar(Saida::Mover { x: 10.0, y: 10.0 }, &mut v, &c);
        let ef = m.aplicar(Saida::Rolar { dx: 0.0, dy: -20.0 }, &mut v, &c);
        assert_eq!(ef.mensagens, vec![Message::PointerAxis { win: 10, dx: 0.0, dy: 10.0 }]);
    }

    #[test]
    fn vibrar_vira_efeito_e_origem_negativa_e_traduzida() {
        let mut v = Vista::nova(&TELA, 2.0);
        let mut m = Mapeador::new();
        assert!(m.aplicar(Saida::Vibrar, &mut v, &cena(&[])).vibrar);
        // Popup que sai pela esquerda: o composto começa em x = -50.
        let popups = [PopupVisivel { id: 10, parent: 1, x: -50, y: 0, w: 100, h: 100 }];
        let c = Cena { raiz: Some((1, (540, 1200))), popups: &popups, origem: (-50, 0), composto: (590, 1200) };
        let ef = m.aplicar(Saida::Mover { x: 20.0, y: 20.0 }, &mut v, &c);
        assert_eq!(ef.mensagens, vec![Message::PointerMove { win: 10, x: 10.0 + 0.0 + 0.0, y: 10.0 }], "(10,10) no composto = (-40,10) na janela = (10,10) no popup");
    }

    /// De ponta a ponta: sequências de toque do `MotionEvent` até as mensagens do protocolo.
    #[test]
    fn toque_curto_na_tela_vira_clique_na_janela() {
        let mut g = Gestos::new(TELA.dpi);
        let mut v = Vista::nova(&TELA, 2.0);
        let c = cena(&[]);
        let mut m = Mapeador::new();
        let p = |x, y| vec![Ponteiro { id: 0, x, y }];
        let mut saidas = g.evento(&Toque { acao: Acao::Baixo, indice: 0, ponteiros: p(200.0, 400.0), t_ms: 0 });
        saidas.extend(g.evento(&Toque { acao: Acao::Cima, indice: 0, ponteiros: p(200.0, 400.0), t_ms: 80 }));
        assert_eq!(
            msgs(saidas, &mut m, &mut v, &c),
            vec![
                Message::PointerMove { win: 1, x: 100.0, y: 200.0 },
                Message::PointerButton { win: 1, button: BTN_LEFT, pressed: true },
                Message::PointerButton { win: 1, button: BTN_LEFT, pressed: false },
            ]
        );
    }

    #[test]
    fn toque_longo_na_tela_vira_clique_direito_e_vibra() {
        let mut g = Gestos::new(TELA.dpi);
        let mut v = Vista::nova(&TELA, 2.0);
        let c = cena(&[]);
        let mut m = Mapeador::new();
        g.evento(&Toque { acao: Acao::Baixo, indice: 0, ponteiros: vec![Ponteiro { id: 0, x: 200.0, y: 400.0 }], t_ms: 0 });
        let mut vibrou = false;
        let mut ms = Vec::new();
        for s in g.tick(600) {
            let ef = m.aplicar(s, &mut v, &c);
            vibrou |= ef.vibrar;
            ms.extend(ef.mensagens);
        }
        assert!(vibrou);
        assert_eq!(ms.len(), 3);
        assert_eq!(ms[1], Message::PointerButton { win: 1, button: BTN_RIGHT, pressed: true });
    }
}
