
use std::collections::VecDeque;
use std::ops::Range;

use ps_codec::{Rect, BLOCK};

pub const MAX_FRAMES_PER_WINDOW: usize = 2;
pub const MIN_BUDGET: usize = 64 * 1024;
pub const MAX_BUDGET: usize = 8 * 1024 * 1024;
pub const BUDGET_SECONDS: f64 = 0.060;
pub const INITIAL_THROUGHPUT: f64 = 4.0 * 1024.0 * 1024.0;
pub const SAMPLE_MIN_BYTES: usize = 16 * 1024;
const EMA_ALPHA: f64 = 0.25;
const DEFAULT_BYTES_PER_BLOCK: f64 = 4096.0;

struct InFlight {
    seq: u32,
    bytes: usize,
    sent_us: u64,
}

struct WinFlow {
    id: u32,
    parent: u32,
    frames: VecDeque<InFlight>,
    last_sent_us: u64,
    bytes_per_block: f64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Candidate {
    pub win: u32,
    pub pending_blocks: usize,
    pub can_slice: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Pick {
    pub win: u32,
    pub slice_bytes: Option<usize>,
}

pub struct Flow {
    throughput: f64,
    adaptive: bool,
    focus: Option<u32>,
    wins: Vec<WinFlow>,
}

impl Flow {
    pub fn new(initial_throughput: f64, adaptive: bool) -> Self {
        Flow { throughput: initial_throughput, adaptive, focus: None, wins: Vec::new() }
    }

    pub fn add_window(&mut self, id: u32, now_us: u64) {
        if !self.wins.iter().any(|w| w.id == id) {
            self.wins.push(WinFlow { id, parent: 0, frames: VecDeque::new(), last_sent_us: now_us, bytes_per_block: 0.0 });
        }
    }

    pub fn set_parent(&mut self, id: u32, parent: u32) {
        if let Some(w) = self.wins.iter_mut().find(|w| w.id == id) {
            w.parent = parent;
        }
    }

    fn root_of(&self, id: u32) -> u32 {
        let mut cur = id;
        for _ in 0..16 {
            match self.wins.iter().find(|w| w.id == cur) {
                Some(w) if w.parent != 0 => cur = w.parent,
                _ => break,
            }
        }
        cur
    }

    pub fn remove_window(&mut self, id: u32) {
        self.wins.retain(|w| w.id != id);
        if self.focus == Some(id) {
            self.focus = None;
        }
    }

    pub fn set_focus(&mut self, id: u32) {
        self.focus = Some(id);
    }

    pub fn clear_focus(&mut self, id: u32) {
        if self.focus == Some(id) {
            self.focus = None;
        }
    }

    pub fn throughput(&self) -> f64 {
        self.throughput
    }

    pub fn budget(&self) -> usize {
        ((self.throughput * BUDGET_SECONDS) as usize).clamp(MIN_BUDGET, MAX_BUDGET)
    }

    pub fn in_flight_bytes(&self) -> usize {
        self.wins.iter().flat_map(|w| w.frames.iter()).map(|f| f.bytes).sum()
    }

    pub fn in_flight_frames(&self, win: u32) -> usize {
        self.wins.iter().find(|w| w.id == win).map_or(0, |w| w.frames.len())
    }

    pub fn bytes_per_block(&self, win: u32) -> f64 {
        self.wins.iter().find(|w| w.id == win).map_or(DEFAULT_BYTES_PER_BLOCK, |w| if w.bytes_per_block > 0.0 { w.bytes_per_block } else { DEFAULT_BYTES_PER_BLOCK })
    }

    pub fn record_encoded(&mut self, win: u32, coded_blocks: usize, bytes: usize) {
        if coded_blocks == 0 {
            return;
        }
        if let Some(w) = self.wins.iter_mut().find(|w| w.id == win) {
            let sample = bytes as f64 / coded_blocks as f64;
            w.bytes_per_block = if w.bytes_per_block > 0.0 { 0.5 * w.bytes_per_block + 0.5 * sample } else { sample };
        }
    }

    pub fn on_sent(&mut self, win: u32, seq: u32, bytes: usize, now_us: u64) {
        if let Some(w) = self.wins.iter_mut().find(|w| w.id == win) {
            w.frames.push_back(InFlight { seq, bytes, sent_us: now_us });
            w.last_sent_us = now_us;
        }
    }

    pub fn on_ack(&mut self, win: u32, seq: u32, now_us: u64) {
        let Some(w) = self.wins.iter_mut().find(|w| w.id == win) else { return };
        let Some(pos) = w.frames.iter().position(|f| f.seq == seq) else { return };
        let Some(f) = w.frames.remove(pos) else { return };
        if self.adaptive && f.bytes > SAMPLE_MIN_BYTES {
            let dt = (now_us.saturating_sub(f.sent_us)).max(1000) as f64 / 1e6;
            let sample = f.bytes as f64 / dt;
            self.throughput = (1.0 - EMA_ALPHA) * self.throughput + EMA_ALPHA * sample;
        }
    }

    pub fn decide(&self, cands: &[Candidate]) -> Option<Pick> {
        let budget = self.budget();
        let total = self.in_flight_bytes();
        let mut order: Vec<&Candidate> = cands.iter().filter(|c| self.wins.iter().any(|w| w.id == c.win)).collect();
        order.sort_by_key(|c| {
            let w = self.wins.iter().find(|w| w.id == c.win);
            (self.focus != Some(self.root_of(c.win)), w.map_or(0, |w| w.last_sent_us), c.win)
        });
        for c in order {
            let w = self.wins.iter().find(|w| w.id == c.win)?;
            if w.frames.len() >= MAX_FRAMES_PER_WINDOW {
                continue;
            }
            let bpb = if w.bytes_per_block > 0.0 { w.bytes_per_block } else { DEFAULT_BYTES_PER_BLOCK };
            let mut est = (c.pending_blocks as f64 * bpb) as usize;
            let mut slice_bytes = None;
            if c.can_slice && est > budget / 2 {
                slice_bytes = Some(budget / 2);
                est = budget / 2;
            }
            if total + est <= budget {
                return Some(Pick { win: c.win, slice_bytes });
            }
            let others_big = self.wins.iter().filter(|o| o.id != c.win).flat_map(|o| o.frames.iter()).any(|f| f.bytes > budget);
            if w.frames.is_empty() && !others_big {
                return Some(Pick { win: c.win, slice_bytes });
            }
        }
        None
    }
}

pub fn plan_slice(strip_dirty: &[usize], cursor: usize, bytes_per_block: f64, target_bytes: usize) -> Range<usize> {
    let n = strip_dirty.len();
    let start = (cursor.min(n)..n).chain(0..cursor.min(n)).find(|&i| strip_dirty[i] > 0);
    let Some(start) = start else { return 0..0 };
    let mut end = start;
    let mut acc = 0.0;
    while end < n {
        acc += strip_dirty[end] as f64 * bytes_per_block;
        end += 1;
        if acc >= target_bytes as f64 {
            break;
        }
    }
    start..end
}

pub fn subtract_band(rects: &mut Vec<Rect>, y0: u32, y1: u32) {
    let mut out = Vec::with_capacity(rects.len() + 2);
    for r in rects.drain(..) {
        let (ry0, ry1) = (r.y, r.y.saturating_add(r.h));
        if ry1 <= y0 || ry0 >= y1 {
            out.push(r);
            continue;
        }
        if ry0 < y0 {
            out.push(Rect::new(r.x, ry0, r.w, y0 - ry0));
        }
        if ry1 > y1 {
            out.push(Rect::new(r.x, y1, r.w, ry1 - y1));
        }
    }
    *rects = out;
}

pub fn strip_rows(strips: &Range<usize>) -> (u32, u32) {
    ((strips.start * BLOCK) as u32, (strips.end * BLOCK) as u32)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ps_codec::testutil::{normalize_x, Rng};
    use ps_codec::{dirty_blocks_per_strip, Decoder, Encoder};

    const KIB: usize = 1024;

    fn flow2() -> Flow {
        let mut f = Flow::new(INITIAL_THROUGHPUT, true);
        f.add_window(1, 0);
        f.add_window(2, 0);
        f
    }

    fn cand(win: u32, blocks: usize) -> Candidate {
        Candidate { win, pending_blocks: blocks, can_slice: true }
    }

    #[test]
    fn orcamento_e_vazao_vezes_60_ms_limitado() {
        let f = Flow::new(4.0 * 1024.0 * 1024.0, true);
        assert_eq!(f.budget(), (4.0 * 1024.0 * 1024.0 * 0.060) as usize);
        assert_eq!(Flow::new(1000.0, true).budget(), MIN_BUDGET);
        assert_eq!(Flow::new(1e12, true).budget(), MAX_BUDGET);
    }

    #[test]
    fn janela_parada_com_quadro_pequeno_sai() {
        let f = flow2();
        let mut f = f;
        f.record_encoded(1, 10, 10 * 100);
        assert_eq!(f.decide(&[cand(1, 10)]), Some(Pick { win: 1, slice_bytes: None }));
    }

    #[test]
    fn orcamento_e_respeitado() {
        let mut f = flow2();
        let budget = f.budget();
        f.on_sent(2, 0, budget - 1000, 0);
        f.record_encoded(1, 1, 2000);
        assert!(f.decide(&[cand(1, 1)]).is_some());
        f.on_sent(1, 0, 10, 0);
        assert_eq!(f.decide(&[cand(1, 1)]), None);
        for _ in 0..11 {
            f.record_encoded(1, 1, 100);
        }
        assert!(f.bytes_per_block(1) < 900.0);
        assert!(f.decide(&[cand(1, 1)]).is_some());
    }

    #[test]
    fn excecao_de_progresso() {
        let mut f = flow2();
        let budget = f.budget();
        f.record_encoded(1, 1, budget * 3);
        let p = f.decide(&[cand(1, 1)]).expect("progresso");
        assert_eq!(p, Pick { win: 1, slice_bytes: Some(budget / 2) });
        let c = Candidate { win: 1, pending_blocks: 1, can_slice: false };
        assert_eq!(f.decide(&[c]), Some(Pick { win: 1, slice_bytes: None }));
        f.on_sent(2, 0, budget + 1, 0);
        assert_eq!(f.decide(&[c]), None);
        f.on_ack(2, 0, 1_000_000);
        assert!(f.decide(&[c]).is_some());
    }

    #[test]
    fn nunca_mais_de_dois_quadros_por_janela() {
        let mut f = flow2();
        f.on_sent(1, 0, 10, 0);
        f.on_sent(1, 1, 10, 0);
        assert_eq!(f.decide(&[cand(1, 1)]), None);
        f.on_ack(1, 0, 10);
        assert!(f.decide(&[cand(1, 1)]).is_some());
    }

    #[test]
    fn janela_com_foco_passa_na_frente() {
        let mut f = flow2();
        f.on_sent(1, 0, 10, 100);
        f.on_ack(1, 0, 200);
        assert_eq!(f.decide(&[cand(1, 1), cand(2, 1)]).unwrap().win, 2);
        f.set_focus(1);
        assert_eq!(f.decide(&[cand(1, 1), cand(2, 1)]).unwrap().win, 1);
        f.on_sent(1, 1, 10, 300);
        f.on_sent(1, 2, 10, 300);
        assert_eq!(f.decide(&[cand(1, 1), cand(2, 1)]).unwrap().win, 2);
    }

    #[test]
    fn popup_herda_o_foco_da_janela_raiz() {
        let mut f = flow2();
        f.add_window(3, 0);
        f.set_parent(3, 1);
        f.add_window(4, 0);
        f.set_parent(4, 3);
        f.on_sent(1, 0, 10, 100);
        f.on_ack(1, 0, 200);
        f.set_focus(1);
        assert_eq!(f.decide(&[cand(2, 1), cand(3, 1)]).unwrap().win, 3);
        assert_eq!(f.decide(&[cand(2, 1), cand(4, 1)]).unwrap().win, 4);
        f.clear_focus(1);
        assert_eq!(f.decide(&[cand(2, 1), cand(3, 1)]).unwrap().win, 2);
        f.set_focus(2);
        f.clear_focus(1);
        assert_eq!(f.decide(&[cand(2, 1), cand(3, 1)]).unwrap().win, 2);
    }

    #[test]
    fn so_fatia_o_que_passa_da_metade_do_orcamento_e_pode_ser_fatiado() {
        let mut f = flow2();
        let budget = f.budget();
        f.record_encoded(1, 1, budget / 4);
        assert_eq!(f.decide(&[cand(1, 1)]).unwrap().slice_bytes, None, "um quarto: inteiro");
        assert_eq!(f.decide(&[cand(1, 3)]).unwrap().slice_bytes, Some(budget / 2), "três quartos: fatiado");
        let primeiro_apos_resize = Candidate { win: 1, pending_blocks: 3, can_slice: false };
        assert_eq!(f.decide(&[primeiro_apos_resize]).unwrap().slice_bytes, None, "primeira FRAME nunca é fatiada");
    }

    #[test]
    fn vazao_so_aprende_com_quadros_grandes() {
        let mut f = flow2();
        let antes = f.throughput();
        f.on_sent(1, 0, 4 * KIB, 0);
        f.on_ack(1, 0, 500_000);
        assert_eq!(f.throughput(), antes);
        f.on_sent(1, 1, 1024 * KIB, 0);
        f.on_ack(1, 1, 1_000_000);
        assert!(f.throughput() < antes && f.throughput() > 1.0 * 1024.0 * 1024.0);
        let mut g = Flow::new(INITIAL_THROUGHPUT, false);
        g.add_window(1, 0);
        g.on_sent(1, 0, 1024 * KIB, 0);
        g.on_ack(1, 0, 1_000_000);
        assert_eq!(g.throughput(), INITIAL_THROUGHPUT);
    }

    #[test]
    fn ack_de_janela_ou_seq_desconhecidos_e_ignorado() {
        let mut f = flow2();
        f.on_ack(9, 0, 1);
        f.on_ack(1, 77, 1);
        f.on_sent(1, 0, 5, 0);
        assert_eq!(f.in_flight_bytes(), 5);
        f.remove_window(1);
        assert_eq!(f.in_flight_bytes(), 0);
    }

    #[test]
    fn plan_slice_percorre_o_quadro_sem_deixar_faixas_sem_vez() {
        let dirty = [4usize, 4, 0, 4, 4, 4];
        let bpb = 100.0;
        let a = plan_slice(&dirty, 0, bpb, 800);
        assert_eq!(a, 0..2);
        let b = plan_slice(&dirty, a.end, bpb, 800);
        assert_eq!(b, 3..5);
        let c = plan_slice(&dirty, b.end, bpb, 800);
        assert_eq!(c, 5..6);
        assert_eq!(plan_slice(&dirty, 6, bpb, 800), 0..2);
        assert_eq!(plan_slice(&dirty, 0, bpb, 1), 0..1);
        assert_eq!(plan_slice(&[0, 0, 0], 1, bpb, 800), 0..0);
        assert_eq!(plan_slice(&dirty, 2, bpb, 100), 3..4);
    }

    #[test]
    fn subtract_band_corta_os_retangulos() {
        let mut r = vec![Rect::new(0, 0, 10, 200), Rect::new(5, 300, 4, 4), Rect::new(0, 100, 3, 10)];
        subtract_band(&mut r, 64, 128);
        assert_eq!(r, vec![Rect::new(0, 0, 10, 64), Rect::new(0, 128, 10, 72), Rect::new(5, 300, 4, 4)]);
    }

    #[test]
    fn fatias_do_planejador_reproduzem_o_quadro_inteiro() {
        let (w, h) = (300usize, 500usize);
        let mut rng = Rng::new(77);
        let mut a = vec![0u8; w * h * 4];
        rng.fill(&mut a);
        let mut b = a.clone();
        let mut dano = vec![Rect::new(0, 0, w as u32, h as u32)];
        rng.fill(&mut b);
        let mut enc = Encoder::with_threads(w as u32, h as u32, 2);
        let mut dec = Decoder::new(w as u32, h as u32);
        let mut o = Vec::new();
        enc.encode(&a, w * 4, None, &mut o);
        dec.decode(&o).unwrap();

        let mut cursor = 0;
        let mut fatias = 0;
        while !dano.is_empty() {
            let por_faixa = dirty_blocks_per_strip(Some(&dano), w as u32, h as u32);
            let r = plan_slice(&por_faixa, cursor, 8000.0, 40_000);
            assert!(!r.is_empty());
            let mut o = Vec::new();
            enc.encode_range(&b, w * 4, Some(&dano), r.clone(), &mut o);
            dec.decode(&o).unwrap();
            let (y0, y1) = strip_rows(&r);
            subtract_band(&mut dano, y0, y1);
            cursor = if r.end >= por_faixa.len() { 0 } else { r.end };
            fatias += 1;
            assert!(fatias < 50);
        }
        assert!(fatias > 1, "deveria ter fatiado");
        let mut esperado = b.clone();
        normalize_x(&mut esperado);
        assert_eq!(dec.pixels(), esperado.as_slice());
    }
}
