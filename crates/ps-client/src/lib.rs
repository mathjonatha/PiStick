
use ps_proto::{CAPS_MARCO3, CAP_BLOCK_CODEC, CAP_SESSION};

pub mod atomic;
pub mod audio;
pub mod autotype;
pub mod clipboard;
pub mod compose;
#[cfg(windows)]
pub mod dpapi;
pub mod discovery;
pub mod keystore;
pub mod mic;
pub mod net;
pub mod png;
pub mod registry;
pub mod secure;
pub mod stats;
pub mod verify;
pub mod video;

#[cfg(feature = "desktop")]
pub mod app;
#[cfg(feature = "desktop")]
mod desktop;
#[cfg(feature = "desktop")]
pub mod keymap;
#[cfg(feature = "desktop")]
pub mod script;

#[cfg(feature = "desktop")]
pub use app::{ExitReason, ForcedFocus};
#[cfg(feature = "desktop")]
pub use desktop::{run, run_connected, run_with_transport, RunError, RunOptions};

pub const CLIENT_CAPS: u32 = CAP_BLOCK_CODEC | CAPS_MARCO3 | CAP_SESSION;
