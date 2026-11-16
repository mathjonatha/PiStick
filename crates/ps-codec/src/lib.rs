
#![allow(unknown_lints, clippy::chunks_exact_to_as_chunks, clippy::needless_late_init)]

use std::fmt;

pub mod lz;
#[doc(hidden)]
pub mod testutil;

pub const BLOCK: usize = 64;

pub const MODE_SKIP: u8 = 0;
pub const MODE_SOLID: u8 = 1;
pub const MODE_PALETTE: u8 = 2;
pub const MODE_LZ: u8 = 3;
pub const MODE_RAW: u8 = 4;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Rect {
    pub x: u32,
    pub y: u32,
    pub w: u32,
    pub h: u32,
}

impl Rect {
    pub const fn new(x: u32, y: u32, w: u32, h: u32) -> Self {
        Rect { x, y, w, h }
    }

    pub fn is_empty(&self) -> bool {
        self.w == 0 || self.h == 0
    }

    pub fn union(self, o: Rect) -> Rect {
        if self.is_empty() {
            return o;
        }
        if o.is_empty() {
            return self;
        }
        let x0 = self.x.min(o.x);
        let y0 = self.y.min(o.y);
        let x1 = self.x.saturating_add(self.w).max(o.x.saturating_add(o.w));
        let y1 = self.y.saturating_add(self.h).max(o.y.saturating_add(o.h));
        Rect { x: x0, y: y0, w: x1 - x0, h: y1 - y0 }
    }

    pub fn intersect(self, o: Rect) -> Option<Rect> {
        let x0 = self.x.max(o.x);
        let y0 = self.y.max(o.y);
        let x1 = self.x.saturating_add(self.w).min(o.x.saturating_add(o.w));
        let y1 = self.y.saturating_add(self.h).min(o.y.saturating_add(o.h));
        if x1 > x0 && y1 > y0 {
            Some(Rect { x: x0, y: y0, w: x1 - x0, h: y1 - y0 })
        } else {
            None
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DecodeError {
    Truncated,
    BadMode(u8),
    BadPalette(u8),
    BadPaletteIndex,
    BadLz(lz::LzError),
    TrailingData,
}

impl fmt::Display for DecodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DecodeError::Truncated => write!(f, "dados do codec truncados"),
            DecodeError::BadMode(m) => write!(f, "modo de bloco desconhecido: {m}"),
            DecodeError::BadPalette(n) => write!(f, "paleta com {n} cores"),
            DecodeError::BadPaletteIndex => write!(f, "índice de paleta fora do intervalo"),
            DecodeError::BadLz(e) => write!(f, "bloco LZ inválido: {e:?}"),
            DecodeError::TrailingData => write!(f, "bytes sobrando depois do último bloco"),
        }
    }
}

impl std::error::Error for DecodeError {}

#[inline]
fn blocks_across(n: usize) -> usize {
    n.div_ceil(BLOCK)
}

#[inline]
fn palette_bits(n: usize) -> usize {
    match n {
        0..=2 => 1,
        3..=4 => 2,
        _ => 4,
    }
}

pub const DEFAULT_PARALLEL_MIN_BLOCKS: usize = 8;

struct Scratch {
    bgr: Vec<u8>,
    idx: Vec<u8>,
    lz_out: Vec<u8>,
    table: Vec<u16>,
}

#[inline(always)]
fn px_color(p: &[u8]) -> u32 {
    u32::from_le_bytes([p[0], p[1], p[2], p[3]]) & 0x00FF_FFFF
}

impl Scratch {
    fn new() -> Self {
        Scratch {
            bgr: Vec::with_capacity(BLOCK * BLOCK * 3),
            lz_out: Vec::with_capacity(BLOCK * BLOCK * 3 + 64),
            table: vec![0u16; lz::HASH_SIZE],
            idx: Vec::with_capacity(BLOCK * BLOCK),
        }
    }

    fn gather(&mut self, frame: &[u8], stride: usize, x0: usize, y0: usize, bw: usize, bh: usize) {
        self.bgr.clear();
        self.bgr.resize(bw * bh * 3, 0);
        for (i, y) in (y0..y0 + bh).enumerate() {
            let row = &frame[y * stride + x0 * 4..y * stride + (x0 + bw) * 4];
            let dst = &mut self.bgr[i * bw * 3..(i + 1) * bw * 3];
            for (d, s) in dst.chunks_exact_mut(3).zip(row.chunks_exact(4)) {
                d.copy_from_slice(&s[..3]);
            }
        }
    }

    fn palette_pass(&mut self, frame: &[u8], stride: usize, x0: usize, y0: usize, bw: usize, bh: usize) -> Option<([u32; 16], usize)> {
        let mut pal = [0u32; 16];
        let mut n = 0usize;
        let mut last_c = u32::MAX;
        let mut last_i = 0u8;
        self.idx.clear();
        for y in y0..y0 + bh {
            let row = &frame[y * stride + x0 * 4..y * stride + (x0 + bw) * 4];
            for p in row.chunks_exact(4) {
                let c = px_color(p);
                if c == last_c {
                    self.idx.push(last_i);
                    continue;
                }
                let i = match pal[..n].iter().position(|&q| q == c) {
                    Some(i) => i,
                    None => {
                        if n == 16 {
                            return None;
                        }
                        pal[n] = c;
                        n += 1;
                        n - 1
                    }
                };
                last_c = c;
                last_i = i as u8;
                self.idx.push(last_i);
            }
        }
        Some((pal, n))
    }

    fn pack_indices(&self, bw: usize, bits: usize, out: &mut Vec<u8>) {
        for row in self.idx.chunks_exact(bw) {
            let mut acc = 0u8;
            let mut nbits = 0;
            for &i in row {
                acc |= i << nbits;
                nbits += bits;
                if nbits == 8 {
                    out.push(acc);
                    acc = 0;
                    nbits = 0;
                }
            }
            if nbits > 0 {
                out.push(acc);
            }
        }
    }

    fn lz_or_raw(&mut self, out: &mut Vec<u8>) {
        self.lz_out.clear();
        let clen = lz::compress(&self.bgr, &mut self.table, &mut self.lz_out);
        if clen + 4 < self.bgr.len() {
            out.push(MODE_LZ);
            out.extend_from_slice(&(clen as u32).to_le_bytes());
            out.extend_from_slice(&self.lz_out);
        } else {
            out.push(MODE_RAW);
            out.extend_from_slice(&self.bgr);
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn encode_block(&mut self, frame: &[u8], stride: usize, x0: usize, y0: usize, bw: usize, bh: usize, out: &mut Vec<u8>) {
        match self.palette_pass(frame, stride, x0, y0, bw, bh) {
            Some((pal, 1)) => {
                out.push(MODE_SOLID);
                out.extend_from_slice(&[pal[0] as u8, (pal[0] >> 8) as u8, (pal[0] >> 16) as u8]);
            }
            Some((pal, n)) => {
                out.push(MODE_PALETTE);
                out.push(n as u8);
                for &c in &pal[..n] {
                    out.extend_from_slice(&[c as u8, (c >> 8) as u8, (c >> 16) as u8]);
                }
                self.pack_indices(bw, palette_bits(n), out);
            }
            None => {
                self.gather(frame, stride, x0, y0, bw, bh);
                self.lz_or_raw(out);
            }
        }
    }
}

#[derive(Clone, Copy)]
struct FrameCtx<'a> {
    frame: &'a [u8],
    stride: usize,
    w: usize,
    h: usize,
    ref_valid: bool,
}

#[allow(clippy::too_many_arguments)]
fn encode_strips(ctx: FrameCtx<'_>, first_strip: usize, reference: &mut [u8], dirty: &[bool], force: &[bool], coded_flags: &mut [bool], scratch: &mut Scratch, out: &mut Vec<u8>) -> usize {
    let mut coded = 0usize;
    let (w, h, stride) = (ctx.w, ctx.h, ctx.stride);
    let bw = blocks_across(w);
    for (i, rstrip) in reference.chunks_mut(BLOCK * w * 4).enumerate() {
        let y0 = (first_strip + i) * BLOCK;
        let bhpx = BLOCK.min(h - y0);
        for bx in 0..bw {
            let x0 = bx * BLOCK;
            let bwpx = BLOCK.min(w - x0);
            if !dirty[i * bw + bx] {
                out.push(MODE_SKIP);
                continue;
            }
            if ctx.ref_valid && !force[i * bw + bx] && block_equals_reference(ctx.frame, stride, rstrip, w, x0, y0, bwpx, bhpx) {
                out.push(MODE_SKIP);
                continue;
            }
            scratch.encode_block(ctx.frame, stride, x0, y0, bwpx, bhpx, out);
            coded_flags[i * bw + bx] = true;
            coded += 1;
            for r in 0..bhpx {
                let f = &ctx.frame[(y0 + r) * stride + x0 * 4..(y0 + r) * stride + (x0 + bwpx) * 4];
                rstrip[(r * w + x0) * 4..(r * w + x0 + bwpx) * 4].copy_from_slice(f);
            }
        }
    }
    coded
}

#[allow(clippy::too_many_arguments)]
fn block_equals_reference(frame: &[u8], stride: usize, rstrip: &[u8], w: usize, x0: usize, y0: usize, bw: usize, bh: usize) -> bool {
    for r in 0..bh {
        let f = &frame[(y0 + r) * stride + x0 * 4..(y0 + r) * stride + (x0 + bw) * 4];
        let q = &rstrip[(r * w + x0) * 4..(r * w + x0 + bw) * 4];
        if f != q && !rows_equal_ignoring_x(f, q) {
            return false;
        }
    }
    true
}

pub struct Encoder {
    width: usize,
    height: usize,
    reference: Vec<u8>,
    ref_valid: bool,
    dirty: Vec<bool>,
    excluded: Vec<bool>,
    force: Vec<bool>,
    coded: Vec<bool>,
    attempted: Vec<bool>,
    threads: usize,
    par_min_blocks: usize,
    scratch: Vec<Scratch>,
    outs: Vec<Vec<u8>>,
}

impl Encoder {
    pub fn new(width: u32, height: u32) -> Self {
        let n = std::thread::available_parallelism().map_or(1, |n| n.get()).min(8);
        Self::with_threads(width, height, n)
    }

    pub fn with_threads(width: u32, height: u32, threads: usize) -> Self {
        let mut e = Encoder {
            width: 0,
            height: 0,
            reference: Vec::new(),
            ref_valid: false,
            dirty: Vec::new(),
            excluded: Vec::new(),
            force: Vec::new(),
            coded: Vec::new(),
            attempted: Vec::new(),
            threads: 1,
            par_min_blocks: DEFAULT_PARALLEL_MIN_BLOCKS,
            scratch: Vec::new(),
            outs: Vec::new(),
        };
        e.set_threads(threads);
        e.reset(width, height);
        e
    }

    pub fn threads(&self) -> usize {
        self.threads
    }

    pub fn set_threads(&mut self, threads: usize) {
        self.threads = threads.max(1);
        self.scratch.resize_with(self.threads, Scratch::new);
        self.outs.resize_with(self.threads, Vec::new);
    }

    pub fn set_parallel_min_blocks(&mut self, n: usize) {
        self.par_min_blocks = n;
    }

    pub fn reset(&mut self, width: u32, height: u32) {
        self.width = width as usize;
        self.height = height as usize;
        self.reference.clear();
        self.reference.resize(self.width * self.height * 4, 0);
        self.ref_valid = false;
        let n = blocks_across(self.width) * blocks_across(self.height);
        for v in [&mut self.excluded, &mut self.force, &mut self.coded, &mut self.attempted] {
            v.clear();
            v.resize(n, false);
        }
    }

    pub fn block_cols(&self) -> usize {
        blocks_across(self.width)
    }

    pub fn block_rows(&self) -> usize {
        blocks_across(self.height)
    }

    pub fn coded_blocks(&self) -> &[bool] {
        &self.coded
    }

    pub fn attempted_blocks(&self) -> &[bool] {
        &self.attempted
    }

    pub fn set_excluded(&mut self, rect: Option<Rect>) {
        self.excluded.fill(false);
        if let Some(r) = rect {
            let (w, h) = (self.width, self.height);
            let excluded = &mut self.excluded;
            for_blocks(r, w, h, |i| excluded[i] = true);
        }
    }

    pub fn invalidate_rect(&mut self, rect: Rect) {
        let (w, h) = (self.width, self.height);
        let force = &mut self.force;
        for_blocks(rect, w, h, |i| force[i] = true);
    }

    pub fn dirty_per_strip(&self, damage: Option<&[Rect]>) -> Vec<usize> {
        if self.width == 0 || self.height == 0 {
            return Vec::new();
        }
        let mut dirty = Vec::new();
        Self::compute_dirty(damage, self.width, self.height, self.ref_valid, &self.excluded, &self.force, &mut dirty);
        dirty.chunks(blocks_across(self.width)).map(|r| r.iter().filter(|&&d| d).count()).collect()
    }

    fn compute_dirty(damage: Option<&[Rect]>, w: usize, h: usize, ref_valid: bool, excluded: &[bool], force: &[bool], dirty: &mut Vec<bool>) {
        mark_dirty(if ref_valid { damage } else { None }, w, h, dirty);
        for (i, d) in dirty.iter_mut().enumerate() {
            if excluded[i] {
                *d = false;
            } else if ref_valid && force[i] {
                *d = true;
            }
        }
    }

    pub fn strip_count(&self) -> usize {
        blocks_across(self.height)
    }

    pub fn encode(&mut self, frame: &[u8], stride: usize, damage: Option<&[Rect]>, out: &mut Vec<u8>) -> usize {
        self.encode_range(frame, stride, damage, 0..usize::MAX, out)
    }

    pub fn encode_range(&mut self, frame: &[u8], stride: usize, damage: Option<&[Rect]>, strips: std::ops::Range<usize>, out: &mut Vec<u8>) -> usize {
        let (w, h) = (self.width, self.height);
        if w == 0 || h == 0 {
            return 0;
        }
        assert!(stride >= w * 4, "stride menor que a linha");
        assert!(frame.len() >= stride * (h - 1) + w * 4, "quadro menor que o declarado");

        let bw = blocks_across(w);
        let bh = blocks_across(h);
        let was_valid = self.ref_valid;
        self.coded.fill(false);
        Self::compute_dirty(damage, w, h, self.ref_valid, &self.excluded, &self.force, &mut self.dirty);
        if self.ref_valid {
            for by in (0..bh).filter(|by| !strips.contains(by)) {
                self.dirty[by * bw..(by + 1) * bw].fill(false);
            }
        }

        self.attempted.clear();
        self.attempted.extend_from_slice(&self.dirty);
        let ctx = FrameCtx { frame, stride, w, h, ref_valid: self.ref_valid };
        let Encoder { reference, dirty, force, coded: coded_flags, scratch, outs, threads, par_min_blocks, .. } = self;
        let n_dirty = dirty.iter().filter(|&&d| d).count();
        let coded;

        if *threads <= 1 || bh <= 1 || n_dirty < *par_min_blocks {
            coded = encode_strips(ctx, 0, reference, dirty, force, coded_flags, &mut scratch[0], out);
        } else {
            let per_strip: Vec<usize> = dirty.chunks(bw).map(|r| r.iter().filter(|&&d| d).count()).collect();
            let mut groups: Vec<(usize, usize)> = Vec::with_capacity(*threads);
            let mut start = 0;
            let mut acc = 0;
            for (s, &c) in per_strip.iter().enumerate() {
                acc += c;
                let g = groups.len();
                if g + 1 < *threads && acc * *threads >= n_dirty * (g + 1) {
                    groups.push((start, s + 1));
                    start = s + 1;
                }
            }
            if start < bh {
                groups.push((start, bh));
            }

            let strip_bytes = BLOCK * w * 4;
            let total = reference.len();
            let mut rest: &mut [u8] = &mut reference[..];
            let mut parts: Vec<&mut [u8]> = Vec::with_capacity(groups.len());
            for &(a, b) in &groups {
                let bytes = (b * strip_bytes).min(total) - a * strip_bytes;
                let (head, tail) = std::mem::take(&mut rest).split_at_mut(bytes);
                parts.push(head);
                rest = tail;
            }
            let dirty: &[bool] = dirty;
            let force: &[bool] = force;
            let mut flag_parts: Vec<&mut [bool]> = Vec::with_capacity(groups.len());
            let mut flags_rest: &mut [bool] = &mut coded_flags[..];
            for &(a, b) in &groups {
                let (head, tail) = std::mem::take(&mut flags_rest).split_at_mut((b - a) * bw);
                flag_parts.push(head);
                flags_rest = tail;
            }
            let mut counts = vec![0usize; groups.len()];
            std::thread::scope(|sc| {
                for (((((part, flags), &(a, b)), sc_state), o), cnt) in
                    parts.into_iter().zip(flag_parts).zip(&groups).zip(scratch.iter_mut()).zip(outs.iter_mut()).zip(counts.iter_mut())
                {
                    o.clear();
                    let d = &dirty[a * bw..b * bw];
                    let f = &force[a * bw..b * bw];
                    sc.spawn(move || *cnt = encode_strips(ctx, a, part, d, f, flags, sc_state, o));
                }
            });
            for o in outs.iter().take(groups.len()) {
                out.extend_from_slice(o);
            }
            coded = counts.iter().sum();
        }
        self.ref_valid = true;
        for i in 0..self.coded.len() {
            if self.coded[i] {
                self.force[i] = false;
            } else if !was_valid && self.excluded[i] {
                self.force[i] = true;
            }
        }
        coded
    }
}

fn for_blocks(r: Rect, w: usize, h: usize, mut f: impl FnMut(usize)) {
    if r.is_empty() || r.x as usize >= w || r.y as usize >= h {
        return;
    }
    let bw = blocks_across(w);
    let x1 = (r.x as usize).saturating_add(r.w as usize).min(w);
    let y1 = (r.y as usize).saturating_add(r.h as usize).min(h);
    for by in (r.y as usize / BLOCK)..=((y1 - 1) / BLOCK) {
        for bx in (r.x as usize / BLOCK)..=((x1 - 1) / BLOCK) {
            f(by * bw + bx);
        }
    }
}

fn mark_dirty(damage: Option<&[Rect]>, w: usize, h: usize, dirty: &mut Vec<bool>) {
    let bw = blocks_across(w);
    let bh = blocks_across(h);
    dirty.clear();
    match damage {
        Some(rects) => {
            dirty.resize(bw * bh, false);
            for r in rects {
                if r.is_empty() || r.x as usize >= w || r.y as usize >= h {
                    continue;
                }
                let x1 = (r.x as usize).saturating_add(r.w as usize).min(w);
                let y1 = (r.y as usize).saturating_add(r.h as usize).min(h);
                for by in (r.y as usize / BLOCK)..=((y1 - 1) / BLOCK) {
                    for bx in (r.x as usize / BLOCK)..=((x1 - 1) / BLOCK) {
                        dirty[by * bw + bx] = true;
                    }
                }
            }
        }
        None => dirty.resize(bw * bh, true),
    }
}

pub fn dirty_blocks_per_strip(damage: Option<&[Rect]>, w: u32, h: u32) -> Vec<usize> {
    let (w, h) = (w as usize, h as usize);
    if w == 0 || h == 0 {
        return Vec::new();
    }
    let mut dirty = Vec::new();
    mark_dirty(damage, w, h, &mut dirty);
    dirty.chunks(blocks_across(w)).map(|r| r.iter().filter(|&&d| d).count()).collect()
}

fn rows_equal_ignoring_x(a: &[u8], b: &[u8]) -> bool {
    const MASK: u64 = 0x00FF_FFFF_00FF_FFFF;
    let (ca, cb) = (a.chunks_exact(8), b.chunks_exact(8));
    let (ra, rb) = (ca.remainder(), cb.remainder());
    ca.zip(cb).all(|(p, q)| (u64::from_le_bytes([p[0], p[1], p[2], p[3], p[4], p[5], p[6], p[7]]) ^ u64::from_le_bytes([q[0], q[1], q[2], q[3], q[4], q[5], q[6], q[7]])) & MASK == 0)
        && ra.chunks_exact(4).zip(rb.chunks_exact(4)).all(|(p, q)| p[..3] == q[..3])
}

pub struct Decoder {
    width: usize,
    height: usize,
    pixels: Vec<u8>,
    scratch: Vec<u8>,
}

impl Decoder {
    pub fn new(width: u32, height: u32) -> Self {
        let mut d = Decoder { width: 0, height: 0, pixels: Vec::new(), scratch: Vec::new() };
        d.reset(width, height);
        d
    }

    pub fn reset(&mut self, width: u32, height: u32) {
        self.width = width as usize;
        self.height = height as usize;
        self.pixels.clear();
        self.pixels.resize(self.width * self.height * 4, 0);
        for px in self.pixels.chunks_exact_mut(4) {
            px[3] = 0xFF;
        }
    }

    pub fn width(&self) -> u32 {
        self.width as u32
    }

    pub fn height(&self) -> u32 {
        self.height as u32
    }

    pub fn pixels(&self) -> &[u8] {
        &self.pixels
    }

    pub fn write_region(&mut self, r: Rect, src: &[u8]) -> bool {
        let (w, h) = (self.width, self.height);
        if r.is_empty() || r.x as usize >= w || r.y as usize >= h {
            return true;
        }
        let row_bytes = r.w as usize * 4;
        if src.len() < row_bytes * r.h as usize {
            return false;
        }
        let cw = (r.w as usize).min(w - r.x as usize);
        let ch = (r.h as usize).min(h - r.y as usize);
        for row in 0..ch {
            let d = ((r.y as usize + row) * w + r.x as usize) * 4;
            let s = row * row_bytes;
            self.pixels[d..d + cw * 4].copy_from_slice(&src[s..s + cw * 4]);
            for px in self.pixels[d..d + cw * 4].chunks_exact_mut(4) {
                px[3] = 0xFF;
            }
        }
        true
    }

    pub fn decode(&mut self, data: &[u8]) -> Result<Vec<Rect>, DecodeError> {
        let (w, h) = (self.width, self.height);
        let bw = blocks_across(w);
        let bh = blocks_across(h);
        let mut pos = 0usize;
        let mut rects: Vec<Rect> = Vec::new();

        for by in 0..bh {
            let y0 = by * BLOCK;
            let bhpx = BLOCK.min(h - y0);
            let mut run: Option<Rect> = None;
            for bx in 0..bw {
                let x0 = bx * BLOCK;
                let bwpx = BLOCK.min(w - x0);
                let mode = *data.get(pos).ok_or(DecodeError::Truncated)?;
                pos += 1;
                if mode == MODE_SKIP {
                    if let Some(r) = run.take() {
                        rects.push(r);
                    }
                    continue;
                }
                pos = self.decode_block(mode, data, pos, x0, y0, bwpx, bhpx)?;
                match &mut run {
                    Some(r) => r.w += bwpx as u32,
                    None => run = Some(Rect::new(x0 as u32, y0 as u32, bwpx as u32, bhpx as u32)),
                }
            }
            if let Some(r) = run.take() {
                rects.push(r);
            }
        }
        if pos != data.len() {
            return Err(DecodeError::TrailingData);
        }
        Ok(rects)
    }

    #[allow(clippy::too_many_arguments)]
    fn decode_block(&mut self, mode: u8, data: &[u8], mut pos: usize, x0: usize, y0: usize, bw: usize, bh: usize) -> Result<usize, DecodeError> {
        let stride = self.width * 4;
        match mode {
            MODE_SOLID => {
                let c = take(data, &mut pos, 3)?;
                for y in y0..y0 + bh {
                    let row = &mut self.pixels[y * stride + x0 * 4..y * stride + (x0 + bw) * 4];
                    for px in row.chunks_exact_mut(4) {
                        px[0] = c[0];
                        px[1] = c[1];
                        px[2] = c[2];
                        px[3] = 0xFF;
                    }
                }
            }
            MODE_PALETTE => {
                let n = take(data, &mut pos, 1)?[0];
                if !(2..=16).contains(&n) {
                    return Err(DecodeError::BadPalette(n));
                }
                let n = n as usize;
                let colors = take(data, &mut pos, n * 3)?;
                let bits = palette_bits(n);
                let row_bytes = (bw * bits).div_ceil(8);
                let mask = (1u8 << bits) - 1;
                for y in y0..y0 + bh {
                    let packed = take(data, &mut pos, row_bytes)?;
                    let row = &mut self.pixels[y * stride + x0 * 4..y * stride + (x0 + bw) * 4];
                    for (i, px) in row.chunks_exact_mut(4).enumerate() {
                        let bit = i * bits;
                        let idx = ((packed[bit / 8] >> (bit % 8)) & mask) as usize;
                        if idx >= n {
                            return Err(DecodeError::BadPaletteIndex);
                        }
                        px[0] = colors[idx * 3];
                        px[1] = colors[idx * 3 + 1];
                        px[2] = colors[idx * 3 + 2];
                        px[3] = 0xFF;
                    }
                }
            }
            MODE_LZ => {
                let cl = take(data, &mut pos, 4)?;
                let clen = u32::from_le_bytes([cl[0], cl[1], cl[2], cl[3]]) as usize;
                let comp = take(data, &mut pos, clen)?;
                self.scratch.clear();
                self.scratch.resize(bw * bh * 3, 0);
                lz::decompress(comp, &mut self.scratch).map_err(DecodeError::BadLz)?;
                blit_bgr(&mut self.pixels, stride, &self.scratch, x0, y0, bw, bh);
            }
            MODE_RAW => {
                let raw = take(data, &mut pos, bw * bh * 3)?;
                blit_bgr(&mut self.pixels, stride, raw, x0, y0, bw, bh);
            }
            other => return Err(DecodeError::BadMode(other)),
        }
        Ok(pos)
    }
}

fn blit_bgr(pixels: &mut [u8], stride: usize, src: &[u8], x0: usize, y0: usize, bw: usize, bh: usize) {
    for (i, srow) in src.chunks_exact(bw * 3).take(bh).enumerate() {
        let y = y0 + i;
        let row = &mut pixels[y * stride + x0 * 4..y * stride + (x0 + bw) * 4];
        for (dst, s) in row.chunks_exact_mut(4).zip(srow.chunks_exact(3)) {
            dst[0] = s[0];
            dst[1] = s[1];
            dst[2] = s[2];
            dst[3] = 0xFF;
        }
    }
}

fn take<'a>(data: &'a [u8], pos: &mut usize, n: usize) -> Result<&'a [u8], DecodeError> {
    let end = pos.checked_add(n).ok_or(DecodeError::Truncated)?;
    if end > data.len() {
        return Err(DecodeError::Truncated);
    }
    let s = &data[*pos..end];
    *pos = end;
    Ok(s)
}

#[cfg(test)]
mod tests;

#[doc(hidden)]
#[derive(Debug, Default, Clone, Copy)]
pub struct PhaseTimes {
    pub compare_ns: u64,
    pub palette_ns: u64,
    pub pack_ns: u64,
    pub gather_ns: u64,
    pub lz_ns: u64,
    pub blocks_solid: usize,
    pub blocks_palette: usize,
    pub blocks_lz_or_raw: usize,
}

#[doc(hidden)]
pub fn profile_phases(frame: &[u8], w: usize, h: usize) -> PhaseTimes {
    use std::time::Instant;
    let mut t = PhaseTimes::default();
    let mut s = Scratch::new();
    let stride = w * 4;
    let mut out = Vec::new();
    let reference = frame.to_vec();
    for by in 0..blocks_across(h) {
        let y0 = by * BLOCK;
        let bh = BLOCK.min(h - y0);
        for bx in 0..blocks_across(w) {
            let x0 = bx * BLOCK;
            let bw = BLOCK.min(w - x0);
            let i = Instant::now();
            let eq = block_equals_reference(&frame[y0 * stride..], stride, &reference[y0 * stride..], w, x0, 0, bw, bh);
            std::hint::black_box(eq);
            t.compare_ns += i.elapsed().as_nanos() as u64;

            let i = Instant::now();
            let pal = s.palette_pass(frame, stride, x0, y0, bw, bh);
            t.palette_ns += i.elapsed().as_nanos() as u64;

            match pal {
                Some((_, 1)) => t.blocks_solid += 1,
                Some((_, n)) => {
                    t.blocks_palette += 1;
                    let i = Instant::now();
                    out.clear();
                    s.pack_indices(bw, palette_bits(n), &mut out);
                    std::hint::black_box(&out);
                    t.pack_ns += i.elapsed().as_nanos() as u64;
                }
                None => {
                    t.blocks_lz_or_raw += 1;
                    let i = Instant::now();
                    s.gather(frame, stride, x0, y0, bw, bh);
                    t.gather_ns += i.elapsed().as_nanos() as u64;
                    let i = Instant::now();
                    out.clear();
                    s.lz_or_raw(&mut out);
                    std::hint::black_box(&out);
                    t.lz_ns += i.elapsed().as_nanos() as u64;
                }
            }
        }
    }
    t
}
