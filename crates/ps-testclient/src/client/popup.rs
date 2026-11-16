
use std::os::fd::OwnedFd;
use std::os::unix::fs::FileExt;

use wayland_client::protocol::wl_buffer::WlBuffer;
use wayland_client::protocol::wl_shm;
use wayland_client::protocol::wl_surface::WlSurface;
use wayland_client::{Connection, Dispatch, QueueHandle};
use wayland_protocols::xdg::shell::client::xdg_popup::{self, XdgPopup};
use wayland_protocols::xdg::shell::client::xdg_positioner::{Anchor, ConstraintAdjustment, Gravity, XdgPositioner};
use wayland_protocols::xdg::shell::client::xdg_surface::XdgSurface;

use super::State;

pub(super) struct PopupWin {
    pub name: String,
    pub surface: WlSurface,
    pub xdg_surface: XdgSurface,
    pub popup: XdgPopup,
    pub size: (u32, u32),
    pub color: u32,
    pub configured: bool,
    pub alive: bool,
    buffer: Option<WlBuffer>,
    _file: Option<std::fs::File>,
}

pub(super) struct Spec {
    pub name: &'static str,
    pub parent: Option<usize>,
    pub anchor_rect: (i32, i32, i32, i32),
    pub anchor: Anchor,
    pub size: (i32, i32),
    pub grab: bool,
    pub adjust: ConstraintAdjustment,
    pub color: u32,
}

impl State {
    pub(super) fn surface_name(&self, s: &WlSurface) -> String {
        if self.surfaces.as_ref().is_some_and(|m| m.surface == *s) {
            return "main".into();
        }
        self.popups.iter().find(|p| p.surface == *s).map_or_else(|| "desconhecida".to_string(), |p| p.name.clone())
    }

    fn popup_index(&self, name: &str) -> Option<usize> {
        self.popups.iter().position(|p| p.alive && p.name == name)
    }

    pub(super) fn open_popup(&mut self, qh: &QueueHandle<State>, spec: Spec) -> usize {
        let idx = self.popups.len();
        let surface = self.compositor.create_surface(qh, ());
        let xdg_surface = self.wm_base.get_xdg_surface(&surface, qh, idx + 1);
        let pos = self.wm_base.create_positioner(qh, ());
        pos.set_size(spec.size.0, spec.size.1);
        let r = spec.anchor_rect;
        pos.set_anchor_rect(r.0, r.1, r.2, r.3);
        pos.set_anchor(spec.anchor);
        pos.set_gravity(Gravity::BottomRight);
        pos.set_constraint_adjustment(spec.adjust);
        let parent_xdg = match spec.parent {
            Some(i) => self.popups[i].xdg_surface.clone(),
            None => self.surfaces.as_ref().expect("janela principal").xdg_surface.clone(),
        };
        let popup = xdg_surface.get_popup(Some(&parent_xdg), &pos, qh, idx);
        if spec.grab {
            popup.grab(&self.seat, self.last_serial);
        }
        surface.commit();
        pos.destroy();
        self.log(format!("popup-open {}", spec.name));
        self.popups.push(PopupWin {
            name: spec.name.to_string(),
            surface,
            xdg_surface,
            popup,
            size: (spec.size.0 as u32, spec.size.1 as u32),
            color: spec.color,
            configured: false,
            alive: true,
            buffer: None,
            _file: None,
        });
        idx
    }

    pub(super) fn popup_configure(&mut self, idx: usize, qh: &QueueHandle<State>) {
        let Some(p) = self.popups.get_mut(idx) else { return };
        if !p.alive || p.configured {
            return;
        }
        p.configured = true;
        let (w, h) = p.size;
        if w == 0 || h == 0 {
            return;
        }
        let mut bytes = Vec::with_capacity((w * h * 4) as usize);
        for y in 0..h {
            for x in 0..w {
                let borda = x < 2 || y < 2 || x + 2 >= w || y + 2 >= h;
                let c = if borda { 0x00FF_FFFF } else { p.color };
                bytes.extend_from_slice(&[c as u8, (c >> 8) as u8, (c >> 16) as u8, 0xFF]);
            }
        }
        let dir = std::env::var_os("XDG_RUNTIME_DIR").map(std::path::PathBuf::from).unwrap_or_else(std::env::temp_dir);
        let Ok(file) = tempfile::tempfile_in(dir) else { return };
        if file.set_len(bytes.len() as u64).is_err() || file.write_all_at(&bytes, 0).is_err() {
            return;
        }
        let Ok(dup) = file.try_clone() else { return };
        let fd: OwnedFd = dup.into();
        let pool = self.shm.create_pool(std::os::fd::AsFd::as_fd(&fd), bytes.len() as i32, qh, ());
        let buffer = pool.create_buffer(0, w as i32, h as i32, (w * 4) as i32, wl_shm::Format::Xrgb8888, qh, usize::MAX);
        pool.destroy();
        p.surface.attach(Some(&buffer), 0, 0);
        p.surface.damage_buffer(0, 0, w as i32, h as i32);
        p.surface.commit();
        p.buffer = Some(buffer);
        p._file = Some(file);
    }

    pub(super) fn destroy_popup(&mut self, idx: usize) {
        let Some(p) = self.popups.get_mut(idx) else { return };
        if !p.alive {
            return;
        }
        p.alive = false;
        if let Some(b) = p.buffer.take() {
            b.destroy();
        }
        p.popup.destroy();
        p.xdg_surface.destroy();
        p.surface.destroy();
    }

    pub(super) fn destroy_all_popups(&mut self) {
        for i in (0..self.popups.len()).rev() {
            self.destroy_popup(i);
        }
    }

    pub(super) fn popup_key(&mut self, key: u32, qh: &QueueHandle<State>) {
        match key {
            25 => {
                if self.popup_index("popup1").is_none() {
                    self.open_popup(qh, Spec { name: "popup1", parent: None, anchor_rect: (100, 50, 40, 20), anchor: Anchor::BottomLeft, size: (200, 150), grab: true, adjust: ConstraintAdjustment::None, color: 0x2040C0 });
                }
            }
            46 => {
                if let (Some(pai), None) = (self.popup_index("popup1"), self.popup_index("popup2")) {
                    self.open_popup(qh, Spec { name: "popup2", parent: Some(pai), anchor_rect: (150, 10, 40, 20), anchor: Anchor::TopRight, size: (120, 80), grab: true, adjust: ConstraintAdjustment::None, color: 0xC04020 });
                }
            }
            20 => {
                if self.popup_index("tip").is_none() {
                    self.open_popup(qh, Spec { name: "tip", parent: None, anchor_rect: (300, 300, 10, 10), anchor: Anchor::TopLeft, size: (120, 40), grab: false, adjust: ConstraintAdjustment::None, color: 0x20A040 });
                }
            }
            22 => {
                if let Some(i) = self.popup_index("tip") {
                    self.log("popup-destroy tip".into());
                    self.destroy_popup(i);
                }
            }
            47 => {
                if let Some(i) = self.popup_index("popup1") {
                    let pos = self.wm_base.create_positioner(qh, ());
                    pos.set_size(200, 150);
                    pos.set_anchor_rect(300, 200, 10, 10);
                    pos.set_anchor(Anchor::BottomLeft);
                    pos.set_gravity(Gravity::BottomRight);
                    self.popups[i].popup.reposition(&pos, 7);
                    pos.destroy();
                }
            }
            48 => {
                if self.popup_index("big").is_none() {
                    let (w, h) = self.size;
                    self.open_popup(
                        qh,
                        Spec { name: "big", parent: None, anchor_rect: (w as i32 - 20, h as i32 - 20, 10, 10), anchor: Anchor::TopLeft, size: (300, 200), grab: true, adjust: ConstraintAdjustment::SlideX | ConstraintAdjustment::SlideY, color: 0x808020 },
                    );
                }
            }
            _ => {}
        }
    }
}

impl Dispatch<XdgPopup, usize> for State {
    fn event(st: &mut Self, _: &XdgPopup, ev: xdg_popup::Event, idx: &usize, _: &Connection, _: &QueueHandle<Self>) {
        let nome = st.popups.get(*idx).map(|p| p.name.clone()).unwrap_or_default();
        match ev {
            xdg_popup::Event::Configure { x, y, width, height } => {
                st.log(format!("popup-configure {nome} {x} {y} {width}x{height}"));
                if let Some(p) = st.popups.get_mut(*idx) {
                    if !p.configured && width > 0 && height > 0 {
                        p.size = (width as u32, height as u32);
                    }
                }
            }
            xdg_popup::Event::PopupDone => {
                st.log(format!("popup-done {nome}"));
                for i in (*idx + 1..st.popups.len()).rev() {
                    st.destroy_popup(i);
                }
                st.destroy_popup(*idx);
            }
            xdg_popup::Event::Repositioned { token } => st.log(format!("popup-repositioned {nome} {token}")),
            _ => {}
        }
    }
}

impl Dispatch<XdgPositioner, ()> for State {
    fn event(_: &mut Self, _: &XdgPositioner, _: <XdgPositioner as wayland_client::Proxy>::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
}
