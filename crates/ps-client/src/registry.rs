
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use crate::net::SharedWindow;

#[derive(Clone)]
pub struct WinInfo {
    pub num: usize,
    pub id: u32,
    pub parent: u32,
    pub popup: bool,
    pub grab: bool,
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

    pub fn snapshot(&self) -> Vec<WinInfo> {
        self.lock().wins.clone()
    }

    pub fn by_num(&self, num: usize) -> Option<WinInfo> {
        self.lock().wins.iter().find(|w| w.num == num).cloned()
    }

    pub fn by_id(&self, id: u32) -> Option<WinInfo> {
        self.lock().wins.iter().rev().find(|w| w.id == id && w.alive).cloned()
    }

    pub fn by_title(&self, texto: &str) -> Option<WinInfo> {
        let t = texto.to_lowercase();
        self.lock().wins.iter().find(|w| w.alive && w.title.to_lowercase().contains(&t)).cloned()
    }

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
        assert_eq!(r.add(info(13, false, 0, "Salvar como")), 4);
        assert!(r.by_num(2).is_some_and(|w| !w.alive));
        assert_eq!(r.by_title("salvar").map(|w| w.id), Some(13));
        assert!(r.by_title("nada").is_none());
    }
}
