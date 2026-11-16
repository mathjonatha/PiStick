//! Cursor do cliente de teste: nomeado (`wp_cursor_shape_v1`), por imagem (`wl_pointer.set_cursor` com
//! uma superfície) ou oculto. É aplicado quando o ponteiro entra na janela.

use std::os::fd::OwnedFd;
use std::os::unix::fs::FileExt;

use wayland_client::protocol::wl_buffer::WlBuffer;
use wayland_client::protocol::wl_shm;
use wayland_client::protocol::wl_surface::WlSurface;
use wayland_client::{Connection, Dispatch, QueueHandle};
use wayland_protocols::wp::cursor_shape::v1::client::wp_cursor_shape_device_v1::{Shape, WpCursorShapeDeviceV1};
use wayland_protocols::wp::cursor_shape::v1::client::wp_cursor_shape_manager_v1::WpCursorShapeManagerV1;

use super::State;

/// Lado da imagem de cursor de teste.
pub const IMAGE_SIDE: u32 = 16;
/// Ponto de ação da imagem de cursor de teste.
pub const IMAGE_HOTSPOT: (i32, i32) = (3, 4);
/// Pixel da imagem de cursor de teste, BGRA pré-multiplicado (vermelho a 50%).
pub const IMAGE_PIXEL: [u8; 4] = [0, 0, 128, 128];

/// Como o cliente de teste define o cursor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CursorMode {
    /// Pelo nome, com `wp_cursor_shape_v1`.
    Named(String),
    /// Imagem: a superfície recebe o buffer antes de `set_cursor`.
    Image,
    /// Imagem: `set_cursor` primeiro, o buffer depois (a outra ordem possível).
    ImageLate,
    /// Cursor oculto (`set_cursor` com superfície nula).
    Hidden,
}

pub(super) struct CursorObjs {
    surface: Option<WlSurface>,
    buffer: Option<WlBuffer>,
    _file: Option<std::fs::File>,
}

impl CursorObjs {
    pub(super) fn new() -> Self {
        CursorObjs { surface: None, buffer: None, _file: None }
    }
}

fn shape_of(nome: &str) -> Shape {
    match nome {
        "text" => Shape::Text,
        "pointer" => Shape::Pointer,
        "ew-resize" => Shape::EwResize,
        "wait" => Shape::Wait,
        "crosshair" => Shape::Crosshair,
        _ => Shape::Default,
    }
}

impl State {
    /// O ponteiro entrou na janela com `serial`: define o cursor do modo escolhido.
    pub(super) fn apply_cursor(&mut self, serial: u32, qh: &QueueHandle<State>) {
        for _ in 0..=self.cursor_repeat {
            self.apply_cursor_once(serial, qh);
        }
    }

    fn apply_cursor_once(&mut self, serial: u32, qh: &QueueHandle<State>) {
        let Some(modo) = self.cursor_mode.clone() else { return };
        let Some(pointer) = self.pointer.clone() else { return };
        match modo {
            CursorMode::Named(nome) => {
                let Some(mgr) = self.cursor_shape_mgr.clone() else {
                    self.log("cursor-shape-indisponivel".into());
                    return;
                };
                let dev: WpCursorShapeDeviceV1 = mgr.get_pointer(&pointer, qh, ());
                dev.set_shape(serial, shape_of(&nome));
                dev.destroy();
                self.log(format!("cursor-set named {nome}"));
            }
            CursorMode::Hidden => {
                pointer.set_cursor(serial, None, 0, 0);
                self.log("cursor-set hidden".into());
            }
            CursorMode::Image | CursorMode::ImageLate => {
                if self.cursor_objs.surface.is_none() {
                    self.cursor_objs.surface = Some(self.compositor.create_surface(qh, ()));
                }
                let surface = self.cursor_objs.surface.clone().expect("superfície do cursor");
                if modo == CursorMode::Image {
                    self.commit_cursor_image(&surface, qh);
                }
                pointer.set_cursor(serial, Some(&surface), IMAGE_HOTSPOT.0, IMAGE_HOTSPOT.1);
                if modo == CursorMode::ImageLate {
                    self.commit_cursor_image(&surface, qh);
                }
                self.log(format!("cursor-set image {IMAGE_SIDE}x{IMAGE_SIDE} hot {},{}", IMAGE_HOTSPOT.0, IMAGE_HOTSPOT.1));
            }
        }
    }

    fn commit_cursor_image(&mut self, surface: &WlSurface, qh: &QueueHandle<State>) {
        if self.cursor_objs.buffer.is_none() {
            let bytes: Vec<u8> = IMAGE_PIXEL.repeat((IMAGE_SIDE * IMAGE_SIDE) as usize);
            let dir = std::env::var_os("XDG_RUNTIME_DIR").map(std::path::PathBuf::from).unwrap_or_else(std::env::temp_dir);
            let Ok(file) = tempfile::tempfile_in(dir) else { return };
            if file.set_len(bytes.len() as u64).is_err() || file.write_all_at(&bytes, 0).is_err() {
                return;
            }
            let Ok(dup) = file.try_clone() else { return };
            let fd: OwnedFd = dup.into();
            let pool = self.shm.create_pool(std::os::fd::AsFd::as_fd(&fd), bytes.len() as i32, qh, ());
            let buffer = pool.create_buffer(0, IMAGE_SIDE as i32, IMAGE_SIDE as i32, (IMAGE_SIDE * 4) as i32, wl_shm::Format::Argb8888, qh, usize::MAX);
            pool.destroy();
            self.cursor_objs.buffer = Some(buffer);
            self.cursor_objs._file = Some(file);
        }
        surface.attach(self.cursor_objs.buffer.as_ref(), 0, 0);
        surface.damage_buffer(0, 0, IMAGE_SIDE as i32, IMAGE_SIDE as i32);
        surface.commit();
    }
}

impl Dispatch<WpCursorShapeManagerV1, ()> for State {
    fn event(_: &mut Self, _: &WpCursorShapeManagerV1, _: <WpCursorShapeManagerV1 as wayland_client::Proxy>::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
}

impl Dispatch<WpCursorShapeDeviceV1, ()> for State {
    fn event(_: &mut Self, _: &WpCursorShapeDeviceV1, _: <WpCursorShapeDeviceV1 as wayland_client::Proxy>::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
}
