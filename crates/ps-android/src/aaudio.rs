//! Saída de som por AAudio (docs/26, decisão 9): fluxo de saída de 48 kHz, estéreo, `i16`, modo de baixa latência, com
//! retorno de chamada de dados que puxa da fila do `AudioEngine` (a mesma fila adaptativa do WASAPI do `psc`).
//!
//! - O retorno de chamada de dados roda numa thread de tempo real do sistema: só chama [`crate::saida_audio::encher`]
//!   (sem alocação, sem registro, sem trava demorada).
//! - O retorno de chamada de erro (aparelho de som trocado, fone plugado) **não** fecha nem reabre nada: só levanta um
//!   sinalizador; uma thread de supervisão reabre o fluxo fora dele, com a espera crescente de [`Reabertura`].
//! - Parar e fechar: [`AudioSink::stop`] (chamado pelo `shutdown` do motor) e o `Drop` do fluxo.
//!
//! Todo o `unsafe` da saída de som está aqui; as declarações do NDK, em `ndk.rs`.

use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Duration;

use ps_client::audio::{AudioEngine, AudioSink};

use crate::ndk::{self, AAudioStream, AAudioStreamBuilder, AAUDIO_CALLBACK_RESULT_CONTINUE, AAUDIO_DIRECTION_OUTPUT, AAUDIO_FORMAT_PCM_I16, AAUDIO_OK, AAUDIO_PERFORMANCE_MODE_LOW_LATENCY};
use crate::registro;
use crate::saida_audio::{encher, Contadores, Reabertura, CANAIS, TAXA};

/// O que o retorno de chamada vê: o motor, os contadores e o sinalizador de erro. Vive (dentro de um `Arc`) até depois
/// de o fluxo ser fechado.
struct Compartilhado {
    engine: Arc<AudioEngine>,
    contadores: Contadores,
    erro: AtomicBool,
}

/// Retorno de chamada de dados (thread de tempo real do sistema).
///
/// # Safety
/// Chamado só pelo AAudio, com `user_data` igual ao `*const Compartilhado` dado em `abrir_fluxo` (vivo enquanto o
/// fluxo existe) e `audio_data` apontando para `num_frames * 2` amostras `i16` graváveis.
unsafe extern "C" fn dados(_stream: *mut AAudioStream, user_data: *mut c_void, audio_data: *mut c_void, num_frames: i32) -> i32 {
    if user_data.is_null() || audio_data.is_null() || num_frames <= 0 {
        return AAUDIO_CALLBACK_RESULT_CONTINUE;
    }
    // SAFETY: contrato da função: `user_data` aponta para o `Compartilhado` vivo (o `Arc` é solto só depois do
    // `close` do fluxo, que espera o retorno de chamada terminar) e `audio_data` tem `num_frames * CANAIS` amostras.
    let (c, buf) = unsafe { (&*(user_data as *const Compartilhado), std::slice::from_raw_parts_mut(audio_data as *mut i16, num_frames as usize * CANAIS)) };
    encher(&c.engine, buf, &c.contadores);
    AAUDIO_CALLBACK_RESULT_CONTINUE
}

/// Retorno de chamada de erro (outra thread do sistema): só levanta o sinalizador.
///
/// # Safety
/// Como em [`dados`].
unsafe extern "C" fn erro(_stream: *mut AAudioStream, user_data: *mut c_void, _codigo: i32) {
    if user_data.is_null() {
        return;
    }
    // SAFETY: contrato da função.
    let c = unsafe { &*(user_data as *const Compartilhado) };
    c.erro.store(true, Ordering::SeqCst);
}

/// Um fluxo aberto e iniciado; o `Drop` o para e o fecha.
struct Fluxo {
    stream: *mut AAudioStream,
}

// SAFETY: o fluxo do AAudio pode ser parado e fechado de qualquer thread; esta estrutura o possui e só a thread de
// supervisão a usa depois de criada.
unsafe impl Send for Fluxo {}

impl Drop for Fluxo {
    fn drop(&mut self) {
        // SAFETY: `stream` é um fluxo válido que esta estrutura possui; parado e fechado uma única vez. O `close` só
        // volta quando os retornos de chamada terminaram, então o `Compartilhado` pode ser solto depois.
        unsafe {
            ndk::AAudioStream_requestStop(self.stream);
            ndk::AAudioStream_close(self.stream);
        }
    }
}

/// Abre e inicia um fluxo de saída que puxa do motor.
fn abrir_fluxo(c: &Arc<Compartilhado>) -> Result<Fluxo, String> {
    let user = Arc::as_ptr(c) as *mut c_void;
    let mut builder: *mut AAudioStreamBuilder = std::ptr::null_mut();
    // SAFETY: `builder` é um ponteiro local que recebe o construtor novo; todas as chamadas seguintes usam esse
    // construtor válido com argumentos de enumeração e funções com a assinatura do NDK; `user` aponta para o
    // `Compartilhado` que o chamador mantém vivo enquanto o fluxo existir; o construtor é apagado no fim (o fluxo já
    // aberto não depende dele).
    unsafe {
        let r = ndk::AAudio_createStreamBuilder(&mut builder);
        if r != AAUDIO_OK || builder.is_null() {
            return Err(format!("AAudio_createStreamBuilder falhou ({r})"));
        }
        ndk::AAudioStreamBuilder_setDirection(builder, AAUDIO_DIRECTION_OUTPUT);
        ndk::AAudioStreamBuilder_setSampleRate(builder, TAXA);
        ndk::AAudioStreamBuilder_setChannelCount(builder, CANAIS as i32);
        ndk::AAudioStreamBuilder_setFormat(builder, AAUDIO_FORMAT_PCM_I16);
        ndk::AAudioStreamBuilder_setPerformanceMode(builder, AAUDIO_PERFORMANCE_MODE_LOW_LATENCY);
        ndk::AAudioStreamBuilder_setDataCallback(builder, dados, user);
        ndk::AAudioStreamBuilder_setErrorCallback(builder, erro, user);
        let mut stream: *mut AAudioStream = std::ptr::null_mut();
        let r = ndk::AAudioStreamBuilder_openStream(builder, &mut stream);
        ndk::AAudioStreamBuilder_delete(builder);
        if r != AAUDIO_OK || stream.is_null() {
            return Err(format!("AAudioStreamBuilder_openStream falhou ({r})"));
        }
        let f = Fluxo { stream };
        let r = ndk::AAudioStream_requestStart(f.stream);
        if r != AAUDIO_OK {
            return Err(format!("AAudioStream_requestStart falhou ({r})"));
        }
        Ok(f)
    }
}

/// A saída de som: o primeiro fluxo é aberto na construção (se falhar, o cliente não anuncia `CAP_AUDIO`); uma thread
/// de supervisão o reabre depois de um erro e registra as faltas.
pub struct SaidaAAudio {
    parar: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl SaidaAAudio {
    pub fn abrir(engine: Arc<AudioEngine>) -> Result<SaidaAAudio, String> {
        let c = Arc::new(Compartilhado { engine, contadores: Contadores::default(), erro: AtomicBool::new(false) });
        let fluxo = abrir_fluxo(&c)?;
        let parar = Arc::new(AtomicBool::new(false));
        let p2 = parar.clone();
        let thread = std::thread::Builder::new()
            .name("psa-aaudio".into())
            .spawn(move || supervisionar(c, fluxo, p2))
            .map_err(|e| format!("não foi possível criar a thread da saída de som: {e}"))?;
        Ok(SaidaAAudio { parar, thread: Some(thread) })
    }
}

/// Reabre o fluxo quando o sistema avisa erro e registra as faltas do retorno de chamada, fora dele.
fn supervisionar(c: Arc<Compartilhado>, fluxo: Fluxo, parar: Arc<AtomicBool>) {
    let mut fluxo = Some(fluxo);
    let mut reab = Reabertura::new();
    let mut faltas_vistas = 0u64;
    while !parar.load(Ordering::SeqCst) {
        std::thread::sleep(Duration::from_millis(100));
        let f = c.contadores.faltas.load(Ordering::Relaxed);
        if f != faltas_vistas {
            registro::info(&format!("saída de som: {} blocos em silêncio por trava ocupada (total {f} de {} blocos)", f - faltas_vistas, c.contadores.blocos.load(Ordering::Relaxed)));
            faltas_vistas = f;
        }
        if c.erro.swap(false, Ordering::SeqCst) || (fluxo.is_none() && reab.falhas() > 0) {
            registro::info("saída de som: erro do AAudio (aparelho de som trocado?); reabrindo o fluxo");
            drop(fluxo.take()); // para e fecha o fluxo antigo (fora do retorno de chamada de erro)
            match abrir_fluxo(&c) {
                Ok(novo) => {
                    fluxo = Some(novo);
                    reab.sucesso();
                    registro::info("saída de som: fluxo reaberto");
                }
                Err(e) => match reab.falha() {
                    Some(espera) => {
                        registro::erro(&format!("saída de som: {e}; nova tentativa em {} ms", espera.as_millis()));
                        std::thread::sleep(espera);
                    }
                    None => {
                        registro::erro(&format!("saída de som: {e}; desisti depois de {} tentativas", Reabertura::MAX_TENTATIVAS));
                        break;
                    }
                },
            }
        }
    }
    // O fluxo é fechado aqui (`Drop`), antes de o `Arc<Compartilhado>` ser solto.
    drop(fluxo);
}

impl AudioSink for SaidaAAudio {
    fn name(&self) -> &'static str {
        "AAudio"
    }

    fn stop(&mut self) {
        self.parar.store(true, Ordering::SeqCst);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

impl Drop for SaidaAAudio {
    fn drop(&mut self) {
        self.stop();
    }
}
