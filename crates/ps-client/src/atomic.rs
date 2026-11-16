
use std::collections::HashSet;
use std::time::{Duration, Instant};

pub const PROGRESS_AFTER: Duration = Duration::from_millis(250);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    Hold,
    Present,
    PresentProgress,
}

#[derive(Debug, Default)]
pub struct SliceGate {
    started: Option<Instant>,
}

impl SliceGate {
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

    pub fn in_progress(&self) -> bool {
        self.started.is_some()
    }

    pub fn reset(&mut self) {
        self.started = None;
    }
}

#[derive(Debug, Default)]
pub struct AtomicProbe {
    start: Option<Instant>,
    presentations: u64,
    mixed: u64,
    distinct: HashSet<u32>,
    progressive: u64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct AtomicReport {
    pub presentations: u64,
    pub mixed: u64,
    pub distinct_frames: u64,
    pub progressive: u64,
    pub secs: f64,
}

impl AtomicReport {
    pub fn fps(&self) -> f64 {
        self.distinct_frames as f64 / self.secs.max(1e-9)
    }

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

    pub fn progressive(&mut self) {
        if self.is_on() {
            self.progressive += 1;
        }
    }

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
        p.observe(&[1, 1]);
        assert!(p.finish(t).is_none());
        p.start(t);
        p.observe(&[5, 5, 5]);
        p.observe(&[5, 5, 5]);
        p.observe(&[6, 6, 6]);
        p.observe(&[7, 6, 6]);
        p.observe(&[u32::MAX, u32::MAX]);
        p.progressive();
        let r = p.finish(ms(t, 2000)).unwrap();
        assert_eq!((r.presentations, r.mixed, r.distinct_frames, r.progressive), (5, 2, 2, 1));
        assert!((r.fps() - 1.0).abs() < 1e-9);
        assert!(r.line().starts_with("ATOMIC apresentacoes=5 mistas=2 quadros_distintos=2 fps=1.0 progressivas=1"), "{}", r.line());
        assert!(!p.is_on());
    }
}
