//! Quais janelas remotas existem, qual está em foco e quais são as abas (docs/26, decisão 5). Lógica pura.
//!
//! O celular mostra **uma janela de nível superior por vez**, com os popups dela por cima; as outras aparecem como
//! abas. Regras:
//! - uma janela nova de nível superior ganha o foco na hora (um diálogo "Salvar como" aparece em foco);
//! - ao fechar a janela em foco, o foco volta à anterior (a que estava em foco antes dela, que ainda existe);
//! - fechar uma janela que não está em foco não muda o foco;
//! - um popup pertence à janela de nível superior de que descende; só os popups da janela em foco são mostrados;
//! - fechar uma janela fecha os popups dela.

use std::collections::HashMap;

/// Limites de tamanho que o servidor informa (`WIN_LIMITS`); zero = sem limite.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Limites {
    pub min_w: u32,
    pub min_h: u32,
    pub max_w: u32,
    pub max_h: u32,
}

#[derive(Debug, Clone)]
struct Entrada {
    id: u32,
    parent: u32,
    popup: bool,
    x: i32,
    y: i32,
    w: u32,
    h: u32,
    titulo: String,
}

/// Um popup visível, com a posição **absoluta** (relativa à janela de nível superior) e o tamanho.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PopupVisivel {
    pub id: u32,
    pub parent: u32,
    pub x: i32,
    pub y: i32,
    pub w: u32,
    pub h: u32,
}

/// O que uma mudança fez com o foco e com a lista de abas.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Mudanca {
    pub foco_antes: Option<u32>,
    pub foco_depois: Option<u32>,
    /// A lista de abas ou o título de alguma mudou.
    pub abas: bool,
}

impl Mudanca {
    pub fn foco_mudou(&self) -> bool {
        self.foco_antes != self.foco_depois
    }
}

#[derive(Debug, Default)]
pub struct Janelas {
    /// Todas as janelas e popups vivos, na ordem de criação.
    entradas: Vec<Entrada>,
    /// Janelas de nível superior, a mais recente em foco no fim.
    pilha: Vec<u32>,
    limites: HashMap<u32, Limites>,
}

impl Janelas {
    pub fn new() -> Janelas {
        Janelas::default()
    }

    fn idx(&self, id: u32) -> Option<usize> {
        self.entradas.iter().position(|e| e.id == id)
    }

    /// A janela de nível superior em foco.
    pub fn foco(&self) -> Option<u32> {
        self.pilha.last().copied()
    }

    fn mudanca(&self, antes: Option<u32>, abas: bool) -> Mudanca {
        Mudanca { foco_antes: antes, foco_depois: self.foco(), abas }
    }

    /// Janela de nível superior nova: ganha o foco.
    pub fn criar(&mut self, id: u32, w: u32, h: u32, titulo: &str) -> Mudanca {
        let antes = self.foco();
        if let Some(i) = self.idx(id) {
            // Id repetido (o servidor não deveria): substitui.
            self.entradas.remove(i);
            self.pilha.retain(|&p| p != id);
        }
        self.entradas.push(Entrada { id, parent: 0, popup: false, x: 0, y: 0, w, h, titulo: titulo.to_string() });
        self.pilha.push(id);
        self.mudanca(antes, true)
    }

    /// Popup novo: `x, y` relativos ao conteúdo da mãe. Mãe desconhecida: ignorado.
    pub fn criar_popup(&mut self, id: u32, parent: u32, x: i32, y: i32, w: u32, h: u32) -> bool {
        if self.idx(parent).is_none() {
            return false;
        }
        if let Some(i) = self.idx(id) {
            self.entradas.remove(i);
        }
        self.entradas.push(Entrada { id, parent, popup: true, x, y, w, h, titulo: String::new() });
        true
    }

    pub fn mover_popup(&mut self, id: u32, x: i32, y: i32, w: u32, h: u32) {
        if let Some(i) = self.idx(id) {
            let e = &mut self.entradas[i];
            if e.popup {
                (e.x, e.y, e.w, e.h) = (x, y, w, h);
            }
        }
    }

    pub fn redimensionar(&mut self, id: u32, w: u32, h: u32) {
        if let Some(i) = self.idx(id) {
            self.entradas[i].w = w;
            self.entradas[i].h = h;
        }
    }

    pub fn titulo(&mut self, id: u32, titulo: &str) -> bool {
        match self.idx(id) {
            Some(i) if !self.entradas[i].popup => {
                self.entradas[i].titulo = titulo.to_string();
                true
            }
            _ => false,
        }
    }

    pub fn definir_limites(&mut self, id: u32, l: Limites) {
        self.limites.insert(id, l);
    }

    pub fn limites(&self, id: u32) -> Limites {
        self.limites.get(&id).copied().unwrap_or_default()
    }

    /// Fecha a janela (ou o popup) e tudo que descende dela. Se era a janela em foco, o foco volta à anterior.
    pub fn destruir(&mut self, id: u32) -> Mudanca {
        let antes = self.foco();
        if self.idx(id).is_none() {
            return self.mudanca(antes, false);
        }
        // A janela e os descendentes (popups e filhos de popups), em qualquer ordem de criação.
        let mut mortos = vec![id];
        loop {
            let novos: Vec<u32> = self.entradas.iter().filter(|e| mortos.contains(&e.parent) && !mortos.contains(&e.id)).map(|e| e.id).collect();
            if novos.is_empty() {
                break;
            }
            mortos.extend(novos);
        }
        let era_aba = self.entradas.iter().any(|e| e.id == id && !e.popup);
        self.entradas.retain(|e| !mortos.contains(&e.id));
        self.pilha.retain(|p| !mortos.contains(p));
        for m in &mortos {
            self.limites.remove(m);
        }
        self.mudanca(antes, era_aba)
    }

    /// Passa o foco a uma janela de nível superior (toque numa aba). Devolve a mudança (sem efeito se não existe).
    pub fn focar(&mut self, id: u32) -> Mudanca {
        let antes = self.foco();
        if self.pilha.contains(&id) {
            self.pilha.retain(|&p| p != id);
            self.pilha.push(id);
        }
        self.mudanca(antes, false)
    }

    /// As abas: janelas de nível superior na ordem de criação, com o título.
    pub fn abas(&self) -> Vec<(u32, String)> {
        self.entradas.iter().filter(|e| !e.popup).map(|e| (e.id, e.titulo.clone())).collect()
    }

    pub fn tamanho(&self, id: u32) -> Option<(u32, u32)> {
        self.idx(id).map(|i| (self.entradas[i].w, self.entradas[i].h))
    }

    /// A janela de nível superior de que `id` descende (ela mesma se já é de nível superior).
    pub fn raiz_de(&self, id: u32) -> Option<u32> {
        let mut atual = id;
        for _ in 0..32 {
            let i = self.idx(atual)?;
            let e = &self.entradas[i];
            if !e.popup || e.parent == 0 {
                return Some(e.id);
            }
            atual = e.parent;
        }
        None
    }

    /// Os popups da janela `raiz` (a mãe direta ou ancestral), na ordem de criação (a de empilhamento), com a
    /// posição absoluta em relação à janela raiz.
    pub fn popups_de(&self, raiz: u32) -> Vec<PopupVisivel> {
        let mut abs: HashMap<u32, (i32, i32)> = HashMap::new();
        abs.insert(raiz, (0, 0));
        let mut v = Vec::new();
        for e in self.entradas.iter().filter(|e| e.popup) {
            if let Some(&(px, py)) = abs.get(&e.parent) {
                let a = (px + e.x, py + e.y);
                abs.insert(e.id, a);
                v.push(PopupVisivel { id: e.id, parent: e.parent, x: a.0, y: a.1, w: e.w, h: e.h });
            }
        }
        v
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn janela_nova_de_nivel_superior_ganha_o_foco() {
        let mut j = Janelas::new();
        let m = j.criar(1, 100, 100, "editor");
        assert_eq!((m.foco_antes, m.foco_depois, m.abas), (None, Some(1), true));
        let m = j.criar(2, 80, 60, "Salvar como");
        assert!(m.foco_mudou());
        assert_eq!((m.foco_antes, m.foco_depois), (Some(1), Some(2)));
        assert_eq!(j.foco(), Some(2));
        assert_eq!(j.abas(), vec![(1, "editor".to_string()), (2, "Salvar como".to_string())]);
    }

    #[test]
    fn ao_fechar_o_foco_volta_a_anterior_e_nao_a_primeira() {
        let mut j = Janelas::new();
        j.criar(1, 10, 10, "a");
        j.criar(2, 10, 10, "b");
        j.criar(3, 10, 10, "c");
        j.focar(1); // a ordem de foco agora é 2, 3, 1
        j.criar(4, 10, 10, "d"); // 2, 3, 1, 4
        let m = j.destruir(4);
        assert_eq!(m.foco_depois, Some(1), "volta a que estava em foco antes");
        let m = j.destruir(1);
        assert_eq!(m.foco_depois, Some(3));
        let m = j.destruir(3);
        assert_eq!(m.foco_depois, Some(2));
        let m = j.destruir(2);
        assert_eq!((m.foco_antes, m.foco_depois), (Some(2), None));
        assert!(j.abas().is_empty());
    }

    #[test]
    fn fechar_janela_sem_foco_nao_muda_o_foco() {
        let mut j = Janelas::new();
        j.criar(1, 10, 10, "a");
        j.criar(2, 10, 10, "b");
        let m = j.destruir(1);
        assert!(!m.foco_mudou() && m.abas);
        assert_eq!(j.foco(), Some(2));
        // Fechar o que não existe não faz nada.
        let m = j.destruir(99);
        assert!(!m.foco_mudou() && !m.abas);
    }

    #[test]
    fn tocar_numa_aba_foca_aquela_janela_e_aba_inexistente_nao_faz_nada() {
        let mut j = Janelas::new();
        j.criar(1, 10, 10, "a");
        j.criar(2, 10, 10, "b");
        let m = j.focar(1);
        assert_eq!((m.foco_antes, m.foco_depois), (Some(2), Some(1)));
        let m = j.focar(77);
        assert!(!m.foco_mudou());
        assert_eq!(j.foco(), Some(1));
        // As abas continuam na ordem de criação, não na de foco.
        assert_eq!(j.abas().iter().map(|a| a.0).collect::<Vec<_>>(), vec![1, 2]);
    }

    #[test]
    fn popups_ficam_com_a_janela_de_origem_e_morrem_com_ela() {
        let mut j = Janelas::new();
        j.criar(1, 200, 200, "a");
        j.criar(2, 200, 200, "b");
        assert!(j.criar_popup(10, 1, 5, 6, 50, 40));
        assert!(j.criar_popup(11, 10, 20, 30, 10, 10), "popup de popup");
        assert!(!j.criar_popup(12, 999, 0, 0, 1, 1), "mãe desconhecida");
        assert_eq!(j.raiz_de(11), Some(1));
        assert_eq!(j.raiz_de(2), Some(2));
        assert_eq!(j.raiz_de(999), None);
        // Posição absoluta: o filho soma a do pai.
        assert_eq!(j.popups_de(1), vec![PopupVisivel { id: 10, parent: 1, x: 5, y: 6, w: 50, h: 40 }, PopupVisivel { id: 11, parent: 10, x: 25, y: 36, w: 10, h: 10 }]);
        assert!(j.popups_de(2).is_empty(), "os popups da janela 1 não aparecem na 2");
        j.mover_popup(10, 1, 1, 60, 45);
        assert_eq!(j.popups_de(1)[0], PopupVisivel { id: 10, parent: 1, x: 1, y: 1, w: 60, h: 45 });
        // Fechar o popup mãe leva o filho junto.
        j.destruir(10);
        assert!(j.popups_de(1).is_empty());
        // Fechar a janela leva os popups.
        j.criar_popup(13, 1, 0, 0, 5, 5);
        let m = j.destruir(1);
        assert!(m.abas);
        assert!(j.popups_de(1).is_empty());
        assert_eq!(j.foco(), Some(2));
    }

    #[test]
    fn titulo_e_limites() {
        let mut j = Janelas::new();
        j.criar(1, 10, 10, "a");
        assert!(j.titulo(1, "novo"));
        assert!(!j.titulo(5, "x"));
        assert_eq!(j.abas()[0].1, "novo");
        assert_eq!(j.limites(1), Limites::default());
        j.definir_limites(1, Limites { min_w: 5, min_h: 6, max_w: 700, max_h: 800 });
        assert_eq!(j.limites(1).max_w, 700);
        j.redimensionar(1, 33, 44);
        assert_eq!(j.tamanho(1), Some((33, 44)));
        j.destruir(1);
        assert_eq!(j.limites(1), Limites::default());
        assert_eq!(j.tamanho(1), None);
    }

    #[test]
    fn id_repetido_substitui_sem_duplicar_a_aba() {
        let mut j = Janelas::new();
        j.criar(1, 10, 10, "a");
        j.criar(1, 20, 20, "a de novo");
        assert_eq!(j.abas().len(), 1);
        assert_eq!(j.foco(), Some(1));
        assert_eq!(j.tamanho(1), Some((20, 20)));
    }
}
