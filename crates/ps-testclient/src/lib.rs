#![cfg(target_os = "linux")]
// Lint novo do clippy do Rust 1.100 (Pi); o Rust 1.95 do notebook não o conhece como erro.
#![allow(unknown_lints, clippy::chunks_exact_to_as_chunks)]
//! Cliente Wayland mínimo para testar o compositor do `psd`.
//!
//! Abre um `xdg_toplevel` com buffers `wl_shm`, desenha um padrão determinístico (módulo
//! [`pattern`]) e declara dano parcial a cada quadro, guiado por `wl_surface.frame`.
//! Redesenha no tamanho pedido pelo `configure` e sai quando recebe `close`. Pode registrar
//! em arquivo os eventos de entrada que recebe.

mod client;
pub mod dmabuf;
pub mod pattern;
pub mod subsurface;

pub use dmabuf::{run_dmabuf, Defeito, DmabufOptions, FormatoDmabuf};
pub use subsurface::{run_sub, Cenario, SubOptions};
pub use client::{run, CursorMode, Options, IMAGE_HOTSPOT, IMAGE_PIXEL, IMAGE_SIDE};
