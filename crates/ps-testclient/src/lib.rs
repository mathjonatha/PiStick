#![cfg(target_os = "linux")]
#![allow(unknown_lints, clippy::chunks_exact_to_as_chunks)]

mod client;
pub mod dmabuf;
pub mod pattern;
pub mod subsurface;

pub use dmabuf::{run_dmabuf, Defeito, DmabufOptions, FormatoDmabuf};
pub use subsurface::{run_sub, Cenario, SubOptions};
pub use client::{run, CursorMode, Options, IMAGE_HOTSPOT, IMAGE_PIXEL, IMAGE_SIDE};
