//! Interface de "fonte de janelas": o que a sessão precisa saber de quem produz os pixels.
//!
//! A sessão (protocolo, controle de fluxo, codec) não sabe se as janelas vêm do gerador
//! sintético ou de um cliente Wayland. A fonte publica eventos; a sessão lê o conteúdo mais
//! recente de uma janela só no momento de codificar (nunca há fila de quadros) e entrega à
//! fonte a entrada do usuário e o aviso de que um quadro saiu.

use std::sync::Arc;
use std::time::Duration;

use ps_codec::Rect;
use ps_proto::{CursorShape, Message};

/// Algo que mudou na fonte.
#[derive(Debug, Clone, PartialEq)]
pub enum SourceEvent {
    Created { win: u32, width: u32, height: u32, title: String, app_id: String },
    Resized { win: u32, width: u32, height: u32 },
    Title { win: u32, title: String },
    Destroyed { win: u32 },
    /// Conteúdo novo (já gravado na fonte) nestes retângulos.
    Damage { win: u32, rects: Vec<Rect> },
    /// Popup (menu ou dica) novo; `x`, `y` relativos ao canto do conteúdo do pai.
    PopupCreated { win: u32, parent: u32, x: i32, y: i32, width: u32, height: u32, grab: bool },
    PopupMoved { win: u32, x: i32, y: i32, width: u32, height: u32 },
    /// Mãe do diálogo (0 = nenhuma). Pode vir antes do `Created` da própria janela.
    Parent { win: u32, parent: u32 },
    /// Tamanhos mínimo e máximo (0 = sem limite).
    Limits { win: u32, min_w: u32, min_h: u32, max_w: u32, max_h: u32 },
    /// Cursor pedido pelo app para a janela sob o ponteiro.
    Cursor { win: u32, shape: CursorShape },
    /// Um app do Pi definiu a seleção de texto.
    Clipboard { mime: String, data: Vec<u8> },
}

/// Acorda a sessão quando a fonte tem eventos novos (chamável de qualquer thread).
pub type Waker = Arc<dyn Fn() + Send + Sync>;

/// Fonte de janelas de uma sessão.
pub trait WindowSource: Send {
    /// Extensões (bits de `caps`) que o cliente anunciou; chamada antes de [`WindowSource::start`].
    fn set_client_caps(&mut self, _caps: u32) {}

    /// Início da sessão: devolve um `Created` por janela já existente. `waker` deve ser
    /// chamado sempre que houver eventos novos a recolher com [`WindowSource::poll`].
    fn start(&mut self, waker: Waker) -> Vec<SourceEvent>;

    /// Recolhe os eventos acumulados. `now_ms`: milissegundos desde o início da sessão.
    fn poll(&mut self, now_ms: u64) -> Vec<SourceEvent>;

    /// Em quanto tempo a fonte quer ser consultada de novo mesmo sem acordar a sessão
    /// (`None`: só quando o `waker` for chamado).
    fn next_poll_in(&self, now_ms: u64) -> Option<Duration>;

    /// Entrega o conteúdo mais recente da janela: largura, altura e pixels BGRX
    /// (stride = largura × 4). Não chama `f` se a janela não existe mais.
    fn with_pixels(&mut self, win: u32, f: &mut dyn FnMut(u32, u32, &[u8]));

    /// Entrada vinda do cliente: `PointerMove`, `PointerButton`, `PointerAxis`, `Key`,
    /// `Focus`, `ResizeReq` (já limitado a 64..=4096), `CloseReq` (de janela ou de popup) e
    /// `ClipboardUp`.
    fn input(&mut self, msg: &Message);

    /// A sessão enviou um quadro da janela (usado para o ritmo dos clientes).
    fn frame_sent(&mut self, win: u32);

    /// A sessão terminou.
    fn stop(&mut self) {}
}
