
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
        unsafe {
            let _ = self.client.Stop();
            let _ = CloseHandle(self.event);
        }
    }
}

fn default_endpoint_id(enumerator: &IMMDeviceEnumerator) -> windows::core::Result<String> {
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
        unsafe {
            let id = default_endpoint_id(enumerator)?;
            let dev = enumerator.GetDefaultAudioEndpoint(eRender, eConsole)?;
            let client: IAudioClient = dev.Activate(CLSCTX_ALL, None)?;
            let fmt = WAVEFORMATEX {
                wFormatTag: 1,
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
            let render: IAudioRenderClient = client.GetService()?;
            let event = CreateEventW(None, false, false, PCWSTR::null())?;
            let mut device = Device { client: client.clone(), render, event, buffer_frames: 0, id };
            client.SetEventHandle(event)?;
            device.buffer_frames = client.GetBufferSize()?;
            let vol: ISimpleAudioVolume = client.GetService()?;
            vol.SetMasterVolume(volume.clamp(0.0, 1.0), std::ptr::null())?;
            let n = device.buffer_frames;
            device.render.GetBuffer(n)?;
            device.render.ReleaseBuffer(n, AUDCLNT_BUFFERFLAGS_SILENT.0 as u32)?;
            client.Start()?;
            Ok(device)
        }
    }

    fn render(&self, engine: &AudioEngine, scratch: &mut Vec<i16>) -> windows::core::Result<()> {
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
    let com = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
    if com.is_err() {
        let _ = first.send(Err(format!("CoInitializeEx falhou: {com:?}")));
        return;
    }
    let enumerator: windows::core::Result<IMMDeviceEnumerator> = unsafe { CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL) };
    let enumerator = match enumerator {
        Ok(e) => e,
        Err(e) => {
            let _ = first.send(Err(format!("não foi possível criar o enumerador de dispositivos: {e}")));
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
    unsafe { CoUninitialize() };
}

pub fn spawn(engine: Arc<AudioEngine>, volume: f32) -> Result<(), String> {
    let (tx, rx) = mpsc::channel();
    std::thread::Builder::new()
        .name("psc-audio-wasapi".into())
        .spawn(move || run(&engine, volume, &tx))
        .map_err(|e| format!("não foi possível criar a thread de áudio: {e}"))?;
    rx.recv().map_err(|_| "a thread de áudio terminou sem responder".to_string())?
}
