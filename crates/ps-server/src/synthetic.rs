//! Fontes sintéticas de janela, para exercitar protocolo, codec e cliente sem compositor.
//!
//! - [`UiWindow`]: aparência de interface (fundo sólido, retângulos, relógio, barra de
//!   progresso e um quadrado que anda devagar). O dano real é pequeno: só o que muda.
//! - [`GradientWindow`]: degradê animado em tela cheia (dano = janela inteira).
//! - [`VideoWindow`]: janela de vídeo do marco 4 (região animada de 640×384 e o resto parado).
//! - [`StripesWindow`]: janela de ruído com marcadores de quadro, para provar a apresentação atômica (`docs/20`).

#![allow(clippy::too_many_arguments)] // rotinas de desenho com coordenadas explícitas

use ps_codec::Rect;

pub const MIN_SIZE: u32 = 64;
pub const MAX_SIZE: u32 = 4096;

/// Conteúdo de uma janela sintética.
pub trait Content: Send {
    fn title(&self) -> &str;
    fn app_id(&self) -> &str;
    fn size(&self) -> (u32, u32);
    /// Muda o tamanho (já limitado a `MIN_SIZE..=MAX_SIZE`). O próximo `tick` devolve dano total.
    fn resize(&mut self, width: u32, height: u32);
    /// Quadro atual, BGRX, stride = largura × 4.
    fn pixels(&self) -> &[u8];
    /// Avança o conteúdo até `now_ms` (milissegundos desde o início da sessão) e devolve o dano.
    fn tick(&mut self, now_ms: u64) -> Vec<Rect>;
}

/// Limita um tamanho pedido ao intervalo aceito.
pub fn clamp_size(w: u32, h: u32) -> (u32, u32) {
    (w.clamp(MIN_SIZE, MAX_SIZE), h.clamp(MIN_SIZE, MAX_SIZE))
}


// ---------------------------------------------------------------- fonte

use crate::source::{SourceEvent, Waker, WindowSource};
use ps_proto::Message;
use std::time::Duration;

/// [`WindowSource`] com duas janelas sintéticas: interface (1) e degradê (2).
pub struct SyntheticSource {
    windows: Vec<(u32, Box<dyn Content>)>,
    queued: Vec<SourceEvent>,
    period_ms: u64,
    next_tick_ms: u64,
}

impl SyntheticSource {
    pub fn new(fps: u32, ui_size: (u32, u32), gradient_size: (u32, u32)) -> Self {
        SyntheticSource {
            windows: vec![
                (1, Box::new(UiWindow::new(ui_size.0, ui_size.1))),
                (2, Box::new(GradientWindow::new(gradient_size.0, gradient_size.1))),
            ],
            queued: Vec::new(),
            period_ms: (1000 / u64::from(fps.max(1))).max(1),
            next_tick_ms: 0,
        }
    }
}

impl SyntheticSource {
    /// Fonte do marco 4: uma só janela (id 1), 1280×720, com a região animada a 30 quadros por
    /// segundo. `pause_after_ms`: a animação para nesse instante (para testar a volta ao modo sem perdas).
    pub fn video(pause_after_ms: Option<u64>) -> Self {
        let (w, h) = (ps_codec::testutil::video::WIDTH, ps_codec::testutil::video::HEIGHT);
        SyntheticSource {
            windows: vec![(1, Box::new(VideoWindow::new(w, h, pause_after_ms)))],
            queued: Vec::new(),
            period_ms: ps_codec::testutil::video::FRAME_MS,
            next_tick_ms: 0,
        }
    }
}

impl SyntheticSource {
    /// Fonte da apresentação atômica (`docs/20`): uma só janela (id 1), 960×600, ruído que não comprime e
    /// marcadores de quadro, a 30 quadros por segundo.
    pub fn stripes() -> Self {
        let (w, h) = (ps_codec::testutil::stripes::WIDTH, ps_codec::testutil::stripes::HEIGHT);
        SyntheticSource {
            windows: vec![(1, Box::new(StripesWindow::new(w, h)))],
            queued: Vec::new(),
            period_ms: ps_codec::testutil::stripes::FRAME_MS,
            next_tick_ms: 0,
        }
    }
}

impl WindowSource for SyntheticSource {
    fn start(&mut self, _waker: Waker) -> Vec<SourceEvent> {
        self.windows
            .iter()
            .map(|(id, c)| {
                let (width, height) = c.size();
                SourceEvent::Created { win: *id, width, height, title: c.title().to_string(), app_id: c.app_id().to_string() }
            })
            .collect()
    }

    fn poll(&mut self, now_ms: u64) -> Vec<SourceEvent> {
        let mut out = std::mem::take(&mut self.queued);
        if now_ms >= self.next_tick_ms {
            for (id, c) in self.windows.iter_mut() {
                let rects = c.tick(now_ms);
                if !rects.is_empty() {
                    out.push(SourceEvent::Damage { win: *id, rects });
                }
            }
            self.next_tick_ms += self.period_ms;
            if self.next_tick_ms < now_ms {
                self.next_tick_ms = now_ms + self.period_ms; // atrasado: não recupera quadros perdidos
            }
        }
        out
    }

    fn next_poll_in(&self, now_ms: u64) -> Option<Duration> {
        if !self.queued.is_empty() {
            return Some(Duration::ZERO);
        }
        Some(Duration::from_millis(self.next_tick_ms.saturating_sub(now_ms)))
    }

    fn with_pixels(&mut self, win: u32, f: &mut dyn FnMut(u32, u32, &[u8])) {
        if let Some((_, c)) = self.windows.iter().find(|(id, _)| *id == win) {
            let (w, h) = c.size();
            f(w, h, c.pixels());
        }
    }

    fn input(&mut self, msg: &Message) {
        match *msg {
            Message::ResizeReq { win, width, height } => {
                if let Some((_, c)) = self.windows.iter_mut().find(|(id, _)| *id == win) {
                    let (nw, nh) = clamp_size(width, height);
                    if (nw, nh) != c.size() {
                        c.resize(nw, nh);
                        self.queued.push(SourceEvent::Resized { win, width: nw, height: nh });
                    }
                }
            }
            Message::CloseReq { win } => {
                if let Some(i) = self.windows.iter().position(|(id, _)| *id == win) {
                    self.windows.remove(i);
                    self.queued.push(SourceEvent::Destroyed { win });
                }
            }
            _ => {}
        }
    }

    fn frame_sent(&mut self, _win: u32) {}
}

// ---------------------------------------------------------------- desenho

type Bgrx = [u8; 4];

const fn cor(r: u8, g: u8, b: u8) -> Bgrx {
    [b, g, r, 0xFF]
}

/// Preenche um retângulo (coordenadas com sinal, recortado ao quadro).
fn fill_rect(buf: &mut [u8], w: u32, h: u32, x: i32, y: i32, rw: i32, rh: i32, c: Bgrx) {
    let x0 = x.clamp(0, w as i32) as usize;
    let y0 = y.clamp(0, h as i32) as usize;
    let x1 = x.saturating_add(rw).clamp(0, w as i32) as usize;
    let y1 = y.saturating_add(rh).clamp(0, h as i32) as usize;
    if x1 <= x0 {
        return;
    }
    for yy in y0..y1 {
        let row = &mut buf[(yy * w as usize + x0) * 4..(yy * w as usize + x1) * 4];
        for px in row.chunks_exact_mut(4) {
            px.copy_from_slice(&c);
        }
    }
}

/// Moldura de 2 px.
fn stroke_rect(buf: &mut [u8], w: u32, h: u32, x: i32, y: i32, rw: i32, rh: i32, c: Bgrx) {
    fill_rect(buf, w, h, x, y, rw, 2, c);
    fill_rect(buf, w, h, x, y + rh - 2, rw, 2, c);
    fill_rect(buf, w, h, x, y, 2, rh, c);
    fill_rect(buf, w, h, x + rw - 2, y, 2, rh, c);
}

/// Fonte 5×7: cada linha são 5 bits, o bit 4 é a coluna da esquerda.
fn glyph(ch: char) -> [u8; 7] {
    match ch {
        '0' => [0b01110, 0b10001, 0b10011, 0b10101, 0b11001, 0b10001, 0b01110],
        '1' => [0b00100, 0b01100, 0b00100, 0b00100, 0b00100, 0b00100, 0b01110],
        '2' => [0b01110, 0b10001, 0b00001, 0b00010, 0b00100, 0b01000, 0b11111],
        '3' => [0b11110, 0b00001, 0b00001, 0b01110, 0b00001, 0b00001, 0b11110],
        '4' => [0b00010, 0b00110, 0b01010, 0b10010, 0b11111, 0b00010, 0b00010],
        '5' => [0b11111, 0b10000, 0b11110, 0b00001, 0b00001, 0b10001, 0b01110],
        '6' => [0b00110, 0b01000, 0b10000, 0b11110, 0b10001, 0b10001, 0b01110],
        '7' => [0b11111, 0b00001, 0b00010, 0b00100, 0b01000, 0b01000, 0b01000],
        '8' => [0b01110, 0b10001, 0b10001, 0b01110, 0b10001, 0b10001, 0b01110],
        '9' => [0b01110, 0b10001, 0b10001, 0b01111, 0b00001, 0b00010, 0b01100],
        '.' => [0, 0, 0, 0, 0, 0b01100, 0b01100],
        ':' => [0, 0b01100, 0b01100, 0, 0b01100, 0b01100, 0],
        _ => [0; 7],
    }
}

fn text_width(text: &str, scale: i32) -> i32 {
    text.chars().count() as i32 * 6 * scale
}

#[allow(clippy::too_many_arguments)]
fn draw_text(buf: &mut [u8], w: u32, h: u32, x: i32, y: i32, scale: i32, text: &str, c: Bgrx) {
    let mut cx = x;
    for ch in text.chars() {
        let g = glyph(ch);
        for (row, bits) in g.iter().enumerate() {
            for col in 0..5 {
                if bits & (0b10000 >> col) != 0 {
                    fill_rect(buf, w, h, cx + col * scale, y + row as i32 * scale, scale, scale, c);
                }
            }
        }
        cx += 6 * scale;
    }
}

fn full_rect(w: u32, h: u32) -> Rect {
    Rect::new(0, 0, w, h)
}

fn to_rect(x: i32, y: i32, rw: i32, rh: i32, w: u32, h: u32) -> Option<Rect> {
    let x0 = x.clamp(0, w as i32);
    let y0 = y.clamp(0, h as i32);
    let x1 = x.saturating_add(rw).clamp(0, w as i32);
    let y1 = y.saturating_add(rh).clamp(0, h as i32);
    if x1 > x0 && y1 > y0 {
        Some(Rect::new(x0 as u32, y0 as u32, (x1 - x0) as u32, (y1 - y0) as u32))
    } else {
        None
    }
}

// ---------------------------------------------------------------- janela de interface

const BG: Bgrx = cor(0x2B, 0x2B, 0x30);
const BAR: Bgrx = cor(0x1E, 0x1E, 0x22);
const SIDE: Bgrx = cor(0x24, 0x24, 0x29);
const TRACK: Bgrx = cor(0x40, 0x40, 0x48);
const FILL: Bgrx = cor(0x4C, 0xAF, 0x50);
const TEXT: Bgrx = cor(0xE6, 0xE6, 0xE6);
const SQUARE: Bgrx = cor(0xFF, 0xB3, 0x00);
const CARDS: [Bgrx; 4] = [cor(0x3A, 0x5F, 0xA8), cor(0x8E, 0x44, 0x9E), cor(0x2E, 0x7D, 0x6B), cor(0xB5, 0x58, 0x3F)];

const CLOCK_SCALE: i32 = 3;
const CLOCK_CHARS: i32 = 7;
const BAR_PERIOD_MS: u64 = 8000;
const SQUARE_SIZE: i32 = 16;
const SQUARE_SPEED_PX_S: u64 = 60;

struct UiLayout {
    clock: (i32, i32, i32, i32),
    track: (i32, i32, i32, i32),
    lane: (i32, i32, i32, i32),
}

/// Janela com aparência de interface: quase tudo parado, três pontos pequenos em movimento.
pub struct UiWindow {
    w: u32,
    h: u32,
    buf: Vec<u8>,
    needs_full: bool,
    last_clock: String,
    last_fill: i32,
    last_square: i32,
}

impl UiWindow {
    pub fn new(width: u32, height: u32) -> Self {
        let (w, h) = clamp_size(width, height);
        UiWindow {
            w,
            h,
            buf: vec![0; (w * h * 4) as usize],
            needs_full: true,
            last_clock: String::new(),
            last_fill: 0,
            last_square: 0,
        }
    }

    fn layout(&self) -> UiLayout {
        let (w, h) = (self.w as i32, self.h as i32);
        let side = (w / 4).min(120);
        let tx = side + 16;
        let tw = (w - tx - 16).max(0);
        let clock_w = CLOCK_CHARS * 6 * CLOCK_SCALE;
        UiLayout {
            clock: (w - 12 - clock_w, 5, clock_w, 7 * CLOCK_SCALE),
            track: (tx, h - 36, tw, 16),
            lane: (tx, h - 72, tw, 24),
        }
    }

    fn square_x(&self, now_ms: u64, lane_w: i32) -> i32 {
        let span = (lane_w - SQUARE_SIZE).max(0) as u64;
        if span == 0 {
            return 0;
        }
        // Onda triangular: vai e volta.
        let d = now_ms * SQUARE_SPEED_PX_S / 1000;
        let m = d % (2 * span);
        (if m <= span { m } else { 2 * span - m }) as i32
    }

    fn fill_for(&self, now_ms: u64, track_w: i32) -> i32 {
        (i64::from(track_w) * (now_ms % BAR_PERIOD_MS) as i64 / BAR_PERIOD_MS as i64) as i32
    }

    fn draw_static(&mut self) {
        let l = self.layout();
        let (w, h) = (self.w, self.h);
        let (wi, hi) = (w as i32, h as i32);
        let b = &mut self.buf;
        fill_rect(b, w, h, 0, 0, wi, hi, BG);
        fill_rect(b, w, h, 0, 0, wi, 32, BAR);
        for i in 0..3 {
            fill_rect(b, w, h, 10 + i * 20, 10, 12, 12, CARDS[i as usize]);
        }
        let side = (wi / 4).min(120);
        fill_rect(b, w, h, 0, 32, side, hi - 32, SIDE);
        for i in 0..6 {
            fill_rect(b, w, h, 12, 48 + i * 28, (side - 24).max(0), 16, TRACK);
        }
        // Cartões 2×2 na área de conteúdo.
        let cx = side + 16;
        let cy = 48;
        let area_w = (wi - cx - 16).max(0);
        let area_h = (hi - 72 - 16 - cy).max(0);
        let cw = (area_w - 16) / 2;
        let ch = (area_h - 16) / 2;
        for (i, &c) in CARDS.iter().enumerate() {
            let (col, row) = ((i % 2) as i32, (i / 2) as i32);
            let x = cx + col * (cw + 16);
            let y = cy + row * (ch + 16);
            fill_rect(b, w, h, x, y, cw, ch, c);
            stroke_rect(b, w, h, x, y, cw, ch, TEXT);
            fill_rect(b, w, h, x + 12, y + 12, (cw - 24).max(0) / 2, 8, TEXT);
        }
        fill_rect(b, w, h, l.lane.0, l.lane.1, l.lane.2, l.lane.3, SIDE);
        fill_rect(b, w, h, l.track.0, l.track.1, l.track.2, l.track.3, TRACK);
    }

    fn draw_clock(&mut self, text: &str) {
        let l = self.layout();
        let (w, h) = (self.w, self.h);
        let (x, y, cw, ch) = l.clock;
        fill_rect(&mut self.buf, w, h, x, y, cw, ch, BAR);
        draw_text(&mut self.buf, w, h, x + cw - text_width(text, CLOCK_SCALE), y, CLOCK_SCALE, text, TEXT);
    }
}

impl Content for UiWindow {
    fn title(&self) -> &str {
        "PiStick — interface sintética"
    }

    fn app_id(&self) -> &str {
        "org.pistick.sintetico.ui"
    }

    fn size(&self) -> (u32, u32) {
        (self.w, self.h)
    }

    fn resize(&mut self, width: u32, height: u32) {
        let (w, h) = clamp_size(width, height);
        self.w = w;
        self.h = h;
        self.buf.clear();
        self.buf.resize((w * h * 4) as usize, 0);
        self.needs_full = true;
    }

    fn pixels(&self) -> &[u8] {
        &self.buf
    }

    fn tick(&mut self, now_ms: u64) -> Vec<Rect> {
        let (w, h) = (self.w, self.h);
        let l = self.layout();
        // Relógio: décimos de segundo, alinhado à direita em 7 posições.
        let clock = format!("{:7.1}", now_ms as f64 / 1000.0);
        let fill = self.fill_for(now_ms, l.track.2);
        let sq = self.square_x(now_ms, l.lane.2);

        if self.needs_full {
            self.needs_full = false;
            self.draw_static();
            self.draw_clock(&clock);
            fill_rect(&mut self.buf, w, h, l.track.0, l.track.1, fill, l.track.3, FILL);
            fill_rect(&mut self.buf, w, h, l.lane.0 + sq, l.lane.1 + 4, SQUARE_SIZE, SQUARE_SIZE, SQUARE);
            self.last_clock = clock;
            self.last_fill = fill;
            self.last_square = sq;
            return vec![full_rect(w, h)];
        }

        let mut dano = Vec::new();
        if clock != self.last_clock {
            self.draw_clock(&clock);
            self.last_clock = clock;
            dano.extend(to_rect(l.clock.0, l.clock.1, l.clock.2, l.clock.3, w, h));
        }
        if fill != self.last_fill {
            let (tx, ty, _, th) = l.track;
            if fill > self.last_fill {
                fill_rect(&mut self.buf, w, h, tx + self.last_fill, ty, fill - self.last_fill, th, FILL);
                dano.extend(to_rect(tx + self.last_fill, ty, fill - self.last_fill, th, w, h));
            } else {
                // A barra reiniciou: limpa a trilha inteira.
                fill_rect(&mut self.buf, w, h, tx, ty, l.track.2, th, TRACK);
                fill_rect(&mut self.buf, w, h, tx, ty, fill, th, FILL);
                dano.extend(to_rect(tx, ty, l.track.2, th, w, h));
            }
            self.last_fill = fill;
        }
        if sq != self.last_square {
            let (lx, ly, lw, _) = l.lane;
            let (old, new) = (self.last_square, sq);
            let x0 = old.min(new);
            let x1 = old.max(new) + SQUARE_SIZE;
            fill_rect(&mut self.buf, w, h, lx + x0, ly + 4, x1 - x0, SQUARE_SIZE, SIDE);
            fill_rect(&mut self.buf, w, h, lx + new, ly + 4, SQUARE_SIZE, SQUARE_SIZE, SQUARE);
            let _ = lw;
            dano.extend(to_rect(lx + x0, ly + 4, x1 - x0, SQUARE_SIZE, w, h));
            self.last_square = sq;
        }
        dano
    }
}

// ---------------------------------------------------------------- degradê animado

/// Degradê em tela cheia que se move a cada quadro. O dano é a janela inteira.
pub struct GradientWindow {
    w: u32,
    h: u32,
    buf: Vec<u8>,
    /// Fase do último quadro desenhado; `None` depois de criar ou redimensionar.
    last_phase: Option<usize>,
}

impl GradientWindow {
    pub fn new(width: u32, height: u32) -> Self {
        let (w, h) = clamp_size(width, height);
        GradientWindow { w, h, buf: vec![0; (w * h * 4) as usize], last_phase: None }
    }
}

impl Content for GradientWindow {
    fn title(&self) -> &str {
        "PiStick — degradê animado"
    }

    fn app_id(&self) -> &str {
        "org.pistick.sintetico.degrade"
    }

    fn size(&self) -> (u32, u32) {
        (self.w, self.h)
    }

    fn resize(&mut self, width: u32, height: u32) {
        let (w, h) = clamp_size(width, height);
        self.w = w;
        self.h = h;
        self.buf.clear();
        self.buf.resize((w * h * 4) as usize, 0);
        self.last_phase = None;
    }

    fn pixels(&self) -> &[u8] {
        &self.buf
    }

    fn tick(&mut self, now_ms: u64) -> Vec<Rect> {
        let (w, h) = (self.w as usize, self.h as usize);
        let phase = (now_ms / 8) as usize;
        if self.last_phase == Some(phase) {
            return Vec::new(); // nada mudou desde o quadro anterior
        }
        self.last_phase = Some(phase);
        // Azul varia na horizontal (suave), verde em faixas na vertical, vermelho em diagonal lenta.
        let blue: Vec<u8> = (0..w).map(|x| ((x * 255 / w + phase) & 0xFF) as u8).collect();
        for y in 0..h {
            let g = (((y * 255 / h) + phase / 2) & 0xF0) as u8;
            let r = (((y + phase) / 4) & 0xFF) as u8;
            let row = &mut self.buf[y * w * 4..(y + 1) * w * 4];
            for (px, &b) in row.chunks_exact_mut(4).zip(blue.iter()) {
                px[0] = b;
                px[1] = g;
                px[2] = r;
                px[3] = 0xFF;
            }
        }
        vec![full_rect(self.w, self.h)]
    }
}

// ---------------------------------------------------------------- vídeo sintético

/// Janela de vídeo (marco 4): região animada de 640×384 em (64, 64) que muda a cada quadro, e o
/// resto parado, com texto e linhas finas. O conteúdo vem de [`ps_codec::testutil::video`], o mesmo
/// gerador que os testes usam como origem.
pub struct VideoWindow {
    w: u32,
    h: u32,
    buf: Vec<u8>,
    needs_full: bool,
    last_frame: Option<u64>,
    pause_after_ms: Option<u64>,
}

impl VideoWindow {
    pub fn new(width: u32, height: u32, pause_after_ms: Option<u64>) -> Self {
        let (w, h) = clamp_size(width, height);
        VideoWindow { w, h, buf: vec![0; (w * h * 4) as usize], needs_full: true, last_frame: None, pause_after_ms }
    }
}

impl Content for VideoWindow {
    fn title(&self) -> &str {
        "PiStick — vídeo sintético"
    }

    fn app_id(&self) -> &str {
        "org.pistick.sintetico.video"
    }

    fn size(&self) -> (u32, u32) {
        (self.w, self.h)
    }

    fn resize(&mut self, width: u32, height: u32) {
        let (w, h) = clamp_size(width, height);
        self.w = w;
        self.h = h;
        self.buf.clear();
        self.buf.resize((w * h * 4) as usize, 0);
        self.needs_full = true;
    }

    fn pixels(&self) -> &[u8] {
        &self.buf
    }

    fn tick(&mut self, now_ms: u64) -> Vec<Rect> {
        use ps_codec::testutil::video;
        let frame = video::frame_at(now_ms, self.pause_after_ms);
        if self.needs_full {
            self.needs_full = false;
            video::draw_static(&mut self.buf, self.w, self.h);
            video::draw_region(&mut self.buf, self.w, self.h, frame);
            self.last_frame = Some(frame);
            return vec![full_rect(self.w, self.h)];
        }
        if self.last_frame == Some(frame) {
            return Vec::new(); // parado (ou ainda no mesmo quadro da animação)
        }
        self.last_frame = Some(frame);
        video::draw_region(&mut self.buf, self.w, self.h, frame);
        video::region_in(self.w, self.h).into_iter().collect()
    }
}

// ---------------------------------------------------------------- listras (apresentação atômica)

/// Janela de ruído com marcadores de quadro (veja [`ps_codec::testutil::stripes`]): a cada `tick` é um quadro
/// novo, com o número do quadro em cada faixa de 32 linhas, e o dano é a janela inteira.
pub struct StripesWindow {
    w: u32,
    h: u32,
    buf: Vec<u8>,
    frame: u32,
}

impl StripesWindow {
    pub fn new(width: u32, height: u32) -> Self {
        let (w, h) = clamp_size(width, height);
        StripesWindow { w, h, buf: vec![0; (w * h * 4) as usize], frame: 0 }
    }
}

impl Content for StripesWindow {
    fn title(&self) -> &str {
        "PiStick — listras sintéticas"
    }

    fn app_id(&self) -> &str {
        "org.pistick.sintetico.listras"
    }

    fn size(&self) -> (u32, u32) {
        (self.w, self.h)
    }

    fn resize(&mut self, width: u32, height: u32) {
        let (w, h) = clamp_size(width, height);
        self.w = w;
        self.h = h;
        self.buf.clear();
        self.buf.resize((w * h * 4) as usize, 0);
    }

    fn pixels(&self) -> &[u8] {
        &self.buf
    }

    fn tick(&mut self, _now_ms: u64) -> Vec<Rect> {
        self.frame += 1;
        ps_codec::testutil::stripes::render_into(&mut self.buf, self.w, self.h, self.frame);
        vec![full_rect(self.w, self.h)]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn aplica_dano(copia: &mut [u8], novo: &[u8], w: u32, dano: &[Rect]) {
        for r in dano {
            for y in r.y..r.y + r.h {
                let a = ((y * w + r.x) * 4) as usize;
                let b = ((y * w + r.x + r.w) * 4) as usize;
                copia[a..b].copy_from_slice(&novo[a..b]);
            }
        }
    }

    /// O dano declarado deve cobrir toda mudança de pixel: copiar só o dano reproduz o quadro novo.
    fn confere_dano(mut c: impl Content) {
        let (w, h) = c.size();
        let mut copia = vec![0u8; (w * h * 4) as usize];
        let primeiro = c.tick(0);
        assert_eq!(primeiro, vec![full_rect(w, h)]);
        copia.copy_from_slice(c.pixels());
        let mut total_dano = 0u64;
        for t in (33..12_000).step_by(33) {
            let dano = c.tick(t);
            for r in &dano {
                assert!(r.x + r.w <= w && r.y + r.h <= h, "dano fora do quadro: {r:?}");
                total_dano += u64::from(r.w) * u64::from(r.h);
            }
            aplica_dano(&mut copia, c.pixels(), w, &dano);
            assert_eq!(copia, c.pixels(), "dano não cobre a mudança em t={t}");
        }
        let _ = total_dano;
    }

    #[test]
    fn dano_da_interface_cobre_as_mudancas() {
        confere_dano(UiWindow::new(640, 420));
        confere_dano(UiWindow::new(64, 64));
        confere_dano(UiWindow::new(200, 100));
    }

    #[test]
    fn interface_tem_dano_pequeno() {
        let mut ui = UiWindow::new(640, 420);
        ui.tick(0);
        let mut max = 0u64;
        for t in (33..8000).step_by(33) {
            let area: u64 = ui.tick(t).iter().map(|r| u64::from(r.w) * u64::from(r.h)).sum();
            max = max.max(area);
        }
        // O reinício da barra repinta a trilha; o resto é ainda menor. Sempre bem abaixo de 10% da janela.
        assert!(max < 640 * 420 / 10, "dano máximo {max}");
    }

    #[test]
    fn degrade_muda_tudo() {
        let mut g = GradientWindow::new(100, 80);
        g.tick(0);
        let a = g.pixels().to_vec();
        let d = g.tick(500);
        assert_eq!(d, vec![Rect::new(0, 0, 100, 80)]);
        assert_ne!(a, g.pixels());
        // Na mesma fase o conteúdo não muda e não há dano.
        assert!(g.tick(503).is_empty());
    }

    #[test]
    fn redimensionar_gera_dano_total_e_novo_tamanho() {
        let mut ui = UiWindow::new(300, 200);
        ui.tick(0);
        ui.resize(500, 333);
        assert_eq!(ui.size(), (500, 333));
        assert_eq!(ui.pixels().len(), 500 * 333 * 4);
        assert_eq!(ui.tick(100), vec![Rect::new(0, 0, 500, 333)]);
        ui.resize(1, 1); // abaixo do mínimo: limitado
        assert_eq!(ui.size(), (MIN_SIZE, MIN_SIZE));
        ui.tick(200);
    }

    #[test]
    fn janela_de_video_reproduz_o_gerador_compartilhado_e_o_dano_cobre_as_mudancas() {
        use ps_codec::testutil::video;
        let mut v = VideoWindow::new(video::WIDTH, video::HEIGHT, Some(2_000));
        assert_eq!(v.tick(0), vec![Rect::new(0, 0, 1280, 720)]);
        assert_eq!(v.pixels(), video::render(1280, 720, 0).as_slice());
        let mut copia = v.pixels().to_vec();
        for t in (33..4_000).step_by(33) {
            let dano = v.tick(t);
            let esperado = video::render(1280, 720, video::frame_at(t, Some(2_000)));
            aplica_dano(&mut copia, v.pixels(), 1280, &dano);
            assert_eq!(copia, esperado, "t = {t}");
            if video::frame_at(t, Some(2_000)) == video::frame_at(t - 33, Some(2_000)) {
                assert!(dano.is_empty(), "sem quadro novo (depois da pausa) não há dano (t = {t})");
            } else {
                assert_eq!(dano, vec![video::REGION], "t = {t}");
            }
        }
    }

    #[test]
    fn janela_de_video_redimensionada_volta_a_dano_total() {
        let mut v = VideoWindow::new(1280, 720, None);
        v.tick(0);
        v.resize(800, 500);
        assert_eq!(v.size(), (800, 500));
        assert_eq!(v.tick(33), vec![Rect::new(0, 0, 800, 500)]);
        assert_eq!(v.pixels(), ps_codec::testutil::video::render(800, 500, 1).as_slice());
    }

    #[test]
    fn janela_de_listras_numera_os_quadros_e_tem_dano_total() {
        use ps_codec::testutil::stripes;
        let mut s = StripesWindow::new(stripes::WIDTH, stripes::HEIGHT);
        for esperado in 1..=3u32 {
            assert_eq!(s.tick(0), vec![Rect::new(0, 0, 960, 600)]);
            assert_eq!(stripes::read_markers(s.pixels(), 960, 600), vec![esperado; 19]);
        }
        let mut f = SyntheticSource::stripes();
        let ev = f.start(std::sync::Arc::new(|| {}));
        assert!(matches!(&ev[0], SourceEvent::Created { win: 1, width: 960, height: 600, .. }));
    }

    #[test]
    fn fonte_de_video_tem_uma_janela_de_1280_por_720() {
        let mut s = SyntheticSource::video(None);
        let ev = s.start(std::sync::Arc::new(|| {}));
        assert_eq!(ev.len(), 1);
        assert!(matches!(&ev[0], SourceEvent::Created { win: 1, width: 1280, height: 720, .. }));
    }
}
