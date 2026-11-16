//! Declarações `extern "C"` das funções do NDK que o núcleo usa, escritas à mão (docs/26, decisão 3 e "Dependências
//! aprovadas": nada de `ndk-sys`). Cada função traz o cabeçalho de onde a assinatura foi copiada (NDK 27.1.12297006,
//! `toolchains/llvm/prebuilt/windows-x86_64/sysroot/usr/include`). Neste arquivo ficam também os chamadores seguros
//! das funções simples.
//!
//! `unsafe` só aqui e em `jni.rs` (mais os módulos de superfície, decodificador e som, nas próximas etapas).

use std::ffi::{c_char, c_int, c_void, CString};

#[link(name = "log")]
extern "C" {
    /// `android/log.h`: `int __android_log_write(int prio, const char* tag, const char* text);`
    fn __android_log_write(prio: c_int, tag: *const c_char, text: *const c_char) -> c_int;
}

/// Grava `texto` no `logcat` com a etiqueta `tag` e a prioridade `prio` (valores de `android_LogPriority`).
/// Texto ou etiqueta com NUL no meio são cortados nele.
pub fn log_write(prio: i32, tag: &str, texto: &str) {
    let c = |s: &str| CString::new(s.split('\0').next().unwrap_or("")).expect("sem NUL depois do corte");
    let (tag, texto) = (c(tag), c(texto));
    // SAFETY: `tag` e `texto` são strings C válidas, terminadas em NUL, que vivem até o fim da chamada; a função só
    // lê os dois ponteiros e não guarda nenhum deles.
    unsafe {
        __android_log_write(prio as c_int, tag.as_ptr(), texto.as_ptr());
    }
}

// ---------------------------------------------------------------------------------------------
// ANativeWindow (a superfície da casca): libandroid
// ---------------------------------------------------------------------------------------------

/// `ANativeWindow`: tipo opaco (`android/native_window.h`).
#[repr(C)]
pub struct ANativeWindow {
    _privado: [u8; 0],
}

/// `ANativeWindow_Buffer` (`android/native_window.h`): `width`, `height` e `stride` em **pixels**; `format` como em
/// `AHardwareBuffer_Format`; `bits` aponta para a primeira linha.
#[repr(C)]
pub struct ANativeWindowBuffer {
    pub width: i32,
    pub height: i32,
    pub stride: i32,
    pub format: i32,
    pub bits: *mut c_void,
    pub reserved: [u32; 6],
}

/// `WINDOW_FORMAT_RGBA_8888` e `WINDOW_FORMAT_RGBX_8888` (`android/native_window.h`).
pub const WINDOW_FORMAT_RGBA_8888: i32 = 1;
pub const WINDOW_FORMAT_RGBX_8888: i32 = 2;

#[link(name = "android")]
extern "C" {
    /// `android/native_window_jni.h`: `ANativeWindow* ANativeWindow_fromSurface(JNIEnv* env, jobject surface);`
    pub fn ANativeWindow_fromSurface(env: *mut c_void, surface: *mut c_void) -> *mut ANativeWindow;
    /// `android/native_window.h`: `void ANativeWindow_release(ANativeWindow* window);`
    pub fn ANativeWindow_release(window: *mut ANativeWindow);
    /// `android/native_window.h`: `int32_t ANativeWindow_setBuffersGeometry(ANativeWindow* window, int32_t width, int32_t height, int32_t format);`
    pub fn ANativeWindow_setBuffersGeometry(window: *mut ANativeWindow, width: i32, height: i32, format: i32) -> i32;
    /// `android/native_window.h`: `int32_t ANativeWindow_lock(ANativeWindow* window, ANativeWindow_Buffer* outBuffer, ARect* inOutDirtyBounds);`
    pub fn ANativeWindow_lock(window: *mut ANativeWindow, out_buffer: *mut ANativeWindowBuffer, dirty: *mut c_void) -> i32;
    /// `android/native_window.h`: `int32_t ANativeWindow_unlockAndPost(ANativeWindow* window);`
    pub fn ANativeWindow_unlockAndPost(window: *mut ANativeWindow) -> i32;
}

// ---------------------------------------------------------------------------------------------
// AMediaCodec e AMediaFormat (decodificador H.264): libmediandk
// ---------------------------------------------------------------------------------------------

/// `AMediaCodec` e `AMediaFormat`: tipos opacos (`media/NdkMediaCodec.h`, `media/NdkMediaFormat.h`).
#[repr(C)]
pub struct AMediaCodec {
    _privado: [u8; 0],
}

#[repr(C)]
pub struct AMediaFormat {
    _privado: [u8; 0],
}

/// `AMediaCodecBufferInfo` (`media/NdkMediaCodec.h`): `offset` e `size` são os bytes válidos do buffer.
#[repr(C)]
pub struct AMediaCodecBufferInfo {
    pub offset: i32,
    pub size: i32,
    pub presentation_time_us: i64,
    pub flags: u32,
}

/// `media_status_t` (`media/NdkMediaError.h`): 0 é `AMEDIA_OK`.
pub const AMEDIA_OK: i32 = 0;

#[link(name = "mediandk")]
extern "C" {
    /// `media/NdkMediaCodec.h`: `AMediaCodec* AMediaCodec_createDecoderByType(const char* mime_type);`
    pub fn AMediaCodec_createDecoderByType(mime_type: *const c_char) -> *mut AMediaCodec;
    /// `media/NdkMediaCodec.h`: `media_status_t AMediaCodec_configure(AMediaCodec*, const AMediaFormat* format, ANativeWindow* surface, AMediaCrypto* crypto, uint32_t flags);`
    pub fn AMediaCodec_configure(codec: *mut AMediaCodec, format: *const AMediaFormat, surface: *mut ANativeWindow, crypto: *mut c_void, flags: u32) -> i32;
    /// `media/NdkMediaCodec.h`: `media_status_t AMediaCodec_start(AMediaCodec*);`
    pub fn AMediaCodec_start(codec: *mut AMediaCodec) -> i32;
    /// `media/NdkMediaCodec.h`: `media_status_t AMediaCodec_stop(AMediaCodec*);`
    pub fn AMediaCodec_stop(codec: *mut AMediaCodec) -> i32;
    /// `media/NdkMediaCodec.h`: `media_status_t AMediaCodec_delete(AMediaCodec*);`
    pub fn AMediaCodec_delete(codec: *mut AMediaCodec) -> i32;
    /// `media/NdkMediaCodec.h`: `ssize_t AMediaCodec_dequeueInputBuffer(AMediaCodec*, int64_t timeoutUs);`
    pub fn AMediaCodec_dequeueInputBuffer(codec: *mut AMediaCodec, timeout_us: i64) -> isize;
    /// `media/NdkMediaCodec.h`: `uint8_t* AMediaCodec_getInputBuffer(AMediaCodec*, size_t idx, size_t* out_size);`
    pub fn AMediaCodec_getInputBuffer(codec: *mut AMediaCodec, idx: usize, out_size: *mut usize) -> *mut u8;
    /// `media/NdkMediaCodec.h`: `media_status_t AMediaCodec_queueInputBuffer(AMediaCodec*, size_t idx, off_t offset, size_t size, uint64_t time, uint32_t flags);`
    pub fn AMediaCodec_queueInputBuffer(codec: *mut AMediaCodec, idx: usize, offset: i64, size: usize, time: u64, flags: u32) -> i32;
    /// `media/NdkMediaCodec.h`: `ssize_t AMediaCodec_dequeueOutputBuffer(AMediaCodec*, AMediaCodecBufferInfo* info, int64_t timeoutUs);`
    pub fn AMediaCodec_dequeueOutputBuffer(codec: *mut AMediaCodec, info: *mut AMediaCodecBufferInfo, timeout_us: i64) -> isize;
    /// `media/NdkMediaCodec.h`: `uint8_t* AMediaCodec_getOutputBuffer(AMediaCodec*, size_t idx, size_t* out_size);`
    pub fn AMediaCodec_getOutputBuffer(codec: *mut AMediaCodec, idx: usize, out_size: *mut usize) -> *mut u8;
    /// `media/NdkMediaCodec.h`: `media_status_t AMediaCodec_releaseOutputBuffer(AMediaCodec*, size_t idx, bool render);`
    pub fn AMediaCodec_releaseOutputBuffer(codec: *mut AMediaCodec, idx: usize, render: bool) -> i32;
    /// `media/NdkMediaCodec.h`: `AMediaFormat* AMediaCodec_getOutputFormat(AMediaCodec*);`
    pub fn AMediaCodec_getOutputFormat(codec: *mut AMediaCodec) -> *mut AMediaFormat;
    /// `media/NdkMediaFormat.h`: `AMediaFormat* AMediaFormat_new();`
    pub fn AMediaFormat_new() -> *mut AMediaFormat;
    /// `media/NdkMediaFormat.h`: `media_status_t AMediaFormat_delete(AMediaFormat*);`
    pub fn AMediaFormat_delete(format: *mut AMediaFormat) -> i32;
    /// `media/NdkMediaFormat.h`: `void AMediaFormat_setString(AMediaFormat*, const char* name, const char* value);`
    pub fn AMediaFormat_setString(format: *mut AMediaFormat, name: *const c_char, value: *const c_char);
    /// `media/NdkMediaFormat.h`: `void AMediaFormat_setInt32(AMediaFormat*, const char* name, int32_t value);`
    pub fn AMediaFormat_setInt32(format: *mut AMediaFormat, name: *const c_char, value: i32);
    /// `media/NdkMediaFormat.h`: `bool AMediaFormat_getInt32(AMediaFormat*, const char* name, int32_t* out);`
    pub fn AMediaFormat_getInt32(format: *mut AMediaFormat, name: *const c_char, out: *mut i32) -> bool;
}

// ---------------------------------------------------------------------------------------------
// AAudio (saída de som): libaaudio
// ---------------------------------------------------------------------------------------------

/// `AAudioStream` e `AAudioStreamBuilder`: tipos opacos (`aaudio/AAudio.h`).
#[repr(C)]
pub struct AAudioStream {
    _privado: [u8; 0],
}

#[repr(C)]
pub struct AAudioStreamBuilder {
    _privado: [u8; 0],
}

/// `AAUDIO_OK`, `AAUDIO_DIRECTION_OUTPUT`, `AAUDIO_FORMAT_PCM_I16`, `AAUDIO_PERFORMANCE_MODE_LOW_LATENCY` e
/// `AAUDIO_CALLBACK_RESULT_CONTINUE` (`aaudio/AAudio.h`).
pub const AAUDIO_OK: i32 = 0;
pub const AAUDIO_DIRECTION_OUTPUT: i32 = 0;
pub const AAUDIO_FORMAT_PCM_I16: i32 = 1;
pub const AAUDIO_PERFORMANCE_MODE_LOW_LATENCY: i32 = 12;
pub const AAUDIO_CALLBACK_RESULT_CONTINUE: i32 = 0;

/// `aaudio_data_callback_result_t (*AAudioStream_dataCallback)(AAudioStream*, void* userData, void* audioData, int32_t numFrames)`.
pub type AAudioDataCallback = unsafe extern "C" fn(stream: *mut AAudioStream, user_data: *mut c_void, audio_data: *mut c_void, num_frames: i32) -> i32;
/// `void (*AAudioStream_errorCallback)(AAudioStream*, void* userData, aaudio_result_t error)`.
pub type AAudioErrorCallback = unsafe extern "C" fn(stream: *mut AAudioStream, user_data: *mut c_void, error: i32);

#[link(name = "aaudio")]
extern "C" {
    /// `aaudio/AAudio.h`: `aaudio_result_t AAudio_createStreamBuilder(AAudioStreamBuilder** builder);`
    pub fn AAudio_createStreamBuilder(builder: *mut *mut AAudioStreamBuilder) -> i32;
    /// `aaudio/AAudio.h`: `void AAudioStreamBuilder_setDirection(AAudioStreamBuilder*, aaudio_direction_t direction);`
    pub fn AAudioStreamBuilder_setDirection(builder: *mut AAudioStreamBuilder, direction: i32);
    /// `aaudio/AAudio.h`: `void AAudioStreamBuilder_setSampleRate(AAudioStreamBuilder*, int32_t sampleRate);`
    pub fn AAudioStreamBuilder_setSampleRate(builder: *mut AAudioStreamBuilder, sample_rate: i32);
    /// `aaudio/AAudio.h`: `void AAudioStreamBuilder_setChannelCount(AAudioStreamBuilder*, int32_t channelCount);`
    pub fn AAudioStreamBuilder_setChannelCount(builder: *mut AAudioStreamBuilder, channel_count: i32);
    /// `aaudio/AAudio.h`: `void AAudioStreamBuilder_setFormat(AAudioStreamBuilder*, aaudio_format_t format);`
    pub fn AAudioStreamBuilder_setFormat(builder: *mut AAudioStreamBuilder, format: i32);
    /// `aaudio/AAudio.h`: `void AAudioStreamBuilder_setPerformanceMode(AAudioStreamBuilder*, aaudio_performance_mode_t mode);`
    pub fn AAudioStreamBuilder_setPerformanceMode(builder: *mut AAudioStreamBuilder, mode: i32);
    /// `aaudio/AAudio.h`: `void AAudioStreamBuilder_setDataCallback(AAudioStreamBuilder*, AAudioStream_dataCallback callback, void* userData);`
    pub fn AAudioStreamBuilder_setDataCallback(builder: *mut AAudioStreamBuilder, callback: AAudioDataCallback, user_data: *mut c_void);
    /// `aaudio/AAudio.h`: `void AAudioStreamBuilder_setErrorCallback(AAudioStreamBuilder*, AAudioStream_errorCallback callback, void* userData);`
    pub fn AAudioStreamBuilder_setErrorCallback(builder: *mut AAudioStreamBuilder, callback: AAudioErrorCallback, user_data: *mut c_void);
    /// `aaudio/AAudio.h`: `aaudio_result_t AAudioStreamBuilder_openStream(AAudioStreamBuilder*, AAudioStream** stream);`
    pub fn AAudioStreamBuilder_openStream(builder: *mut AAudioStreamBuilder, stream: *mut *mut AAudioStream) -> i32;
    /// `aaudio/AAudio.h`: `aaudio_result_t AAudioStreamBuilder_delete(AAudioStreamBuilder*);`
    pub fn AAudioStreamBuilder_delete(builder: *mut AAudioStreamBuilder) -> i32;
    /// `aaudio/AAudio.h`: `aaudio_result_t AAudioStream_requestStart(AAudioStream*);`
    pub fn AAudioStream_requestStart(stream: *mut AAudioStream) -> i32;
    /// `aaudio/AAudio.h`: `aaudio_result_t AAudioStream_requestStop(AAudioStream*);`
    pub fn AAudioStream_requestStop(stream: *mut AAudioStream) -> i32;
    /// `aaudio/AAudio.h`: `aaudio_result_t AAudioStream_close(AAudioStream*);`
    pub fn AAudioStream_close(stream: *mut AAudioStream) -> i32;
}
