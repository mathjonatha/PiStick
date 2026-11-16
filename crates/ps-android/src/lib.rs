
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
