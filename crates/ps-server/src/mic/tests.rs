use std::io::Read;
use std::net::TcpListener;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::mpsc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use ps_audio::{encode, samples_to_pcm, synth};

use super::*;

const DESPEJO: &str = "\
= pistick-mic:capture_FL
=   |-> pw-record:input_FL
= pistick-mic:capture_FR
=   |-> pw-record:input_FR
= pw-record:input_FL
=   |<- pistick-mic:capture_FL
= pw-record:input_FR
=   |<- pistick-mic:capture_FR
";
const SEGUNDO_ENTRA: &str = "\
+ pw-record:input_FL
+ pw-record:monitor_FL
+ pw-record:input_FR
+ pw-record:monitor_FR
+ pistick-mic:capture_FL -> pw-record:input_FL
+ pistick-mic:capture_FR -> pw-record:input_FR
";
const PRIMEIRO_SAI: &str = "\
- pistick-mic:capture_FL -> pw-record:input_FL
- pw-record:input_FL
- pistick-mic:capture_FR -> pw-record:input_FR
- pw-record:input_FR
- pw-record:monitor_FL
- pw-record:monitor_FR
";

fn alimenta(t: &mut LinkTracker, texto: &str) -> Vec<bool> {
    texto.lines().map(|l| t.feed_line(l)).collect()
}

#[test]
fn despejo_inicial_com_gravador_conta_como_gravando() {
    let mut t = LinkTracker::new();
    assert!(!t.recording());
    let mudou = alimenta(&mut t, DESPEJO);
    assert!(t.recording());
    assert_eq!(t.consumers(), vec!["pw-record".to_string()]);
    assert!(mudou.contains(&true));
    assert_eq!(t.counts.values().sum::<u32>(), 2);
}

#[test]
fn dois_gravadores_com_o_mesmo_nome_so_apagam_um_por_vez() {
    let mut t = LinkTracker::new();
    alimenta(&mut t, DESPEJO);
    alimenta(&mut t, SEGUNDO_ENTRA);
    assert_eq!(t.counts.values().sum::<u32>(), 4);
    alimenta(&mut t, PRIMEIRO_SAI);
    assert!(t.recording(), "o segundo gravador continua ligado");
    assert_eq!(t.counts.values().sum::<u32>(), 2);
    alimenta(&mut t, PRIMEIRO_SAI);
    assert!(!t.recording());
    assert!(t.consumers().is_empty());
}

#[test]
fn portas_e_outras_ligacoes_nao_contam() {
    let mut t = LinkTracker::new();
    for l in [
        "+ pistick:monitor_FL",
        "+ pistick:monitor_FL -> pistick-saida:input_FL",
        "+ pistick-mic-escrita:receive_FL -> pistick-mic-entrada:playback_FL",
        "+ pistick-mic:capture_FL",
        "+ pistick-mic-entrada:capture_FL -> loop:input_FL",
        "= pistick:monitor_FL",
        "=   |-> pistick-saida:input_FL",
        "",
        "lixo qualquer",
    ] {
        assert!(!t.feed_line(l), "linha {l:?}");
    }
    assert!(!t.recording());
}

#[test]
fn nomes_com_espacos_e_crlf_funcionam() {
    let mut t = LinkTracker::new();
    assert!(t.feed_line("+ pistick-mic:capture_FL -> Chromium input:input_FL\r"));
    assert_eq!(t.consumers(), vec!["Chromium input".to_string()]);
    assert!(t.feed_line("- pistick-mic:capture_FL -> Chromium input:input_FL\r"));
    assert!(!t.recording());
    assert!(!t.feed_line("- pistick-mic:capture_FL -> Chromium input:input_FL"));
}

#[test]
fn reset_esquece_o_estado() {
    let mut t = LinkTracker::new();
    alimenta(&mut t, DESPEJO);
    t.reset();
    assert!(!t.recording());
    assert!(!t.feed_line("=   |-> pw-record:input_FL"));
}

#[test]
fn analisador_aguenta_entrada_arbitraria() {
    let mut t = LinkTracker::new();
    let mut x = 12345u32;
    for _ in 0..5000 {
        let mut linha = String::new();
        for _ in 0..(x % 40) {
            x = x.wrapping_mul(1103515245).wrapping_add(12345);
            let c = (*b"+-= >|:api-")[(x >> 16) as usize % 11];
            linha.push(c as char);
        }
        t.feed_line(&linha);
    }
}

#[test]
fn porta_liga_na_hora_e_desliga_depois_da_espera() {
    let t0 = Instant::now();
    let mut g = RequestGate::new();
    assert_eq!(g.update(false, t0), None);
    assert_eq!(g.update(true, t0), Some(true));
    assert_eq!(g.update(true, t0 + Duration::from_secs(1)), None);
    assert_eq!(g.update(false, t0 + Duration::from_secs(2)), None);
    assert_eq!(g.next_deadline(), Some(t0 + Duration::from_secs(2) + OFF_DELAY));
    assert_eq!(g.update(false, t0 + Duration::from_secs(2) + OFF_DELAY - Duration::from_millis(1)), None);
    assert_eq!(g.update(true, t0 + Duration::from_secs(2) + Duration::from_millis(300)), None);
    assert_eq!(g.next_deadline(), None);
    assert_eq!(g.update(false, t0 + Duration::from_secs(3)), None);
    assert_eq!(g.update(false, t0 + Duration::from_secs(3) + OFF_DELAY), Some(false));
    assert!(!g.is_on());
    assert_eq!(g.update(false, t0 + Duration::from_secs(9)), None);
    assert_eq!(g.update(true, t0 + Duration::from_secs(9)), Some(true));
}

fn nova() -> MicQueue {
    MicQueue::with_max_ms(150)
}

fn bloco_de(valor: i16) -> Vec<i16> {
    vec![valor; BLOCK_FRAMES * CHANNELS]
}

fn fila_cheia(n: u64) -> MicQueue {
    let mut q = nova();
    for _ in 0..n {
        q.push(&bloco_de(1000), 0);
    }
    q
}

#[test]
fn fila_constante_nao_esvazia_nem_descarta_e_o_atraso_do_comeco_encolhe_aos_poucos() {
    let mut q = fila_cheia(15);
    let mut out = vec![0i16; BLOCK_FRAMES * CHANNELS];
    let mut cheios = 0;
    for i in 1..=6000u64 {
        q.push(&bloco_de(1000), i * 10_000);
        if q.pop_block(&mut out) == Pop::Full {
            cheios += 1;
        }
    }
    let s = q.stats();
    assert_eq!(s.underruns, 0);
    assert_eq!(s.dropped_frames, 0, "nada é jogado fora de uma vez");
    assert_eq!(cheios, 6000);
    assert!(s.skipped_frames > 0, "o excesso do começo foi consumido aos poucos");
    assert!(q.target_ms() <= 30.0, "alvo {}", q.target_ms());
    assert!(q.level_ms() <= 40.0, "nível {}", q.level_ms());
    let mut q = nova();
    for i in 0..550u64 {
        q.push(&bloco_de(1), i * 10_000);
    }
    assert!(q.target_ms() >= 150.0, "alvo no aquecimento {}", q.target_ms());
}

#[test]
fn fila_sem_dados_entrega_silencio_e_espera_encher_ate_o_alvo() {
    let mut q = nova();
    let mut out = vec![7i16; BLOCK_FRAMES * CHANNELS];
    assert_eq!(q.pop_block(&mut out), Pop::Silence);
    assert!(out.iter().all(|&s| s == 0));
    for i in 0..14u64 {
        q.push(&bloco_de(500), i * 10_000);
        assert_eq!(q.pop_block(&mut out), Pop::Silence, "bloco {i}");
    }
    q.push(&bloco_de(500), 140_000);
    assert_eq!(q.pop_block(&mut out), Pop::Full);
    assert!(out.iter().all(|&s| s == 500));
}

#[test]
fn fila_esvaziada_completa_com_zeros_e_reenche() {
    let mut out = vec![0i16; BLOCK_FRAMES * CHANNELS];
    let mut q = fila_cheia(15);
    for _ in 0..15 {
        assert_eq!(q.pop_block(&mut out), Pop::Full);
    }
    assert_eq!(q.pop_block(&mut out), Pop::Underrun);
    assert!(out.iter().all(|&s| s == 0));
    assert_eq!(q.stats().underruns, 1);
    assert_eq!(q.pop_block(&mut out), Pop::Silence);
    let mut q = fila_cheia(15);
    q.push(&vec![9i16; 240 * CHANNELS], 0);
    for _ in 0..15 {
        assert_eq!(q.pop_block(&mut out), Pop::Full);
    }
    assert_eq!(q.pop_block(&mut out), Pop::Underrun);
    assert!(out[..240 * CHANNELS].iter().all(|&s| s == 9));
    assert!(out[240 * CHANNELS..].iter().all(|&s| s == 0));
}

#[test]
fn fila_descarta_o_excesso_acima_do_alvo() {
    let mut q = nova();
    for i in 0..30u64 {
        q.push(&bloco_de(i as i16), i * 100);
    }
    assert!(q.level_ms() <= 200.0, "nível {}", q.level_ms());
    assert!(q.stats().dropped_frames > 0);
    let mut out = vec![0i16; BLOCK_FRAMES * CHANNELS];
    let mut primeiro = None;
    let mut maior = -1;
    while q.pop_block(&mut out) != Pop::Silence {
        primeiro.get_or_insert(out[0]);
        maior = maior.max(*out.iter().max().unwrap());
    }
    assert_eq!(maior, 29);
    assert!(primeiro.unwrap() >= 9, "primeiro bloco entregue {primeiro:?}");
}

fn regular(q: &mut MicQueue, t: &mut u64, segundos: u64) {
    let mut out = vec![0i16; BLOCK_FRAMES * CHANNELS];
    for _ in 0..segundos * 100 {
        q.push(&bloco_de(1), *t);
        q.pop_block(&mut out);
        *t += 10_000;
    }
}

#[test]
fn alvo_sobe_com_o_jitter_e_desce_depois() {
    let mut q = nova();
    let mut t = 0u64;
    regular(&mut q, &mut t, 8);
    let calmo = q.target_ms();
    assert!(calmo <= 30.0, "alvo calmo {calmo}");
    t += 60_000;
    for _ in 0..7 {
        q.push(&bloco_de(1), t);
    }
    let agitado = q.target_ms();
    assert!(agitado >= 60.0, "alvo agitado {agitado}");
    assert!(agitado <= QUEUE_MAX_MS as f64);
    regular(&mut q, &mut t, 35);
    assert!(q.target_ms() <= 40.0, "alvo depois {}", q.target_ms());
    assert!(q.target_ms() >= QUEUE_MIN_MS as f64);
}

#[test]
fn depois_de_uma_rajada_a_fila_volta_ao_alvo_sem_descarte_nem_vazio() {
    let mut q = nova();
    let mut t = 0u64;
    regular(&mut q, &mut t, 30);
    let antes = q.stats();
    t += 80_000;
    for _ in 0..9 {
        q.push(&bloco_de(1), t);
    }
    assert!(q.level_ms() >= 70.0, "nível logo depois da rajada {}", q.level_ms());
    let mut out = vec![0i16; BLOCK_FRAMES * CHANNELS];
    for _ in 0..1500 {
        t += 10_000;
        q.push(&bloco_de(1), t);
        assert_eq!(q.pop_block(&mut out), Pop::Full);
    }
    let s = q.stats();
    assert_eq!(s.underruns, antes.underruns, "nenhum esvaziamento novo");
    assert_eq!(s.dropped_frames, antes.dropped_frames, "nenhum descarte de uma vez");
    assert!(s.skipped_frames > 0);
    assert!(q.level_ms() <= q.target_ms() + 15.0, "nível {} alvo {}", q.level_ms(), q.target_ms());
}

#[test]
fn o_consumo_acelerado_pula_quadros_espalhados_e_em_ordem() {
    let mut q = nova();
    let mut t = 0u64;
    regular(&mut q, &mut t, 30);
    let rampa: Vec<i16> = (0..BLOCK_FRAMES * 17).flat_map(|k| [(k + 1000) as i16, (k + 1000) as i16]).collect();
    q.push(&rampa, t);
    let mut out = vec![0i16; BLOCK_FRAMES * CHANNELS];
    let antes = q.stats().skipped_frames;
    let mut achou = false;
    for _ in 0..10 {
        assert_eq!(q.pop_block(&mut out), Pop::Full);
        if out[0] >= 1000 {
            achou = true;
            break;
        }
    }
    assert!(achou, "nenhum bloco da rampa");
    assert!(q.stats().skipped_frames > antes, "o consumo acelerou");
    let mut passos2 = 0;
    for i in 1..BLOCK_FRAMES {
        assert_eq!(out[i * 2], out[i * 2 + 1], "os dois canais andam juntos");
        let d = i32::from(out[i * 2]) - i32::from(out[(i - 1) * 2]);
        assert!(d == 1 || d == 2, "passo {d} no quadro {i}");
        if d == 2 {
            passos2 += 1;
        }
    }
    assert!((1..=10).contains(&passos2), "passos de 2 quadros: {passos2}");
}

#[test]
fn o_maximo_da_fila_e_configuravel() {
    let mut q = MicQueue::with_max_ms(60);
    assert_eq!(q.target_ms(), 60.0, "o alvo inicial é o máximo");
    q.push(&bloco_de(1), 0);
    q.push(&bloco_de(1), 2_000_000);
    assert_eq!(q.target_ms(), 60.0);
    for i in 0..30u64 {
        q.push(&bloco_de(1), 2_000_000 + i * 100);
    }
    assert!(q.level_ms() <= 110.0, "nível {}", q.level_ms());
    assert_eq!(MicQueue::with_max_ms(5).target_ms(), QUEUE_MIN_MS as f64);
    assert_eq!(MicQueue::with_max_ms(5000).target_ms(), 300.0);
    q.reset();
    assert_eq!(q.target_ms(), 60.0);
}

#[test]
fn alvo_nunca_passa_do_maximo() {
    let mut q = MicQueue::new();
    q.push(&bloco_de(1), 0);
    q.push(&bloco_de(1), 2_000_000);
    assert_eq!(q.target_ms(), QUEUE_MAX_MS as f64);
}

struct Falso {
    tx: mpsc::Sender<Option<Vec<u8>>>,
    abertas: Arc<AtomicU32>,
    mortes: Arc<AtomicU32>,
}

struct LeitorCanal {
    rx: mpsc::Receiver<Option<Vec<u8>>>,
    resto: Vec<u8>,
    fim: bool,
}

impl Read for LeitorCanal {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if self.resto.is_empty() && !self.fim {
            match self.rx.recv() {
                Ok(Some(v)) => self.resto = v,
                _ => self.fim = true,
            }
        }
        let n = self.resto.len().min(buf.len());
        buf[..n].copy_from_slice(&self.resto[..n]);
        self.resto.drain(..n);
        Ok(n)
    }
}

fn origem_falsa() -> (OpenFeed, Falso) {
    let (tx, rx) = mpsc::channel::<Option<Vec<u8>>>();
    let rx = Arc::new(Mutex::new(Some(rx)));
    let abertas = Arc::new(AtomicU32::new(0));
    let mortes = Arc::new(AtomicU32::new(0));
    let (a2, m2, tx2) = (abertas.clone(), mortes.clone(), tx.clone());
    let open: OpenFeed = Arc::new(move || {
        a2.fetch_add(1, Ordering::SeqCst);
        let rx = rx.lock().unwrap().take().ok_or_else(|| std::io::Error::other("só uma abertura no teste"))?;
        let (m3, tx3) = (m2.clone(), tx2.clone());
        let kill: Arc<dyn Fn() + Send + Sync> = Arc::new(move || {
            m3.fetch_add(1, Ordering::SeqCst);
            let _ = tx3.send(None);
        });
        Ok(LinkFeed { reader: Box::new(LeitorCanal { rx, resto: Vec::new(), fim: false }), kill })
    });
    (open, Falso { tx, abertas, mortes })
}

fn log_mudo() -> LogFn {
    Arc::new(|_| {})
}

fn waker() -> (Waker, Arc<AtomicU32>) {
    let n = Arc::new(AtomicU32::new(0));
    let n2 = n.clone();
    (
        Arc::new(move || {
            n2.fetch_add(1, Ordering::SeqCst);
        }),
        n,
    )
}

fn espera(mut cond: impl FnMut() -> bool, ate: Duration) -> bool {
    let t0 = Instant::now();
    while t0.elapsed() < ate {
        if cond() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    cond()
}

fn tom_estereo(n: usize, inicio: usize) -> Vec<i16> {
    let mut v = Vec::with_capacity(n * 2);
    for i in 0..n {
        let (l, r) = synth::frame((inicio + i) as u64);
        v.push(l);
        v.push(r);
    }
    v
}

#[test]
fn sem_gravador_nao_ha_pedido_nem_escrita_e_com_gravador_o_pcm_chega() {
    let servidor_pcm = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = servidor_pcm.local_addr().unwrap().to_string();
    let (open, falso) = origem_falsa();
    let hub = MicHub::new(open, &addr, log_mudo());
    let (w, acordou) = waker();
    let mut sessao = hub.attach(w);

    assert!(espera(|| falso.abertas.load(Ordering::SeqCst) == 1, Duration::from_secs(2)));
    assert_eq!(sessao.poll(Instant::now()), None);
    servidor_pcm.set_nonblocking(true).unwrap();
    std::thread::sleep(Duration::from_millis(100));
    assert!(servidor_pcm.accept().is_err(), "ninguém deve conectar sem pedido");

    falso.tx.send(Some(b"+ pistick-mic:capture_FL -> pw-record:input_FL\n+ pistick-mic:capture_FR -> pw-record:input_FR\n".to_vec())).unwrap();
    assert!(espera(|| acordou.load(Ordering::SeqCst) > 0, Duration::from_secs(2)), "o waker deve acordar a sessão");
    assert_eq!(sessao.recording(), (true, vec!["pw-record".to_string()]));
    assert_eq!(sessao.poll(Instant::now()), Some(true));
    assert!(sessao.requested());

    assert!(sessao.on_data(0, &[0; 4], 0).is_err());
    sessao.on_start(CODEC_PSA1, 2, 48000, 480).unwrap();
    servidor_pcm.set_nonblocking(false).unwrap();
    let (mut conexao, _) = servidor_pcm.accept().unwrap();
    conexao.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
    let t0 = Instant::now();
    let mut seq = 0u32;
    let mut enviados = 0usize;
    while t0.elapsed() < Duration::from_millis(600) {
        while (seq as u128) * 10 <= t0.elapsed().as_millis() {
            let bloco = tom_estereo(480, enviados);
            let mut carga = Vec::new();
            encode(&bloco, 2, &mut carga).unwrap();
            sessao.on_data(seq, &carga, u64::from(seq) * 10_000).unwrap();
            seq += 1;
            enviados += 480;
        }
        std::thread::sleep(Duration::from_millis(2));
    }
    let mut recebido = Vec::new();
    let mut buf = [0u8; 4096];
    while recebido.len() < 480 * 4 * 55 {
        let n = conexao.read(&mut buf).unwrap();
        assert!(n > 0);
        recebido.extend_from_slice(&buf[..n]);
    }
    let amostras: Vec<i16> = recebido.chunks_exact(2).map(|c| i16::from_le_bytes([c[0], c[1]])).collect();
    let (l, r) = synth::split_stereo(&amostras[20 * 480 * 2..50 * 480 * 2]);
    assert!((synth::goertzel(&l, 440.0) - 0.25).abs() < 0.02, "esq 440: {}", synth::goertzel(&l, 440.0));
    assert!((synth::goertzel(&r, 880.0) - 0.25).abs() < 0.02, "dir 880: {}", synth::goertzel(&r, 880.0));
    assert!(synth::goertzel(&l, 880.0) < 0.03 && synth::goertzel(&r, 440.0) < 0.03);

    falso.tx.send(Some(b"- pistick-mic:capture_FL -> pw-record:input_FL\n- pistick-mic:capture_FR -> pw-record:input_FR\n".to_vec())).unwrap();
    assert!(espera(|| !sessao.recording().0, Duration::from_secs(2)));
    assert_eq!(sessao.poll(Instant::now()), None);
    std::thread::sleep(OFF_DELAY + Duration::from_millis(50));
    assert_eq!(sessao.poll(Instant::now()), Some(false));
    conexao.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
    let fechou = espera(
        || loop {
            match conexao.read(&mut buf) {
                Ok(0) => break true,
                Ok(_) => continue,
                Err(_) => break false,
            }
        },
        Duration::from_secs(3),
    );
    assert!(fechou, "o PipeWire deve ver a conexão fechar");
    assert!(sessao.on_data(seq, &[0; 4], 0).is_err());
    assert_eq!(sessao.on_data(seq + 1, &[0; 4], 0), Ok(()), "só o primeiro descarte do episódio é relatado");
    assert!(sessao.discarded() >= 3);

    drop(sessao);
    assert!(espera(|| falso.mortes.load(Ordering::SeqCst) >= 1, Duration::from_secs(2)));
}

#[test]
fn mono_e_pcm_viram_estereo() {
    let servidor_pcm = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = servidor_pcm.local_addr().unwrap().to_string();
    let (open, falso) = origem_falsa();
    let hub = MicHub::new(open, &addr, log_mudo());
    let (w, _) = waker();
    let mut s = hub.attach(w);
    falso.tx.send(Some(b"+ pistick-mic:capture_FL -> x:input_FL\n".to_vec())).unwrap();
    assert!(espera(|| s.recording().0, Duration::from_secs(2)));
    assert_eq!(s.poll(Instant::now()), Some(true));
    s.on_start(CODEC_PCM, 1, 48000, 480).unwrap();
    let mono: Vec<i16> = (0..480).map(|i| i as i16).collect();
    let mut carga = Vec::new();
    samples_to_pcm(&mono, &mut carga);
    s.on_data(0, &carga, 0).unwrap();
    let q = s.inner.queue();
    assert_eq!(q.level_frames(), 480);
    assert_eq!(q.buf[2], 1);
    assert_eq!(q.buf[3], 1);
    drop(q);
    assert!(s.on_start(CODEC_PCM, 2, 44100, 480).is_err());
    assert!(s.on_start(9, 2, 48000, 480).is_err());
    s.on_start(CODEC_PCM, 2, 48000, 480).unwrap();
    assert!(s.on_data(0, &[1, 0, 2, 0, 3, 0], 0).is_err());
    s.on_start(CODEC_PSA1, 2, 48000, 480).unwrap();
    assert!(s.on_data(1, &[0xFF; 9], 0).is_err());
    assert!(s.on_stop());
    assert!(!s.on_stop());
}

#[test]
fn salto_de_seq_e_contado_e_nao_derruba() {
    let (open, falso) = origem_falsa();
    let hub = MicHub::new(open, "127.0.0.1:1", log_mudo());
    let (w, _) = waker();
    let mut s = hub.attach(w);
    falso.tx.send(Some(b"+ pistick-mic:capture_FL -> x:input_FL\n".to_vec())).unwrap();
    assert!(espera(|| s.recording().0, Duration::from_secs(2)));
    s.poll(Instant::now());
    s.on_start(CODEC_PCM, 2, 48000, 480).unwrap();
    let carga = vec![0u8; 480 * 4];
    s.on_data(10, &carga, 0).unwrap();
    s.on_data(11, &carga, 10_000).unwrap();
    s.on_data(15, &carga, 20_000).unwrap();
    assert_eq!(s.seq_gaps(), 1);
}

#[test]
fn monitor_que_termina_e_reaberto_e_o_estado_zera() {
    let abertas = Arc::new(AtomicU32::new(0));
    let (tx_atual, rx_atual) = mpsc::channel::<mpsc::Sender<Option<Vec<u8>>>>();
    let a2 = abertas.clone();
    let open: OpenFeed = Arc::new(move || {
        a2.fetch_add(1, Ordering::SeqCst);
        let (tx, rx) = mpsc::channel::<Option<Vec<u8>>>();
        let _ = tx_atual.send(tx.clone());
        let kill: Arc<dyn Fn() + Send + Sync> = Arc::new(move || {
            let _ = tx.send(None);
        });
        Ok(LinkFeed { reader: Box::new(LeitorCanal { rx, resto: Vec::new(), fim: false }), kill })
    });
    let hub = MicHub::new(open, "127.0.0.1:1", log_mudo());
    let (w, _) = waker();
    let s = hub.attach(w);
    let tx1 = rx_atual.recv_timeout(Duration::from_secs(2)).unwrap();
    tx1.send(Some(b"+ pistick-mic:capture_FL -> x:input_FL\n".to_vec())).unwrap();
    assert!(espera(|| s.recording().0, Duration::from_secs(2)));
    tx1.send(None).unwrap();
    assert!(espera(|| !s.recording().0, Duration::from_secs(2)));
    let tx2 = rx_atual.recv_timeout(Duration::from_secs(3)).unwrap();
    assert_eq!(abertas.load(Ordering::SeqCst), 2);
    tx2.send(Some(b"= pistick-mic:capture_FL\n=   |-> y:input_FL\n".to_vec())).unwrap();
    assert!(espera(|| s.recording().0, Duration::from_secs(2)));
    drop(s);
}

#[test]
fn falha_ao_abrir_o_monitor_tenta_de_novo_sem_travar() {
    let abertas = Arc::new(AtomicU32::new(0));
    let a2 = abertas.clone();
    let open: OpenFeed = Arc::new(move || {
        a2.fetch_add(1, Ordering::SeqCst);
        Err(std::io::Error::new(std::io::ErrorKind::NotFound, "o programa pw-link não está instalado"))
    });
    let hub = MicHub::new(open, "127.0.0.1:1", log_mudo());
    let (w, _) = waker();
    let mut s = hub.attach(w);
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(s.poll(Instant::now()), None);
    assert!(abertas.load(Ordering::SeqCst) >= 1);
    drop(s);
    let n = abertas.load(Ordering::SeqCst);
    std::thread::sleep(Duration::from_millis(2300));
    assert!(abertas.load(Ordering::SeqCst) <= n + 1);
}

#[test]
fn so_um_cliente_recebe_o_microfone_por_vez_e_o_seguinte_assume_quando_o_primeiro_sai() {
    let servidor_pcm = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = servidor_pcm.local_addr().unwrap().to_string();
    type Canal = mpsc::Sender<Option<Vec<u8>>>;
    let atual: Arc<Mutex<Option<Canal>>> = Arc::new(Mutex::new(None));
    let abertas = Arc::new(AtomicU32::new(0));
    let (a2, ab2) = (atual.clone(), abertas.clone());
    let open: OpenFeed = Arc::new(move || {
        ab2.fetch_add(1, Ordering::SeqCst);
        let (tx, rx) = mpsc::channel::<Option<Vec<u8>>>();
        *a2.lock().unwrap() = Some(tx.clone());
        let kill: Arc<dyn Fn() + Send + Sync> = Arc::new(move || {
            let _ = tx.send(None);
        });
        Ok(LinkFeed { reader: Box::new(LeitorCanal { rx, resto: Vec::new(), fim: false }), kill })
    });
    let hub = MicHub::new(open, &addr, log_mudo());
    let (w1, acordou1) = waker();
    let (w2, acordou2) = waker();
    let mut s1 = hub.attach(w1);
    let mut s2 = hub.attach(w2);
    assert!(espera(|| abertas.load(Ordering::SeqCst) == 1, Duration::from_secs(2)));
    std::thread::sleep(Duration::from_millis(100));
    assert_eq!(abertas.load(Ordering::SeqCst), 1, "a que espera não sobe monitor");
    let liga = b"+ pistick-mic:capture_FL -> pw-record:input_FL
+ pistick-mic:capture_FR -> pw-record:input_FR
".to_vec();
    atual.lock().unwrap().as_ref().unwrap().send(Some(liga.clone())).unwrap();
    assert!(espera(|| acordou1.load(Ordering::SeqCst) > 0, Duration::from_secs(2)));
    assert!(s1.recording().0);
    assert_eq!(s2.recording(), (false, vec![]), "a segunda não vê o gravador");
    assert_eq!(s1.poll(Instant::now()), Some(true));
    assert_eq!(s2.poll(Instant::now()), None, "a segunda nunca recebe pedido enquanto a primeira está conectada");
    assert_eq!(acordou2.load(Ordering::SeqCst), 0);
    assert!(s2.on_data(0, &[0; 4], 0).is_err());
    drop(s1);
    assert!(espera(|| abertas.load(Ordering::SeqCst) == 2, Duration::from_secs(2)), "monitor novo para a nova dona");
    assert!(espera(|| acordou2.load(Ordering::SeqCst) > 0, Duration::from_secs(2)), "a nova dona é acordada");
    assert_eq!(s2.recording(), (false, vec![]), "o monitor novo ainda não viu ligação nenhuma");
    atual.lock().unwrap().as_ref().unwrap().send(Some(b"= pistick-mic:capture_FL
=   |-> pw-record:input_FL
".to_vec())).unwrap();
    assert!(espera(|| s2.recording().0, Duration::from_secs(2)));
    assert_eq!(s2.poll(Instant::now()), Some(true));
    let (w3, _) = waker();
    let s3 = hub.attach(w3);
    drop(s3);
    assert!(s2.recording().0);
    drop(s2);
}
