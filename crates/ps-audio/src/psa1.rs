
use std::fmt;

pub const MAX_FRAMES: usize = 4800;

const ESCAPE_Q: u32 = 32;
const RAW_K: u8 = 255;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EncodeError {
    BadChannels,
    BadLength,
}

impl fmt::Display for EncodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            EncodeError::BadChannels => write!(f, "número de canais inválido"),
            EncodeError::BadLength => write!(f, "tamanho do pacote de áudio inválido"),
        }
    }
}

impl std::error::Error for EncodeError {}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DecodeError {
    BadChannels,
    Truncated,
    TrailingData,
    BadFrames,
    BadOrder,
    BadParam,
    OutOfRange,
}

impl fmt::Display for DecodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            DecodeError::BadChannels => "número de canais inválido",
            DecodeError::Truncated => "carga de áudio truncada",
            DecodeError::TrailingData => "dados sobrando na carga de áudio",
            DecodeError::BadFrames => "contagem de quadros inválida",
            DecodeError::BadOrder => "ordem do preditor inválida",
            DecodeError::BadParam => "parâmetro de Rice inválido",
            DecodeError::OutOfRange => "amostra fora da faixa",
        };
        f.write_str(s)
    }
}

impl std::error::Error for DecodeError {}

struct BitWriter<'a> {
    out: &'a mut Vec<u8>,
    acc: u64,
    nbits: u32,
}

impl<'a> BitWriter<'a> {
    fn new(out: &'a mut Vec<u8>) -> Self {
        BitWriter { out, acc: 0, nbits: 0 }
    }

    fn put(&mut self, v: u64, n: u32) {
        debug_assert!(n <= 32);
        if n == 0 {
            return;
        }
        self.acc = (self.acc << n) | (v & ((1u64 << n) - 1));
        self.nbits += n;
        while self.nbits >= 8 {
            self.nbits -= 8;
            self.out.push((self.acc >> self.nbits) as u8);
        }
        self.acc &= (1u64 << self.nbits) - 1;
    }

    fn ones(&mut self, mut n: u32) {
        while n > 0 {
            let c = n.min(32);
            self.put((1u64 << c) - 1, c);
            n -= c;
        }
    }

    fn finish(mut self) {
        if self.nbits > 0 {
            let pad = 8 - self.nbits;
            self.put(0, pad);
        }
    }
}

struct BitReader<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> BitReader<'a> {
    fn new(data: &'a [u8]) -> Self {
        BitReader { data, pos: 0 }
    }

    fn remaining(&self) -> usize {
        self.data.len() * 8 - self.pos
    }

    fn bit(&mut self) -> Result<u32, DecodeError> {
        if self.pos >= self.data.len() * 8 {
            return Err(DecodeError::Truncated);
        }
        let b = (self.data[self.pos >> 3] >> (7 - (self.pos & 7))) & 1;
        self.pos += 1;
        Ok(b as u32)
    }

    fn get(&mut self, n: u32) -> Result<u64, DecodeError> {
        if (n as usize) > self.remaining() {
            return Err(DecodeError::Truncated);
        }
        let mut v = 0u64;
        for _ in 0..n {
            v = (v << 1) | self.bit()? as u64;
        }
        Ok(v)
    }

    fn align(&mut self) -> Result<(), DecodeError> {
        while self.pos & 7 != 0 {
            if self.bit()? != 0 {
                return Err(DecodeError::TrailingData);
            }
        }
        Ok(())
    }
}

#[inline]
fn zigzag(r: i64) -> u64 {
    ((r << 1) ^ (r >> 63)) as u64
}

#[inline]
fn unzigzag(u: u64) -> i64 {
    ((u >> 1) as i64) ^ -((u & 1) as i64)
}

#[inline]
fn predict(order: usize, x: &[i32], i: usize) -> i64 {
    let g = |d: usize| x[i - d] as i64;
    match order {
        0 => 0,
        1 => g(1),
        2 => 2 * g(1) - g(2),
        3 => 3 * g(1) - 3 * g(2) + g(3),
        _ => 4 * g(1) - 6 * g(2) + 4 * g(3) - g(4),
    }
}

fn residual_sum(x: &[i32], order: usize) -> u64 {
    #[inline(always)]
    fn zz(r: i32) -> u64 {
        (((r << 1) ^ (r >> 31)) as u32) as u64
    }
    let mut sum = 0u64;
    match order {
        0 => x.iter().for_each(|&v| sum += zz(v)),
        1 => x.windows(2).for_each(|w| sum += zz(w[1] - w[0])),
        2 => x.windows(3).for_each(|w| sum += zz(w[2] - 2 * w[1] + w[0])),
        3 => x.windows(4).for_each(|w| sum += zz(w[3] - 3 * w[2] + 3 * w[1] - w[0])),
        _ => x.windows(5).for_each(|w| sum += zz(w[4] - 4 * w[3] + 6 * w[2] - 4 * w[1] + w[0])),
    }
    sum
}

fn pick_k(count: u64, sum: u64) -> (u64, u8) {
    let mean = (sum / count.max(1)).max(1);
    let k0 = (63 - mean.leading_zeros()).min(15) as i32;
    let mut best = (u64::MAX, 0u8);
    for k in (k0 - 1).max(0)..=(k0 + 1).min(15) {
        let bits = count * (k as u64 + 1) + (sum >> k);
        if bits < best.0 {
            best = (bits, k as u8);
        }
    }
    best
}

fn encode_channel(x: &[i32], width: u32, out: &mut Vec<u8>) {
    let n = x.len();
    let mut best_order = 0usize;
    let mut best_k = RAW_K;
    let mut best_bits = n as u64 * width as u64;
    let mut prev = u64::MAX;
    for order in 0..=4usize.min(n.saturating_sub(1)) {
        let count = (n - order) as u64;
        let (bits, k) = pick_k(count, residual_sum(x, order));
        let total = bits + order as u64 * width as u64;
        if total < best_bits {
            best_bits = total;
            best_order = order;
            best_k = k;
        }
        if total > prev {
            break;
        }
        prev = total;
    }
    out.push(if best_k == RAW_K { 0 } else { best_order as u8 });
    out.push(best_k);
    let mut w = BitWriter::new(out);
    if best_k == RAW_K {
        for &s in x {
            w.put(s as u32 as u64, width);
        }
    } else {
        for &s in &x[..best_order] {
            w.put(s as u32 as u64, width);
        }
        let k = best_k as u32;
        for i in best_order..n {
            let u = zigzag(x[i] as i64 - predict(best_order, x, i));
            let q = u >> k;
            if q < ESCAPE_Q as u64 {
                w.ones(q as u32);
                w.put(0, 1);
                w.put(u & ((1u64 << k) - 1), k);
            } else {
                w.ones(ESCAPE_Q);
                w.put(u, 32);
            }
        }
    }
    w.finish();
}

pub fn encode(pcm: &[i16], channels: usize, out: &mut Vec<u8>) -> Result<(), EncodeError> {
    if channels != 1 && channels != 2 {
        return Err(EncodeError::BadChannels);
    }
    if pcm.is_empty() || !pcm.len().is_multiple_of(channels) || pcm.len() / channels > MAX_FRAMES {
        return Err(EncodeError::BadLength);
    }
    let n = pcm.len() / channels;
    out.extend_from_slice(&(n as u16).to_le_bytes());
    if channels == 1 {
        let x: Vec<i32> = pcm.iter().map(|&s| s as i32).collect();
        encode_channel(&x, 16, out);
    } else {
        let mut m = Vec::with_capacity(n);
        let mut s = Vec::with_capacity(n);
        for f in pcm.chunks_exact(2) {
            let (l, r) = (f[0] as i32, f[1] as i32);
            m.push((l + r) >> 1);
            s.push(l - r);
        }
        encode_channel(&m, 16, out);
        encode_channel(&s, 17, out);
    }
    Ok(())
}

fn sign_extend(v: u64, width: u32) -> i32 {
    let shift = 64 - width;
    (((v << shift) as i64) >> shift) as i32
}

fn decode_channel(r: &mut BitReader<'_>, n: usize, width: u32, hdr: (u8, u8), x: &mut Vec<i32>) -> Result<(), DecodeError> {
    let (order, k) = hdr;
    let order = order as usize;
    let lo = -(1i64 << (width - 1));
    let hi = (1i64 << (width - 1)) - 1;
    x.clear();
    if k == RAW_K {
        if order != 0 {
            return Err(DecodeError::BadParam);
        }
        if (n as u64) * (width as u64) > r.remaining() as u64 {
            return Err(DecodeError::Truncated);
        }
        for _ in 0..n {
            x.push(sign_extend(r.get(width)?, width));
        }
    } else {
        if k > 15 {
            return Err(DecodeError::BadParam);
        }
        if order > 4 || order > n {
            return Err(DecodeError::BadOrder);
        }
        for _ in 0..order {
            x.push(sign_extend(r.get(width)?, width));
        }
        for i in order..n {
            let mut q = 0u32;
            while q < ESCAPE_Q && r.bit()? == 1 {
                q += 1;
            }
            let u = if q == ESCAPE_Q { r.get(32)? } else { ((q as u64) << k) | r.get(k as u32)? };
            let v = predict(order, x, i) + unzigzag(u);
            if v < lo || v > hi {
                return Err(DecodeError::OutOfRange);
            }
            x.push(v as i32);
        }
    }
    r.align()
}

pub fn decode(data: &[u8], channels: usize, out: &mut Vec<i16>) -> Result<usize, DecodeError> {
    if channels != 1 && channels != 2 {
        return Err(DecodeError::BadChannels);
    }
    if data.len() < 2 {
        return Err(DecodeError::Truncated);
    }
    let n = u16::from_le_bytes([data[0], data[1]]) as usize;
    if n == 0 || n > MAX_FRAMES {
        return Err(DecodeError::BadFrames);
    }
    let mut pos = 2usize;
    let mut chans: Vec<Vec<i32>> = Vec::with_capacity(channels);
    for c in 0..channels {
        if pos + 2 > data.len() {
            return Err(DecodeError::Truncated);
        }
        let hdr = (data[pos], data[pos + 1]);
        pos += 2;
        let width = if channels == 2 && c == 1 { 17 } else { 16 };
        let mut r = BitReader::new(&data[pos..]);
        let mut x = Vec::with_capacity(n);
        decode_channel(&mut r, n, width, hdr, &mut x)?;
        pos += r.pos / 8;
        chans.push(x);
    }
    if pos != data.len() {
        return Err(DecodeError::TrailingData);
    }
    let start = out.len();
    if channels == 1 {
        out.extend(chans[0].iter().map(|&v| v as i16));
    } else {
        for (&m, &s) in chans[0].iter().zip(chans[1].iter()) {
            let m2 = (m << 1) | (s & 1);
            let l = (m2 + s) >> 1;
            let r = (m2 - s) >> 1;
            if l < i16::MIN as i32 || l > i16::MAX as i32 || r < i16::MIN as i32 || r > i16::MAX as i32 {
                out.truncate(start);
                return Err(DecodeError::OutOfRange);
            }
            out.push(l as i16);
            out.push(r as i16);
        }
    }
    Ok(n)
}
