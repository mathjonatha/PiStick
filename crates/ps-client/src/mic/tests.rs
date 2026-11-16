use std::sync::atomic::{AtomicU32, AtomicUsize};

use ps_audio::decode;

use super::*;

// ---------------------------------------------------------------------------------------------------------------
// Auxiliares
// ---------------------------------------------------------------------------------------------------------------

fn log_guardado() -> (LogFn, Arc<Mutex<Vec<String>>>) {
    let v = Arc::new(Mutex::new(Vec::<String>::new()));
    let v2 = v.clone();
    (Arc::new(move |m: &str| v2.lock().unwrap().push(m.to_string())), v)
}

fn tem_linha(v: &Arc<Mutex<Vec<String>>>, trecho: &str) -> bool {
    v.lock().unwrap().iter().any(|l| l.contains(trecho))
}

fn espera(mut cond: impl FnMut() -> bool, ate: Duration) -> bool {
    let fim = Instant::now() + ate;
    while Instant::now() < fim {
        if cond() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(2));
    }
    cond()
}

/// Captura de mentira: conta as chamadas, registra se o "dispositivo" foi liberado ao retornar e manda pacotes
/// constantes de amplitude `amp` a cada milissegundo até `stop`.
struct Falsa {
    chamadas: Arc<AtomicU32>,
    liberada: Arc<AtomicBool>,
    aberta: Arc<AtomicBool>,
}

fn falsa(falha: bool, amp: i16) -> (RunFn, Falsa) {
    let f = Falsa { chamadas: Arc::new(AtomicU32::new(0)), liberada: Arc::new(AtomicBool::new(true)), aberta: Arc::new(AtomicBool::new(false)) };
    let (c, l, a) = (f.chamadas.clone(), f.liberada.clone(), f.aberta.clone());
    let run: RunFn = Arc::new(move |stop, on_open, sink| {
        c.fetch_add(1, Ordering::SeqCst);
        if falha {
            return Err("sem dispositivo de gravação padrão (teste)".to_string());
        }
        l.store(false, Ordering::SeqCst);
        a.store(true, Ordering::SeqCst);
        on_open();
        let bloco = vec![amp; BLOCK_SAMPLES];
        while !stop.load(Ordering::SeqCst) {
            sink(&bloco);
            std::thread::sleep(Duration::from_millis(1));
        }
        // O dispositivo é liberado ANTES de retornar.
        a.store(false, Ordering::SeqCst);
        l.store(true, Ordering::SeqCst);
        Ok(())
    });
    (run, f)
}

fn mensagens(rx: &Receiver<Message>) -> Vec<Message> {
    let mut v = Vec::new();
    while let Ok(m) = rx.try_recv() {
        v.push(m);
    }
    v
}

fn wav_bytes(rate: u32, canais: u16, amostras: &[i16]) -> Vec<u8> {
    let dados = amostras.len() * 2;
    let mut b = Vec::new();
    b.extend_from_slice(b"RIFF");
    b.extend_from_slice(&(36 + dados as u32).to_le_bytes());
    b.extend_from_slice(b"WAVEfmt ");
    b.extend_from_slice(&16u32.to_le_bytes());
    b.extend_from_slice(&1u16.to_le_bytes());
    b.extend_from_slice(&canais.to_le_bytes());
    b.extend_from_slice(&rate.to_le_bytes());
    b.extend_from_slice(&(rate * u32::from(canais) * 2).to_le_bytes());
    b.extend_from_slice(&(canais * 2).to_le_bytes());
    b.extend_from_slice(&16u16.to_le_bytes());
    b.extend_from_slice(b"data");
    b.extend_from_slice(&(dados as u32).to_le_bytes());
    for s in amostras {
        b.extend_from_slice(&s.to_le_bytes());
    }
    b
}

fn arquivo_temp(nome: &str, bytes: &[u8]) -> PathBuf {
    static N: AtomicUsize = AtomicUsize::new(0);
    let p = std::env::temp_dir().join(format!("psc-mic-teste-{}-{}-{nome}", std::process::id(), N.fetch_add(1, Ordering::SeqCst)));
    std::fs::write(&p, bytes).unwrap();
    p
}

// ---------------------------------------------------------------------------------------------------------------
// Decisão de privacidade
// ---------------------------------------------------------------------------------------------------------------

#[test]
fn sem_mic_nada_e_anunciado_em_nenhuma_conexao() {
    let o = MicOptions::default();
    assert!(!o.wanted());
    for link in [LinkTrust::Untrusted, LinkTrust::Loopback, LinkTrust::Paired] {
        assert_eq!(decide(&o, link), Decision::Off, "{link:?}");
    }
}

#[test]
fn com_mic_so_anuncia_em_sessao_cifrada_pareada_ou_laco_local() {
    let o = MicOptions { enabled: true, file: None };
    assert_eq!(decide(&o, LinkTrust::Paired), Decision::Announce);
    assert_eq!(decide(&o, LinkTrust::Loopback), Decision::Announce);
    // `--insecure` fora do laço (texto claro), ou conexão desconhecida: não anuncia, mesmo com --mic.
    assert!(matches!(decide(&o, LinkTrust::Untrusted), Decision::Blocked(_)));
    // O padrão de `LinkTrust` é o mais seguro.
    assert_eq!(LinkTrust::default(), LinkTrust::Untrusted);
    assert!(matches!(decide(&o, LinkTrust::default()), Decision::Blocked(_)));
}

#[test]
fn mic_file_implica_mic_e_segue_a_mesma_regra() {
    let o = MicOptions { enabled: false, file: Some(PathBuf::from("x.wav")) };
    assert!(o.wanted());
    assert_eq!(decide(&o, LinkTrust::Paired), Decision::Announce);
    assert_eq!(decide(&o, LinkTrust::Loopback), Decision::Announce);
    assert!(matches!(decide(&o, LinkTrust::Untrusted), Decision::Blocked(_)));
}

#[test]
fn plano_sem_mic_nao_cria_captura_e_nao_registra_nada() {
    let (log, linhas) = log_guardado();
    let p = plan(&MicOptions::default(), LinkTrust::Paired, &log).unwrap();
    assert!(!p.announce);
    assert!(p.run.is_none(), "sem --mic não existe nem a função de captura");
    assert!(linhas.lock().unwrap().is_empty());
}

#[test]
fn plano_bloqueado_registra_o_motivo_e_nao_anuncia() {
    let (log, linhas) = log_guardado();
    let p = plan(&MicOptions { enabled: true, file: None }, LinkTrust::Untrusted, &log).unwrap();
    assert!(!p.announce);
    assert!(p.run.is_none());
    assert!(tem_linha(&linhas, "NÃO anunciado"), "{:?}", linhas.lock().unwrap());
    assert!(tem_linha(&linhas, "--insecure"));
}

#[test]
fn plano_com_arquivo_anuncia_sem_abrir_dispositivo() {
    let (log, linhas) = log_guardado();
    let p = arquivo_temp("tom.wav", &wav_bytes(48_000, 2, &vec![100i16; 2 * 4800]));
    let pl = plan(&MicOptions { enabled: false, file: Some(p.clone()) }, LinkTrust::Loopback, &log).unwrap();
    assert!(pl.announce);
    assert!(pl.run.is_some());
    assert!(tem_linha(&linhas, "nenhum dispositivo de captura será aberto"));
    let _ = std::fs::remove_file(p);
}

#[test]
fn arquivo_invalido_e_erro_do_plano() {
    let (log, _) = log_guardado();
    let ruim = arquivo_temp("ruim.wav", b"isto nao e um wav");
    assert!(plan(&MicOptions { enabled: true, file: Some(ruim.clone()) }, LinkTrust::Loopback, &log).is_err());
    let _ = std::fs::remove_file(ruim);
    assert!(plan(&MicOptions { enabled: true, file: Some(PathBuf::from("nao-existe-mesmo.wav")) }, LinkTrust::Loopback, &log).is_err());
}

// ---------------------------------------------------------------------------------------------------------------
// WAV em laço
// ---------------------------------------------------------------------------------------------------------------

#[test]
fn wav_so_aceita_48k_e_um_ou_dois_canais_e_mono_vira_estereo() {
    let a = arquivo_temp("a.wav", &wav_bytes(44_100, 2, &vec![1i16; 2 * 1000]));
    assert!(load_wav(&a).unwrap_err().contains("44100"));
    let b = arquivo_temp("b.wav", &wav_bytes(48_000, 1, &(0..1000).map(|i| i as i16).collect::<Vec<_>>()));
    let w = load_wav(&b).unwrap();
    assert_eq!(w.frames(), 1000);
    let mut cur = 0;
    let mut out = vec![0i16; BLOCK_SAMPLES];
    w.fill(&mut cur, &mut out);
    assert_eq!(&out[..6], &[0, 0, 1, 1, 2, 2], "o mono é duplicado nos dois canais");
    let c = arquivo_temp("c.wav", &wav_bytes(48_000, 2, &vec![1i16; 2 * 100]));
    assert!(load_wav(&c).unwrap_err().contains("curto"));
    for p in [a, b, c] {
        let _ = std::fs::remove_file(p);
    }
}

#[test]
fn o_laco_do_wav_volta_ao_comeco_sem_perder_amostra() {
    let amostras: Vec<i16> = (0..2 * 700).map(|i| i as i16).collect();
    let w = WavLoop::from_stereo(amostras.clone()).unwrap();
    let mut cur = 0;
    let mut tudo = Vec::new();
    let mut bloco = vec![0i16; BLOCK_SAMPLES];
    for _ in 0..5 {
        w.fill(&mut cur, &mut bloco);
        tudo.extend_from_slice(&bloco);
    }
    for (i, &s) in tudo.iter().enumerate() {
        assert_eq!(s, amostras[i % amostras.len()], "amostra {i}");
    }
}

#[test]
fn captura_de_arquivo_anda_em_tempo_real_e_para_logo() {
    let w = WavLoop::from_stereo(vec![7i16; 2 * 4800]).unwrap();
    let run = file_run(w);
    let stop = Arc::new(AtomicBool::new(false));
    let (abriu, blocos) = (Arc::new(AtomicU32::new(0)), Arc::new(AtomicU32::new(0)));
    let (s2, a2, b2) = (stop.clone(), abriu.clone(), blocos.clone());
    let h = std::thread::spawn(move || run(&s2, &mut || { a2.fetch_add(1, Ordering::SeqCst); }, &mut |blk: &[i16]| {
        assert_eq!(blk.len(), BLOCK_SAMPLES);
        b2.fetch_add(1, Ordering::SeqCst);
    }));
    std::thread::sleep(Duration::from_millis(305));
    let n = blocos.load(Ordering::SeqCst);
    let t0 = Instant::now();
    stop.store(true, Ordering::SeqCst);
    h.join().unwrap().unwrap();
    assert!(t0.elapsed() < Duration::from_millis(100), "parou em {:?}", t0.elapsed());
    assert_eq!(abriu.load(Ordering::SeqCst), 1);
    // 10 ms por bloco: cerca de 30 blocos em 305 ms (margem larga para máquina ocupada).
    assert!((20..=34).contains(&n), "blocos em 305 ms: {n}");
}

// ---------------------------------------------------------------------------------------------------------------
// A máquina de pedidos
// ---------------------------------------------------------------------------------------------------------------

#[test]
fn sem_pedido_a_captura_nunca_abre() {
    let (run, f) = falsa(false, 100);
    let (tx, rx) = mpsc::channel();
    let (log, _) = log_guardado();
    let mic = Mic::new(Some(run), true, tx, log);
    std::thread::sleep(Duration::from_millis(100));
    assert_eq!(f.chamadas.load(Ordering::SeqCst), 0, "o dispositivo só abre depois de um pedido");
    assert!(!mic.is_open());
    mic.shutdown();
    assert_eq!(f.chamadas.load(Ordering::SeqCst), 0);
    assert!(mensagens(&rx).is_empty(), "nenhuma mensagem sai sem pedido");
    let r = mic.report();
    assert_eq!((r.packets, r.requests, r.offs, r.peak), (0, 0, 0, 0));
}

#[test]
fn sem_cap_mic_o_pedido_e_contado_e_ignorado() {
    let (run, f) = falsa(false, 100);
    let (tx, rx) = mpsc::channel();
    let (log, linhas) = log_guardado();
    // `run` existe, mas o microfone não foi anunciado: nenhuma thread, nenhuma captura.
    let mic = Mic::new(Some(run), false, tx, log);
    mic.request(true);
    mic.request(false);
    std::thread::sleep(Duration::from_millis(60));
    assert_eq!(f.chamadas.load(Ordering::SeqCst), 0);
    assert!(mensagens(&rx).is_empty());
    assert!(tem_linha(&linhas, "sem o microfone anunciado"));
    let r = mic.report();
    assert_eq!((r.requests, r.offs, r.packets, r.announced), (1, 1, 0, false));
    assert!(r.line().contains("pedidos=1") && r.line().contains("pacotes=0") && r.line().contains("anunciou=0"), "{}", r.line());
}

#[test]
fn pedido_liga_manda_start_e_dados_e_o_desligar_libera_antes_do_stop() {
    let (run, f) = falsa(false, 1000);
    let (tx, rx) = mpsc::channel();
    let (log, linhas) = log_guardado();
    let mic = Mic::new(Some(run), true, tx, log);
    mic.request(true);
    assert!(espera(|| mic.report().packets >= 5, Duration::from_secs(3)));
    assert!(mic.is_open());
    assert!(f.aberta.load(Ordering::SeqCst));
    mic.request(false);
    // O AUDIO_STOP só chega depois de o dispositivo ser liberado.
    let mut msgs = Vec::new();
    let fim = Instant::now() + Duration::from_secs(3);
    while Instant::now() < fim && !msgs.iter().any(|m| matches!(m, Message::AudioStop { .. })) {
        if let Ok(m) = rx.recv_timeout(Duration::from_millis(20)) {
            msgs.push(m);
        }
    }
    assert!(f.liberada.load(Ordering::SeqCst), "o dispositivo foi liberado");
    assert!(!mic.is_open());
    assert!(matches!(msgs[0], Message::AudioStart { stream: 1, codec: 1, channels: 2, rate: 48_000, frames: 480 }), "{:?}", msgs[0]);
    let mut seq = 0u32;
    for m in &msgs[1..msgs.len() - 1] {
        let Message::AudioData { stream, seq: s, data, flags, .. } = m else { panic!("esperava AUDIO_DATA, veio {m:?}") };
        assert_eq!((*stream, *s), (1, seq));
        assert_eq!(*flags & AUDIO_FLAG_DISCONTINUITY != 0, seq == 0);
        let mut pcm = Vec::new();
        decode(data, 2, &mut pcm).unwrap();
        assert_eq!(pcm, vec![1000i16; BLOCK_SAMPLES]);
        seq += 1;
    }
    assert!(seq >= 5);
    assert!(matches!(msgs.last().unwrap(), Message::AudioStop { stream: 1 }));
    assert!(tem_linha(&linhas, "captura LIGADA"));
    assert!(tem_linha(&linhas, "dispositivo de captura ABERTO"));
    assert!(tem_linha(&linhas, "captura DESLIGADA"));
    let r = mic.report();
    assert_eq!((r.requests, r.offs, r.peak, r.open), (1, 1, 1000, false));
    assert!(r.line().starts_with("MICSTATS pacotes="), "{}", r.line());
    assert!(r.line().contains("pico=1000") && r.line().contains("captura=fechada") && r.line().contains("anunciou=1"));
}

#[test]
fn dois_pedidos_de_ligar_seguidos_abrem_uma_vez_so() {
    let (run, f) = falsa(false, 5);
    let (tx, _rx) = mpsc::channel();
    let (log, _) = log_guardado();
    let mic = Mic::new(Some(run), true, tx, log);
    mic.request(true);
    mic.request(true);
    assert!(espera(|| mic.is_open(), Duration::from_secs(3)));
    std::thread::sleep(Duration::from_millis(50));
    assert_eq!(f.chamadas.load(Ordering::SeqCst), 1);
    mic.shutdown();
    assert!(f.liberada.load(Ordering::SeqCst));
}

#[test]
fn desligar_sem_captura_nao_faz_nada() {
    let (run, f) = falsa(false, 5);
    let (tx, rx) = mpsc::channel();
    let (log, _) = log_guardado();
    let mic = Mic::new(Some(run), true, tx, log);
    mic.request(false);
    std::thread::sleep(Duration::from_millis(50));
    assert_eq!(f.chamadas.load(Ordering::SeqCst), 0);
    assert!(mensagens(&rx).is_empty());
    assert_eq!(mic.report().offs, 1);
}

#[test]
fn sem_dispositivo_responde_com_audio_stop_imediato_e_tenta_de_novo_no_pedido_seguinte() {
    let (run, f) = falsa(true, 5);
    let (tx, rx) = mpsc::channel();
    let (log, linhas) = log_guardado();
    let mic = Mic::new(Some(run), true, tx, log);
    mic.request(true);
    let m = rx.recv_timeout(Duration::from_secs(3)).expect("AUDIO_STOP imediato");
    assert!(matches!(m, Message::AudioStop { stream: 1 }), "{m:?}");
    assert!(tem_linha(&linhas, "sem captura"));
    assert!(!mic.is_open());
    // Nenhum AUDIO_START nem AUDIO_DATA foi enviado.
    assert!(mensagens(&rx).is_empty());
    // Um novo pedido (gravador reabriu) tenta de novo.
    mic.request(false);
    mic.request(true);
    assert!(espera(|| f.chamadas.load(Ordering::SeqCst) == 2, Duration::from_secs(3)));
    mic.shutdown();
}

#[test]
fn fim_da_sessao_libera_o_dispositivo() {
    let (run, f) = falsa(false, 5);
    let (tx, rx) = mpsc::channel();
    let (log, linhas) = log_guardado();
    let mic = Mic::new(Some(run), true, tx, log);
    mic.request(true);
    assert!(espera(|| mic.is_open(), Duration::from_secs(3)));
    mic.shutdown();
    assert!(f.liberada.load(Ordering::SeqCst), "shutdown libera o dispositivo antes de voltar");
    assert!(!mic.is_open());
    assert!(tem_linha(&linhas, "fim da sessão"));
    assert!(mensagens(&rx).iter().any(|m| matches!(m, Message::AudioStop { .. })));
    // Chamar de novo é inofensivo; pedidos depois do fim são só contados.
    mic.shutdown();
    mic.request(true);
    assert_eq!(f.chamadas.load(Ordering::SeqCst), 1);
}

#[test]
fn largar_o_mic_tambem_libera() {
    let (run, f) = falsa(false, 5);
    let (tx, _rx) = mpsc::channel();
    let (log, _) = log_guardado();
    let mic = Mic::new(Some(run), true, tx, log);
    mic.request(true);
    assert!(espera(|| f.aberta.load(Ordering::SeqCst), Duration::from_secs(3)));
    drop(mic);
    assert!(f.liberada.load(Ordering::SeqCst));
}

#[test]
fn conexao_fechada_para_a_captura_sozinha() {
    let (run, f) = falsa(false, 5);
    let (tx, rx) = mpsc::channel();
    let (log, _) = log_guardado();
    let mic = Mic::new(Some(run), true, tx, log);
    mic.request(true);
    assert!(espera(|| mic.is_open(), Duration::from_secs(3)));
    // A fila de saída da conexão sumiu (escritor encerrou): a captura percebe no próximo envio e libera.
    drop(rx);
    assert!(espera(|| f.liberada.load(Ordering::SeqCst), Duration::from_secs(3)), "a captura deve parar quando a conexão some");
}

#[test]
fn zerar_as_estatisticas_comeca_uma_janela_nova() {
    let (run, _f) = falsa(false, 300);
    let (tx, _rx) = mpsc::channel();
    let (log, _) = log_guardado();
    let mic = Mic::new(Some(run), true, tx, log);
    mic.request(true);
    assert!(espera(|| mic.report().packets >= 3, Duration::from_secs(3)));
    mic.request(false);
    assert!(espera(|| !mic.is_open(), Duration::from_secs(3)));
    mic.reset_stats();
    let r = mic.report();
    assert_eq!((r.packets, r.requests, r.offs, r.peak), (0, 0, 0, 0));
    assert!(r.secs < 1.0);
}

#[test]
fn o_relogio_do_registro_tem_formato_de_hora() {
    let h = utc_clock();
    assert_eq!(h.len(), 13, "{h}");
    assert!(h.ends_with('Z') && h.as_bytes()[2] == b':' && h.as_bytes()[8] == b'.');
}
