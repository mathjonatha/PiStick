//! Captura do microfone no Windows por WASAPI, modo compartilhado, dirigida por evento (marco 7B). Junto do
//! `wasapi.rs`, é o único arquivo do projeto de áudio com `unsafe` (chamadas COM do `windows`; cada bloco tem o
//! `// SAFETY:` da sua invariante).
//!
//! - **Só é chamado depois de um `AUDIO_REQUEST on=1`** (veja `crate::mic`). Nada aqui abre dispositivo antes disso.
//! - O formato é pedido sempre em 48 kHz, 16 bits, estéreo, com `AUTOCONVERTPCM` e `SRC_DEFAULT_QUALITY` (as mesmas
//!   opções da reprodução): o sistema converte taxa e número de canais; um microfone mono sai duplicado nos dois canais.
//! - **Liberação:** [`run`] só retorna depois de soltar todos os objetos COM (cliente de áudio, cliente de captura,
//!   enumerador) e fechar o evento; o indicador de microfone em uso do Windows apaga nesse instante. Vale para o fim
//!   normal (`stop`), para erro e para sessão encerrada.
//! - Dispositivo perdido no meio (desconectado, invalidado): tenta reabrir a cada segundo enquanto `stop` não vem.

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use windows::core::PCWSTR;
use windows::Win32::Foundation::{CloseHandle, HANDLE};
use windows::Win32::Media::Audio::{
    eCapture, eConsole, IAudioCaptureClient, IAudioClient, IMMDeviceEnumerator, MMDeviceEnumerator, AUDCLNT_BUFFERFLAGS_SILENT, AUDCLNT_SHAREMODE_SHARED, AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM,
    AUDCLNT_STREAMFLAGS_EVENTCALLBACK, AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY, WAVEFORMATEX,
};
use windows::Win32::System::Com::{CoCreateInstance, CoInitializeEx, CoUninitialize, CLSCTX_ALL, COINIT_MULTITHREADED};
use windows::Win32::System::Threading::{CreateEventW, WaitForSingleObject};

use crate::mic::BLOCK_FRAMES;

/// Buffer pedido ao WASAPI (50 ms, em unidades de 100 ns): folga para o escalonador do sistema.
const BUFFER_100NS: i64 = 500_000;

/// Um dispositivo de captura aberto e iniciado. O `Drop` o para e fecha o evento; os objetos COM são soltos logo depois,
/// pelos `Drop` do próprio `windows`.
struct Capture {
    client: IAudioClient,
    capture: IAudioCaptureClient,
    event: HANDLE,
}

impl Drop for Capture {
    fn drop(&mut self) {
        // SAFETY: `client` é válido; parar um cliente já parado é inofensivo. `event` foi criado por `CreateEventW`
        // em `Capture::open` e só é fechado aqui, uma vez (o `Capture` só existe depois do evento criado).
        unsafe {
            let _ = self.client.Stop();
            let _ = CloseHandle(self.event);
        }
    }
}

impl Capture {
    /// Abre o microfone padrão. `Err` com o motivo se não há dispositivo de gravação ou ele não abre.
    fn open(enumerator: &IMMDeviceEnumerator) -> Result<Capture, String> {
        // SAFETY: sequência de chamadas COM sobre objetos recém-criados e válidos. `fmt` vive até depois do
        // `Initialize`, que copia o formato. O evento criado é entregue ao `Capture` na mesma instrução em que nasce
        // o objeto, então qualquer erro depois dali o fecha pelo `Drop`.
        unsafe {
            let dev = enumerator.GetDefaultAudioEndpoint(eCapture, eConsole).map_err(|e| format!("sem dispositivo de gravação padrão ({e})"))?;
            let client: IAudioClient = dev.Activate(CLSCTX_ALL, None).map_err(|e| format!("não foi possível ativar o dispositivo de gravação ({e})"))?;
            let fmt = WAVEFORMATEX {
                wFormatTag: 1, // WAVE_FORMAT_PCM
                nChannels: 2,
                nSamplesPerSec: 48_000,
                nAvgBytesPerSec: 48_000 * 4,
                nBlockAlign: 4,
                wBitsPerSample: 16,
                cbSize: 0,
            };
            client
                .Initialize(
                    AUDCLNT_SHAREMODE_SHARED,
                    AUDCLNT_STREAMFLAGS_EVENTCALLBACK | AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM | AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY,
                    BUFFER_100NS,
                    0,
                    &fmt,
                    None,
                )
                .map_err(|e| format!("o dispositivo de gravação não aceitou o formato de 48 kHz, 16 bits, estéreo ({e})"))?;
            let capture: IAudioCaptureClient = client.GetService().map_err(|e| format!("IAudioCaptureClient indisponível ({e})"))?;
            let event = CreateEventW(None, false, false, PCWSTR::null()).map_err(|e| format!("CreateEventW falhou ({e})"))?;
            let this = Capture { client: client.clone(), capture, event };
            client.SetEventHandle(event).map_err(|e| format!("SetEventHandle falhou ({e})"))?;
            client.Start().map_err(|e| format!("não foi possível iniciar a captura ({e})"))?;
            Ok(this)
        }
    }

    /// Espera o evento (até 20 ms) e acrescenta a `acc` tudo o que o dispositivo já entregou (estéreo S16 intercalado).
    /// `Err` pede para reabrir.
    fn read(&self, acc: &mut Vec<i16>) -> windows::core::Result<()> {
        // SAFETY: `event` é um evento válido de propriedade de `self`. `GetBuffer` devolve um ponteiro para `frames`
        // quadros de 4 bytes (estéreo S16) válido até o `ReleaseBuffer` seguinte; a cópia é feita byte a byte para um
        // `Vec<i16>` redimensionado antes (sem exigir alinhamento do ponteiro do sistema), com exatamente `frames * 4`
        // bytes, e o `ReleaseBuffer(frames)` vem logo depois, sem outra chamada ao `capture` no meio.
        unsafe {
            let _ = WaitForSingleObject(self.event, 20);
            loop {
                let pending = self.capture.GetNextPacketSize()?;
                if pending == 0 {
                    return Ok(());
                }
                let mut data: *mut u8 = std::ptr::null_mut();
                let mut frames: u32 = 0;
                let mut flags: u32 = 0;
                self.capture.GetBuffer(&mut data, &mut frames, &mut flags, None, None)?;
                let n = frames as usize * 2;
                let at = acc.len();
                acc.resize(at + n, 0);
                if flags & (AUDCLNT_BUFFERFLAGS_SILENT.0 as u32) == 0 && !data.is_null() {
                    std::ptr::copy_nonoverlapping(data as *const u8, acc[at..].as_mut_ptr() as *mut u8, n * 2);
                }
                self.capture.ReleaseBuffer(frames)?;
            }
        }
    }
}

/// Entrega de `acc` os pacotes inteiros de 480 quadros a `sink`.
fn drain_blocks(acc: &mut Vec<i16>, sink: &mut dyn FnMut(&[i16])) {
    let block = BLOCK_FRAMES * 2;
    let mut used = 0;
    while acc.len() - used >= block {
        sink(&acc[used..used + block]);
        used += block;
    }
    acc.drain(..used);
}

/// Pausa de até `total` que acorda cedo se `stop` vier.
fn sleep_unless_stopped(stop: &AtomicBool, total: Duration) {
    let t0 = std::time::Instant::now();
    while t0.elapsed() < total && !stop.load(Ordering::SeqCst) {
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// Todos os objetos COM vivem aqui dentro e são soltos antes de voltar.
fn run_inner(stop: &AtomicBool, on_open: &mut dyn FnMut(), sink: &mut dyn FnMut(&[i16])) -> Result<(), String> {
    // SAFETY: criação do enumerador padrão do sistema; COM já está inicializado nesta thread por `run`.
    let enumerator: IMMDeviceEnumerator = unsafe { CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL) }.map_err(|e| format!("não foi possível criar o enumerador de dispositivos ({e})"))?;
    let mut announced = false;
    let mut acc: Vec<i16> = Vec::new();
    while !stop.load(Ordering::SeqCst) {
        let dev = match Capture::open(&enumerator) {
            Ok(d) => d,
            Err(e) => {
                if !announced {
                    return Err(e);
                }
                println!("[psc] microfone: o dispositivo de gravação sumiu ({e}); nova tentativa em 1 s");
                sleep_unless_stopped(stop, Duration::from_secs(1));
                continue;
            }
        };
        if !announced {
            announced = true;
            on_open();
        } else {
            println!("[psc] microfone: dispositivo de gravação reaberto");
        }
        acc.clear();
        while !stop.load(Ordering::SeqCst) {
            if let Err(e) = dev.read(&mut acc) {
                println!("[psc] microfone: o dispositivo de gravação falhou ({e}); reabrindo");
                break;
            }
            drain_blocks(&mut acc, sink);
        }
        // `dev` sai de escopo aqui: o cliente de áudio para, o evento fecha e os objetos COM são soltos.
    }
    Ok(())
}

/// Captura o microfone padrão até `stop`. Chama `on_open` quando o dispositivo abre e `sink` com cada pacote de
/// 480 quadros estéreo. Só retorna depois de liberar o dispositivo.
pub fn run(stop: &AtomicBool, on_open: &mut dyn FnMut(), sink: &mut dyn FnMut(&[i16])) -> Result<(), String> {
    // SAFETY: COM inicializado nesta thread antes de qualquer uso e desinicializado no fim dela, depois de
    // `run_inner` ter soltado todos os objetos COM.
    let com = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
    if com.is_err() {
        return Err(format!("CoInitializeEx falhou ({com:?})"));
    }
    let result = run_inner(stop, on_open, sink);
    // SAFETY: par do `CoInitializeEx` acima, na mesma thread; nenhum objeto COM sobrou em `run_inner`.
    unsafe { CoUninitialize() };
    result
}
