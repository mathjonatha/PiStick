//! A parte pura da saída de som do Android (docs/26, decisão 9): o que o retorno de chamada de dados do AAudio faz e a
//! política de reabertura do fluxo. O que fala com o NDK está em `aaudio.rs`.
//!
//! O retorno de chamada de dados roda numa thread de tempo real do sistema: não aloca, não registra e nunca espera uma
//! trava ([`encher`] usa [`AudioEngine::pull_nowait`] e só soma contadores atômicos). Uma falta (a trava estava com a
//! thread de rede naquele instante) entrega silêncio no bloco e é contada, para o registro fora da thread de tempo real.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use ps_client::audio::AudioEngine;

/// Canais da saída (estéreo, `i16`, 48 kHz).
pub const CANAIS: usize = 2;
pub const TAXA: i32 = 48_000;

/// Contadores do retorno de chamada, lidos fora dele.
#[derive(Debug, Default)]
pub struct Contadores {
    /// Blocos entregues.
    pub blocos: AtomicU64,
    /// Blocos em que a fila estava com a thread de rede e saiu silêncio.
    pub faltas: AtomicU64,
    /// Blocos pedidos com tamanho inválido (entregues em silêncio).
    pub invalidos: AtomicU64,
}

/// O corpo do retorno de chamada de dados: preenche `dados` (`quadros * 2` amostras) com o próximo áudio da fila.
/// Sem alocação, sem registro, sem espera. Se `dados` não tem um número par de amostras, sai silêncio.
pub fn encher(engine: &AudioEngine, dados: &mut [i16], c: &Contadores) {
    c.blocos.fetch_add(1, Ordering::Relaxed);
    if dados.is_empty() || !dados.len().is_multiple_of(CANAIS) {
        dados.fill(0);
        c.invalidos.fetch_add(1, Ordering::Relaxed);
        return;
    }
    if !engine.pull_nowait(dados) {
        c.faltas.fetch_add(1, Ordering::Relaxed);
    }
}

/// Política de reabertura do fluxo depois de um erro (fone plugado, aparelho de som trocado): espera crescente entre
/// as tentativas, e desiste depois de [`Reabertura::MAX_TENTATIVAS`] falhas seguidas.
#[derive(Debug, Default)]
pub struct Reabertura {
    falhas: u32,
}

impl Reabertura {
    pub const MAX_TENTATIVAS: u32 = 20;
    const BASE_MS: u64 = 100;
    const TETO_MS: u64 = 2000;

    pub fn new() -> Reabertura {
        Reabertura::default()
    }

    /// A reabertura funcionou.
    pub fn sucesso(&mut self) {
        self.falhas = 0;
    }

    /// A reabertura falhou: quanto esperar antes da próxima, ou `None` se é hora de desistir.
    pub fn falha(&mut self) -> Option<Duration> {
        self.falhas += 1;
        if self.falhas > Self::MAX_TENTATIVAS {
            return None;
        }
        let ms = (Self::BASE_MS << (self.falhas - 1).min(8)).min(Self::TETO_MS);
        Some(Duration::from_millis(ms))
    }

    pub fn falhas(&self) -> u32 {
        self.falhas
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ps_audio::{samples_to_pcm, CODEC_PCM};

    /// Um motor com som: 440 Hz no canal esquerdo e silêncio no direito, alvo mínimo da fila.
    fn motor_com_som(blocos: usize) -> std::sync::Arc<AudioEngine> {
        let e = AudioEngine::new(None);
        e.on_start(0, CODEC_PCM, 2, 48_000, 480).unwrap();
        let mut t = 0f64;
        for seq in 0..blocos as u32 {
            let mut s = Vec::with_capacity(960);
            for _ in 0..480 {
                s.push((f64::sin(t) * 8000.0) as i16);
                s.push(0);
                t += 2.0 * std::f64::consts::PI * 440.0 / 48_000.0;
            }
            let mut pcm = Vec::new();
            samples_to_pcm(&s, &mut pcm);
            e.on_data(0, 0, seq, seq as u64 * 10_000, &pcm).unwrap();
        }
        e
    }

    #[test]
    fn o_retorno_de_chamada_puxa_da_fila_sem_esperar_e_conta_os_blocos() {
        let e = motor_com_som(20);
        let c = Contadores::default();
        let mut saida = vec![0i16; 480 * 2];
        let (mut esq, mut dir) = (false, false);
        for _ in 0..3 {
            encher(&e, &mut saida, &c);
            esq |= saida.iter().step_by(2).any(|&s| s != 0);
            dir |= saida.iter().skip(1).step_by(2).any(|&s| s != 0);
        }
        assert_eq!(c.blocos.load(Ordering::Relaxed), 3);
        assert_eq!(c.faltas.load(Ordering::Relaxed), 0);
        assert!(esq, "o canal esquerdo tem som");
        assert!(!dir, "o direito é silêncio");
    }

    #[test]
    fn trava_ocupada_entrega_silencio_conta_a_falta_e_nao_espera() {
        let e = motor_com_som(20);
        let c = Contadores::default();
        // Uma thread "de rede" segura a trava do motor durante o bloco.
        let e2 = e.clone();
        let (pronto_tx, pronto_rx) = std::sync::mpsc::channel();
        let (solta_tx, solta_rx) = std::sync::mpsc::channel::<()>();
        let t = std::thread::spawn(move || e2.hold_for_test(|| {
            pronto_tx.send(()).unwrap();
            let _ = solta_rx.recv();
        }));
        pronto_rx.recv().unwrap();
        let mut saida = vec![7i16; 480 * 2];
        let t0 = std::time::Instant::now();
        encher(&e, &mut saida, &c);
        assert!(t0.elapsed() < Duration::from_millis(50), "não esperou a trava");
        assert!(saida.iter().all(|&s| s == 0));
        assert_eq!(c.faltas.load(Ordering::Relaxed), 1);
        solta_tx.send(()).unwrap();
        t.join().unwrap();
        // Livre de novo: volta a puxar.
        encher(&e, &mut saida, &c);
        assert_eq!(c.faltas.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn bloco_com_tamanho_invalido_sai_em_silencio_sem_panico() {
        let e = motor_com_som(5);
        let c = Contadores::default();
        let mut impar = vec![5i16; 7];
        encher(&e, &mut impar, &c);
        assert!(impar.iter().all(|&s| s == 0));
        let mut vazio: [i16; 0] = [];
        encher(&e, &mut vazio, &c);
        assert_eq!(c.invalidos.load(Ordering::Relaxed), 2);
    }

    #[test]
    fn reabertura_espera_cada_vez_mais_e_desiste_no_fim() {
        let mut r = Reabertura::new();
        let esperas: Vec<u64> = (0..6).map(|_| r.falha().unwrap().as_millis() as u64).collect();
        assert_eq!(esperas, vec![100, 200, 400, 800, 1600, 2000]);
        assert_eq!(r.falha().unwrap().as_millis(), 2000, "o teto vale para sempre");
        r.sucesso();
        assert_eq!(r.falhas(), 0);
        assert_eq!(r.falha().unwrap().as_millis(), 100, "depois de um sucesso recomeça");
        let mut r = Reabertura::new();
        for _ in 0..Reabertura::MAX_TENTATIVAS {
            assert!(r.falha().is_some());
        }
        assert!(r.falha().is_none(), "desiste depois de {} falhas seguidas", Reabertura::MAX_TENTATIVAS);
    }
}
