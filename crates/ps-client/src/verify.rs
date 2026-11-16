
use ps_codec::testutil::video;
use ps_codec::Rect;

pub const SEARCH_RADIUS: u64 = 150;

#[derive(Debug, Clone, Copy)]
pub enum Area {
    All,
    Inside(Rect),
    Outside(Rect),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct DiffInfo {
    pub count: u64,
    pub bbox: Option<Rect>,
}

impl DiffInfo {
    fn add(&mut self, x: u32, y: u32) {
        self.count += 1;
        self.bbox = Some(match self.bbox {
            None => Rect::new(x, y, 1, 1),
            Some(r) => {
                let (x0, y0) = (r.x.min(x), r.y.min(y));
                let (x1, y1) = ((r.x + r.w).max(x + 1), (r.y + r.h).max(y + 1));
                Rect::new(x0, y0, x1 - x0, y1 - y0)
            }
        });
    }

    fn merge(self, o: DiffInfo) -> DiffInfo {
        let bbox = match (self.bbox, o.bbox) {
            (Some(a), Some(b)) => {
                let (x0, y0) = (a.x.min(b.x), a.y.min(b.y));
                let (x1, y1) = ((a.x + a.w).max(b.x + b.w), (a.y + a.h).max(b.y + b.h));
                Some(Rect::new(x0, y0, x1 - x0, y1 - y0))
            }
            (a, b) => a.or(b),
        };
        DiffInfo { count: self.count + o.count, bbox }
    }
}

fn spans(area: Area, y: u32, w: u32) -> [(u32, u32); 2] {
    let clip = |r: Rect| (r.x.min(w), (r.x.saturating_add(r.w)).min(w));
    let in_rows = |r: Rect| y >= r.y && y < r.y.saturating_add(r.h);
    match area {
        Area::All => [(0, w), (0, 0)],
        Area::Inside(r) if in_rows(r) => [clip(r), (0, 0)],
        Area::Inside(_) => [(0, 0), (0, 0)],
        Area::Outside(r) if in_rows(r) => {
            let (x0, x1) = clip(r);
            [(0, x0), (x1, w)]
        }
        Area::Outside(_) => [(0, w), (0, 0)],
    }
}

pub fn diff_count(a: &[u8], b: &[u8], w: u32, h: u32, area: Area) -> DiffInfo {
    assert!(a.len() >= (w as usize) * (h as usize) * 4 && b.len() >= (w as usize) * (h as usize) * 4, "buffers menores que {w}x{h}");
    let mut d = DiffInfo::default();
    for y in 0..h {
        for (x0, x1) in spans(area, y, w) {
            for x in x0..x1 {
                let i = (y as usize * w as usize + x as usize) * 4;
                if a[i..i + 3] != b[i..i + 3] {
                    d.add(x, y);
                }
            }
        }
    }
    d
}

pub fn psnr_rect(a: &[u8], b: &[u8], w: u32, r: Rect) -> f64 {
    let (mut se, mut n) = (0f64, 0f64);
    for y in r.y..r.y + r.h {
        let i0 = (y as usize * w as usize + r.x as usize) * 4;
        let i1 = (y as usize * w as usize + (r.x + r.w) as usize) * 4;
        for (p, q) in a[i0..i1].as_chunks::<4>().0.iter().zip(b[i0..i1].as_chunks::<4>().0.iter()) {
            for k in 0..3 {
                let d = f64::from(p[k]) - f64::from(q[k]);
                se += d * d;
            }
            n += 3.0;
        }
    }
    if se == 0.0 || n == 0.0 {
        return f64::INFINITY;
    }
    10.0 * (255.0f64 * 255.0 / (se / n)).log10()
}

fn static_image(w: u32, h: u32) -> Vec<u8> {
    let mut buf = vec![0u8; w as usize * h as usize * 4];
    video::draw_static(&mut buf, w, h);
    buf
}

fn candidates(center: u64, radius: u64) -> impl Iterator<Item = u64> {
    std::iter::once(center).chain((1..=radius).flat_map(move |k| [center.checked_sub(k), center.checked_add(k)].into_iter().flatten()))
}

pub fn frame_from_capture_us(capture_us: u64) -> u64 {
    video::frame_at(capture_us / 1000, None)
}

#[derive(Debug, Clone, PartialEq)]
pub struct VideoReport {
    pub ok: bool,
    pub frame: u64,
    pub psnr: f64,
    pub outside_diff: DiffInfo,
    pub problem: Option<String>,
}

pub fn check_video(img: &[u8], w: u32, h: u32, active: Option<Rect>, center: Option<u64>, radius: u64, min_db: f64) -> VideoReport {
    let mut r = VideoReport { ok: false, frame: 0, psnr: f64::NEG_INFINITY, outside_diff: DiffInfo::default(), problem: None };
    let Some(active) = active else {
        r.problem = Some("não há região de vídeo ativa".into());
        return r;
    };
    let Some(expected) = video::region_in(w, h) else {
        r.problem = Some(format!("a janela {w}x{h} não alcança a região animada do gerador"));
        return r;
    };
    if active != expected {
        r.problem = Some(format!("a região ativa {}x{}+{}+{} difere da região animada do gerador {}x{}+{}+{}", active.w, active.h, active.x, active.y, expected.w, expected.h, expected.x, expected.y));
    }
    let mut origin = static_image(w, h);
    r.outside_diff = diff_count(img, &origin, w, h, Area::Outside(active));
    let Some(center) = center else {
        r.problem.get_or_insert_with(|| "nenhum quadro recebido: não há como estimar o número do quadro".into());
        return r;
    };
    let mut best = (center, f64::NEG_INFINITY);
    for n in candidates(center, radius) {
        video::draw_region(&mut origin, w, h, n);
        let p = psnr_rect(&origin, img, w, active);
        if p > best.1 {
            best = (n, p);
        }
        if p.is_infinite() {
            break;
        }
    }
    (r.frame, r.psnr) = best;
    r.ok = r.problem.is_none() && r.outside_diff.count == 0 && r.psnr >= min_db;
    r
}

#[derive(Debug, Clone, PartialEq)]
pub struct StillReport {
    pub ok: bool,
    pub frame: u64,
    pub diff: DiffInfo,
    pub problem: Option<String>,
}

pub fn check_still(img: &[u8], w: u32, h: u32, video_active: bool, center: Option<u64>, radius: u64) -> StillReport {
    let mut r = StillReport { ok: false, frame: 0, diff: DiffInfo::default(), problem: None };
    if video_active {
        r.problem = Some("ainda há região de vídeo ativa".into());
        return r;
    }
    let Some(center) = center else {
        r.problem = Some("nenhum quadro recebido: não há como estimar o número do quadro".into());
        return r;
    };
    let mut origin = static_image(w, h);
    let region = video::region_in(w, h);
    let outside = match region {
        Some(reg) => diff_count(img, &origin, w, h, Area::Outside(reg)),
        None => diff_count(img, &origin, w, h, Area::All),
    };
    let mut best: Option<(u64, DiffInfo)> = None;
    for n in candidates(center, radius) {
        let total = match region {
            Some(reg) => {
                video::draw_region(&mut origin, w, h, n);
                outside.merge(diff_count(img, &origin, w, h, Area::Inside(reg)))
            }
            None => outside,
        };
        if best.is_none_or(|(_, b)| total.count < b.count) {
            best = Some((n, total));
        }
        if total.count == 0 || region.is_none() {
            break;
        }
    }
    let (n, d) = best.unwrap_or((center, outside));
    r.frame = n;
    r.diff = d;
    r.ok = d.count == 0;
    r
}

#[cfg(test)]
mod tests {
    use super::*;

    const W: u32 = video::WIDTH;
    const H: u32 = video::HEIGHT;

    fn reg() -> Rect {
        video::REGION
    }

    fn ruido(img: &mut [u8], r: Rect, delta: u8) {
        for y in r.y..r.y + r.h {
            for x in r.x..r.x + r.w {
                let i = (y as usize * W as usize + x as usize) * 4;
                for k in 0..3 {
                    img[i + k] = img[i + k].saturating_add(delta);
                }
            }
        }
    }

    #[test]
    fn quadro_identico_passa() {
        let img = video::render(W, H, 123);
        let r = check_video(&img, W, H, Some(reg()), Some(123), 5, 30.0);
        assert!(r.ok, "{r:?}");
        assert_eq!(r.frame, 123);
        assert!(r.psnr.is_infinite());
        assert_eq!(r.outside_diff.count, 0);
        let s = check_still(&img, W, H, false, Some(123), 5);
        assert!(s.ok, "{s:?}");
        assert_eq!(s.frame, 123);
    }

    #[test]
    fn a_busca_acha_o_quadro_mesmo_com_o_centro_errado() {
        let img = video::render(W, H, 200);
        let r = check_video(&img, W, H, Some(reg()), Some(190), 30, 30.0);
        assert!(r.ok && r.frame == 200, "{r:?}");
        let s = check_still(&img, W, H, false, Some(207), 30);
        assert!(s.ok && s.frame == 200, "{s:?}");
        let s = check_still(&img, W, H, false, Some(300), 30);
        assert!(!s.ok, "{s:?}");
    }

    #[test]
    fn um_pixel_diferente_fora_da_regiao_reprova() {
        let mut img = video::render(W, H, 50);
        let i = (600 * W as usize + 900) * 4;
        img[i] ^= 1;
        let r = check_video(&img, W, H, Some(reg()), Some(50), 3, 30.0);
        assert!(!r.ok);
        assert_eq!(r.outside_diff.count, 1);
        assert_eq!(r.outside_diff.bbox, Some(Rect::new(900, 600, 1, 1)));
        assert!(r.psnr.is_infinite(), "a região continua idêntica");
        let s = check_still(&img, W, H, false, Some(50), 3);
        assert!(!s.ok);
        assert_eq!((s.diff.count, s.diff.bbox), (1, Some(Rect::new(900, 600, 1, 1))));
    }

    #[test]
    fn o_quarto_byte_nao_conta() {
        let mut img = video::render(W, H, 7);
        img[3] = 0;
        img[(600 * W as usize + 900) * 4 + 3] = 0;
        assert!(check_video(&img, W, H, Some(reg()), Some(7), 2, 30.0).ok);
        assert!(check_still(&img, W, H, false, Some(7), 2).ok);
    }

    #[test]
    fn regiao_com_ruido_leve_tem_psnr_finito_e_coerente() {
        let mut img = video::render(W, H, 80);
        ruido(&mut img, reg(), 2);
        let r = check_video(&img, W, H, Some(reg()), Some(80), 3, 30.0);
        assert!(r.ok, "{r:?}");
        assert_eq!(r.frame, 80);
        assert!(r.psnr.is_finite() && (41.5..45.0).contains(&r.psnr), "psnr {}", r.psnr);
        let mut img2 = video::render(W, H, 80);
        ruido(&mut img2, reg(), 20);
        let r2 = check_video(&img2, W, H, Some(reg()), Some(80), 3, 30.0);
        assert!(!r2.ok && r2.frame == 80 && (20.0..25.0).contains(&r2.psnr), "{r2:?}");
        assert!(r2.psnr < r.psnr);
        assert!(check_video(&img2, W, H, Some(reg()), Some(80), 3, 20.0).ok);
        let s = check_still(&img, W, H, false, Some(80), 3);
        assert!(!s.ok);
        assert_eq!(s.diff.bbox, Some(reg()), "{s:?}");
    }

    #[test]
    fn sem_regiao_ativa_ou_com_regiao_errada_reprova() {
        let img = video::render(W, H, 10);
        let r = check_video(&img, W, H, None, Some(10), 2, 30.0);
        assert!(!r.ok && r.problem.is_some());
        let menor = Rect::new(64, 64, 320, 192);
        let r = check_video(&img, W, H, Some(menor), Some(10), 2, 30.0);
        assert!(!r.ok && r.problem.as_deref().is_some_and(|p| p.contains("difere")), "{r:?}");
        assert!(check_video(&img, W, H, Some(reg()), None, 2, 30.0).problem.is_some());
        let s = check_still(&img, W, H, true, Some(10), 2);
        assert!(!s.ok && s.problem.is_some());
    }

    #[test]
    fn contagem_e_retangulo_das_diferencas() {
        let a = vec![0u8; 8 * 6 * 4];
        let mut b = a.clone();
        for (x, y) in [(2usize, 1usize), (5, 4)] {
            b[(y * 8 + x) * 4 + 1] = 9;
        }
        let d = diff_count(&a, &b, 8, 6, Area::All);
        assert_eq!((d.count, d.bbox), (2, Some(Rect::new(2, 1, 4, 4))));
        let r = Rect::new(2, 1, 2, 2);
        assert_eq!(diff_count(&a, &b, 8, 6, Area::Inside(r)).count, 1);
        assert_eq!(diff_count(&a, &b, 8, 6, Area::Outside(r)).bbox, Some(Rect::new(5, 4, 1, 1)));
        assert!(psnr_rect(&a, &b, 8, Rect::new(0, 0, 8, 6)).is_finite());
        assert!(psnr_rect(&a, &a, 8, Rect::new(0, 0, 8, 6)).is_infinite());
    }

    #[test]
    fn quadro_a_partir_do_capture_us() {
        assert_eq!(frame_from_capture_us(0), 0);
        assert_eq!(frame_from_capture_us(33_000), 1);
        assert_eq!(frame_from_capture_us(10_000_000), 10_000 / video::FRAME_MS);
    }
}
