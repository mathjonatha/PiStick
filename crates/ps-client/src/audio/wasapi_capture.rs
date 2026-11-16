
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

const BUFFER_100NS: i64 = 500_000;

struct Capture {
    client: IAudioClient,
    capture: IAudioCaptureClient,
    event: HANDLE,
}

impl Drop for Capture {
    fn drop(&mut self) {
        unsafe {
            let _ = self.client.Stop();
            let _ = CloseHandle(self.event);
        }
    }
}

impl Capture {
    fn open(enumerator: &IMMDeviceEnumerator) -> Result<Capture, String> {
        unsafe {
            let dev = enumerator.GetDefaultAudioEndpoint(eCapture, eConsole).map_err(|e| format!("sem dispositivo de gravação padrão ({e})"))?;
            let client: IAudioClient = dev.Activate(CLSCTX_ALL, None).map_err(|e| format!("não foi possível ativar o dispositivo de gravação ({e})"))?;
            let fmt = WAVEFORMATEX {
                wFormatTag: 1,
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

    fn read(&self, acc: &mut Vec<i16>) -> windows::core::Result<()> {
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

fn drain_blocks(acc: &mut Vec<i16>, sink: &mut dyn FnMut(&[i16])) {
    let block = BLOCK_FRAMES * 2;
    let mut used = 0;
    while acc.len() - used >= block {
        sink(&acc[used..used + block]);
        used += block;
    }
    acc.drain(..used);
}

fn sleep_unless_stopped(stop: &AtomicBool, total: Duration) {
    let t0 = std::time::Instant::now();
    while t0.elapsed() < total && !stop.load(Ordering::SeqCst) {
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn run_inner(stop: &AtomicBool, on_open: &mut dyn FnMut(), sink: &mut dyn FnMut(&[i16])) -> Result<(), String> {
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
    }
    Ok(())
}

pub fn run(stop: &AtomicBool, on_open: &mut dyn FnMut(), sink: &mut dyn FnMut(&[i16])) -> Result<(), String> {
    let com = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
    if com.is_err() {
        return Err(format!("CoInitializeEx falhou ({com:?})"));
    }
    let result = run_inner(stop, on_open, sink);
    unsafe { CoUninitialize() };
    result
}
