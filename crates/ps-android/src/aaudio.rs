
use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Duration;

use ps_client::audio::{AudioEngine, AudioSink};

use crate::ndk::{self, AAudioStream, AAudioStreamBuilder, AAUDIO_CALLBACK_RESULT_CONTINUE, AAUDIO_DIRECTION_OUTPUT, AAUDIO_FORMAT_PCM_I16, AAUDIO_OK, AAUDIO_PERFORMANCE_MODE_LOW_LATENCY};
use crate::registro;
use crate::saida_audio::{encher, Contadores, Reabertura, CANAIS, TAXA};

struct Compartilhado {
    engine: Arc<AudioEngine>,
    contadores: Contadores,
    erro: AtomicBool,
}

unsafe extern "C" fn dados(_stream: *mut AAudioStream, user_data: *mut c_void, audio_data: *mut c_void, num_frames: i32) -> i32 {
    if user_data.is_null() || audio_data.is_null() || num_frames <= 0 {
        return AAUDIO_CALLBACK_RESULT_CONTINUE;
    }
    let (c, buf) = unsafe { (&*(user_data as *const Compartilhado), std::slice::from_raw_parts_mut(audio_data as *mut i16, num_frames as usize * CANAIS)) };
    encher(&c.engine, buf, &c.contadores);
    AAUDIO_CALLBACK_RESULT_CONTINUE
}

unsafe extern "C" fn erro(_stream: *mut AAudioStream, user_data: *mut c_void, _codigo: i32) {
    if user_data.is_null() {
        return;
    }
    let c = unsafe { &*(user_data as *const Compartilhado) };
    c.erro.store(true, Ordering::SeqCst);
}

struct Fluxo {
    stream: *mut AAudioStream,
}

unsafe impl Send for Fluxo {}

impl Drop for Fluxo {
    fn drop(&mut self) {
        unsafe {
            ndk::AAudioStream_requestStop(self.stream);
            ndk::AAudioStream_close(self.stream);
        }
    }
}

fn abrir_fluxo(c: &Arc<Compartilhado>) -> Result<Fluxo, String> {
    let user = Arc::as_ptr(c) as *mut c_void;
    let mut builder: *mut AAudioStreamBuilder = std::ptr::null_mut();
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
            drop(fluxo.take());
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
