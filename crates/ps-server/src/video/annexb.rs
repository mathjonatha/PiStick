
pub const NAL_SLICE: u8 = 1;
pub const NAL_IDR: u8 = 5;
pub const NAL_SEI: u8 = 6;
pub const NAL_SPS: u8 = 7;
pub const NAL_PPS: u8 = 8;
pub const NAL_AUD: u8 = 9;

pub fn nal_units(data: &[u8]) -> Vec<(u8, &[u8])> {
    let mut starts = Vec::new();
    let mut i = 0;
    while i + 3 <= data.len() {
        if data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 1 {
            starts.push((i, i + 3));
            i += 3;
        } else {
            i += 1;
        }
    }
    let mut out = Vec::new();
    for (k, &(_, begin)) in starts.iter().enumerate() {
        let mut end = starts.get(k + 1).map_or(data.len(), |&(s, _)| s);
        while end > begin && data[end - 1] == 0 {
            end -= 1;
        }
        if end > begin {
            out.push((data[begin] & 0x1F, &data[begin..end]));
        }
    }
    out
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Sps {
    pub profile_idc: u8,
    pub level_idc: u8,
    pub coded_width: u32,
    pub coded_height: u32,
    pub width: u32,
    pub height: u32,
    pub max_num_ref_frames: u32,
    pub matrix_coefficients: Option<u8>,
    pub full_range: Option<bool>,
}

struct Bits<'a> {
    d: &'a [u8],
    pos: usize,
}

impl Bits<'_> {
    fn bit(&mut self) -> Result<u32, String> {
        let byte = *self.d.get(self.pos / 8).ok_or("SPS truncado")?;
        let b = (byte >> (7 - self.pos % 8)) & 1;
        self.pos += 1;
        Ok(u32::from(b))
    }
    fn bits(&mut self, n: u32) -> Result<u32, String> {
        let mut v = 0;
        for _ in 0..n {
            v = (v << 1) | self.bit()?;
        }
        Ok(v)
    }
    fn ue(&mut self) -> Result<u32, String> {
        let mut zeros = 0;
        while self.bit()? == 0 {
            zeros += 1;
            if zeros > 31 {
                return Err("Exp-Golomb longo demais".into());
            }
        }
        Ok((1u32 << zeros) - 1 + self.bits(zeros)?)
    }
    fn se(&mut self) -> Result<i32, String> {
        let k = self.ue()?;
        Ok(if !k.is_multiple_of(2) { k.div_ceil(2) as i32 } else { -((k / 2) as i32) })
    }
}

pub fn parse_sps(nal: &[u8]) -> Result<Sps, String> {
    if nal.first().map(|b| b & 0x1F) != Some(NAL_SPS) {
        return Err("não é um SPS".into());
    }
    let mut rbsp = Vec::with_capacity(nal.len());
    let mut zeros = 0;
    for &b in &nal[1..] {
        if zeros >= 2 && b == 3 {
            zeros = 0;
            continue;
        }
        zeros = if b == 0 { zeros + 1 } else { 0 };
        rbsp.push(b);
    }
    let mut r = Bits { d: &rbsp, pos: 0 };
    let profile_idc = r.bits(8)? as u8;
    let _constraints = r.bits(8)?;
    let level_idc = r.bits(8)? as u8;
    let _id = r.ue()?;
    let mut chroma_format_idc = 1;
    if [100, 110, 122, 244, 44, 83, 86, 118, 128, 138, 139, 134, 135].contains(&profile_idc) {
        chroma_format_idc = r.ue()?;
        if chroma_format_idc == 3 {
            r.bit()?;
        }
        r.ue()?;
        r.ue()?;
        r.bit()?;
        if r.bit()? == 1 {
            return Err("matrizes de quantização no SPS não são tratadas".into());
        }
    }
    let _log2_max_frame_num = r.ue()?;
    match r.ue()? {
        0 => {
            r.ue()?;
        }
        1 => {
            r.bit()?;
            r.se()?;
            r.se()?;
            for _ in 0..r.ue()? {
                r.se()?;
            }
        }
        _ => {}
    }
    let max_num_ref_frames = r.ue()?;
    r.bit()?;
    let mbs_w = r.ue()? + 1;
    let map_units_h = r.ue()? + 1;
    let frame_mbs_only = r.bit()?;
    if frame_mbs_only == 0 {
        r.bit()?;
    }
    r.bit()?;
    let (coded_width, coded_height) = (mbs_w * 16, map_units_h * 16 * (2 - frame_mbs_only));
    let (mut cl, mut cr, mut ct, mut cb) = (0, 0, 0, 0);
    if r.bit()? == 1 {
        cl = r.ue()?;
        cr = r.ue()?;
        ct = r.ue()?;
        cb = r.ue()?;
    }
    let (unit_x, unit_y) = if chroma_format_idc == 0 { (1, 2 - frame_mbs_only) } else { (2, 2 * (2 - frame_mbs_only)) };
    let width = coded_width.saturating_sub(unit_x * (cl + cr));
    let height = coded_height.saturating_sub(unit_y * (ct + cb));
    let (mut matrix_coefficients, mut full_range) = (None, None);
    if r.bit()? == 1 {
        if r.bit()? == 1 && r.bits(8)? == 255 {
            r.bits(32)?;
        }
        if r.bit()? == 1 {
            r.bit()?;
        }
        if r.bit()? == 1 {
            r.bits(3)?;
            full_range = Some(r.bit()? == 1);
            if r.bit()? == 1 {
                r.bits(8)?;
                r.bits(8)?;
                matrix_coefficients = Some(r.bits(8)? as u8);
            }
        }
    }
    Ok(Sps { profile_idc, level_idc, coded_width, coded_height, width, height, max_num_ref_frames, matrix_coefficients, full_range })
}

#[cfg(test)]
mod tests {
    use super::*;

    struct W(Vec<u8>, usize);
    impl W {
        fn bit(&mut self, b: u32) {
            if self.1.is_multiple_of(8) {
                self.0.push(0);
            }
            if b == 1 {
                let n = self.0.len() - 1;
                self.0[n] |= 0x80 >> (self.1 % 8);
            }
            self.1 += 1;
        }
        fn bits(&mut self, v: u32, n: u32) {
            for i in (0..n).rev() {
                self.bit((v >> i) & 1);
            }
        }
        fn ue(&mut self, v: u32) {
            let x = v + 1;
            let n = 32 - x.leading_zeros();
            self.bits(0, n - 1);
            self.bits(x, n);
        }
    }

    fn sps(mbs_w: u32, mbs_h: u32, crop_r: u32, crop_b: u32, matrix: u8, full: bool) -> Vec<u8> {
        let mut w = W(Vec::new(), 0);
        w.bits(100, 8);
        w.bits(0, 8);
        w.bits(40, 8);
        w.ue(0);
        w.ue(1);
        w.ue(0);
        w.ue(0);
        w.bit(0);
        w.bit(0);
        w.ue(0);
        w.ue(2);
        w.ue(1);
        w.bit(0);
        w.ue(mbs_w - 1);
        w.ue(mbs_h - 1);
        w.bit(1);
        w.bit(1);
        let crop = crop_r + crop_b > 0;
        w.bit(u32::from(crop));
        if crop {
            w.ue(0);
            w.ue(crop_r);
            w.ue(0);
            w.ue(crop_b);
        }
        w.bit(1);
        w.bit(0);
        w.bit(0);
        w.bit(1);
        w.bits(5, 3);
        w.bit(u32::from(full));
        w.bit(1);
        w.bits(u32::from(matrix), 8);
        w.bits(u32::from(matrix), 8);
        w.bits(u32::from(matrix), 8);
        w.bit(1);
        let mut nal = vec![0x67];
        nal.extend(w.0);
        nal
    }

    #[test]
    fn le_o_sps_com_recorte_e_cor() {
        let s = parse_sps(&sps(20, 13, 0, 4, 1, false)).unwrap();
        assert_eq!((s.coded_width, s.coded_height, s.width, s.height), (320, 208, 320, 200));
        assert_eq!((s.profile_idc, s.level_idc, s.max_num_ref_frames), (100, 40, 1));
        assert_eq!((s.matrix_coefficients, s.full_range), (Some(1), Some(false)));
        let s = parse_sps(&sps(80, 45, 0, 0, 6, true)).unwrap();
        assert_eq!((s.width, s.height), (1280, 720));
        assert_eq!((s.matrix_coefficients, s.full_range), (Some(6), Some(true)));
        let s = parse_sps(&sps(80, 45, 0, 0, 1, false)).unwrap();
        assert_eq!((s.coded_height, s.height), (720, 720));
        let s = parse_sps(&sps(80, 46, 0, 8, 1, false)).unwrap();
        assert_eq!((s.coded_height, s.height), (736, 720));
    }

    #[test]
    fn sps_truncado_ou_de_outro_tipo_da_erro() {
        let full = sps(20, 13, 0, 4, 1, false);
        for corte in 1..full.len() - 2 {
            assert!(parse_sps(&full[..corte]).is_err(), "corte {corte}");
        }
        assert!(parse_sps(&[0x68, 1, 2, 3]).is_err());
    }

    #[test]
    fn separa_as_nal_de_um_fluxo() {
        let mut d = vec![0, 0, 0, 1, 0x67, 1, 2, 0, 0, 0, 1, 0x68, 3, 0, 0, 1, 0x65, 9, 9, 0];
        d.extend([0, 0, 1, 0x41, 7]);
        let n = nal_units(&d);
        let tipos: Vec<u8> = n.iter().map(|(t, _)| *t).collect();
        assert_eq!(tipos, vec![NAL_SPS, NAL_PPS, NAL_IDR, NAL_SLICE]);
        assert_eq!(n[0].1, &[0x67, 1, 2]);
        assert_eq!(n[2].1, &[0x65, 9, 9]);
        assert!(nal_units(&[]).is_empty());
        assert!(nal_units(&[1, 2, 3]).is_empty());
    }
}
