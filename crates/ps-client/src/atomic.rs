//! Apresentação atômica de quadros fatiados (`docs/20`, `docs/05`): a regra de quando apresentar uma fatia e a
//! medição que o roteiro (`atomic start` / `atomic report`) usa para provar que nenhuma imagem apresentada mistura
//! instantes diferentes.
//!
//! Lógica pura, sem relógio próprio (o instante vem de fora), para testar com relógio simulado.

use std::collections::HashSet;
use std::time::{Duration, Instant};

/// Depois deste tempo desde a primeira fatia com `MORE`, um quadro fatiado que não terminou passa a ser
/// apresentado fatia a fatia (num enlace lento é melhor ver a tela se formando do que ficar parado).
pub const PROGRESS_AFTER: Duration = Duration::from_millis(250);

/// O que fazer com um `FRAME` que acabou de ser decodificado.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// Fatia com `MORE` de um quadro que ainda está dentro do prazo: confirmar na hora, sem pedir redesenho.
    Hold,
    /// Última fatia (ou quadro inteiro): pedir redesenho e confirmar na apresentação.
    Present,
    /// Fatia com `MORE` de um quadro em curso há mais de [`PROGRESS_AFTER`]: apresentar assim mesmo.
    PresentProgress,
}

/// Estado do quadro fatiado de uma janela. A marca do tempo é do cliente, a partir da primeira fatia com `MORE`.
#[derive(Debug, Default)]
pub struct SliceGate {
    started: Option<Instant>,
}

impl SliceGate {
    /// Chegou um `FRAME` (`more` = a fatia não é a última do quadro).
    pub fn on_frame(&mut self, more: bool, now: Instant) -> Verdict {
        if !more {
            self.started = None;
            return Verdict::Present;
        }
        let inicio = *self.started.get_or_insert(now);
        if now.saturating_duration_since(inicio) > PROGRESS_AFTER {
            Verdict::PresentProgress
        } else {
            Verdict::Hold
        }
    }

    /// Há um quadro fatiado em curso (já chegou fatia com `MORE` e a última ainda não).
    pub fn in_progress(&self) -> bool {
        self.started.is_some()
    }

    /// Esquece o quadro em curso (a janela mudou de tamanho).
    pub fn reset(&mut self) {
        self.started = None;
    }
}

/// Contadores da medição `atomic start` / `atomic report`.
#[derive(Debug, Default)]
pub struct AtomicProbe {
    start: Option<Instant>,
    presentations: u64,
    mixed: u64,
    distinct: HashSet<u32>,
    progressive: u64,
}

/// Resultado de uma medição.
#[derive(Debug, Clone, PartialEq)]
pub struct AtomicReport {
    pub presentations: u64,
    /// Apresentações cujos marcadores tinham números de quadro diferentes.
    pub mixed: u64,
    pub distinct_frames: u64,
    /// Apresentações pedidas por fatias de um quadro em curso há mais de 250 ms (modo de progresso).
    pub progressive: u64,
    pub secs: f64,
}

impl AtomicReport {
    /// Quadros distintos por segundo.
    pub fn fps(&self) -> f64 {
        self.distinct_frames as f64 / self.secs.max(1e-9)
    }

    /// A linha impressa pelo roteiro.
    pub fn line(&self) -> String {
        format!(
            "ATOMIC apresentacoes={} mistas={} quadros_distintos={} fps={:.1} progressivas={} janela={:.1}s",
            self.presentations,
            self.mixed,
            self.distinct_frames,
            self.fps(),
            self.progressive,
            self.secs
        )
    }
}

impl AtomicProbe {
    pub fn is_on(&self) -> bool {
        self.start.is_some()
    }

    pub fn start(&mut self, now: Instant) {
        *self = AtomicProbe { start: Some(now), ..AtomicProbe::default() };
    }

    /// Uma apresentação causada por `FRAME`, com os marcadores da imagem que vai para a tela.
    pub fn observe(&mut self, markers: &[u32]) {
        if !self.is_on() {
            return;
        }
        self.presentations += 1;
        match markers.split_first() {
            Some((&n, resto)) if n != u32::MAX && resto.iter().all(|&m| m == n) => {
                self.distinct.insert(n);
            }
            _ => self.mixed += 1,
        }
    }

    /// Uma apresentação pedida por uma fatia no modo de progresso.
    pub fn progressive(&mut self) {
        if self.is_on() {
            self.progressive += 1;
        }
    }

    /// Fecha a medição; `None` se nenhuma foi aberta.
    pub fn finish(&mut self, now: Instant) -> Option<AtomicReport> {
        let p = std::mem::take(self);
        let inicio = p.start?;
        Some(AtomicReport {
            presentations: p.presentations,
            mixed: p.mixed,
            distinct_frames: p.distinct.len() as u64,
            progressive: p.progressive,
            secs: now.saturating_duration_since(inicio).as_secs_f64(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ms(base: Instant, n: u64) -> Instant {
        base + Duration::from_millis(n)
    }

    #[test]
    fn quadro_inteiro_e_ultima_fatia_apresentam() {
        let t = Instant::now();
        let mut g = SliceGate::default();
        assert_eq!(g.on_frame(false, t), Verdict::Present);
        assert!(!g.in_progress());
        assert_eq!(g.on_frame(true, ms(t, 10)), Verdict::Hold);
        assert!(g.in_progress());
        assert_eq!(g.on_frame(false, ms(t, 20)), Verdict::Present);
        assert!(!g.in_progress(), "a última fatia fecha o quadro");
    }

    #[test]
    fn dentro_dos_250_ms_as_fatias_ficam_retidas() {
        let t = Instant::now();
        let mut g = SliceGate::default();
        for i in 0..=25 {
            // 0, 10, ..., 250 ms: no limite exato (250 ms) ainda não é "mais de 250 ms".
            assert_eq!(g.on_frame(true, ms(t, i * 10)), Verdict::Hold, "fatia em {} ms", i * 10);
        }
        assert_eq!(g.on_frame(false, ms(t, 255)), Verdict::Present);
    }

    #[test]
    fn passando_de_250_ms_apresenta_cada_fatia_ate_o_quadro_acabar() {
        let t = Instant::now();
        let mut g = SliceGate::default();
        assert_eq!(g.on_frame(true, t), Verdict::Hold);
        assert_eq!(g.on_frame(true, ms(t, 100)), Verdict::Hold);
        assert_eq!(g.on_frame(true, ms(t, 251)), Verdict::PresentProgress);
        assert_eq!(g.on_frame(true, ms(t, 300)), Verdict::PresentProgress);
        assert_eq!(g.on_frame(true, ms(t, 2000)), Verdict::PresentProgress);
        assert_eq!(g.on_frame(false, ms(t, 2100)), Verdict::Present);
        // O quadro seguinte começa a contar do zero, a partir da própria primeira fatia com MORE.
        assert_eq!(g.on_frame(true, ms(t, 2200)), Verdict::Hold);
        assert_eq!(g.on_frame(true, ms(t, 2400)), Verdict::Hold);
        assert_eq!(g.on_frame(true, ms(t, 2451)), Verdict::PresentProgress);
    }

    #[test]
    fn reset_esquece_o_quadro_em_curso() {
        let t = Instant::now();
        let mut g = SliceGate::default();
        g.on_frame(true, t);
        g.reset();
        assert!(!g.in_progress());
        assert_eq!(g.on_frame(true, ms(t, 1000)), Verdict::Hold);
    }

    #[test]
    fn medicao_conta_mistas_distintos_e_progressivas() {
        let t = Instant::now();
        let mut p = AtomicProbe::default();
        p.observe(&[1, 1]); // fora da medição: não conta
        assert!(p.finish(t).is_none());
        p.start(t);
        p.observe(&[5, 5, 5]);
        p.observe(&[5, 5, 5]);
        p.observe(&[6, 6, 6]);
        p.observe(&[7, 6, 6]); // mista
        p.observe(&[u32::MAX, u32::MAX]); // não é do gerador: conta como mista
        p.progressive();
        let r = p.finish(ms(t, 2000)).unwrap();
        assert_eq!((r.presentations, r.mixed, r.distinct_frames, r.progressive), (5, 2, 2, 1));
        assert!((r.fps() - 1.0).abs() < 1e-9);
        assert!(r.line().starts_with("ATOMIC apresentacoes=5 mistas=2 quadros_distintos=2 fps=1.0 progressivas=1"), "{}", r.line());
        assert!(!p.is_on());
    }
}
