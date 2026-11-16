//! Núcleo nativo do cliente Android do PiStick (marco 6, `docs/26`).
//!
//! A casca Java (`android/`) carrega esta biblioteca e a chama pelo JNI; o núcleo **não chama Java** (decisão 17).
//! Todo o `unsafe` do crate fica nos módulos da decisão 3: `jni` (fronteira com a casca) e `ndk` (funções do NDK).
//! A lógica pura (sem `cfg(target_os = "android")`) é testada no computador pelo `cargo test --workspace`; fora do
//! Android a biblioteca só tem essa parte.

pub mod decodificador;
pub mod entrada;
pub mod eventos;
pub mod gestos;
pub mod janelas;
pub mod layout;
pub mod registro;
pub mod saida_audio;
pub mod sessao;
pub mod teclado;
pub mod tela;
pub mod versao;

#[cfg(target_os = "android")]
mod aaudio;
#[cfg(target_os = "android")]
mod jni;
#[cfg(target_os = "android")]
mod mediacodec;
#[cfg(target_os = "android")]
mod ndk;
#[cfg(target_os = "android")]
mod surface;
