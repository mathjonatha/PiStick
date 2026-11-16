
use std::ffi::{c_char, c_int, c_void, CString};

#[link(name = "log")]
extern "C" {
    fn __android_log_write(prio: c_int, tag: *const c_char, text: *const c_char) -> c_int;
}

pub fn log_write(prio: i32, tag: &str, texto: &str) {
    let c = |s: &str| CString::new(s.split('\0').next().unwrap_or("")).expect("sem NUL depois do corte");
    let (tag, texto) = (c(tag), c(texto));
    unsafe {
        __android_log_write(prio as c_int, tag.as_ptr(), texto.as_ptr());
    }
}

#[repr(C)]
pub struct ANativeWindow {
    _privado: [u8; 0],
}

#[repr(C)]
pub struct ANativeWindowBuffer {
    pub width: i32,
    pub height: i32,
    pub stride: i32,
    pub format: i32,
    pub bits: *mut c_void,
    pub reserved: [u32; 6],
}

pub const WINDOW_FORMAT_RGBA_8888: i32 = 1;
pub const WINDOW_FORMAT_RGBX_8888: i32 = 2;

#[link(name = "android")]
extern "C" {
    pub fn ANativeWindow_fromSurface(env: *mut c_void, surface: *mut c_void) -> *mut ANativeWindow;
    pub fn ANativeWindow_release(window: *mut ANativeWindow);
    pub fn ANativeWindow_setBuffersGeometry(window: *mut ANativeWindow, width: i32, height: i32, format: i32) -> i32;
    pub fn ANativeWindow_lock(window: *mut ANativeWindow, out_buffer: *mut ANativeWindowBuffer, dirty: *mut c_void) -> i32;
    pub fn ANativeWindow_unlockAndPost(window: *mut ANativeWindow) -> i32;
}

#[repr(C)]
pub struct AMediaCodec {
    _privado: [u8; 0],
}

#[repr(C)]
pub struct AMediaFormat {
    _privado: [u8; 0],
}

#[repr(C)]
pub struct AMediaCodecBufferInfo {
    pub offset: i32,
    pub size: i32,
    pub presentation_time_us: i64,
    pub flags: u32,
}

pub const AMEDIA_OK: i32 = 0;

#[link(name = "mediandk")]
extern "C" {
    pub fn AMediaCodec_createDecoderByType(mime_type: *const c_char) -> *mut AMediaCodec;
    pub fn AMediaCodec_configure(codec: *mut AMediaCodec, format: *const AMediaFormat, surface: *mut ANativeWindow, crypto: *mut c_void, flags: u32) -> i32;
    pub fn AMediaCodec_start(codec: *mut AMediaCodec) -> i32;
    pub fn AMediaCodec_stop(codec: *mut AMediaCodec) -> i32;
    pub fn AMediaCodec_delete(codec: *mut AMediaCodec) -> i32;
    pub fn AMediaCodec_dequeueInputBuffer(codec: *mut AMediaCodec, timeout_us: i64) -> isize;
    pub fn AMediaCodec_getInputBuffer(codec: *mut AMediaCodec, idx: usize, out_size: *mut usize) -> *mut u8;
    pub fn AMediaCodec_queueInputBuffer(codec: *mut AMediaCodec, idx: usize, offset: i64, size: usize, time: u64, flags: u32) -> i32;
    pub fn AMediaCodec_dequeueOutputBuffer(codec: *mut AMediaCodec, info: *mut AMediaCodecBufferInfo, timeout_us: i64) -> isize;
    pub fn AMediaCodec_getOutputBuffer(codec: *mut AMediaCodec, idx: usize, out_size: *mut usize) -> *mut u8;
    pub fn AMediaCodec_releaseOutputBuffer(codec: *mut AMediaCodec, idx: usize, render: bool) -> i32;
    pub fn AMediaCodec_getOutputFormat(codec: *mut AMediaCodec) -> *mut AMediaFormat;
    pub fn AMediaFormat_new() -> *mut AMediaFormat;
    pub fn AMediaFormat_delete(format: *mut AMediaFormat) -> i32;
    pub fn AMediaFormat_setString(format: *mut AMediaFormat, name: *const c_char, value: *const c_char);
    pub fn AMediaFormat_setInt32(format: *mut AMediaFormat, name: *const c_char, value: i32);
    pub fn AMediaFormat_getInt32(format: *mut AMediaFormat, name: *const c_char, out: *mut i32) -> bool;
}

#[repr(C)]
pub struct AAudioStream {
    _privado: [u8; 0],
}

#[repr(C)]
pub struct AAudioStreamBuilder {
    _privado: [u8; 0],
}

pub const AAUDIO_OK: i32 = 0;
pub const AAUDIO_DIRECTION_OUTPUT: i32 = 0;
pub const AAUDIO_FORMAT_PCM_I16: i32 = 1;
pub const AAUDIO_PERFORMANCE_MODE_LOW_LATENCY: i32 = 12;
pub const AAUDIO_CALLBACK_RESULT_CONTINUE: i32 = 0;

pub type AAudioDataCallback = unsafe extern "C" fn(stream: *mut AAudioStream, user_data: *mut c_void, audio_data: *mut c_void, num_frames: i32) -> i32;
pub type AAudioErrorCallback = unsafe extern "C" fn(stream: *mut AAudioStream, user_data: *mut c_void, error: i32);

#[link(name = "aaudio")]
extern "C" {
    pub fn AAudio_createStreamBuilder(builder: *mut *mut AAudioStreamBuilder) -> i32;
    pub fn AAudioStreamBuilder_setDirection(builder: *mut AAudioStreamBuilder, direction: i32);
    pub fn AAudioStreamBuilder_setSampleRate(builder: *mut AAudioStreamBuilder, sample_rate: i32);
    pub fn AAudioStreamBuilder_setChannelCount(builder: *mut AAudioStreamBuilder, channel_count: i32);
    pub fn AAudioStreamBuilder_setFormat(builder: *mut AAudioStreamBuilder, format: i32);
    pub fn AAudioStreamBuilder_setPerformanceMode(builder: *mut AAudioStreamBuilder, mode: i32);
    pub fn AAudioStreamBuilder_setDataCallback(builder: *mut AAudioStreamBuilder, callback: AAudioDataCallback, user_data: *mut c_void);
    pub fn AAudioStreamBuilder_setErrorCallback(builder: *mut AAudioStreamBuilder, callback: AAudioErrorCallback, user_data: *mut c_void);
    pub fn AAudioStreamBuilder_openStream(builder: *mut AAudioStreamBuilder, stream: *mut *mut AAudioStream) -> i32;
    pub fn AAudioStreamBuilder_delete(builder: *mut AAudioStreamBuilder) -> i32;
    pub fn AAudioStream_requestStart(stream: *mut AAudioStream) -> i32;
    pub fn AAudioStream_requestStop(stream: *mut AAudioStream) -> i32;
    pub fn AAudioStream_close(stream: *mut AAudioStream) -> i32;
}
