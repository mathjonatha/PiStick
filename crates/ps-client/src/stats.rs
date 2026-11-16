
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering::Relaxed};
use std::sync::Mutex;
use std::time::Instant;

use crate::atomic::{AtomicProbe, AtomicReport};

#[derive(Default)]
pub struct Stats {
    pub frames: AtomicU64,
    pub bytes: AtomicU64,
    pub decode_total_us: AtomicU64,
    pub decode_max_us: AtomicU64,
    pub present_total_us: AtomicU64,
    pub present_count: AtomicU64,
    pub acks: AtomicU64,
    pub rtt_count: AtomicU64,
    pub rtt_total_us: AtomicU64,
    pub rtt_min_us: AtomicU64,
    pub rtt_max_us: AtomicU64,
    pub video_frames: AtomicU64,
    pub video_bytes: AtomicU64,
    pub video_decode_total_us: AtomicU64,
    pub video_decode_max_us: AtomicU64,
    window: Mutex<Window>,
    atomic_probe: Mutex<AtomicProbe>,
    atomic_on: AtomicBool,
}

#[derive(Default)]
struct Window {
    start: Option<Instant>,
    video_frames: u64,
    video_bytes: u64,
    video_decode_us: u64,
    block_frames: u64,
    block_bytes: u64,
    rtts_us: Vec<u64>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct WindowReport {
    pub secs: f64,
    pub video_fps: f64,
    pub video_kbps: f64,
    pub video_decode_avg_ms: f64,
    pub block_fps: f64,
    pub block_kbps: f64,
    pub rtt_median_ms: Option<f64>,
    pub rtt_samples: usize,
}

impl WindowReport {
    pub fn videostats_line(&self) -> String {
        let rtt = self.rtt_median_ms.map_or("sem-amostras".to_string(), |m| format!("{m:.2}ms"));
        format!(
            "VIDEOSTATS fps={:.1} rtt_mediano={rtt} decod_media={:.2}ms video_kbit_s={:.0} blocos_fps={:.1} blocos_kbit_s={:.0} janela={:.1}s rtt_amostras={}",
            self.video_fps, self.video_decode_avg_ms, self.video_kbps, self.block_fps, self.block_kbps, self.secs, self.rtt_samples
        )
    }
}

pub fn median_us(v: &mut [u64]) -> Option<u64> {
    if v.is_empty() {
        return None;
    }
    v.sort_unstable();
    let n = v.len();
    Some(if n % 2 == 1 { v[n / 2] } else { (v[n / 2 - 1] + v[n / 2]) / 2 })
}

fn max_to(a: &AtomicU64, v: u64) {
    a.fetch_max(v, Relaxed);
}

impl Stats {
    fn with_window(&self, f: impl FnOnce(&mut Window)) {
        let mut w = self.window.lock().unwrap_or_else(|e| e.into_inner());
        if w.start.is_some() {
            f(&mut w);
        }
    }

    pub fn atomic_is_on(&self) -> bool {
        self.atomic_on.load(Relaxed)
    }

    pub fn atomic_start(&self) {
        self.atomic_probe.lock().unwrap_or_else(|e| e.into_inner()).start(Instant::now());
        self.atomic_on.store(true, Relaxed);
    }

    pub fn atomic_observe(&self, markers: &[u32]) {
        if self.atomic_is_on() {
            self.atomic_probe.lock().unwrap_or_else(|e| e.into_inner()).observe(markers);
        }
    }

    pub fn atomic_progressive(&self) {
        if self.atomic_is_on() {
            self.atomic_probe.lock().unwrap_or_else(|e| e.into_inner()).progressive();
        }
    }

    pub fn atomic_report(&self) -> Option<AtomicReport> {
        self.atomic_on.store(false, Relaxed);
        self.atomic_probe.lock().unwrap_or_else(|e| e.into_inner()).finish(Instant::now())
    }

    pub fn window_start(&self) {
        *self.window.lock().unwrap_or_else(|e| e.into_inner()) = Window { start: Some(Instant::now()), ..Window::default() };
    }

    pub fn window_report(&self) -> Option<WindowReport> {
        let mut w = std::mem::take(&mut *self.window.lock().unwrap_or_else(|e| e.into_inner()));
        let secs = w.start?.elapsed().as_secs_f64().max(1e-9);
        Some(WindowReport {
            secs,
            video_fps: w.video_frames as f64 / secs,
            video_kbps: w.video_bytes as f64 * 8.0 / 1000.0 / secs,
            video_decode_avg_ms: w.video_decode_us.checked_div(w.video_frames).map_or(0.0, |u| u as f64 / 1000.0),
            block_fps: w.block_frames as f64 / secs,
            block_kbps: w.block_bytes as f64 * 8.0 / 1000.0 / secs,
            rtt_median_ms: median_us(&mut w.rtts_us).map(|u| u as f64 / 1000.0),
            rtt_samples: w.rtts_us.len(),
        })
    }

    pub fn record_decode(&self, bytes: usize, us: u64) {
        self.with_window(|w| {
            w.block_frames += 1;
            w.block_bytes += bytes as u64;
        });
        self.frames.fetch_add(1, Relaxed);
        self.bytes.fetch_add(bytes as u64, Relaxed);
        self.decode_total_us.fetch_add(us, Relaxed);
        max_to(&self.decode_max_us, us);
    }

    pub fn record_video(&self, bytes: usize, us: u64) {
        self.with_window(|w| {
            w.video_frames += 1;
            w.video_bytes += bytes as u64;
            w.video_decode_us += us;
        });
        self.video_frames.fetch_add(1, Relaxed);
        self.video_bytes.fetch_add(bytes as u64, Relaxed);
        self.video_decode_total_us.fetch_add(us, Relaxed);
        max_to(&self.video_decode_max_us, us);
    }

    pub fn record_present(&self, us: u64) {
        self.present_total_us.fetch_add(us, Relaxed);
        self.present_count.fetch_add(1, Relaxed);
    }

    pub fn record_rtt(&self, us: u64) {
        self.with_window(|w| w.rtts_us.push(us));
        self.rtt_count.fetch_add(1, Relaxed);
        self.rtt_total_us.fetch_add(us, Relaxed);
        max_to(&self.rtt_max_us, us);
        let mut cur = self.rtt_min_us.load(Relaxed);
        while cur == 0 || us + 1 < cur {
            match self.rtt_min_us.compare_exchange_weak(cur, us + 1, Relaxed, Relaxed) {
                Ok(_) => break,
                Err(atual) => cur = atual,
            }
        }
    }

    pub fn take_line(&self) -> String {
        let frames = self.frames.swap(0, Relaxed);
        let bytes = self.bytes.swap(0, Relaxed);
        let dec_total = self.decode_total_us.swap(0, Relaxed);
        let dec_max = self.decode_max_us.swap(0, Relaxed);
        let pres_total = self.present_total_us.swap(0, Relaxed);
        let pres_n = self.present_count.swap(0, Relaxed);
        let acks = self.acks.swap(0, Relaxed);
        let rtt_n = self.rtt_count.swap(0, Relaxed);
        let rtt_total = self.rtt_total_us.swap(0, Relaxed);
        let rtt_min = self.rtt_min_us.swap(0, Relaxed);
        let rtt_max = self.rtt_max_us.swap(0, Relaxed);

        let vframes = self.video_frames.swap(0, Relaxed);
        let vbytes = self.video_bytes.swap(0, Relaxed);
        let vtotal = self.video_decode_total_us.swap(0, Relaxed);
        let vmax = self.video_decode_max_us.swap(0, Relaxed);
        let video = vtotal.checked_div(vframes).map_or(String::new(), |avg| {
            format!(" | vídeo: {vframes} quadros/s ({:.0} kbit/s), decodificação média {avg} µs, máx {vmax} µs", vbytes as f64 * 8.0 / 1000.0)
        });
        let dec_avg = dec_total.checked_div(frames).unwrap_or(0);
        let pres_avg = pres_total.checked_div(pres_n).unwrap_or(0);
        let rtt = if rtt_n > 0 {
            format!(
                "RTT (PING/PONG) média {:.2} ms, mín {:.2} ms, máx {:.2} ms ({rtt_n} amostras)",
                rtt_total as f64 / rtt_n as f64 / 1000.0,
                rtt_min.saturating_sub(1) as f64 / 1000.0,
                rtt_max as f64 / 1000.0
            )
        } else {
            "RTT sem amostras".to_string()
        };
        format!(
            "{frames} quadros recebidos ({:.1} KiB), decodificação média {dec_avg} µs, máx {dec_max} µs | apresentação média {pres_avg} µs | {acks} confirmações enviadas | {rtt}{video}",
            bytes as f64 / 1024.0
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mediana_e_janela_de_medicao() {
        assert_eq!(median_us(&mut []), None);
        assert_eq!(median_us(&mut [5, 1, 3]), Some(3));
        assert_eq!(median_us(&mut [4, 1, 3, 10]), Some(3));
        let s = Stats::default();
        s.record_video(1000, 2000);
        assert!(s.window_report().is_none());
        s.window_start();
        s.record_video(1000, 2000);
        s.record_video(3000, 4000);
        s.record_decode(500, 10);
        for r in [1000, 9000, 2000] {
            s.record_rtt(r);
        }
        let r = s.window_report().unwrap();
        assert_eq!(r.rtt_median_ms, Some(2.0));
        assert_eq!(r.rtt_samples, 3);
        assert!((r.video_decode_avg_ms - 3.0).abs() < 1e-9);
        assert!(r.video_fps > 0.0 && r.block_fps > 0.0);
        assert!(s.window_report().is_none(), "o relatório fecha a janela");
    }

    #[test]
    fn linha_com_e_sem_amostras() {
        let s = Stats::default();
        assert!(s.take_line().contains("RTT sem amostras"));
        s.record_decode(2048, 100);
        s.record_decode(2048, 300);
        s.record_rtt(1000);
        s.record_rtt(3000);
        s.record_present(50);
        let l = s.take_line();
        assert!(l.contains("2 quadros recebidos (4.0 KiB)"), "{l}");
        assert!(l.contains("decodificação média 200 µs, máx 300 µs"), "{l}");
        assert!(l.contains("média 2.00 ms, mín 1.00 ms, máx 3.00 ms"), "{l}");
        assert!(s.take_line().contains("0 quadros"));
        assert!(!s.take_line().contains("vídeo"));
        s.record_video(1250, 4000);
        s.record_video(1250, 8000);
        let l = s.take_line();
        assert!(l.contains("vídeo: 2 quadros/s (20 kbit/s), decodificação média 6000 µs, máx 8000 µs"), "{l}");
    }

    #[test]
    fn a_linha_videostats_tem_os_campos_do_psc() {
        let r = WindowReport { secs: 20.0, video_fps: 29.5, video_kbps: 4900.0, video_decode_avg_ms: 6.25, block_fps: 0.0, block_kbps: 0.0, rtt_median_ms: Some(11.4), rtt_samples: 80 };
        assert_eq!(r.videostats_line(), "VIDEOSTATS fps=29.5 rtt_mediano=11.40ms decod_media=6.25ms video_kbit_s=4900 blocos_fps=0.0 blocos_kbit_s=0 janela=20.0s rtt_amostras=80");
        let sem = WindowReport { rtt_median_ms: None, rtt_samples: 0, ..r };
        assert!(sem.videostats_line().contains("rtt_mediano=sem-amostras"));
    }
}
