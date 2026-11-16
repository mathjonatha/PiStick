//! Gera o WAV do tom do roteiro do microfone (marco 7B): estéreo, 48 kHz, 16 bits, `FREQ_ESQ` Hz no canal esquerdo e
//! `FREQ_DIR` Hz no direito, amplitude `AMPLITUDE` (fração da escala cheia), `SEGUNDOS` de tom contínuo. Com números
//! inteiros de ciclos por segundo (440 e 880) o arquivo emenda sem estalo quando `psc --mic-file` o toca em laço.
//!
//! Uso: `cargo run -p ps-audio --example gerar_tom_wav -- SAIDA.wav [SEGUNDOS [FREQ_ESQ [FREQ_DIR [AMPLITUDE [PULSO_MS]]]]]`
//! (padrão: 2 s, 440, 880, 0,25, sem pulso). Com `PULSO_MS` acima de zero o arquivo não é tom contínuo: cada segundo tem
//! silêncio e, a partir de 0,5 s, um pulso do tom com essa duração (serve à medição de atraso do microfone: o início do
//! pulso é o instante a ser achado na gravação).

use ps_audio::synth::tone_frame;

fn arg<T: std::str::FromStr>(n: usize, padrao: T) -> T {
    std::env::args().nth(n).and_then(|a| a.parse().ok()).unwrap_or(padrao)
}

fn main() {
    let Some(saida) = std::env::args().nth(1) else {
        eprintln!("uso: gerar_tom_wav SAIDA.wav [SEGUNDOS [FREQ_ESQ [FREQ_DIR [AMPLITUDE]]]]");
        std::process::exit(2);
    };
    let segundos: u64 = arg(2, 2);
    let (fe, fd, amp): (f64, f64, f64) = (arg(3, 440.0), arg(4, 880.0), arg(5, 0.25));
    let pulso_ms: u64 = arg(6, 0);
    let quadros = segundos * 48_000;
    let dados = quadros * 4;
    let mut b: Vec<u8> = Vec::with_capacity(44 + dados as usize);
    b.extend_from_slice(b"RIFF");
    b.extend_from_slice(&((36 + dados) as u32).to_le_bytes());
    b.extend_from_slice(b"WAVEfmt ");
    b.extend_from_slice(&16u32.to_le_bytes());
    b.extend_from_slice(&1u16.to_le_bytes());
    b.extend_from_slice(&2u16.to_le_bytes());
    b.extend_from_slice(&48_000u32.to_le_bytes());
    b.extend_from_slice(&(48_000u32 * 4).to_le_bytes());
    b.extend_from_slice(&4u16.to_le_bytes());
    b.extend_from_slice(&16u16.to_le_bytes());
    b.extend_from_slice(b"data");
    b.extend_from_slice(&(dados as u32).to_le_bytes());
    for n in 0..quadros {
        let (l, r) = if pulso_ms == 0 || (24_000..24_000 + pulso_ms * 48).contains(&(n % 48_000)) { tone_frame(n, fe, fd, amp) } else { (0, 0) };
        b.extend_from_slice(&l.to_le_bytes());
        b.extend_from_slice(&r.to_le_bytes());
    }
    if let Err(e) = std::fs::write(&saida, &b) {
        eprintln!("não foi possível gravar {saida}: {e}");
        std::process::exit(1);
    }
    println!("{saida}: {segundos} s, {fe} Hz à esquerda, {fd} Hz à direita, amplitude {amp}{}", if pulso_ms > 0 { format!(", pulso de {pulso_ms} ms a cada segundo (a partir de 0,5 s)") } else { String::new() });
}
