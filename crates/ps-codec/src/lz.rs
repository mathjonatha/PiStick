//! Compressor e descompressor LZ próprios, no formato de bloco do LZ4.
//!
//! O compressor segue as restrições do formato LZ4 (a última cópia começa pelo
//! menos 12 bytes antes do fim e os últimos 5 bytes são literais), então a saída
//! também é lida por um descompressor LZ4 de bloco qualquer.

/// Tamanho mínimo de uma cópia.
const MIN_MATCH: usize = 4;
/// A última cópia deve começar ao menos 12 bytes antes do fim da entrada.
const MF_LIMIT: usize = 12;
/// Os últimos 5 bytes da entrada são sempre literais.
const LAST_LITERALS: usize = 5;

const HASH_BITS: u32 = 12;
/// Tamanho da tabela hash (entradas de `u16`).
pub const HASH_SIZE: usize = 1 << HASH_BITS;
const EMPTY: u16 = u16::MAX;

/// Maior entrada aceita pelo compressor: posições cabem em `u16` (EMPTY reservado).
pub const MAX_INPUT: usize = (u16::MAX as usize) - 1;

/// Erro do descompressor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LzError {
    /// A entrada comprimida acabou antes do esperado.
    Truncated,
    /// Offset zero ou apontando para antes do início da saída.
    BadOffset,
    /// A saída passaria do tamanho esperado.
    Overrun,
    /// A saída terminou menor que o tamanho esperado.
    Underrun,
}

#[inline]
fn hash4(v: u32) -> usize {
    (v.wrapping_mul(2_654_435_761) >> (32 - HASH_BITS)) as usize
}

#[inline]
fn read_u32(src: &[u8], i: usize) -> u32 {
    u32::from_le_bytes([src[i], src[i + 1], src[i + 2], src[i + 3]])
}

fn put_len_ext(out: &mut Vec<u8>, mut rem: usize) {
    while rem >= 255 {
        out.push(255);
        rem -= 255;
    }
    out.push(rem as u8);
}

fn emit_literals_only(out: &mut Vec<u8>, lits: &[u8]) {
    let ll = lits.len();
    out.push((ll.min(15) as u8) << 4);
    if ll >= 15 {
        put_len_ext(out, ll - 15);
    }
    out.extend_from_slice(lits);
}

/// Comprime `src` acrescentando a `out`. `table` é um buffer reutilizável de
/// [`HASH_SIZE`] entradas (reinicializado aqui). Devolve o número de bytes
/// acrescentados. Pânico se `src.len() > MAX_INPUT`.
pub fn compress(src: &[u8], table: &mut [u16], out: &mut Vec<u8>) -> usize {
    assert!(src.len() <= MAX_INPUT, "entrada grande demais para o compressor");
    assert!(table.len() >= HASH_SIZE);
    let start_len = out.len();
    let n = src.len();
    if n < MF_LIMIT + 1 {
        emit_literals_only(out, src);
        return out.len() - start_len;
    }
    table[..HASH_SIZE].fill(EMPTY);

    let match_end_limit = n - LAST_LITERALS; // uma cópia não pode terminar depois daqui
    let last_start = n - MF_LIMIT; // última posição onde uma cópia pode começar
    let mut anchor = 0usize;
    let mut ip = 0usize;
    let mut misses = 0usize;

    while ip <= last_start {
        let seq = read_u32(src, ip);
        let h = hash4(seq);
        let cand = table[h];
        table[h] = ip as u16;
        if cand != EMPTY {
            let cand = cand as usize;
            // Posições estão em [0, 65534], então o offset sempre cabe em u16.
            if read_u32(src, cand) == seq {
                let mut mlen = MIN_MATCH;
                while ip + mlen < match_end_limit && src[cand + mlen] == src[ip + mlen] {
                    mlen += 1;
                }
                let lits = &src[anchor..ip];
                let ll = lits.len();
                let ml = mlen - MIN_MATCH;
                out.push(((ll.min(15) as u8) << 4) | (ml.min(15) as u8));
                if ll >= 15 {
                    put_len_ext(out, ll - 15);
                }
                out.extend_from_slice(lits);
                out.extend_from_slice(&((ip - cand) as u16).to_le_bytes());
                if ml >= 15 {
                    put_len_ext(out, ml - 15);
                }
                ip += mlen;
                anchor = ip;
                misses = 0;
                continue;
            }
        }
        misses += 1;
        // Em dados incompressíveis, acelera o passo (como o LZ4).
        ip += 1 + (misses >> 6);
    }
    emit_literals_only(out, &src[anchor..]);
    out.len() - start_len
}

/// Descomprime `src` para `dst`, que deve ficar exatamente cheio.
/// Nunca entra em pânico nem acessa fora dos limites, qualquer que seja a entrada.
pub fn decompress(src: &[u8], dst: &mut [u8]) -> Result<(), LzError> {
    let mut ip = 0usize;
    let mut op = 0usize;
    loop {
        let token = *src.get(ip).ok_or(LzError::Truncated)?;
        ip += 1;

        // Literais
        let mut lit_len = (token >> 4) as usize;
        if lit_len == 15 {
            lit_len = read_ext(src, &mut ip, lit_len)?;
        }
        let lit_end = ip.checked_add(lit_len).ok_or(LzError::Truncated)?;
        if lit_end > src.len() {
            return Err(LzError::Truncated);
        }
        let out_end = op.checked_add(lit_len).ok_or(LzError::Overrun)?;
        if out_end > dst.len() {
            return Err(LzError::Overrun);
        }
        dst[op..out_end].copy_from_slice(&src[ip..lit_end]);
        ip = lit_end;
        op = out_end;

        // A última sequência tem só literais: termina quando a entrada acaba.
        if ip == src.len() {
            return if op == dst.len() { Ok(()) } else { Err(LzError::Underrun) };
        }

        // Cópia
        if ip + 2 > src.len() {
            return Err(LzError::Truncated);
        }
        let offset = u16::from_le_bytes([src[ip], src[ip + 1]]) as usize;
        ip += 2;
        if offset == 0 || offset > op {
            return Err(LzError::BadOffset);
        }
        let mut match_len = (token & 0x0F) as usize;
        if match_len == 15 {
            match_len = read_ext(src, &mut ip, match_len)?;
        }
        match_len = match_len.checked_add(MIN_MATCH).ok_or(LzError::Overrun)?;
        let copy_end = op.checked_add(match_len).ok_or(LzError::Overrun)?;
        if copy_end > dst.len() {
            return Err(LzError::Overrun);
        }
        let from = op - offset;
        if offset >= match_len {
            dst.copy_within(from..from + match_len, op);
        } else {
            // Regiões sobrepostas: cópia byte a byte, repetindo o padrão.
            for i in 0..match_len {
                dst[op + i] = dst[from + i];
            }
        }
        op = copy_end;
    }
}

/// Lê bytes de extensão de tamanho: soma até um byte diferente de 255.
fn read_ext(src: &[u8], ip: &mut usize, mut total: usize) -> Result<usize, LzError> {
    loop {
        let b = *src.get(*ip).ok_or(LzError::Truncated)?;
        *ip += 1;
        total = total.checked_add(b as usize).ok_or(LzError::Overrun)?;
        if b != 255 {
            return Ok(total);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::Rng;

    fn ida_e_volta(dado: &[u8]) -> usize {
        let mut tabela = vec![0u16; HASH_SIZE];
        let mut c = Vec::new();
        compress(dado, &mut tabela, &mut c);
        let mut d = vec![0u8; dado.len()];
        decompress(&c, &mut d).expect("descompressão");
        assert_eq!(d, dado);
        c.len()
    }

    #[test]
    fn entradas_pequenas_e_vazias() {
        for n in 0..40 {
            let v: Vec<u8> = (0..n).map(|i| (i * 7) as u8).collect();
            ida_e_volta(&v);
        }
    }

    #[test]
    fn repetitivo_comprime_muito() {
        let v = vec![0x55u8; 12288];
        assert!(ida_e_volta(&v) < 100);
        let padrao: Vec<u8> = (0..12288).map(|i| [1u8, 2, 3][i % 3]).collect();
        assert!(ida_e_volta(&padrao) < 200);
    }

    #[test]
    fn aleatorio_ida_e_volta() {
        let mut rng = Rng::new(1234);
        for n in [13, 100, 4096, 12288, 30_000, MAX_INPUT] {
            let mut v = vec![0u8; n];
            rng.fill(&mut v);
            ida_e_volta(&v);
        }
    }

    #[test]
    fn misto_ida_e_volta() {
        let mut rng = Rng::new(99);
        for _ in 0..200 {
            let n = 1 + rng.below(12288) as usize;
            let mut v = vec![0u8; n];
            // Alterna trechos aleatórios com trechos repetidos.
            let mut i = 0;
            while i < n {
                let run = 1 + rng.below(300) as usize;
                let fim = (i + run).min(n);
                if rng.below(2) == 0 {
                    rng.fill(&mut v[i..fim]);
                } else {
                    let b = rng.next_u64() as u8;
                    v[i..fim].fill(b);
                }
                i = fim;
            }
            ida_e_volta(&v);
        }
    }

    #[test]
    fn descompressor_conhecido_lz4() {
        // Vetor escrito à mão no formato LZ4: literais "abcd", cópia offset 4 comprimento 8,
        // depois literais finais "12345".
        let c = [0x44u8, b'a', b'b', b'c', b'd', 4, 0, 0x50, b'1', b'2', b'3', b'4', b'5'];
        let mut d = vec![0u8; 4 + 8 + 5];
        decompress(&c, &mut d).unwrap();
        assert_eq!(&d, b"abcdabcdabcd12345");
    }

    #[test]
    fn truncamentos_nunca_entram_em_panico() {
        let mut rng = Rng::new(5);
        let mut v = vec![0u8; 5000];
        for (i, b) in v.iter_mut().enumerate() {
            *b = if i % 97 < 60 { (i % 13) as u8 } else { rng.next_u64() as u8 };
        }
        let mut tabela = vec![0u16; HASH_SIZE];
        let mut c = Vec::new();
        compress(&v, &mut tabela, &mut c);
        for corte in 0..c.len() {
            let mut d = vec![0u8; v.len()];
            let r = decompress(&c[..corte], &mut d);
            assert!(r.is_err(), "corte {corte} deveria falhar");
        }
    }

    #[test]
    fn corrupcao_nunca_entra_em_panico() {
        let mut rng = Rng::new(77);
        let mut v = vec![0u8; 3000];
        for (i, b) in v.iter_mut().enumerate() {
            *b = if i % 50 < 30 { (i % 7) as u8 } else { rng.next_u64() as u8 };
        }
        let mut tabela = vec![0u16; HASH_SIZE];
        let mut c = Vec::new();
        compress(&v, &mut tabela, &mut c);
        for _ in 0..20_000 {
            let mut x = c.clone();
            let k = 1 + rng.below(4);
            for _ in 0..k {
                let pos = rng.below(x.len() as u32) as usize;
                x[pos] = rng.next_u64() as u8;
            }
            let mut d = vec![0u8; v.len()];
            let _ = decompress(&x, &mut d); // qualquer resultado serve; só não pode dar pânico
        }
        // Lixo puro, de vários tamanhos e tamanhos de saída
        for _ in 0..5_000 {
            let n = rng.below(200) as usize;
            let mut x = vec![0u8; n];
            rng.fill(&mut x);
            let mut d = vec![0u8; rng.below(500) as usize];
            let _ = decompress(&x, &mut d);
        }
    }

    #[test]
    fn extensoes_gigantes_nao_estouram() {
        // Literais com extensão de 255 repetida: soma enorme, mas entrada curta.
        let mut x = vec![0xF0u8];
        x.extend(std::iter::repeat_n(255, 10_000));
        let mut d = vec![0u8; 100];
        assert!(decompress(&x, &mut d).is_err());
        let mut x = vec![0x0Fu8, 1, 0, 0xFF, 0xFF, 0xFF];
        x.extend(std::iter::repeat_n(255, 1000));
        assert!(decompress(&x, &mut d).is_err());
    }

    #[test]
    fn offset_invalido() {
        // 1 literal, depois cópia com offset 2 (> 1 byte produzido)
        let c = [0x10u8, b'a', 2, 0, 0x00];
        let mut d = vec![0u8; 16];
        assert_eq!(decompress(&c, &mut d), Err(LzError::BadOffset));
        // offset 0
        let c = [0x10u8, b'a', 0, 0, 0x00];
        assert_eq!(decompress(&c, &mut d), Err(LzError::BadOffset));
    }
}
