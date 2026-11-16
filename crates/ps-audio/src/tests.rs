use crate::psa1::{decode, encode, DecodeError, EncodeError, MAX_FRAMES};
use crate::synth;
use crate::*;

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
    fn i16(&mut self) -> i16 {
        (self.next() >> 48) as i16
    }
}

fn roundtrip(pcm: &[i16], channels: usize) -> usize {
    let mut enc = Vec::new();
    encode(pcm, channels, &mut enc).unwrap();
    let mut dec = Vec::new();
    let n = decode(&enc, channels, &mut dec).unwrap();
    assert_eq!(n, pcm.len() / channels);
    assert_eq!(dec, pcm, "ida e volta diferente (canais={channels}, n={n})");
    enc.len()
}

fn sine(n: usize, channels: usize, f: f64, amp: f64) -> Vec<i16> {
    let mut v = Vec::new();
    for i in 0..n {
        for c in 0..channels {
            let fr = f * (c + 1) as f64;
            v.push((amp * (2.0 * std::f64::consts::PI * fr * i as f64 / 48000.0).sin() * 32767.0).round() as i16);
        }
    }
    v
}

#[test]
fn silencio_vira_quase_nada() {
    for ch in [1usize, 2] {
        let pcm = vec![0i16; 480 * ch];
        let bytes = roundtrip(&pcm, ch);
        assert!(bytes < 2 + ch * (2 + 60 + 2), "silêncio ocupou {bytes} bytes");
    }
}

#[test]
fn senoide_comprime() {
    for ch in [1usize, 2] {
        let pcm = sine(480, ch, 440.0, 0.5);
        let bytes = roundtrip(&pcm, ch);
        assert!(bytes < pcm.len() * 2 / 2, "senoide ocupou {bytes} de {}", pcm.len() * 2);
    }
}

#[test]
fn ruido_branco_ida_e_volta_e_nao_explode() {
    let mut r = Rng(1);
    for ch in [1usize, 2] {
        let pcm: Vec<i16> = (0..480 * ch).map(|_| r.i16()).collect();
        let bytes = roundtrip(&pcm, ch);
        let limite = pcm.len() * 2 + if ch == 2 { 480 / 8 + 1 } else { 0 } + 2 + 2 * ch;
        assert!(bytes <= limite, "ruído ocupou {bytes}, limite {limite}");
    }
}

#[test]
fn estouro_de_escala_alternado() {
    for ch in [1usize, 2] {
        let mut pcm = Vec::new();
        for i in 0..960 {
            for c in 0..ch {
                pcm.push(if (i + c) % 2 == 0 { 32767 } else { -32767 });
            }
        }
        roundtrip(&pcm, ch);
        let mut pcm = Vec::new();
        for i in 0..500 {
            let (a, b) = match i % 4 {
                0 => (32767, -32768),
                1 => (-32768, 32767),
                2 => (-32768, -32768),
                _ => (32767, 32767),
            };
            pcm.push(a);
            if ch == 2 {
                pcm.push(b);
            }
        }
        roundtrip(&pcm, ch);
    }
}

#[test]
fn tamanhos_de_1_a_4800_quadros() {
    let mut r = Rng(7);
    let mut sizes: Vec<usize> = (1..=70).collect();
    sizes.extend([100, 479, 480, 481, 960, 1000, 2048, 4799, 4800]);
    for n in sizes {
        for ch in [1usize, 2] {
            let mut pcm = sine(n, ch, 300.0, 0.4);
            for s in pcm.iter_mut() {
                *s = s.saturating_add(r.i16() >> 12);
            }
            roundtrip(&pcm, ch);
            let ruido: Vec<i16> = (0..n * ch).map(|_| r.i16()).collect();
            roundtrip(&ruido, ch);
        }
    }
}

#[test]
fn degrau_e_impulso_usam_o_escape() {
    let mut pcm = vec![0i16; 480];
    pcm[200] = 32767;
    pcm[201] = -32768;
    roundtrip(&pcm, 1);
    let mut st = vec![-32768i16; 480];
    for s in st.iter_mut().skip(240) {
        *s = 32767;
    }
    roundtrip(&st, 1);
}

#[test]
fn rejeita_parametros_invalidos_do_codificador() {
    let mut out = Vec::new();
    assert_eq!(encode(&[0; 4], 3, &mut out), Err(EncodeError::BadChannels));
    assert_eq!(encode(&[], 2, &mut out), Err(EncodeError::BadLength));
    assert_eq!(encode(&[0; 3], 2, &mut out), Err(EncodeError::BadLength));
    assert_eq!(encode(&vec![0; MAX_FRAMES + 1], 1, &mut out), Err(EncodeError::BadLength));
    assert!(out.is_empty());
}

#[test]
fn decodificador_rejeita_cabecalhos_ruins() {
    let mut out = Vec::new();
    assert_eq!(decode(&[], 2, &mut out), Err(DecodeError::Truncated));
    assert_eq!(decode(&[0, 0], 1, &mut out), Err(DecodeError::BadFrames));
    assert_eq!(decode(&[0xFF, 0xFF, 0, 0], 1, &mut out), Err(DecodeError::BadFrames));
    assert_eq!(decode(&[1, 0, 0, 0, 0, 0], 3, &mut out), Err(DecodeError::BadChannels));
    assert_eq!(decode(&[4, 0, 5, 0, 0, 0, 0, 0], 1, &mut out), Err(DecodeError::BadOrder));
    assert_eq!(decode(&[4, 0, 0, 16, 0, 0, 0, 0], 1, &mut out), Err(DecodeError::BadParam));
    assert_eq!(decode(&[4, 0, 1, 255, 0, 0, 0, 0, 0, 0, 0], 1, &mut out), Err(DecodeError::BadParam));
    assert_eq!(decode(&[1, 0, 2, 0, 0, 0, 0, 0], 1, &mut out), Err(DecodeError::BadOrder));
    assert!(out.is_empty());
}

#[test]
fn decodificador_rejeita_truncamento_e_sobra() {
    let pcm = sine(480, 2, 440.0, 0.3);
    let mut enc = Vec::new();
    encode(&pcm, 2, &mut enc).unwrap();
    for cut in 0..enc.len() {
        let mut out = Vec::new();
        assert!(decode(&enc[..cut], 2, &mut out).is_err(), "truncado em {cut} foi aceito");
        assert!(out.is_empty());
    }
    let mut mais = enc.clone();
    mais.push(0);
    assert!(decode(&mais, 2, &mut Vec::new()).is_err());
    let mut sujo = enc.clone();
    *sujo.last_mut().unwrap() ^= 1;
    let _ = decode(&sujo, 2, &mut Vec::new());
}

#[test]
fn decodificador_nao_entra_em_panico_com_10000_cargas_aleatorias() {
    let mut r = Rng(0xABCDEF);
    for i in 0..10_000 {
        let len = (r.next() % 300) as usize;
        let mut data: Vec<u8> = (0..len).map(|_| r.next() as u8).collect();
        if i % 2 == 0 && data.len() >= 4 {
            let n = 1 + (r.next() % 480) as u16;
            data[..2].copy_from_slice(&n.to_le_bytes());
            data[2] %= 6;
            data[3] = if r.next().is_multiple_of(4) { 255 } else { (r.next() % 17) as u8 };
        }
        for ch in [1usize, 2] {
            let mut out = Vec::new();
            let res = decode(&data, ch, &mut out);
            if res.is_err() {
                assert!(out.is_empty());
            }
        }
    }
}

#[test]
fn mutacoes_de_cargas_validas_nao_entram_em_panico_e_ou_dao_erro_ou_decodificam() {
    let mut r = Rng(99);
    for t in 0..300 {
        let ch = 1 + t % 2;
        let pcm = if t % 3 == 0 { sine(480, ch, 500.0, 0.4) } else { (0..480 * ch).map(|_| r.i16() >> 4).collect() };
        let mut enc = Vec::new();
        encode(&pcm, ch, &mut enc).unwrap();
        for _ in 0..40 {
            let mut m = enc.clone();
            let flips = 1 + r.next() % 3;
            for _ in 0..flips {
                let i = (r.next() % m.len() as u64) as usize;
                m[i] ^= 1 << (r.next() % 8);
            }
            if r.next().is_multiple_of(5) {
                let cut = (r.next() % m.len() as u64) as usize;
                m.truncate(cut);
            }
            let mut out = Vec::new();
            let _ = decode(&m, ch, &mut out);
        }
    }
}

#[test]
fn pcm_e_amostras() {
    let s = [0i16, 1, -1, 32767, -32768];
    let mut b = Vec::new();
    samples_to_pcm(&s, &mut b);
    assert_eq!(b.len(), 10);
    let mut back = Vec::new();
    pcm_to_samples(&b, &mut back);
    assert_eq!(back, s);
    assert!(is_silent(&[0, 0, 0]));
    assert!(!is_silent(&[0, 0, 1]));
}

#[test]
fn sintetico_tem_as_frequencias_e_o_silencio() {
    let mut x = Vec::new();
    for n in 0..synth::SEGMENT_FRAMES * 2 {
        let (l, r) = synth::frame(n);
        x.push(l);
        x.push(r);
    }
    let (som, silencio) = x.split_at(synth::SEGMENT_FRAMES as usize * 2);
    let (l, r) = synth::split_stereo(som);
    assert!((synth::goertzel(&l, 440.0) - synth::AMPLITUDE).abs() < 0.01);
    assert!((synth::goertzel(&r, 880.0) - synth::AMPLITUDE).abs() < 0.01);
    assert!(synth::goertzel(&l, 880.0) < 0.01);
    assert!(synth::goertzel(&r, 440.0) < 0.01);
    assert!(is_silent(silencio));
}

#[test]
#[ignore]
fn compressao_de_arquivo_pcm() {
    let Ok(path) = std::env::var("PSA1_RAW") else {
        println!("PSA1_RAW não definida; nada a medir");
        return;
    };
    let bytes = std::fs::read(path).unwrap();
    let mut pcm = Vec::new();
    pcm_to_samples(&bytes, &mut pcm);
    let (mut total_in, mut total_out) = (0usize, 0usize);
    let (mut t_enc, mut t_dec) = (std::time::Duration::ZERO, std::time::Duration::ZERO);
    for pkt in pcm.chunks_exact(480 * 2) {
        let mut enc = Vec::new();
        let t0 = std::time::Instant::now();
        encode(pkt, 2, &mut enc).unwrap();
        t_enc += t0.elapsed();
        let mut dec = Vec::new();
        let t1 = std::time::Instant::now();
        decode(&enc, 2, &mut dec).unwrap();
        t_dec += t1.elapsed();
        assert_eq!(dec, pkt);
        total_in += pkt.len() * 2;
        total_out += enc.len();
    }
    let por_s = total_in as f64 / 192_000.0;
    println!(
        "PSA1: {} bytes -> {} bytes ({:.1}% do PCM; {:.2} Mbit/s com 48 kHz estéreo); por 1 s de áudio: codificação {:.2} ms, decodificação {:.2} ms",
        total_in,
        total_out,
        total_out as f64 * 100.0 / total_in as f64,
        total_out as f64 * 100.0 / total_in as f64 * 1.536 / 100.0,
        t_enc.as_secs_f64() * 1000.0 / por_s,
        t_dec.as_secs_f64() * 1000.0 / por_s
    );
}

#[test]
fn tom_continuo_tem_a_frequencia_e_a_amplitude_pedidas_o_tempo_todo() {
    let x: Vec<i16> = (0..96_000u64)
        .flat_map(|n| {
            let (l, r) = synth::tone_frame(n, 440.0, 880.0, 0.25);
            [l, r]
        })
        .collect();
    let (l, r) = synth::split_stereo(&x);
    for metade in [&l[..48_000], &l[48_000..]] {
        assert!((synth::goertzel(metade, 440.0) - 0.25).abs() < 0.002);
    }
    assert!((synth::goertzel(&r, 880.0) - 0.25).abs() < 0.002);
    assert!(synth::goertzel(&l, 880.0) < 0.002 && synth::goertzel(&r, 440.0) < 0.002);
}
