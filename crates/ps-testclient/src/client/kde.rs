//! Decoração pelo protocolo do KDE (`org_kde_kwin_server_decoration`), a única que o GTK 3 entende.

use wayland_client::globals::GlobalList;
use wayland_client::protocol::wl_surface::WlSurface;
use wayland_client::{Connection, Dispatch, QueueHandle, WEnum};
use wayland_protocols_misc::server_decoration::client::org_kde_kwin_server_decoration::{self, Mode, OrgKdeKwinServerDecoration};
use wayland_protocols_misc::server_decoration::client::org_kde_kwin_server_decoration_manager::{self, OrgKdeKwinServerDecorationManager};

use super::State;

/// Faz o bind do global, cria a decoração da superfície e pede o modo "servidor".
pub(super) fn request_server_mode(globals: &GlobalList, qh: &QueueHandle<State>, surface: &WlSurface) {
    let Ok(mgr) = globals.bind::<OrgKdeKwinServerDecorationManager, _, _>(qh, 1..=1, ()) else { return };
    let deco = mgr.create(surface, qh, ());
    deco.request_mode(Mode::Server);
    // Os objetos ficam vivos até o fim do processo.
    std::mem::forget(deco);
    std::mem::forget(mgr);
}

impl Dispatch<OrgKdeKwinServerDecorationManager, ()> for State {
    fn event(st: &mut Self, _: &OrgKdeKwinServerDecorationManager, ev: org_kde_kwin_server_decoration_manager::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
        if let org_kde_kwin_server_decoration_manager::Event::DefaultMode { mode } = ev {
            // O modo do gerenciador é outro enum (mesmos valores); o nome sai do `Debug`.
            let nome = format!("{mode:?}");
            st.log(format!("kde-default-mode {}", nome.trim_start_matches("Value(").trim_end_matches(')')));
        }
    }
}

impl Dispatch<OrgKdeKwinServerDecoration, ()> for State {
    fn event(st: &mut Self, _: &OrgKdeKwinServerDecoration, ev: org_kde_kwin_server_decoration::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
        if let org_kde_kwin_server_decoration::Event::Mode { mode } = ev {
            st.log(format!("kde-mode {}", nome(mode)));
        }
    }
}

fn nome(m: WEnum<Mode>) -> String {
    match m {
        WEnum::Value(Mode::None) => "None".into(),
        WEnum::Value(Mode::Client) => "Client".into(),
        WEnum::Value(Mode::Server) => "Server".into(),
        _ => "?".into(),
    }
}
