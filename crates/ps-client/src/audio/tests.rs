use ps_audio::{encode, samples_to_pcm};
use ps_proto::{AUDIO_FLAG_DISCONTINUITY, AUDIO_FLAG_DROPPED};

use super::dump::{read_wav, WavDump};
use super::*;

const BLOCK: usize = 480;
const TARGET_FRAMES: usize = 40 * 48;
const MAX_FRAMES: usize = 140 * 48;
const ADJUST_ON_FRAMES: f64 = 10.0 * 48.0;

fn pkt(counter: &mut i32) -> Vec<i16> {
    let mut v = Vec::with_capacity(BLOCK * 2);
    for _ in 0..BLOCK {
        *counter = *counter % 30000 + 1;
        v.push(*counter as i16);
        v.push(-(*counter as i16));
    }
    v
}

fn simula(q: &mut JitterQueue, prod_hz: f64, segundos: f64, mut a_cada_tick: impl FnMut(&JitterQueue)) {
    let mut counter = 0;
    let mut t_prod = 0.0f64;
    let passo = 480.0 / prod_hz;
    let ticks = (segundos * 100.0) as usize;
    let mut out = vec![0i16; BLOCK * 2];
    for n in 0..ticks {
        let t = n as f64 * 0.01;
        while t_prod <= t {
            q.push(&pkt(&mut counter), false);
            t_prod += passo;
        }
        q.pull(&mut out);
        a_cada_tick(q);
    }
}

#[test]
fn espera_encher_ate_o_alvo_antes_de_tocar() {
    let mut q = JitterQueue::new(QueueConfig::fixed_ms(40));
    let mut c = 0;
    let mut out = vec![1i16; BLOCK * 2];
    for _ in 0..3 {
        q.push(&pkt(&mut c), false);
    }
    q.pull(&mut out);
    assert!(out.iter().all(|&s| s == 0));
    assert!(!q.is_playing());
    q.push(&pkt(&mut c), false);
    assert!(q.is_playing());
    q.pull(&mut out);
    let mut c2 = 0;
    assert_eq!(out, pkt(&mut c2));
}

#[test]
fn regime_estavel_mantem_o_nivel_no_alvo_sem_esvaziar() {
    let mut q = JitterQueue::new(QueueConfig::fixed_ms(40));
    let (mut max, mut min) = (0usize, usize::MAX);
    simula(&mut q, 48_000.0, 60.0, |q| {
        if q.is_playing() {
            max = max.max(q.level_frames());
            min = min.min(q.level_frames());
        }
    });
    let s = q.stats();
    assert_eq!(s.underruns, 0);
    assert_eq!(s.overflows, 0);
    assert_eq!((s.removed, s.repeated), (0, 0), "sem desvio, sem correção");
    assert!(max <= TARGET_FRAMES + BLOCK && min + 2 * BLOCK >= TARGET_FRAMES, "nível entre {min} e {max}");
    let avg = s.level_avg_ms();
    assert!((30.0..=60.0).contains(&avg), "nível médio {avg:.1} ms");
}

#[test]
fn passagem_sem_perdas_quando_nao_ha_desvio() {
    let mut q = JitterQueue::new(QueueConfig::fixed_ms(40));
    let mut c = 0;
    let mut entrada = Vec::new();
    let mut saida = Vec::new();
    let mut out = vec![0i16; BLOCK * 2];
    for _ in 0..4 {
        let p = pkt(&mut c);
        entrada.extend_from_slice(&p);
        q.push(&p, false);
    }
    for _ in 0..200 {
        let p = pkt(&mut c);
        entrada.extend_from_slice(&p);
        q.push(&p, false);
        q.pull(&mut out);
        saida.extend_from_slice(&out);
    }
    assert_eq!(saida, entrada[..saida.len()]);
    assert_eq!(q.stats().underruns, 0);
}

#[test]
fn falta_de_dados_conta_esvaziamento_e_refaz_o_alvo() {
    let mut q = JitterQueue::new(QueueConfig::fixed_ms(40));
    let mut c = 0;
    let mut out = vec![0i16; BLOCK * 2];
    for _ in 0..6 {
        q.push(&pkt(&mut c), false);
    }
    for _ in 0..6 {
        q.pull(&mut out);
        assert!(out.iter().any(|&s| s != 0));
    }
    assert_eq!(q.stats().underruns, 0);
    q.pull(&mut out);
    assert!(out.iter().all(|&s| s == 0));
    assert_eq!(q.stats().underruns, 1);
    assert!(!q.is_playing());
    q.push(&pkt(&mut c), false);
    q.pull(&mut out);
    assert!(out.iter().all(|&s| s == 0));
    assert_eq!(q.stats().underruns, 1, "esperar encher não é novo esvaziamento");
    for _ in 0..3 {
        q.push(&pkt(&mut c), false);
    }
    assert!(q.is_playing());
}

#[test]
fn esvaziamento_no_meio_do_bloco_completa_com_silencio() {
    let mut q = JitterQueue::new(QueueConfig::fixed_ms(40));
    let mut c = 0;
    for _ in 0..4 {
        q.push(&pkt(&mut c), false);
    }
    let mut out = vec![0i16; BLOCK * 2 * 6];
    q.pull(&mut out);
    assert!(out[..BLOCK * 2 * 4].iter().all(|&s| s != 0));
    assert!(out[BLOCK * 2 * 4..].iter().all(|&s| s == 0));
    assert_eq!(q.stats().underruns, 1);
}

#[test]
fn fim_de_trecho_de_silencio_nao_e_esvaziamento() {
    let mut q = JitterQueue::new(QueueConfig::fixed_ms(40));
    let mut c = 0;
    let mut out = vec![0i16; BLOCK * 2];
    for _ in 0..5 {
        q.push(&pkt(&mut c), false);
    }
    for _ in 0..20 {
        q.push(&vec![0i16; BLOCK * 2], false);
    }
    for _ in 0..200 {
        q.pull(&mut out);
    }
    assert_eq!(q.stats().underruns, 0);
    assert!(!q.is_playing());
    for k in 0..4 {
        q.push(&pkt(&mut c), k == 0);
    }
    assert!(q.is_playing());
}

#[test]
fn rajada_acima_do_maximo_descarta_o_excesso_de_uma_vez() {
    let mut q = JitterQueue::new(QueueConfig::fixed_ms(40));
    let mut c = 0;
    q.push(&(0..30).flat_map(|_| pkt(&mut c)).collect::<Vec<_>>(), false);
    let s = q.stats();
    assert_eq!(s.overflows, 1);
    assert_eq!(q.level_frames(), TARGET_FRAMES);
    assert_eq!(s.overflow_frames as usize, 30 * BLOCK - TARGET_FRAMES);
    assert!(q.is_playing());
    let mut out = vec![0i16; 2];
    q.pull(&mut out);
    assert_ne!(out[0], 0);
    let mut q = JitterQueue::new(QueueConfig::fixed_ms(40));
    q.push(&(0..10).flat_map(|_| pkt(&mut c)).collect::<Vec<_>>(), false);
    assert_eq!(q.stats().overflows, 0);
    assert_eq!(q.level_frames(), 10 * BLOCK);
}

#[test]
fn descontinuidade_recomeca_a_fila() {
    let mut q = JitterQueue::new(QueueConfig::fixed_ms(40));
    let mut c = 0;
    for _ in 0..8 {
        q.push(&pkt(&mut c), false);
    }
    assert!(q.is_playing());
    q.push(&pkt(&mut c), true);
    assert_eq!(q.level_frames(), BLOCK, "a fila antiga foi descartada");
    assert!(!q.is_playing());
    assert_eq!(q.stats().restarts, 1);
}

#[test]
fn produtor_mais_rapido_que_o_consumidor_e_corrigido_sem_estourar() {
    let mut q = JitterQueue::new(QueueConfig::fixed_ms(40));
    let mut max = 0usize;
    simula(&mut q, 48_048.0, 600.0, |q| max = max.max(q.level_frames()));
    let s = q.stats();
    assert_eq!(s.overflows, 0, "correção deve evitar o descarte; removidos {}", s.removed);
    assert_eq!(s.underruns, 0);
    assert!(s.removed > 1000, "removeu {} quadros", s.removed);
    assert_eq!(s.repeated, 0);
    assert!(max < MAX_FRAMES, "nível máximo {max}");
    assert!(q.level_frames() < TARGET_FRAMES + ADJUST_ON_FRAMES as usize * 2, "nível final {}", q.level_frames());
}

#[test]
fn produtor_mais_lento_que_o_consumidor_e_corrigido_sem_esvaziar() {
    let mut q = JitterQueue::new(QueueConfig::fixed_ms(40));
    let mut min = usize::MAX;
    simula(&mut q, 47_952.0, 600.0, |q| {
        if q.is_playing() {
            min = min.min(q.level_frames());
        }
    });
    let s = q.stats();
    assert_eq!(s.underruns, 0, "repetidos {}", s.repeated);
    assert_eq!(s.overflows, 0);
    assert!(s.repeated > 1000, "repetiu {} quadros", s.repeated);
    assert!(min >= BLOCK, "nível mínimo {min}");
}

#[test]
fn desvio_pequeno_dentro_de_10_ms_nao_aciona_a_correcao() {
    let mut q = JitterQueue::new(QueueConfig::fixed_ms(40));
    simula(&mut q, 48_002.4, 60.0, |_| {});
    let s = q.stats();
    assert_eq!((s.removed, s.repeated, s.underruns, s.overflows), (0, 0, 0, 0));
}

#[test]
fn correcao_rapida_remove_no_maximo_um_quadro_a_cada_2_ms() {
    let mut q = JitterQueue::new(QueueConfig::fixed_ms(40));
    simula(&mut q, 48_480.0, 30.0, |_| {});
    let s = q.stats();
    assert!(s.removed <= 30 * 48_000 / 96, "removeu {}", s.removed);
    assert!(s.removed > 0);
    assert_eq!((s.underruns, s.overflows), (0, 0));
}

fn seno(frames: usize) -> Vec<i16> {
    (0..frames)
        .flat_map(|i| {
            let v = (8000.0 * (2.0 * std::f64::consts::PI * 440.0 * i as f64 / 48000.0).sin()) as i16;
            [v, v]
        })
        .collect()
}

fn maior_degrau(x: &[i16]) -> i32 {
    x.as_chunks::<2>().0.iter().zip(x.as_chunks::<2>().0.iter().skip(1)).map(|(a, b)| (b[0] as i32 - a[0] as i32).abs()).max().unwrap_or(0)
}

#[test]
fn emenda_da_correcao_nao_faz_degrau() {
    let natural = maior_degrau(&seno(4800));
    assert!((440..=470).contains(&natural), "{natural}");
    for (hz, remocao) in [(48_072.0, true), (47_928.0, false)] {
        let mut q = JitterQueue::new(QueueConfig::fixed_ms(40));
        let mut saida = Vec::new();
        let mut out = vec![0i16; BLOCK * 2];
        let mut gerado = 0usize;
        let mut t_prod = 0.0f64;
        let fonte = seno(48_000 * 20);
        for n in 0..1500 {
            let t = n as f64 * 0.01;
            while t_prod <= t && gerado + BLOCK <= 48_000 * 20 {
                q.push(&fonte[gerado * 2..(gerado + BLOCK) * 2], false);
                gerado += BLOCK;
                t_prod += 480.0 / hz;
            }
            q.pull(&mut out);
            if n > 800 {
                saida.extend_from_slice(&out);
            }
        }
        let st = q.stats();
        assert!(if remocao { st.removed > 100 } else { st.repeated > 100 }, "removidos {} repetidos {}", st.removed, st.repeated);
        let degrau = maior_degrau(&saida);
        assert!(degrau as f64 <= natural as f64 * 1.6, "degrau {degrau} contra o natural {natural} (hz {hz})");
    }
}

struct Rede {
    ppm: f64,
    atraso: Box<dyn Fn(u64) -> u64>,
}

struct Resultado {
    q: JitterQueue,
    stats: QueueStats,
    underruns_em: Vec<(u64, u64)>,
    serie: Vec<(u64, f64, usize)>,
}

fn simula_rede(cfg: QueueConfig, rede: Rede, segundos: u64) -> Resultado {
    let mut ev: Vec<(u64, u8, u64, Vec<i16>)> = Vec::new();
    let mut c = 0;
    let mut ultima_chegada = 0u64;
    let total = segundos * 100;
    for k in 0..total {
        let s = k * 10_000;
        let nominal = (s as f64 * (1.0 + rede.ppm * 1e-6)) as u64 + (rede.atraso)(s);
        let chegada = nominal.max(ultima_chegada);
        ultima_chegada = chegada;
        ev.push((chegada, 0, s, pkt(&mut c)));
    }
    let mut j = 0;
    while 100_000 + j * 10_000 <= ultima_chegada {
        ev.push((100_000 + j * 10_000, 1, 0, Vec::new()));
        j += 1;
    }
    ev.sort_by_key(|e| (e.0, e.1));
    let mut q = JitterQueue::new(cfg);
    let mut out = vec![0i16; BLOCK * 2];
    let mut primeiro = true;
    let (mut underruns_em, mut serie) = (Vec::new(), Vec::new());
    let mut stats_pull = QueueStats::default();
    for (t, ordem, pts, dados) in ev {
        if ordem == 0 {
            q.push_at(&dados, primeiro, Some(pts), t);
            primeiro = false;
        } else {
            q.pull_at(&mut out, t);
            underruns_em.push((t, q.stats().underruns));
            serie.push((t, q.target_ms(), q.level_frames()));
            stats_pull = q.stats();
        }
    }
    Resultado { q, stats: stats_pull, underruns_em, serie }
}

fn esvaziamentos_apos(r: &Resultado, depois_de_us: u64) -> u64 {
    let antes = r.underruns_em.iter().filter(|(t, _)| *t <= depois_de_us).map(|(_, u)| *u).max().unwrap_or(0);
    r.stats.underruns - antes
}

fn pausa_110(s: u64) -> u64 {
    let ciclo = s % 9_000_000;
    if ciclo < 110_000 && s >= 9_000_000 {
        110_000 - ciclo
    } else {
        0
    }
}

#[test]
fn adaptativa_rede_sem_jitter_fica_em_40_ms() {
    let r = simula_rede(QueueConfig::default(), Rede { ppm: 0.0, atraso: Box::new(|_| 0) }, 120);
    let s = r.stats;
    assert_eq!((s.underruns, s.overflows), (0, 0));
    assert!(r.serie.iter().all(|(_, alvo, _)| (*alvo - 40.0).abs() < 0.5), "o alvo saiu de 40 ms");
    assert!(r.q.jitter_ms() < 1.0, "jitter {}", r.q.jitter_ms());
    assert!((30.0..=60.0).contains(&s.level_avg_ms()), "nível médio {}", s.level_avg_ms());
}

#[test]
fn adaptativa_pausas_de_110_ms_a_cada_9_s_estabilizam_o_alvo_sem_esvaziar() {
    let r = simula_rede(QueueConfig::default(), Rede { ppm: 0.0, atraso: Box::new(pausa_110) }, 300);
    assert_eq!(esvaziamentos_apos(&r, 20_000_000), 0, "esvaziamentos depois dos 20 s; total {}", r.stats.underruns);
    let (_, alvo_final, _) = *r.serie.last().unwrap();
    assert!((120.0..=180.0).contains(&alvo_final), "alvo final {alvo_final}");
    let nivel_medio = r.stats.level_avg_ms();
    assert!(nivel_medio < 200.0, "nível médio {nivel_medio}");
}

#[test]
fn adaptativa_jitter_some_e_o_alvo_volta_a_40_ms_em_ate_90_s() {
    let r = simula_rede(QueueConfig::default(), Rede { ppm: 0.0, atraso: Box::new(|s| if s >= 60_000_000 { 0 } else { pausa_110(s) }) }, 200);
    let alto = r.serie.iter().filter(|(t, _, _)| *t < 60_000_000).map(|(_, a, _)| *a).fold(0.0f64, f64::max);
    assert!(alto > 120.0, "o alvo devia ter subido (máximo {alto})");
    let voltou = r.serie.iter().find(|(t, a, _)| *t > 60_000_000 && (*a - 40.0).abs() < 0.5).map(|(t, _, _)| *t);
    let voltou = voltou.expect("o alvo nunca voltou a 40 ms");
    assert!(voltou - 60_000_000 <= 90_000_000, "voltou depois de {} s", (voltou - 60_000_000) / 1_000_000);
}

#[test]
fn adaptativa_rajada_unica_de_250_ms_custa_no_maximo_um_esvaziamento() {
    let rajada = |s: u64| if (20_000_000..20_250_000).contains(&s) { 20_250_000 - s } else { 0 };
    let r = simula_rede(QueueConfig::default(), Rede { ppm: 0.0, atraso: Box::new(rajada) }, 90);
    assert!(r.stats.underruns <= 1, "esvaziamentos {}", r.stats.underruns);
    let ok = r.serie.iter().filter(|(t, _, _)| (35_000_000..60_000_000).contains(t)).all(|(_, alvo, nivel)| (*nivel as f64 / 48.0 - *alvo).abs() <= 25.0);
    assert!(ok, "o nível não voltou ao alvo em 15 s");
}

#[test]
fn adaptativa_desvio_de_relogio_de_500_ppm_por_10_minutos_sem_esvaziar_nem_estourar() {
    for ppm in [500.0, -500.0] {
        let r = simula_rede(QueueConfig::default(), Rede { ppm, atraso: Box::new(|_| 0) }, 600);
        let s = r.stats;
        assert_eq!((s.underruns, s.overflows), (0, 0), "ppm {ppm}: removidos {} repetidos {}", s.removed, s.repeated);
        let max_alvo = r.serie.iter().map(|(_, a, _)| *a).fold(0.0f64, f64::max);
        assert!(max_alvo < 100.0, "ppm {ppm}: alvo máximo {max_alvo}");
    }
}

#[test]
fn alvo_fixo_nao_se_adapta() {
    let r = simula_rede(QueueConfig::fixed_ms(40), Rede { ppm: 0.0, atraso: Box::new(pausa_110) }, 60);
    assert!(r.serie.iter().all(|(_, a, _)| (*a - 40.0).abs() < 0.5));
    assert!(r.stats.underruns > 0);
}

#[test]
fn esvaziamento_real_sobe_o_alvo_em_50_por_cento_por_30_s() {
    let mut q = JitterQueue::new(QueueConfig::default());
    let mut c = 0;
    let mut out = vec![0i16; BLOCK * 2];
    for _ in 0..4 {
        q.push_at(&pkt(&mut c), false, None, 0);
    }
    for k in 0..5 {
        q.pull_at(&mut out, 10_000 * k);
    }
    assert_eq!(q.stats().underruns, 1);
    assert!((q.target_ms() - 60.0).abs() < 0.5, "alvo {}", q.target_ms());
    q.pull_at(&mut out, 20_000_000);
    assert!((q.target_ms() - 60.0).abs() < 0.5);
    q.pull_at(&mut out, 40_000_000);
    assert!(q.target_ms() < 60.0, "o piso devia ter acabado: {}", q.target_ms());
}

fn senoide(frames: usize, amp: f64) -> Vec<i16> {
    let mut v = Vec::new();
    for i in 0..frames {
        let t = i as f64 / 48000.0;
        v.push((amp * (2.0 * std::f64::consts::PI * 440.0 * t).sin() * 32767.0) as i16);
        v.push((amp * (2.0 * std::f64::consts::PI * 880.0 * t).sin() * 32767.0) as i16);
    }
    v
}

fn psa1(samples: &[i16]) -> Vec<u8> {
    let mut d = Vec::new();
    encode(samples, 2, &mut d).unwrap();
    d
}

#[test]
fn motor_decodifica_psa1_e_pcm_e_acompanha_a_sequencia() {
    for codec in [CODEC_PSA1, CODEC_PCM] {
        let e = AudioEngine::new(None);
        e.on_start(0, codec, 2, 48000, 480).unwrap();
        let s = senoide(480, 0.3);
        let data = if codec == CODEC_PSA1 {
            psa1(&s)
        } else {
            let mut b = Vec::new();
            samples_to_pcm(&s, &mut b);
            b
        };
        for seq in 0..5u32 {
            e.on_data(0, if seq == 0 { AUDIO_FLAG_DISCONTINUITY } else { 0 }, seq, seq as u64 * 10_000, &data).unwrap();
        }
        let st = e.stats();
        assert_eq!((st.packets, st.frames), (5, 2400));
        assert_eq!(st.resumes, 1);
        assert_eq!(st.drops, 0);
        assert!(st.open);
        assert!(e.on_data(0, 0, 7, 0, &data).is_err());
    }
}

#[test]
fn motor_recusa_violacoes_de_protocolo() {
    let e = AudioEngine::new(None);
    let s = senoide(480, 0.3);
    let d = psa1(&s);
    assert!(e.on_data(0, 0, 0, 0, &d).is_err());
    assert!(e.on_start(1, CODEC_PSA1, 2, 48000, 480).is_err(), "fluxo 1 é o microfone");
    assert!(e.on_start(0, CODEC_PSA1, 2, 44100, 480).is_err());
    assert!(e.on_start(0, 2, 2, 48000, 480).is_err(), "codec 2 é reservado");
    assert!(e.on_start(0, CODEC_PSA1, 3, 48000, 480).is_err());
    assert!(e.on_start(0, CODEC_PSA1, 2, 48000, 0).is_err());
    e.on_start(0, CODEC_PSA1, 2, 48000, 480).unwrap();
    assert!(e.on_data(0, 0, 0, 0, &d[..d.len() - 3]).is_err());
    let e2 = AudioEngine::new(None);
    e2.on_start(0, CODEC_PCM, 2, 48000, 480).unwrap();
    assert!(e2.on_data(0, 0, 0, 0, &[1, 2, 3]).is_err());
    assert!(e2.on_data(0, 0, 0, 0, &[]).is_err());
    let e3 = AudioEngine::new(None);
    e3.on_start(0, CODEC_PSA1, 2, 48000, 480).unwrap();
    e3.on_data(0, 0, 0, 0, &d).unwrap();
    e3.on_stop(0);
    assert!(e3.on_data(0, 0, 1, 0, &d).is_err());
    e3.on_start(0, CODEC_PSA1, 2, 48000, 480).unwrap();
    e3.on_data(0, 0, 0, 0, &d).unwrap();
}

#[test]
fn motor_mono_vira_estereo() {
    let e = AudioEngine::new(None);
    e.on_start(0, CODEC_PCM, 1, 48000, 480).unwrap();
    let mono: Vec<i16> = (1..=480).collect();
    let mut b = Vec::new();
    samples_to_pcm(&mono, &mut b);
    for seq in 0..4 {
        e.on_data(0, 0, seq, seq as u64 * 10_000, &b).unwrap();
    }
    let mut out = vec![0i16; 4];
    e.pull(&mut out);
    assert_eq!(out, [1, 1, 2, 2]);
}

#[test]
fn motor_conta_descarte_e_retomada_separados() {
    let e = AudioEngine::new(None);
    e.on_start(0, CODEC_PSA1, 2, 48000, 480).unwrap();
    let d = psa1(&senoide(480, 0.3));
    e.on_data(0, AUDIO_FLAG_DISCONTINUITY, 0, 0, &d).unwrap();
    e.on_data(0, 0, 1, 10_000, &d).unwrap();
    e.on_data(0, AUDIO_FLAG_DISCONTINUITY, 2, 2_000_000, &d).unwrap();
    e.on_data(0, AUDIO_FLAG_DISCONTINUITY | AUDIO_FLAG_DROPPED, 3, 2_010_000, &d).unwrap();
    let s = e.stats();
    assert_eq!((s.resumes, s.drops), (2, 1));
    e.reset_stats();
    let s = e.stats();
    assert_eq!((s.packets, s.resumes, s.drops, s.queue.underruns), (0, 0, 0, 0));
}

#[test]
fn recent_sound_pega_o_trecho_contiguo_mais_recente() {
    let e = AudioEngine::new(None);
    e.on_start(0, CODEC_PSA1, 2, 48000, 480).unwrap();
    let d = psa1(&senoide(480, 0.3));
    let silencio = psa1(&vec![0i16; 960]);
    for seq in 0..30u32 {
        e.on_data(0, if seq == 0 { AUDIO_FLAG_DISCONTINUITY } else { 0 }, seq, seq as u64 * 10_000, &d).unwrap();
    }
    for seq in 30..50u32 {
        e.on_data(0, 0, seq, seq as u64 * 10_000, &silencio).unwrap();
    }
    for k in 0..20u32 {
        e.on_data(0, if k == 0 { AUDIO_FLAG_DISCONTINUITY } else { 0 }, 50 + k, 1_700_000 + k as u64 * 10_000, &d).unwrap();
    }
    let (pkts, samples) = e.recent_sound();
    assert_eq!(pkts, 20);
    assert_eq!(samples.len(), 20 * 960);
}

#[test]
fn wav_tem_lacunas_de_silencio_nas_posicoes_certas() {
    let dir = std::env::temp_dir().join(format!("psc-wav-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("t.wav");
    {
        let mut w = WavDump::create(&path).unwrap();
        let mut c = 0;
        for k in 0..10u64 {
            w.write(k * 10_000 + (k % 3) * 400, &pkt(&mut c));
        }
        for k in 0..5u64 {
            w.write(1_100_000 + k * 10_000, &pkt(&mut c));
        }
        assert_eq!(w.frames(), 4800 + 48_000 + 2400);
    }
    let x = read_wav(&path).unwrap();
    assert_eq!(x.len() / 2, 4800 + 48_000 + 2400);
    let frame = |i: usize| (x[i * 2], x[i * 2 + 1]);
    assert_ne!(frame(10), (0, 0));
    assert_ne!(frame(4799), (0, 0));
    assert_eq!(frame(4800), (0, 0));
    assert_eq!(frame(4800 + 47_999), (0, 0));
    assert_ne!(frame(4800 + 48_000), (0, 0));
    std::fs::remove_dir_all(&dir).ok();
}

struct SaidaFalsa(Arc<std::sync::atomic::AtomicU32>);

impl AudioSink for SaidaFalsa {
    fn name(&self) -> &'static str {
        "falsa"
    }

    fn stop(&mut self) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}

#[test]
fn o_motor_para_a_saida_registrada_no_shutdown_e_ao_trocar_uma_vez_cada() {
    let paradas = Arc::new(std::sync::atomic::AtomicU32::new(0));
    let e = AudioEngine::new(None);
    e.attach_sink(Box::new(SaidaFalsa(paradas.clone())));
    e.attach_sink(Box::new(SaidaFalsa(paradas.clone())));
    assert_eq!(paradas.load(Ordering::SeqCst), 1, "a anterior foi parada ao trocar");
    e.shutdown();
    assert_eq!(paradas.load(Ordering::SeqCst), 2);
    e.shutdown();
    assert_eq!(paradas.load(Ordering::SeqCst), 2, "uma vez só");
    assert!(e.is_shutdown());
}

#[test]
fn pull_nowait_funciona_como_pull_e_nao_espera_a_trava() {
    let e = AudioEngine::new(None);
    let mut out = vec![1i16; 480 * 2];
    assert!(e.pull_nowait(&mut out));
    assert!(out.iter().all(|&s| s == 0), "fila vazia: silêncio");
    let g = e.queue.lock().unwrap();
    let mut out = vec![5i16; 480 * 2];
    let t0 = Instant::now();
    assert!(!e.pull_nowait(&mut out));
    assert!(t0.elapsed() < Duration::from_millis(50));
    assert!(out.iter().all(|&s| s == 0));
    drop(g);
}

#[test]
fn a_linha_audiostats_tem_todos_os_campos_do_psc() {
    let e = AudioEngine::new(None);
    let l = e.stats().audiostats_line();
    for campo in ["pacotes=", "quadros=", "kbit_s=", "retomadas=", "descartes=", "esvaziamentos=", "estouros=", "removidos=", "repetidos=", "recomecos=", "nivel_medio=", "nivel_max=", "nivel_agora=", "alvo=", "jitter=", "janela="] {
        assert!(l.contains(campo), "{campo} em {l}");
    }
    assert!(l.starts_with("AUDIOSTATS "));
}

fn samples_to_pcm_vec(s: &[i16]) -> Vec<u8> {
    s.iter().flat_map(|v| v.to_le_bytes()).collect()
}

#[test]
fn com_a_trava_da_rede_presa_o_pull_nowait_ainda_toca() {
    let e = AudioEngine::with_queue(QueueConfig::fixed_ms(40), None);
    e.on_start(0, CODEC_PCM, 2, 48000, 480).unwrap();
    let mut c = 0;
    for seq in 0..5u32 {
        e.on_data(0, if seq == 0 { AUDIO_FLAG_DISCONTINUITY } else { 0 }, seq, seq as u64 * 10_000, &samples_to_pcm_vec(&pkt(&mut c))).unwrap();
    }
    e.hold_net_for_test(|| {
        let mut out = vec![0i16; 480 * 2];
        assert!(e.pull_nowait(&mut out), "a trava da rede não é da fila");
        assert!(out.iter().any(|&s| s != 0), "com som");
    });
}

fn rodada_de_concorrencia() -> u32 {
    let e = AudioEngine::with_queue(QueueConfig::fixed_ms(40), None);
    e.on_start(0, CODEC_PCM, 2, 48000, 480).unwrap();
    let parar = Arc::new(AtomicBool::new(false));
    let enviados = Arc::new(std::sync::atomic::AtomicU32::new(0));
    let (e2, p2, n2) = (e.clone(), parar.clone(), enviados.clone());
    let rede = std::thread::spawn(move || {
        let mut c = 0;
        let mut seq = 0u32;
        while !p2.load(Ordering::Relaxed) {
            let d = samples_to_pcm_vec(&pkt(&mut c));
            e2.on_data(0, if seq == 0 { AUDIO_FLAG_DISCONTINUITY } else { 0 }, seq, seq as u64 * 10_000, &d).unwrap();
            seq += 1;
            n2.store(seq, Ordering::Relaxed);
            std::thread::yield_now();
        }
        seq
    });
    let mut out = vec![0i16; 480 * 2];
    let mut faltas = 0u32;
    let t0 = Instant::now();
    let mut n = 0u32;
    while (n < 20_000 || enviados.load(Ordering::Relaxed) < 500) && t0.elapsed() < Duration::from_secs(10) {
        if !e.pull_nowait(&mut out) {
            faltas += 1;
        }
        n += 1;
        std::thread::yield_now();
    }
    parar.store(true, Ordering::Relaxed);
    assert!(rede.join().unwrap() >= 500, "a thread de rede não rodou");
    faltas * 20_000 / n.max(1)
}

#[test]
fn on_data_em_sequencia_quase_nao_causa_faltas_no_pull_nowait() {
    let todas: Vec<u32> = (0..5).map(|_| rodada_de_concorrencia()).collect();
    let melhor = *todas.iter().min().unwrap();
    assert!(melhor < 20, "faltas na melhor rodada: {melhor} de 20000");
}
