
use std::io;
use std::path::Path;

fn crc32(partes: &[&[u8]]) -> u32 {
    let mut crc = 0xFFFF_FFFFu32;
    for parte in partes {
        for &b in *parte {
            crc ^= u32::from(b);
            for _ in 0..8 {
                crc = if crc & 1 != 0 { (crc >> 1) ^ 0xEDB8_8320 } else { crc >> 1 };
            }
        }
    }
    !crc
}

fn adler32(dados: &[u8]) -> u32 {
    let (mut a, mut b) = (1u32, 0u32);
    for &x in dados {
        a = (a + u32::from(x)) % 65521;
        b = (b + a) % 65521;
    }
    (b << 16) | a
}

fn chunk(out: &mut Vec<u8>, tipo: &[u8; 4], dados: &[u8]) {
    out.extend_from_slice(&(dados.len() as u32).to_be_bytes());
    out.extend_from_slice(tipo);
    out.extend_from_slice(dados);
    out.extend_from_slice(&crc32(&[tipo, dados]).to_be_bytes());
}

pub fn encode_bgrx(width: u32, height: u32, bgrx: &[u8]) -> Vec<u8> {
    let (w, h) = (width as usize, height as usize);
    let mut raw = Vec::with_capacity(h * (w * 3 + 1));
    for y in 0..h {
        raw.push(0);
        for px in bgrx[y * w * 4..(y + 1) * w * 4].as_chunks::<4>().0.iter() {
            raw.extend_from_slice(&[px[2], px[1], px[0]]);
        }
    }
    let mut z = vec![0x78, 0x01];
    let mut blocos = raw.chunks(65535).peekable();
    if blocos.peek().is_none() {
        z.extend_from_slice(&[1, 0, 0, 0xFF, 0xFF]);
    }
    while let Some(b) = blocos.next() {
        z.push(u8::from(blocos.peek().is_none()));
        z.extend_from_slice(&(b.len() as u16).to_le_bytes());
        z.extend_from_slice(&(!(b.len() as u16)).to_le_bytes());
        z.extend_from_slice(b);
    }
    z.extend_from_slice(&adler32(&raw).to_be_bytes());

    let mut out = vec![0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
    let mut ihdr = Vec::with_capacity(13);
    ihdr.extend_from_slice(&width.to_be_bytes());
    ihdr.extend_from_slice(&height.to_be_bytes());
    ihdr.extend_from_slice(&[8, 2, 0, 0, 0]);
    chunk(&mut out, b"IHDR", &ihdr);
    chunk(&mut out, b"IDAT", &z);
    chunk(&mut out, b"IEND", &[]);
    out
}

pub fn save_bgrx(path: &Path, width: u32, height: u32, bgrx: &[u8]) -> io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(path, encode_bgrx(width, height, bgrx))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crc_e_adler_conhecidos() {
        assert_eq!(crc32(&[b"123456789"]), 0xCBF4_3926);
        assert_eq!(adler32(b"Wikipedia"), 0x11E6_0398);
    }

    #[test]
    fn estrutura_do_png() {
        let px = [0, 0, 255, 255, 0, 255, 0, 255, 255, 0, 0, 255, 255, 255, 255, 255];
        let png = encode_bgrx(2, 2, &px);
        assert_eq!(&png[..8], &[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A]);
        assert_eq!(&png[12..16], b"IHDR");
        assert_eq!(u32::from_be_bytes(png[16..20].try_into().unwrap()), 2);
        assert_eq!(&png[png.len() - 8..png.len() - 4], b"IEND");
        let bruto = [0, 255, 0, 0, 0, 255, 0, 0, 0, 0, 255, 255, 255, 255];
        assert!(png.windows(bruto.len()).any(|j| j == bruto));
    }

    #[test]
    fn quadro_grande_usa_varios_blocos() {
        let png = encode_bgrx(300, 300, &vec![0x80; 300 * 300 * 4]);
        assert!(png.len() > 270_300);
    }
}
