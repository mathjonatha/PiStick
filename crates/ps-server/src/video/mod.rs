
use std::io;

pub mod annexb;
pub mod detect;
#[cfg(target_os = "linux")]
pub mod v4l2;

pub const VIDEO_FPS: u32 = 30;
pub const MIN_CAPTURE_INTERVAL_MS: u64 = 33;
pub const DEFAULT_BITRATE: u32 = 5_000_000;
pub const DEFAULT_MAX_QP: u32 = 36;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VideoConfig {
    pub width: u32,
    pub height: u32,
    pub bitrate: u32,
    pub fps: u32,
    pub max_qp: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EncodedFrame {
    pub data: Vec<u8>,
    pub keyframe: bool,
}

pub trait VideoEncoder: Send {
    fn encode(&mut self, pixels: &[u8], stride: usize) -> io::Result<EncodedFrame>;

    fn describe(&self) -> Option<String> {
        None
    }
}

pub trait VideoEncoderFactory: Send + Sync {
    fn open(&self, cfg: &VideoConfig) -> io::Result<Box<dyn VideoEncoder>>;
}
