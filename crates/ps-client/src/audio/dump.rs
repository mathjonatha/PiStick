
use std::fs::File;
use std::io::{self, BufWriter, Seek, SeekFrom, Write};
use std::path::Path;

use ps_audio::SAMPLE_RATE;

const MIN_GAP_FRAMES: u64 = SAMPLE_RATE as u64 / 10;

pub struct WavDump {
    w: BufWriter<File>,
    t0_us: Option<u64>,
    frames: u64,
    failed: bool,
}

impl WavDump {
    pub fn create(path: &Path) -> io::Result<WavDump> {
        let mut w = BufWriter::new(File::create(path)?);
        w.write_all(&header(0))?;
        Ok(WavDump { w, t0_us: None, frames: 0, failed: false })
    }

    pub fn write(&mut self, pts_us: u64, samples: &[i16]) {
        if self.failed {
            return;
        }
        let t0 = *self.t0_us.get_or_insert(pts_us);
        let at = pts_us.saturating_sub(t0) * SAMPLE_RATE as u64 / 1_000_000;
        if at > self.frames && at - self.frames >= MIN_GAP_FRAMES {
            let gap = at - self.frames;
            if self.put_silence(gap).is_err() {
                self.failed = true;
                return;
            }
        }
        let mut bytes = Vec::with_capacity(samples.len() * 2);
        ps_audio::samples_to_pcm(samples, &mut bytes);
        if self.w.write_all(&bytes).is_err() {
            self.failed = true;
            return;
        }
        self.frames += (samples.len() / 2) as u64;
    }

    fn put_silence(&mut self, frames: u64) -> io::Result<()> {
        let zeros = vec![0u8; 4 * 4800];
        let mut left = frames * 4;
        while left > 0 {
            let n = left.min(zeros.len() as u64) as usize;
            self.w.write_all(&zeros[..n])?;
            left -= n as u64;
        }
        self.frames += frames;
        Ok(())
    }

    pub fn flush(&mut self) {
        if self.failed {
            return;
        }
        let r = (|| -> io::Result<()> {
            self.w.flush()?;
            let f = self.w.get_mut();
            let pos = f.stream_position()?;
            f.seek(SeekFrom::Start(0))?;
            f.write_all(&header(self.frames * 4))?;
            f.seek(SeekFrom::Start(pos))?;
            Ok(())
        })();
        if r.is_err() {
            self.failed = true;
        }
    }

    pub fn frames(&self) -> u64 {
        self.frames
    }
}

impl Drop for WavDump {
    fn drop(&mut self) {
        self.flush();
    }
}

fn header(data_bytes: u64) -> [u8; 44] {
    let data = data_bytes.min(u32::MAX as u64 - 36) as u32;
    let mut h = [0u8; 44];
    h[0..4].copy_from_slice(b"RIFF");
    h[4..8].copy_from_slice(&(36 + data).to_le_bytes());
    h[8..12].copy_from_slice(b"WAVE");
    h[12..16].copy_from_slice(b"fmt ");
    h[16..20].copy_from_slice(&16u32.to_le_bytes());
    h[20..22].copy_from_slice(&1u16.to_le_bytes());
    h[22..24].copy_from_slice(&2u16.to_le_bytes());
    h[24..28].copy_from_slice(&SAMPLE_RATE.to_le_bytes());
    h[28..32].copy_from_slice(&(SAMPLE_RATE * 4).to_le_bytes());
    h[32..34].copy_from_slice(&4u16.to_le_bytes());
    h[34..36].copy_from_slice(&16u16.to_le_bytes());
    h[36..40].copy_from_slice(b"data");
    h[40..44].copy_from_slice(&data.to_le_bytes());
    h
}

pub fn read_wav(path: &Path) -> io::Result<Vec<i16>> {
    let b = std::fs::read(path)?;
    if b.len() < 44 || &b[0..4] != b"RIFF" || &b[8..12] != b"WAVE" || &b[36..40] != b"data" {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "não é o WAV que o psc grava"));
    }
    let n = (u32::from_le_bytes([b[40], b[41], b[42], b[43]]) as usize).min(b.len() - 44);
    Ok(b[44..44 + n].as_chunks::<2>().0.iter().map(|&c| i16::from_le_bytes(c)).collect())
}
