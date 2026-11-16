use std::io::Write;
use std::net::TcpListener;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use ps_audio::{decode, samples_to_pcm};
use ps_proto::{AUDIO_FLAG_DISCONTINUITY, AUDIO_FLAG_DROPPED};

use super::*;

fn waker() -> (Waker, Arc<AtomicU32>) {
    let n = Arc::new(AtomicU32::new(0));
    let n2 = n.clone();
    (Arc::new(move || {
        n2.fetch_add(1, Ordering::SeqCst);
    }), n)
}

fn tone(amp: i16) -> Vec<i16> {
    (0..BLOCK_FRAMES * CHANNELS).map(|i| if i % 4 < 2 { amp } else { -amp }).collect()
}

fn silence() -> Vec<i16> {
    vec![0; BLOCK_FRAMES * CHANNELS]
}

#[test]
fn sem_inscricao_o_bloco_e_jogado_fora() {
    let hub = AudioHub::new(AudioCodec::Pcm);
    hub.push_block(&tone(1000), 0);
    let (w, n) = waker();
    let sub = hub.subscribe(w);
    let mut v = Vec::new();
    sub.drain(&mut v);
    assert!(v.is_empty());
    assert_eq!(n.load(Ordering::SeqCst), 0);
}

#[test]
fn pacotes_chegam_em_ordem_com_o_codec_escolhido() {
    for codec in [AudioCodec::Pcm, AudioCodec::Psa1] {
        let hub = AudioHub::new(codec);
        let (w, n) = waker();
        let sub = hub.subscribe(w);
        let blocos: Vec<Vec<i16>> = (1..=5).map(|k| tone(k * 1000)).collect();
        for (i, b) in blocos.iter().enumerate() {
            hub.push_block(b, i as u64 * 10_000);
        }
        assert_eq!(n.load(Ordering::SeqCst), 5, "um aviso por pacote");
        let mut v = Vec::new();
        sub.drain(&mut v);
        assert_eq!(v.len(), 5);
        for (i, (p, b)) in v.iter().zip(&blocos).enumerate() {
            assert_eq!(p.pts_us, i as u64 * 10_000);
            let mut out = Vec::new();
            match codec {
                AudioCodec::Pcm => ps_audio::pcm_to_samples(&p.data, &mut out),
                AudioCodec::Psa1 => {
                    decode(&p.data, CHANNELS, &mut out).unwrap();
                }
            }
            assert_eq!(&out, b, "{codec:?} bloco {i}");
            // Só o primeiro pacote do fluxo leva a marca (não havia envio antes).
            assert_eq!(p.flags, if i == 0 { AUDIO_FLAG_DISCONTINUITY } else { 0 });
        }
    }
}

#[test]
fn silencio_para_depois_de_200_ms_e_volta_com_marca() {
    let hub = AudioHub::new(AudioCodec::Pcm);
    let (w, _) = waker();
    let sub = hub.subscribe(w);
    hub.push_block(&tone(500), 0);
    let mut v = Vec::new();
    // 20 blocos de silêncio ainda são enviados (200 ms); do 21º em diante, nada.
    for i in 0..40 {
        hub.push_block(&silence(), 10_000 * (1 + i));
        sub.drain(&mut v);
    }
    assert_eq!(v.len(), 1 + SILENCE_HANG_BLOCKS as usize);
    // Volta o som.
    hub.push_block(&tone(500), 500_000);
    let mut v = Vec::new();
    sub.drain(&mut v);
    assert_eq!(v.len(), 1);
    assert_eq!(v[0].flags, AUDIO_FLAG_DISCONTINUITY, "retomada depois do silêncio");
    assert_eq!(v[0].pts_us, 500_000);
    // Silêncio curto (abaixo de 200 ms) entre sons não gera marca.
    for i in 0..5 {
        hub.push_block(&silence(), 510_000 + 10_000 * i);
    }
    hub.push_block(&tone(500), 600_000);
    let mut v = Vec::new();
    sub.drain(&mut v);
    assert_eq!(v.len(), 6);
    assert!(v.iter().all(|p| p.flags == 0));
}

#[test]
fn mais_de_200_ms_na_fila_descarta_os_mais_velhos_com_marca() {
    let hub = AudioHub::new(AudioCodec::Pcm);
    let (w, _) = waker();
    let sub = hub.subscribe(w);
    for i in 0..35u64 {
        hub.push_block(&tone(300), i * 10_000);
    }
    let mut v = Vec::new();
    sub.drain(&mut v);
    assert_eq!(v.len(), MAX_QUEUED_PACKETS);
    assert_eq!(sub.dropped(), 15);
    // Sobraram os 20 mais novos, e o primeiro leva as marcas de descontinuidade e descarte.
    assert_eq!(v[0].pts_us, 15 * 10_000);
    assert_eq!(v[0].flags, AUDIO_FLAG_DISCONTINUITY | AUDIO_FLAG_DROPPED);
    assert!(v[1..].iter().all(|p| p.flags == 0));
}

#[test]
fn nova_inscricao_zera_a_fila_e_a_antiga_nao_atrapalha() {
    let hub = AudioHub::new(AudioCodec::Pcm);
    let (w1, _) = waker();
    let sub1 = hub.subscribe(w1);
    hub.push_block(&tone(100), 0);
    let (w2, _) = waker();
    let sub2 = hub.subscribe(w2);
    let mut v = Vec::new();
    sub2.drain(&mut v);
    assert!(v.is_empty());
    // Largar a inscrição antiga não derruba a nova.
    drop(sub1);
    assert!(hub.has_subscriber());
    hub.push_block(&tone(100), 10_000);
    sub2.drain(&mut v);
    assert_eq!(v.len(), 1);
    drop(sub2);
    assert!(!hub.has_subscriber());
}

#[test]
fn sintetico_gera_som_e_silencio_em_tempo_real() {
    let hub = AudioHub::new(AudioCodec::Psa1);
    let h = spawn_synthetic(hub.clone());
    let (w, _) = waker();
    let sub = hub.subscribe(w);
    let t0 = Instant::now();
    let mut pkts = Vec::new();
    while t0.elapsed() < Duration::from_millis(1500) {
        sub.drain(&mut pkts);
        std::thread::sleep(Duration::from_millis(5));
    }
    sub.drain(&mut pkts);
    drop(sub);
    hub.stop();
    h.join().unwrap();
    // Em 1,5 s de relógio: 1 s de som (100 pacotes) mais 200 ms de silêncio enviado (20) e depois nada.
    assert!((115..=125).contains(&pkts.len()), "pacotes: {}", pkts.len());
    let mut out = Vec::new();
    for p in &pkts[..100] {
        decode(&p.data, CHANNELS, &mut out).unwrap();
    }
    let (l, r) = ps_audio::synth::split_stereo(&out);
    assert!((ps_audio::synth::goertzel(&l, 440.0) - ps_audio::synth::AMPLITUDE).abs() < 0.01);
    assert!((ps_audio::synth::goertzel(&r, 880.0) - ps_audio::synth::AMPLITUDE).abs() < 0.01);
}

/// Servidor local de PCM que escreve em pedaços de tamanho ímpar, para provar o fechamento dos blocos.
fn pcm_server(chunks: Vec<Vec<u8>>, keep_open: Duration) -> (String, std::thread::JoinHandle<()>) {
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = l.local_addr().unwrap().to_string();
    let h = std::thread::spawn(move || {
        let (mut s, _) = l.accept().unwrap();
        s.set_nodelay(true).unwrap();
        for c in chunks {
            s.write_all(&c).unwrap();
            std::thread::sleep(Duration::from_millis(2));
        }
        std::thread::sleep(keep_open);
    });
    (addr, h)
}

#[test]
fn leitor_de_pcm_fecha_blocos_de_10_ms_mesmo_com_pedacos_ímpares() {
    let hub = AudioHub::new(AudioCodec::Pcm);
    let (w, _) = waker();
    let sub = hub.subscribe(w);
    // 6 blocos distintos, entregues em pedaços de 999 bytes.
    let mut bytes = Vec::new();
    let mut esperado = Vec::new();
    for k in 1..=6i16 {
        let b = tone(k * 700);
        samples_to_pcm(&b, &mut bytes);
        esperado.push(b);
    }
    let chunks: Vec<Vec<u8>> = bytes.chunks(999).map(|c| c.to_vec()).collect();
    let (addr, server) = pcm_server(chunks, Duration::from_millis(300));
    let logs = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
    let l2 = logs.clone();
    let reader = spawn_pcm_reader(hub.clone(), addr, Arc::new(move |m| l2.lock().unwrap().push(m.to_string())));
    let mut v = Vec::new();
    let t0 = Instant::now();
    while v.len() < 6 && t0.elapsed() < Duration::from_secs(3) {
        sub.drain(&mut v);
        std::thread::sleep(Duration::from_millis(5));
    }
    hub.stop();
    drop(sub);
    reader.join().unwrap();
    server.join().unwrap();
    assert_eq!(v.len(), 6);
    for (p, b) in v.iter().zip(&esperado) {
        let mut out = Vec::new();
        ps_audio::pcm_to_samples(&p.data, &mut out);
        assert_eq!(&out, b);
    }
}

#[test]
fn leitor_de_pcm_reconecta_quando_o_servidor_demora_ou_cai() {
    let hub = AudioHub::new(AudioCodec::Pcm);
    let (w, _) = waker();
    let sub = hub.subscribe(w);
    // Escolhe uma porta, a libera e só abre o servidor depois: as primeiras tentativas falham.
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = l.local_addr().unwrap().to_string();
    drop(l);
    let logs = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
    let l2 = logs.clone();
    let reader = spawn_pcm_reader(hub.clone(), addr.clone(), Arc::new(move |m| l2.lock().unwrap().push(m.to_string())));
    std::thread::sleep(Duration::from_millis(700));
    let l = TcpListener::bind(&addr).unwrap();
    let server = std::thread::spawn(move || {
        // Primeira conexão: um bloco e queda; segunda: um bloco.
        for k in [1i16, 2] {
            let (mut s, _) = l.accept().unwrap();
            let mut b = Vec::new();
            samples_to_pcm(&tone(k * 1000), &mut b);
            s.write_all(&b).unwrap();
            std::thread::sleep(Duration::from_millis(50));
        }
    });
    let mut v = Vec::new();
    let t0 = Instant::now();
    while v.len() < 2 && t0.elapsed() < Duration::from_secs(5) {
        sub.drain(&mut v);
        std::thread::sleep(Duration::from_millis(5));
    }
    hub.stop();
    drop(sub);
    reader.join().unwrap();
    server.join().unwrap();
    assert_eq!(v.len(), 2, "logs: {:?}", logs.lock().unwrap());
    // Depois da reconexão o pacote leva a marca de descontinuidade.
    assert_eq!(v[1].flags & AUDIO_FLAG_DISCONTINUITY, AUDIO_FLAG_DISCONTINUITY);
    let logs = logs.lock().unwrap();
    assert!(logs.iter().any(|m| m.contains("sem conexão")), "{logs:?}");
    assert!(logs.iter().any(|m| m.contains("conectado")), "{logs:?}");
}
