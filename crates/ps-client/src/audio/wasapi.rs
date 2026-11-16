//! Saída de áudio no Windows por WASAPI, modo compartilhado, dirigida por evento. É o único arquivo do projeto
//! do marco 7 com `unsafe` (chamadas COM do `windows`; cada bloco tem o `// SAFETY:` da sua invariante).
//!
//! - O fluxo é pedido sempre em 48 kHz, 16 bits, estéreo, com `AUTOCONVERTPCM` e `SRC_DEFAULT_QUALITY`: se o
//!   dispositivo trabalha em outra taxa ou formato, o próprio WASAPI converte.
//! - Uma thread própria espera o evento do buffer e o enche com [`AudioEngine::pull`].
//! - Troca do dispositivo padrão do Windows (ou dispositivo invalidado): a thread confere o identificador do
//!   dispositivo padrão uma vez por segundo, e também reage a `AUDCLNT_E_DEVICE_INVALIDATED`; nos dois casos
//!   fecha tudo e reabre no novo dispositivo.

use std::sync::mpsc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use windows::core::PCWSTR;
use windows::Win32::Foundation::{CloseHandle, HANDLE, WAIT_OBJECT_0};
use windows::Win32::Media::Audio::{
    eConsole, eRender, IAudioClient, IAudioRenderClient, IMMDeviceEnumerator, ISimpleAudioVolume, MMDeviceEnumerator, AUDCLNT_BUFFERFLAGS_SILENT, AUDCLNT_SHAREMODE_SHARED,
    AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM, AUDCLNT_STREAMFLAGS_EVENTCALLBACK, AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY, WAVEFORMATEX,
};
use windows::Win32::System::Com::{CoCreateInstance, CoInitializeEx, CoTaskMemFree, CoUninitialize, CLSCTX_ALL, COINIT_MULTITHREADED};
use windows::Win32::System::Threading::{CreateEventW, WaitForSingleObject};

use super::AudioEngine;

/// Buffer pedido ao WASAPI (20 ms, em unidades de 100 ns).
const BUFFER_100NS: i64 = 200_000;

struct Device {
    client: IAudioClient,
    render: IAudioRenderClient,
    event: HANDLE,
    buffer_frames: u32,
    id: String,
}

impl Drop for Device {
    fn drop(&mut self) {
        // SAFETY: `client` é válido; parar um cliente já parado é inofensivo. `event` foi criado por
        // `CreateEventW` em `Device::open` e só é fechado aqui, uma vez.
        unsafe {
            let _ = self.client.Stop();
            let _ = CloseHandle(self.event);
        }
    }
}

fn default_endpoint_id(enumerator: &IMMDeviceEnumerator) -> windows::core::Result<String> {
    // SAFETY: chamadas COM com o enumerador válido; o `PWSTR` devolvido por `GetId` é de propriedade do
    // chamador e é liberado com `CoTaskMemFree` depois de copiado para uma `String`.
    unsafe {
        let dev = enumerator.GetDefaultAudioEndpoint(eRender, eConsole)?;
        let pw = dev.GetId()?;
        let s = pw.to_string();
        CoTaskMemFree(Some(pw.0 as *const _));
        s.map_err(|_| windows::core::Error::from(windows::Win32::Foundation::E_FAIL))
    }
}

impl Device {
    fn open(enumerator: &IMMDeviceEnumerator, volume: f32) -> windows::core::Result<Device> {
        // SAFETY: sequência de chamadas COM sobre objetos recém-criados e válidos. `fmt` vive até depois do
        // `Initialize`, que copia o formato. O ponteiro devolvido por `GetBuffer` só é usado entre `GetBuffer`
        // e `ReleaseBuffer`, com o tamanho pedido. O evento é fechado pelo `Drop` do `Device`, criado logo depois dele.
        unsafe {
            let id = default_endpoint_id(enumerator)?;
            let dev = enumerator.GetDefaultAudioEndpoint(eRender, eConsole)?;
            let client: IAudioClient = dev.Activate(CLSCTX_ALL, None)?;
            let fmt = WAVEFORMATEX {
                wFormatTag: 1, // WAVE_FORMAT_PCM
                nChannels: 2,
                nSamplesPerSec: 48_000,
                nAvgBytesPerSec: 48_000 * 4,
                nBlockAlign: 4,
                wBitsPerSample: 16,
                cbSize: 0,
            };
            client.Initialize(
                AUDCLNT_SHAREMODE_SHARED,
                AUDCLNT_STREAMFLAGS_EVENTCALLBACK | AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM | AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY,
                BUFFER_100NS,
                0,
                &fmt,
                None,
            )?;
            // Tudo o que pode falhar e não precisa do evento vem antes dele; o `Device` nasce junto com o evento,
            // então o `Drop` o fecha em qualquer erro dali em diante.
            let render: IAudioRenderClient = client.GetService()?;
            let event = CreateEventW(None, false, false, PCWSTR::null())?;
            let mut device = Device { client: client.clone(), render, event, buffer_frames: 0, id };
            client.SetEventHandle(event)?;
            device.buffer_frames = client.GetBufferSize()?;
            let vol: ISimpleAudioVolume = client.GetService()?;
            vol.SetMasterVolume(volume.clamp(0.0, 1.0), std::ptr::null())?;
            // Buffer inicial em silêncio, para o evento começar a bater.
            let n = device.buffer_frames;
            device.render.GetBuffer(n)?;
            device.render.ReleaseBuffer(n, AUDCLNT_BUFFERFLAGS_SILENT.0 as u32)?;
            client.Start()?;
            Ok(device)
        }
    }

    /// Espera o evento (até 100 ms) e enche o espaço livre do buffer. `Err` pede para reabrir.
    fn render(&self, engine: &AudioEngine, scratch: &mut Vec<i16>) -> windows::core::Result<()> {
        // SAFETY: `event` é um evento válido de propriedade de `self`. `GetBuffer(avail)` devolve um bloco de
        // `avail` quadros de 4 bytes (estéreo S16); `slice_mut` o cobre exatamente, e `ReleaseBuffer(avail)`
        // vem logo depois, sem outra chamada ao `render` no meio.
        unsafe {
            let w = WaitForSingleObject(self.event, 100);
            if w != WAIT_OBJECT_0 {
                return Ok(());
            }
            let padding = self.client.GetCurrentPadding()?;
            let avail = self.buffer_frames.saturating_sub(padding);
            if avail == 0 {
                return Ok(());
            }
            scratch.resize(avail as usize * 2, 0);
            engine.pull(scratch);
            let p = self.render.GetBuffer(avail)?;
            let dst = std::slice::from_raw_parts_mut(p as *mut i16, avail as usize * 2);
            dst.copy_from_slice(scratch);
            self.render.ReleaseBuffer(avail, 0)?;
            Ok(())
        }
    }
}

fn run(engine: &AudioEngine, volume: f32, first: &mpsc::Sender<Result<(), String>>) {
    // SAFETY: COM inicializado nesta thread antes de qualquer uso e desinicializado no fim dela.
    let com = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
    if com.is_err() {
        let _ = first.send(Err(format!("CoInitializeEx falhou: {com:?}")));
        return;
    }
    // SAFETY: criação do enumerador padrão do sistema; COM já está inicializado.
    let enumerator: windows::core::Result<IMMDeviceEnumerator> = unsafe { CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL) };
    let enumerator = match enumerator {
        Ok(e) => e,
        Err(e) => {
            let _ = first.send(Err(format!("não foi possível criar o enumerador de dispositivos: {e}")));
            // SAFETY: par do `CoInitializeEx` acima.
            unsafe { CoUninitialize() };
            return;
        }
    };
    let mut announced = false;
    let mut scratch: Vec<i16> = Vec::new();
    while !engine.is_shutdown() {
        let dev = match Device::open(&enumerator, volume) {
            Ok(d) => d,
            Err(e) => {
                if !announced {
                    let _ = first.send(Err(format!("não foi possível abrir o dispositivo de áudio: {e}")));
                    break;
                }
                println!("[psc] áudio: não foi possível reabrir o dispositivo ({e}); nova tentativa em 1 s");
                std::thread::sleep(Duration::from_secs(1));
                continue;
            }
        };
        if !announced {
            announced = true;
            let _ = first.send(Ok(()));
        } else {
            println!("[psc] áudio: reaberto no dispositivo padrão atual");
        }
        let mut last_check = Instant::now();
        loop {
            if engine.is_shutdown() {
                break;
            }
            if let Err(e) = dev.render(engine, &mut scratch) {
                println!("[psc] áudio: o dispositivo falhou ({e}); reabrindo");
                break;
            }
            if last_check.elapsed() >= Duration::from_secs(1) {
                last_check = Instant::now();
                match default_endpoint_id(&enumerator) {
                    Ok(id) if id != dev.id => {
                        println!("[psc] áudio: o dispositivo padrão mudou; reabrindo no novo");
                        break;
                    }
                    _ => {}
                }
            }
        }
    }
    drop(enumerator);
    // SAFETY: par do `CoInitializeEx` acima, na mesma thread.
    unsafe { CoUninitialize() };
}

/// Abre o dispositivo padrão e inicia a thread de saída. Erro se o primeiro `open` falhar.
pub fn spawn(engine: Arc<AudioEngine>, volume: f32) -> Result<(), String> {
    let (tx, rx) = mpsc::channel();
    std::thread::Builder::new()
        .name("psc-audio-wasapi".into())
        .spawn(move || run(&engine, volume, &tx))
        .map_err(|e| format!("não foi possível criar a thread de áudio: {e}"))?;
    rx.recv().map_err(|_| "a thread de áudio terminou sem responder".to_string())?
}
