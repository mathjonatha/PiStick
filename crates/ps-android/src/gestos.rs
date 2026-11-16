//! Reconhecedor de gestos de toque (docs/26, decisão 10). Lógica pura: recebe o `MotionEvent` cru (ação, dedos,
//! posições, tempo) e devolve ações em **coordenadas da tela**; quem as traduz em mensagens do protocolo, já em
//! coordenadas da janela remota, é `entrada.rs`.
//!
//! Modo direto (o dedo é o ponteiro, na posição tocada):
//! - toque curto: move e clica com o botão esquerdo;
//! - toque longo (500 ms sem sair do lugar): clique com o botão direito e vibração curta. O relógio anda por
//!   [`Gestos::tick`] (a casca chama o `poll` a cada quadro) e também ao levantar o dedo;
//! - arrastar com um dedo: depois de [`LIMIAR_DP`] pontos independentes de densidade, move com o botão esquerdo
//!   apertado (seleção, arrastar dentro do app); antes do limiar ainda conta como toque;
//! - arrastar com dois dedos: rolagem nos dois eixos;
//! - pinça (a distância entre os dois dedos muda): ampliação local, com deslocamento pelo centro dos dedos;
//! - toque duplo com dois dedos (sem movimento): volta ao ajuste;
//! - um dedo que entra e sai no meio, ou um terceiro dedo, não vira clique: o que sobra é ignorado até todos saírem.

/// Toque longo: tempo parado até virar clique direito.
pub const LONGO_MS: u64 = 500;
/// Um toque de dois dedos é curto se dura no máximo isto.
pub const DOIS_DEDOS_MS: u64 = 300;
/// Dois toques de dois dedos a no máximo isto um do outro voltam ao ajuste.
pub const DUPLO_MS: u64 = 400;
/// Limiar de movimento, em pontos independentes de densidade (1 ponto = 1/160 de polegada).
pub const LIMIAR_DP: f32 = 8.0;
/// Máximo de dedos aceitos num evento.
const MAX_DEDOS: usize = 10;

/// A ação do `MotionEvent` (`getActionMasked`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Acao {
    Baixo,
    Cima,
    Mover,
    Cancelar,
    PonteiroBaixo,
    PonteiroCima,
}

impl Acao {
    /// De `MotionEvent.ACTION_*`: DOWN 0, UP 1, MOVE 2, CANCEL 3, POINTER_DOWN 5, POINTER_UP 6. Outras são ignoradas.
    pub fn de_android(mascarada: i32) -> Option<Acao> {
        Some(match mascarada {
            0 => Acao::Baixo,
            1 => Acao::Cima,
            2 => Acao::Mover,
            3 => Acao::Cancelar,
            5 => Acao::PonteiroBaixo,
            6 => Acao::PonteiroCima,
            _ => return None,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Ponteiro {
    pub id: i32,
    pub x: f32,
    pub y: f32,
}

/// Um `MotionEvent` já separado. Em `PonteiroCima` e `Cima` a lista ainda inclui o dedo que sai.
#[derive(Debug, Clone, PartialEq)]
pub struct Toque {
    pub acao: Acao,
    /// Índice do dedo da ação (`getActionIndex`).
    pub indice: usize,
    pub ponteiros: Vec<Ponteiro>,
    pub t_ms: u64,
}

/// Monta um [`Toque`] do que a casca manda: a ação mascarada, o índice, `dados = [id, x, y]` por dedo e o tempo.
/// `None` se a entrada não serve (ação desconhecida, vetor com tamanho errado, número fora do limite, índice fora da
/// lista, valor que não é número, lista vazia).
pub fn decodificar_toque(acao_mascarada: i32, indice: i32, dados: &[f32], t_ms: i64) -> Option<Toque> {
    let acao = Acao::de_android(acao_mascarada)?;
    if dados.is_empty() || !dados.len().is_multiple_of(3) || dados.len() / 3 > MAX_DEDOS || indice < 0 || t_ms < 0 {
        return None;
    }
    let ponteiros: Vec<Ponteiro> = dados.as_chunks::<3>().0.iter().map(|d| Ponteiro { id: d[0] as i32, x: d[1], y: d[2] }).collect();
    if ponteiros.iter().any(|p| !p.x.is_finite() || !p.y.is_finite()) || indice as usize >= ponteiros.len() {
        return None;
    }
    Some(Toque { acao, indice: indice as usize, ponteiros, t_ms: t_ms as u64 })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Botao {
    Esquerdo,
    Direito,
}

/// O que o gesto pede, em pixels da tela.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Saida {
    Mover { x: f32, y: f32 },
    Botao { botao: Botao, pressionado: bool, x: f32, y: f32 },
    /// Rolagem: deslocamento dos dedos desde a última ação (o conteúdo acompanha os dedos).
    Rolar { dx: f32, dy: f32 },
    /// Pinça: `razao` é a distância atual entre os dedos dividida pela da ação anterior; `x, y` é o centro.
    Pinca { x: f32, y: f32, razao: f32 },
    /// Deslocamento do centro dos dois dedos durante a pinça.
    Deslocar { dx: f32, dy: f32 },
    /// Toque duplo com dois dedos.
    VoltarAoAjuste,
    /// Vibração curta (o toque longo virou clique direito).
    Vibrar,
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Modo {
    Indefinido,
    Rolagem,
    Pinca,
}

#[derive(Debug, Clone, PartialEq)]
enum Estado {
    Ocioso,
    /// Um dedo apoiado, ainda sem decidir entre toque, toque longo e arrasto.
    Pendente { id: i32, x0: f32, y0: f32, t0: u64 },
    Arrastando { id: i32, x: f32, y: f32 },
    /// O gesto acabou de valer (toque longo feito, dois dedos terminados): ignora o resto até todos saírem.
    Consumido,
    DoisDedos { a: i32, b: i32, c0: (f32, f32), d0: f32, ult_c: (f32, f32), ult_d: f32, modo: Modo, t0: u64 },
}

#[derive(Debug)]
pub struct Gestos {
    limiar: f32,
    estado: Estado,
    ultimo_toque_de_dois: Option<u64>,
}

fn dist(a: (f32, f32), b: (f32, f32)) -> f32 {
    ((a.0 - b.0).powi(2) + (a.1 - b.1).powi(2)).sqrt()
}

impl Gestos {
    /// `dpi`: densidade da tela (o limiar é [`LIMIAR_DP`] pontos, ou seja, `8 * dpi / 160` pixels).
    pub fn new(dpi: u32) -> Gestos {
        let dpi = if dpi == 0 { 160.0 } else { dpi as f32 };
        Gestos { limiar: LIMIAR_DP * dpi / 160.0, estado: Estado::Ocioso, ultimo_toque_de_dois: None }
    }

    pub fn limiar_px(&self) -> f32 {
        self.limiar
    }

    fn achar(t: &Toque, id: i32) -> Option<Ponteiro> {
        t.ponteiros.iter().find(|p| p.id == id).copied()
    }

    fn toque_longo(x: f32, y: f32) -> Vec<Saida> {
        vec![
            Saida::Mover { x, y },
            Saida::Botao { botao: Botao::Direito, pressionado: true, x, y },
            Saida::Botao { botao: Botao::Direito, pressionado: false, x, y },
            Saida::Vibrar,
        ]
    }

    /// Entra um evento de toque; devolve o que o gesto pede.
    pub fn evento(&mut self, t: &Toque) -> Vec<Saida> {
        let mut out = Vec::new();
        match t.acao {
            Acao::Baixo => {
                // Um DOWN novo recomeça tudo; se um arrasto ficou sem UP, solta o botão antes.
                if let Estado::Arrastando { x, y, .. } = self.estado {
                    out.push(Saida::Botao { botao: Botao::Esquerdo, pressionado: false, x, y });
                }
                self.estado = match t.ponteiros.first() {
                    Some(p) => Estado::Pendente { id: p.id, x0: p.x, y0: p.y, t0: t.t_ms },
                    None => Estado::Ocioso,
                };
            }
            Acao::PonteiroBaixo => self.segundo_dedo(t, &mut out),
            Acao::Mover => self.mover(t, &mut out),
            Acao::PonteiroCima => self.dedo_saiu(t, &mut out),
            Acao::Cima => self.levantou(t, &mut out),
            Acao::Cancelar => {
                if let Estado::Arrastando { x, y, .. } = self.estado {
                    out.push(Saida::Botao { botao: Botao::Esquerdo, pressionado: false, x, y });
                }
                self.estado = Estado::Ocioso;
            }
        }
        out
    }

    /// O relógio: vale o toque longo de um dedo parado há [`LONGO_MS`].
    pub fn tick(&mut self, agora_ms: u64) -> Vec<Saida> {
        if let Estado::Pendente { x0, y0, t0, .. } = self.estado {
            if agora_ms.saturating_sub(t0) >= LONGO_MS {
                self.estado = Estado::Consumido;
                return Self::toque_longo(x0, y0);
            }
        }
        Vec::new()
    }

    fn segundo_dedo(&mut self, t: &Toque, out: &mut Vec<Saida>) {
        let novo = t.ponteiros.get(t.indice).copied();
        let (primeiro, x_solta) = match self.estado {
            Estado::Pendente { id, .. } => (Some(id), None),
            Estado::Arrastando { id, x, y } => (Some(id), Some((x, y))),
            Estado::DoisDedos { .. } => return, // um terceiro dedo não muda nada
            _ => (None, None),
        };
        if let Some((x, y)) = x_solta {
            out.push(Saida::Botao { botao: Botao::Esquerdo, pressionado: false, x, y });
        }
        let par = primeiro.and_then(|id| Self::achar(t, id)).zip(novo);
        self.estado = match par {
            Some((a, b)) => {
                let c = ((a.x + b.x) / 2.0, (a.y + b.y) / 2.0);
                let d = dist((a.x, a.y), (b.x, b.y));
                Estado::DoisDedos { a: a.id, b: b.id, c0: c, d0: d, ult_c: c, ult_d: d, modo: Modo::Indefinido, t0: t.t_ms }
            }
            None => Estado::Consumido,
        };
    }

    fn mover(&mut self, t: &Toque, out: &mut Vec<Saida>) {
        match self.estado.clone() {
            Estado::Pendente { id, x0, y0, .. } => {
                if let Some(p) = Self::achar(t, id) {
                    if dist((x0, y0), (p.x, p.y)) > self.limiar {
                        out.push(Saida::Mover { x: x0, y: y0 });
                        out.push(Saida::Botao { botao: Botao::Esquerdo, pressionado: true, x: x0, y: y0 });
                        out.push(Saida::Mover { x: p.x, y: p.y });
                        self.estado = Estado::Arrastando { id, x: p.x, y: p.y };
                    }
                }
            }
            Estado::Arrastando { id, .. } => {
                if let Some(p) = Self::achar(t, id) {
                    out.push(Saida::Mover { x: p.x, y: p.y });
                    self.estado = Estado::Arrastando { id, x: p.x, y: p.y };
                }
            }
            Estado::DoisDedos { a, b, c0, d0, ult_c, ult_d, modo, t0 } => {
                let (Some(pa), Some(pb)) = (Self::achar(t, a), Self::achar(t, b)) else { return };
                let c = ((pa.x + pb.x) / 2.0, (pa.y + pb.y) / 2.0);
                let d = dist((pa.x, pa.y), (pb.x, pb.y));
                let modo = match modo {
                    Modo::Indefinido if (d - d0).abs() > self.limiar => Modo::Pinca,
                    Modo::Indefinido if dist(c, c0) > self.limiar => Modo::Rolagem,
                    m => m,
                };
                let (dx, dy) = (c.0 - ult_c.0, c.1 - ult_c.1);
                match modo {
                    Modo::Rolagem => out.push(Saida::Rolar { dx, dy }),
                    Modo::Pinca => {
                        let razao = if ult_d > 1.0 { d / ult_d } else { 1.0 };
                        out.push(Saida::Pinca { x: c.0, y: c.1, razao });
                        out.push(Saida::Deslocar { dx, dy });
                    }
                    Modo::Indefinido => {}
                }
                // Enquanto não decidiu, os pontos de referência ficam onde estavam (o primeiro movimento decisivo
                // leva o deslocamento acumulado).
                let (ult_c, ult_d) = if modo == Modo::Indefinido { (ult_c, ult_d) } else { (c, d) };
                self.estado = Estado::DoisDedos { a, b, c0, d0, ult_c, ult_d, modo, t0 };
            }
            Estado::Ocioso | Estado::Consumido => {}
        }
    }

    fn dedo_saiu(&mut self, t: &Toque, out: &mut Vec<Saida>) {
        let saiu = t.ponteiros.get(t.indice).map(|p| p.id);
        if let Estado::DoisDedos { a, b, modo, t0, .. } = self.estado {
            if saiu == Some(a) || saiu == Some(b) {
                // Dois dedos rápidos e sem movimento: um toque de dois dedos; dois seguidos voltam ao ajuste.
                if modo == Modo::Indefinido && t.t_ms.saturating_sub(t0) <= DOIS_DEDOS_MS {
                    match self.ultimo_toque_de_dois {
                        Some(antes) if t.t_ms.saturating_sub(antes) <= DUPLO_MS => {
                            self.ultimo_toque_de_dois = None;
                            out.push(Saida::VoltarAoAjuste);
                        }
                        _ => self.ultimo_toque_de_dois = Some(t.t_ms),
                    }
                }
                self.estado = Estado::Consumido;
            }
        }
    }

    fn levantou(&mut self, t: &Toque, out: &mut Vec<Saida>) {
        let estado = std::mem::replace(&mut self.estado, Estado::Ocioso);
        match estado {
            Estado::Pendente { id, x0, y0, t0 } => {
                if t.t_ms.saturating_sub(t0) >= LONGO_MS {
                    out.extend(Self::toque_longo(x0, y0));
                } else {
                    let (x, y) = Self::achar(t, id).map_or((x0, y0), |p| (p.x, p.y));
                    out.push(Saida::Mover { x, y });
                    out.push(Saida::Botao { botao: Botao::Esquerdo, pressionado: true, x, y });
                    out.push(Saida::Botao { botao: Botao::Esquerdo, pressionado: false, x, y });
                }
            }
            Estado::Arrastando { id, x, y } => {
                let (x, y) = Self::achar(t, id).map_or((x, y), |p| (p.x, p.y));
                out.push(Saida::Mover { x, y });
                out.push(Saida::Botao { botao: Botao::Esquerdo, pressionado: false, x, y });
            }
            Estado::DoisDedos { .. } => {
                // Os dois dedos saíram ao mesmo tempo (só chegou o UP final): vale como PONTEIRO_CIMA do primeiro.
                let mut t2 = t.clone();
                t2.acao = Acao::PonteiroCima;
                self.estado = estado;
                self.dedo_saiu(&t2, out);
                self.estado = Estado::Ocioso;
            }
            Estado::Ocioso | Estado::Consumido => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 440 dpi: limiar de 22 pixels.
    fn g() -> Gestos {
        Gestos::new(440)
    }

    fn p(id: i32, x: f32, y: f32) -> Ponteiro {
        Ponteiro { id, x, y }
    }

    fn t(acao: Acao, indice: usize, ponteiros: &[Ponteiro], t_ms: u64) -> Toque {
        Toque { acao, indice, ponteiros: ponteiros.to_vec(), t_ms }
    }

    fn clique(botao: Botao, x: f32, y: f32) -> Vec<Saida> {
        vec![Saida::Mover { x, y }, Saida::Botao { botao, pressionado: true, x, y }, Saida::Botao { botao, pressionado: false, x, y }]
    }

    #[test]
    fn limiar_acompanha_a_densidade() {
        assert!((Gestos::new(440).limiar_px() - 22.0).abs() < 1e-4);
        assert!((Gestos::new(160).limiar_px() - 8.0).abs() < 1e-4);
        assert!((Gestos::new(0).limiar_px() - 8.0).abs() < 1e-4);
    }

    #[test]
    fn toque_curto_e_clique_esquerdo() {
        let mut g = g();
        assert!(g.evento(&t(Acao::Baixo, 0, &[p(0, 100.0, 200.0)], 0)).is_empty());
        assert!(g.tick(100).is_empty());
        let out = g.evento(&t(Acao::Cima, 0, &[p(0, 101.0, 201.0)], 120));
        assert_eq!(out, clique(Botao::Esquerdo, 101.0, 201.0));
        // O estado voltou ao repouso: um UP perdido não faz nada.
        assert!(g.evento(&t(Acao::Cima, 0, &[p(0, 0.0, 0.0)], 130)).is_empty());
    }

    #[test]
    fn toque_longo_pelo_relogio_vira_clique_direito_com_vibracao_uma_so_vez() {
        let mut g = g();
        g.evento(&t(Acao::Baixo, 0, &[p(0, 50.0, 60.0)], 1000));
        assert!(g.tick(1499).is_empty());
        let out = g.tick(1500);
        assert_eq!(out[..3], clique(Botao::Direito, 50.0, 60.0)[..]);
        assert_eq!(out[3], Saida::Vibrar);
        // Não repete, e o resto do toque é ignorado.
        assert!(g.tick(2500).is_empty());
        assert!(g.evento(&t(Acao::Mover, 0, &[p(0, 300.0, 300.0)], 2600)).is_empty());
        assert!(g.evento(&t(Acao::Cima, 0, &[p(0, 300.0, 300.0)], 2700)).is_empty());
    }

    #[test]
    fn toque_longo_sem_relogio_vale_ao_levantar_o_dedo() {
        let mut g = g();
        g.evento(&t(Acao::Baixo, 0, &[p(0, 50.0, 60.0)], 0));
        let out = g.evento(&t(Acao::Cima, 0, &[p(0, 50.0, 60.0)], 700));
        assert_eq!(out.last(), Some(&Saida::Vibrar));
        assert_eq!(out[1], Saida::Botao { botao: Botao::Direito, pressionado: true, x: 50.0, y: 60.0 });
    }

    #[test]
    fn movimento_abaixo_do_limiar_ainda_e_toque_e_nao_cancela_o_toque_longo() {
        let mut g = g();
        g.evento(&t(Acao::Baixo, 0, &[p(0, 100.0, 100.0)], 0));
        assert!(g.evento(&t(Acao::Mover, 0, &[p(0, 110.0, 110.0)], 50)).is_empty(), "14 px < 22 px");
        assert!(!g.tick(600).is_empty(), "continua valendo o toque longo");
        let mut g2 = Gestos::new(440);
        g2.evento(&t(Acao::Baixo, 0, &[p(0, 100.0, 100.0)], 0));
        g2.evento(&t(Acao::Mover, 0, &[p(0, 112.0, 100.0)], 50));
        let out = g2.evento(&t(Acao::Cima, 0, &[p(0, 112.0, 100.0)], 100));
        assert_eq!(out, clique(Botao::Esquerdo, 112.0, 100.0), "levantou antes do limiar: é toque");
    }

    #[test]
    fn arrasto_acima_do_limiar_aperta_o_botao_no_ponto_de_partida_e_solta_no_fim() {
        let mut g = g();
        g.evento(&t(Acao::Baixo, 0, &[p(0, 100.0, 100.0)], 0));
        let out = g.evento(&t(Acao::Mover, 0, &[p(0, 130.0, 100.0)], 40));
        assert_eq!(
            out,
            vec![
                Saida::Mover { x: 100.0, y: 100.0 },
                Saida::Botao { botao: Botao::Esquerdo, pressionado: true, x: 100.0, y: 100.0 },
                Saida::Mover { x: 130.0, y: 100.0 }
            ]
        );
        assert_eq!(g.evento(&t(Acao::Mover, 0, &[p(0, 200.0, 150.0)], 60)), vec![Saida::Mover { x: 200.0, y: 150.0 }]);
        // O relógio não vira toque longo no meio do arrasto.
        assert!(g.tick(5000).is_empty());
        let out = g.evento(&t(Acao::Cima, 0, &[p(0, 210.0, 160.0)], 80));
        assert_eq!(out, vec![Saida::Mover { x: 210.0, y: 160.0 }, Saida::Botao { botao: Botao::Esquerdo, pressionado: false, x: 210.0, y: 160.0 }]);
    }

    #[test]
    fn dois_dedos_paralelos_rolam_nos_dois_eixos() {
        let mut g = g();
        g.evento(&t(Acao::Baixo, 0, &[p(0, 100.0, 500.0)], 0));
        assert!(g.evento(&t(Acao::PonteiroBaixo, 1, &[p(0, 100.0, 500.0), p(1, 300.0, 500.0)], 20)).is_empty());
        // Primeiro movimento pequeno: ainda indefinido.
        assert!(g.evento(&t(Acao::Mover, 0, &[p(0, 100.0, 490.0), p(1, 300.0, 490.0)], 40)).is_empty());
        // Passou do limiar: rola com o deslocamento acumulado desde o começo (-30 em y) e 0 em x.
        let out = g.evento(&t(Acao::Mover, 0, &[p(0, 100.0, 470.0), p(1, 300.0, 470.0)], 60));
        assert_eq!(out, vec![Saida::Rolar { dx: 0.0, dy: -30.0 }]);
        let out = g.evento(&t(Acao::Mover, 0, &[p(0, 110.0, 450.0), p(1, 310.0, 450.0)], 80));
        assert_eq!(out, vec![Saida::Rolar { dx: 10.0, dy: -20.0 }]);
        // Soltar os dois não clica nada.
        assert!(g.evento(&t(Acao::PonteiroCima, 1, &[p(0, 110.0, 450.0), p(1, 310.0, 450.0)], 100)).is_empty());
        assert!(g.evento(&t(Acao::Cima, 0, &[p(0, 110.0, 450.0)], 110)).is_empty());
    }

    #[test]
    fn pinca_afastando_os_dedos_amplia_e_desloca_pelo_centro() {
        let mut g = g();
        g.evento(&t(Acao::Baixo, 0, &[p(0, 400.0, 500.0)], 0));
        g.evento(&t(Acao::PonteiroBaixo, 1, &[p(0, 400.0, 500.0), p(1, 600.0, 500.0)], 10));
        // Distância 200 -> 300: passou do limiar.
        let out = g.evento(&t(Acao::Mover, 0, &[p(0, 350.0, 500.0), p(1, 650.0, 500.0)], 30));
        assert_eq!(out.len(), 2);
        let Saida::Pinca { x, y, razao } = out[0] else { panic!("{out:?}") };
        assert_eq!((x, y), (500.0, 500.0));
        assert!((razao - 1.5).abs() < 1e-5, "{razao}");
        assert_eq!(out[1], Saida::Deslocar { dx: 0.0, dy: 0.0 });
        // Aproximando: razão relativa ao evento anterior (300 -> 150).
        let out = g.evento(&t(Acao::Mover, 0, &[p(0, 425.0, 520.0), p(1, 575.0, 520.0)], 50));
        let Saida::Pinca { razao, .. } = out[0] else { panic!() };
        assert!((razao - 0.5).abs() < 1e-5, "{razao}");
        assert_eq!(out[1], Saida::Deslocar { dx: 0.0, dy: 20.0 });
    }

    #[test]
    fn dedo_que_entra_e_sai_no_meio_nao_vira_clique_nem_arrasto() {
        let mut g = g();
        g.evento(&t(Acao::Baixo, 0, &[p(0, 100.0, 100.0)], 0));
        assert!(g.evento(&t(Acao::PonteiroBaixo, 1, &[p(0, 100.0, 100.0), p(1, 200.0, 100.0)], 50)).is_empty());
        assert!(g.evento(&t(Acao::PonteiroCima, 1, &[p(0, 100.0, 100.0), p(1, 200.0, 100.0)], 120)).is_empty());
        // O primeiro dedo segue apoiado: nada de arrasto nem de clique ao mexer ou levantar.
        assert!(g.evento(&t(Acao::Mover, 0, &[p(0, 300.0, 300.0)], 150)).is_empty());
        assert!(g.tick(5000).is_empty());
        assert!(g.evento(&t(Acao::Cima, 0, &[p(0, 300.0, 300.0)], 200)).is_empty());
        // E o gesto seguinte funciona normalmente.
        g.evento(&t(Acao::Baixo, 0, &[p(0, 10.0, 10.0)], 1000));
        assert_eq!(g.evento(&t(Acao::Cima, 0, &[p(0, 10.0, 10.0)], 1050)), clique(Botao::Esquerdo, 10.0, 10.0));
    }

    #[test]
    fn segundo_dedo_durante_o_arrasto_solta_o_botao() {
        let mut g = g();
        g.evento(&t(Acao::Baixo, 0, &[p(0, 100.0, 100.0)], 0));
        g.evento(&t(Acao::Mover, 0, &[p(0, 150.0, 100.0)], 20));
        let out = g.evento(&t(Acao::PonteiroBaixo, 1, &[p(0, 150.0, 100.0), p(1, 300.0, 300.0)], 40));
        assert_eq!(out, vec![Saida::Botao { botao: Botao::Esquerdo, pressionado: false, x: 150.0, y: 100.0 }]);
        assert!(g.evento(&t(Acao::Cima, 0, &[p(0, 150.0, 100.0)], 80)).is_empty());
    }

    #[test]
    fn terceiro_dedo_e_ignorado() {
        let mut g = g();
        g.evento(&t(Acao::Baixo, 0, &[p(0, 100.0, 500.0)], 0));
        g.evento(&t(Acao::PonteiroBaixo, 1, &[p(0, 100.0, 500.0), p(1, 300.0, 500.0)], 10));
        assert!(g.evento(&t(Acao::PonteiroBaixo, 2, &[p(0, 100.0, 500.0), p(1, 300.0, 500.0), p(2, 500.0, 500.0)], 20)).is_empty());
        let out = g.evento(&t(Acao::Mover, 0, &[p(0, 100.0, 450.0), p(1, 300.0, 450.0), p(2, 900.0, 0.0)], 30));
        assert_eq!(out, vec![Saida::Rolar { dx: 0.0, dy: -50.0 }], "só os dois primeiros dedos contam");
        // O terceiro sair não encerra o gesto.
        assert!(g.evento(&t(Acao::PonteiroCima, 2, &[p(0, 100.0, 450.0), p(1, 300.0, 450.0), p(2, 900.0, 0.0)], 40)).is_empty());
        let out = g.evento(&t(Acao::Mover, 0, &[p(0, 100.0, 440.0), p(1, 300.0, 440.0)], 50));
        assert_eq!(out, vec![Saida::Rolar { dx: 0.0, dy: -10.0 }]);
    }

    #[test]
    fn toque_duplo_de_dois_dedos_emite_voltar_ao_ajuste_no_segundo_levantar() {
        let mut g = g();
        let mut vistos = Vec::new();
        for (i, base) in [0u64, 200].into_iter().enumerate() {
            g.evento(&t(Acao::Baixo, 0, &[p(0, 100.0, 500.0)], base));
            g.evento(&t(Acao::PonteiroBaixo, 1, &[p(0, 100.0, 500.0), p(1, 300.0, 500.0)], base + 5));
            let out = g.evento(&t(Acao::PonteiroCima, 1, &[p(0, 100.0, 500.0), p(1, 300.0, 500.0)], base + 80));
            vistos.push((i, out));
            g.evento(&t(Acao::Cima, 0, &[p(0, 100.0, 500.0)], base + 85));
        }
        assert!(vistos[0].1.is_empty());
        assert_eq!(vistos[1].1, vec![Saida::VoltarAoAjuste]);
        // Dois toques longe um do outro (mais de 400 ms) não valem.
        let mut g = Gestos::new(440);
        for base in [0u64, 900] {
            g.evento(&t(Acao::Baixo, 0, &[p(0, 100.0, 500.0)], base));
            g.evento(&t(Acao::PonteiroBaixo, 1, &[p(0, 100.0, 500.0), p(1, 300.0, 500.0)], base + 5));
            let out = g.evento(&t(Acao::PonteiroCima, 1, &[p(0, 100.0, 500.0), p(1, 300.0, 500.0)], base + 80));
            assert!(out.is_empty());
            g.evento(&t(Acao::Cima, 0, &[p(0, 100.0, 500.0)], base + 85));
        }
        // Dois dedos lentos (mais de 300 ms) também não contam como toque.
        let mut g = Gestos::new(440);
        for base in [0u64, 500] {
            g.evento(&t(Acao::Baixo, 0, &[p(0, 100.0, 500.0)], base));
            g.evento(&t(Acao::PonteiroBaixo, 1, &[p(0, 100.0, 500.0), p(1, 300.0, 500.0)], base + 5));
            assert!(g.evento(&t(Acao::PonteiroCima, 1, &[p(0, 100.0, 500.0), p(1, 300.0, 500.0)], base + 400)).is_empty());
            g.evento(&t(Acao::Cima, 0, &[p(0, 100.0, 500.0)], base + 405));
        }
    }

    #[test]
    fn cancelamento_solta_o_botao_e_nao_clica() {
        let mut g = g();
        g.evento(&t(Acao::Baixo, 0, &[p(0, 100.0, 100.0)], 0));
        g.evento(&t(Acao::Mover, 0, &[p(0, 200.0, 100.0)], 20));
        let out = g.evento(&t(Acao::Cancelar, 0, &[p(0, 200.0, 100.0)], 30));
        assert_eq!(out, vec![Saida::Botao { botao: Botao::Esquerdo, pressionado: false, x: 200.0, y: 100.0 }]);
        // Cancelar um toque ainda pendente: nada.
        g.evento(&t(Acao::Baixo, 0, &[p(0, 1.0, 1.0)], 100));
        assert!(g.evento(&t(Acao::Cancelar, 0, &[p(0, 1.0, 1.0)], 110)).is_empty());
        assert!(g.tick(10_000).is_empty(), "cancelado: o relógio não vira toque longo");
        // Cancelar com dois dedos.
        g.evento(&t(Acao::Baixo, 0, &[p(0, 100.0, 500.0)], 200));
        g.evento(&t(Acao::PonteiroBaixo, 1, &[p(0, 100.0, 500.0), p(1, 300.0, 500.0)], 210));
        assert!(g.evento(&t(Acao::Cancelar, 0, &[p(0, 100.0, 500.0), p(1, 300.0, 500.0)], 220)).is_empty());
        assert!(g.evento(&t(Acao::Mover, 0, &[p(0, 100.0, 400.0), p(1, 300.0, 400.0)], 230)).is_empty());
    }

    #[test]
    fn down_sem_up_anterior_solta_o_botao_preso() {
        let mut g = g();
        g.evento(&t(Acao::Baixo, 0, &[p(0, 100.0, 100.0)], 0));
        g.evento(&t(Acao::Mover, 0, &[p(0, 200.0, 100.0)], 20));
        let out = g.evento(&t(Acao::Baixo, 0, &[p(0, 5.0, 5.0)], 500));
        assert_eq!(out, vec![Saida::Botao { botao: Botao::Esquerdo, pressionado: false, x: 200.0, y: 100.0 }]);
    }

    #[test]
    fn eventos_de_outro_dedo_ou_sem_estado_nao_fazem_nada() {
        let mut g = g();
        assert!(g.evento(&t(Acao::Mover, 0, &[p(0, 1.0, 1.0)], 0)).is_empty());
        assert!(g.evento(&t(Acao::PonteiroCima, 0, &[p(0, 1.0, 1.0)], 0)).is_empty());
        assert!(g.evento(&t(Acao::Cima, 0, &[p(0, 1.0, 1.0)], 0)).is_empty());
        assert!(g.evento(&t(Acao::PonteiroBaixo, 0, &[p(0, 1.0, 1.0)], 0)).is_empty());
        g.evento(&t(Acao::Baixo, 0, &[p(0, 100.0, 100.0)], 0));
        // MOVE sem o dedo rastreado na lista: nada.
        assert!(g.evento(&t(Acao::Mover, 0, &[p(7, 900.0, 900.0)], 10)).is_empty());
    }

    #[test]
    fn decodificar_toque_aceita_o_valido_e_recusa_o_invalido_sem_panico() {
        let ok = decodificar_toque(5, 1, &[0.0, 10.0, 20.0, 1.0, 30.0, 40.0], 123).unwrap();
        assert_eq!(ok.acao, Acao::PonteiroBaixo);
        assert_eq!((ok.indice, ok.t_ms, ok.ponteiros.len()), (1, 123, 2));
        assert_eq!(ok.ponteiros[1], p(1, 30.0, 40.0));
        assert!(decodificar_toque(4, 0, &[0.0, 1.0, 2.0], 0).is_none(), "ação desconhecida");
        assert!(decodificar_toque(0, 0, &[], 0).is_none());
        assert!(decodificar_toque(0, 0, &[0.0, 1.0], 0).is_none());
        assert!(decodificar_toque(0, 1, &[0.0, 1.0, 2.0], 0).is_none(), "índice fora da lista");
        assert!(decodificar_toque(0, -1, &[0.0, 1.0, 2.0], 0).is_none());
        assert!(decodificar_toque(0, 0, &[0.0, f32::NAN, 2.0], 0).is_none());
        assert!(decodificar_toque(0, 0, &[0.0, f32::INFINITY, 2.0], 0).is_none());
        assert!(decodificar_toque(0, 0, &[0.0, 1.0, 2.0], -5).is_none());
        assert!(decodificar_toque(0, 0, &[0.0; 3 * 11], 0).is_none(), "dedos demais");
    }
}
