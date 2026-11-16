//! Tabela das janelas remotas conhecidas pelo cliente, na ordem de criação.
//!
//! Quem a mantém é a thread de rede (ao receber `WIN_CREATE`, `POPUP_CREATE`, `WIN_DESTROY`...);
//! quem a consulta é o roteiro de teste (`--script`), que seleciona janelas por número ou título
//! e lê os pixels já decodificados sem passar pela interface.

use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use crate::net::SharedWindow;

/// Uma janela (ou popup) remota.
#[derive(Clone)]
pub struct WinInfo {
    /// Número de ordem de criação, a partir de 1. Nunca é reaproveitado.
    pub num: usize,
    /// Id remoto (o mesmo do protocolo).
    pub id: u32,
    /// Janela mãe (0 = nenhuma). Para popups é o pai da cadeia.
    pub parent: u32,
    pub popup: bool,
    pub grab: bool,
    /// Posição do popup relativa ao conteúdo do pai.
    pub x: i32,
    pub y: i32,
    pub title: String,
    pub shared: Arc<SharedWindow>,
    pub alive: bool,
}

#[derive(Default)]
struct Inner {
    wins: Vec<WinInfo>,
    created: usize,
}

#[derive(Default)]
pub struct Registry {
    inner: Mutex<Inner>,
    cv: Condvar,
}

impl Registry {
    pub fn new() -> Arc<Registry> {
        Arc::new(Registry::default())
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Registra uma janela nova e devolve o número de ordem dela.
    pub fn add(&self, mut info: WinInfo) -> usize {
        let mut g = self.lock();
        g.created += 1;
        info.num = g.created;
        info.alive = true;
        let num = info.num;
        g.wins.push(info);
        drop(g);
        self.cv.notify_all();
        num
    }

    pub fn destroy(&self, id: u32) {
        let mut g = self.lock();
        if let Some(w) = g.wins.iter_mut().find(|w| w.id == id && w.alive) {
            w.alive = false;
        }
        drop(g);
        self.cv.notify_all();
    }

    pub fn set_title(&self, id: u32, title: &str) {
        if let Some(w) = self.lock().wins.iter_mut().find(|w| w.id == id && w.alive) {
            w.title = title.to_string();
        }
    }

    pub fn set_parent(&self, id: u32, parent: u32) {
        if let Some(w) = self.lock().wins.iter_mut().find(|w| w.id == id && w.alive) {
            w.parent = parent;
        }
    }

    pub fn set_popup_pos(&self, id: u32, x: i32, y: i32) {
        if let Some(w) = self.lock().wins.iter_mut().find(|w| w.id == id && w.alive) {
            w.x = x;
            w.y = y;
        }
    }

    /// Todas as janelas já criadas (vivas ou não), na ordem de criação.
    pub fn snapshot(&self) -> Vec<WinInfo> {
        self.lock().wins.clone()
    }

    pub fn by_num(&self, num: usize) -> Option<WinInfo> {
        self.lock().wins.iter().find(|w| w.num == num).cloned()
    }

    pub fn by_id(&self, id: u32) -> Option<WinInfo> {
        self.lock().wins.iter().rev().find(|w| w.id == id && w.alive).cloned()
    }

    /// Primeira janela viva cujo título contém `texto` (sem diferenciar maiúsculas).
    pub fn by_title(&self, texto: &str) -> Option<WinInfo> {
        let t = texto.to_lowercase();
        self.lock().wins.iter().find(|w| w.alive && w.title.to_lowercase().contains(&t)).cloned()
    }

    /// Janela raiz (a que não é popup) da cadeia de `id`.
    pub fn root_of(&self, id: u32) -> u32 {
        let g = self.lock();
        let mut cur = id;
        for _ in 0..16 {
            match g.wins.iter().find(|w| w.id == cur && w.alive) {
                Some(w) if w.popup && w.parent != 0 => cur = w.parent,
                _ => break,
            }
        }
        cur
    }

    /// Espera até `cond` ser verdadeira para a lista de janelas ou o prazo acabar.
    pub fn wait_until(&self, timeout: Duration, mut cond: impl FnMut(&[WinInfo]) -> bool) -> bool {
        let fim = Instant::now() + timeout;
        let mut g = self.lock();
        loop {
            if cond(&g.wins) {
                return true;
            }
            let agora = Instant::now();
            if agora >= fim {
                return false;
            }
            g = self.cv.wait_timeout(g, fim - agora).unwrap_or_else(|e| e.into_inner()).0;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn info(id: u32, popup: bool, parent: u32, title: &str) -> WinInfo {
        WinInfo { num: 0, id, parent, popup, grab: popup, x: 0, y: 0, title: title.into(), shared: SharedWindow::new(8, 8), alive: true }
    }

    #[test]
    fn numeracao_estavel_e_raiz_de_popups() {
        let r = Registry::new();
        assert_eq!(r.add(info(10, false, 0, "Editor")), 1);
        assert_eq!(r.add(info(11, true, 10, "")), 2);
        assert_eq!(r.add(info(12, true, 11, "")), 3);
        assert_eq!(r.root_of(12), 10);
        r.destroy(11);
        // O número 2 não é reaproveitado.
        assert_eq!(r.add(info(13, false, 0, "Salvar como")), 4);
        assert!(r.by_num(2).is_some_and(|w| !w.alive));
        assert_eq!(r.by_title("salvar").map(|w| w.id), Some(13));
        assert!(r.by_title("nada").is_none());
    }
}
