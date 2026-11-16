//! Área de transferência do cliente de teste (`wl_data_device`): oferece um texto quando recebe a
//! tecla `y` e registra o texto de toda seleção nova que o compositor lhe apresenta.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::os::fd::AsFd;

use wayland_client::backend::ObjectId;
use wayland_client::protocol::wl_data_device::{self, WlDataDevice};
use wayland_client::protocol::wl_data_device_manager::WlDataDeviceManager;
use wayland_client::protocol::wl_data_offer::{self, WlDataOffer};
use wayland_client::protocol::wl_data_source::{self, WlDataSource};
use wayland_client::{event_created_child, Connection, Dispatch, Proxy, QueueHandle};

use super::State;

const MIME: &str = "text/plain;charset=utf-8";

/// Objetos e estado da área de transferência do cliente.
pub(super) struct ClipObjs {
    pub manager: Option<WlDataDeviceManager>,
    pub device: Option<WlDataDevice>,
    /// Tipos MIME anunciados por cada oferta.
    offers: HashMap<ObjectId, Vec<String>>,
    /// Fonte em uso (viva enquanto o compositor não a cancelar).
    source: Option<WlDataSource>,
}

impl ClipObjs {
    pub(super) fn new(manager: Option<WlDataDeviceManager>) -> Self {
        ClipObjs { manager, device: None, offers: HashMap::new(), source: None }
    }
}

impl State {
    /// Cria o `wl_data_device` do seat (depois de o seat estar pronto).
    pub(super) fn create_data_device(&mut self, qh: &QueueHandle<State>) {
        if let (Some(m), None) = (&self.clip.manager, &self.clip.device) {
            self.clip.device = Some(m.get_data_device(&self.seat, qh, ()));
        }
    }

    /// Define a seleção com o texto de `--clipboard`.
    pub(super) fn set_selection(&mut self, qh: &QueueHandle<State>) {
        let (Some(m), Some(dev), Some(_)) = (&self.clip.manager, &self.clip.device, &self.clipboard_text) else { return };
        let source = m.create_data_source(qh, ());
        source.offer(MIME.to_string());
        dev.set_selection(Some(&source), self.last_serial);
        self.clip.source = Some(source);
        self.log("selection-set".into());
    }
}

impl Dispatch<WlDataDeviceManager, ()> for State {
    fn event(_: &mut Self, _: &WlDataDeviceManager, _: <WlDataDeviceManager as Proxy>::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
}

impl Dispatch<WlDataDevice, ()> for State {
    fn event(st: &mut Self, _: &WlDataDevice, ev: wl_data_device::Event, _: &(), conn: &Connection, _: &QueueHandle<Self>) {
        if let wl_data_device::Event::Selection { id } = ev {
            let Some(offer) = id else {
                st.log("selection-cleared".into());
                return;
            };
            // A seleção que o próprio app definiu também lhe é apresentada; lê-la aqui travaria o despacho
            // (quem escreve o texto é o `Send` desta mesma fila).
            if st.clip.source.is_some() {
                st.log("selection-own".into());
                return;
            }
            let tem_texto = st.clip.offers.get(&offer.id()).is_some_and(|m| m.iter().any(|t| t == MIME));
            if !tem_texto {
                st.log("selection-sem-texto".into());
                return;
            }
            let Ok((leitura, escrita)) = std::io::pipe() else { return };
            offer.receive(MIME.to_string(), escrita.as_fd());
            drop(escrita);
            let _ = conn.flush();
            // O compositor escreve de outra thread: a leitura bloqueante termina quando ele fecha o pipe.
            let mut texto = String::new();
            let _ = std::fs::File::from(std::os::fd::OwnedFd::from(leitura)).read_to_string(&mut texto);
            st.log(format!("selection-received {texto:?}"));
        }
    }

    event_created_child!(State, WlDataDevice, [
        wl_data_device::EVT_DATA_OFFER_OPCODE => (WlDataOffer, ()),
    ]);
}

impl Dispatch<WlDataOffer, ()> for State {
    fn event(st: &mut Self, offer: &WlDataOffer, ev: wl_data_offer::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
        if let wl_data_offer::Event::Offer { mime_type } = ev {
            st.clip.offers.entry(offer.id()).or_default().push(mime_type);
        }
    }
}

impl Dispatch<WlDataSource, ()> for State {
    fn event(st: &mut Self, fonte: &WlDataSource, ev: wl_data_source::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
        match ev {
            wl_data_source::Event::Send { mime_type, fd } => {
                let Some(texto) = st.clipboard_text.clone() else { return };
                if mime_type != MIME {
                    return;
                }
                // Em outra thread: um texto grande não pode travar o despacho do cliente.
                std::thread::spawn(move || {
                    let mut f = std::fs::File::from(fd);
                    let _ = f.write_all(texto.as_bytes());
                });
                st.log("selection-sent".into());
            }
            wl_data_source::Event::Cancelled => {
                // O cancelamento de uma fonte antiga não pode apagar a nova.
                if st.clip.source.as_ref().is_some_and(|s| s.id() == fonte.id()) {
                    st.clip.source = None;
                }
                st.log("selection-cancelled".into());
            }
            _ => {}
        }
    }
}
