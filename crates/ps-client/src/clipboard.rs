//! Área de transferência do sistema hospedeiro, atrás de um trait, e a sincronização com o servidor.
//!
//! Regras (docs/05-protocolo.md, "Área de transferência"):
//! - Pi -> hospedeiro: o texto recebido em `CLIPBOARD` vai para a área de transferência do sistema;
//! - hospedeiro -> Pi: quando uma janela ganha foco e o texto do sistema mudou desde o último
//!   envio ou recebimento, o cliente envia `CLIPBOARD`;
//! - nenhum lado reenvia um conteúdo idêntico ao último que recebeu.

use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};

use ps_proto::{Message, MAX_CLIPBOARD, MIME_TEXT};

/// Acesso ao texto da área de transferência do sistema.
pub trait HostClipboard: Send + Sync {
    /// Texto atual, ou `None` se vazia, ocupada ou sem texto.
    fn get_text(&self) -> Option<String>;
    /// Substitui o conteúdo por `text`. Devolve se conseguiu.
    fn set_text(&self, text: &str) -> bool;
}

/// Implementação do sistema: `clipboard-win` no Windows; nos demais sistemas é vazia por enquanto.
#[derive(Default)]
pub struct SystemClipboard;

#[cfg(windows)]
impl HostClipboard for SystemClipboard {
    fn get_text(&self) -> Option<String> {
        clipboard_win::get_clipboard_string().ok()
    }

    fn set_text(&self, text: &str) -> bool {
        clipboard_win::set_clipboard_string(text).is_ok()
    }
}

#[cfg(not(windows))]
impl HostClipboard for SystemClipboard {
    fn get_text(&self) -> Option<String> {
        None
    }

    fn set_text(&self, _text: &str) -> bool {
        false
    }
}

/// Sincronização entre a área de transferência do sistema e o servidor.
pub struct ClipboardSync {
    host: Arc<dyn HostClipboard>,
    out: Sender<Message>,
    /// Último texto enviado ou recebido.
    last: Mutex<Option<String>>,
}

impl ClipboardSync {
    pub fn new(host: Arc<dyn HostClipboard>, out: Sender<Message>) -> Arc<ClipboardSync> {
        Arc::new(ClipboardSync { host, out, last: Mutex::new(None) })
    }

    fn last(&self) -> std::sync::MutexGuard<'_, Option<String>> {
        self.last.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn send_up(&self, text: &str) {
        if text.is_empty() || text.len() > MAX_CLIPBOARD {
            return;
        }
        let _ = self.out.send(Message::ClipboardUp { mime: MIME_TEXT.to_string(), data: text.as_bytes().to_vec() });
    }

    /// Uma janela ganhou foco: envia o texto do sistema se mudou. Devolve se enviou.
    pub fn on_focus(&self) -> bool {
        let Some(text) = self.host.get_text() else { return false };
        let mut last = self.last();
        if last.as_deref() == Some(text.as_str()) {
            return false;
        }
        *last = Some(text.clone());
        drop(last);
        self.send_up(&text);
        !text.is_empty() && text.len() <= MAX_CLIPBOARD
    }

    /// Chegou `CLIPBOARD` do servidor. Devolve se o conteúdo do sistema foi trocado.
    pub fn from_server(&self, mime: &str, data: &[u8]) -> Option<String> {
        if mime != MIME_TEXT {
            return None;
        }
        let text = String::from_utf8_lossy(data).into_owned();
        let mut last = self.last();
        if last.as_deref() == Some(text.as_str()) {
            return None;
        }
        if !self.host.set_text(&text) {
            return None;
        }
        *last = Some(text.clone());
        Some(text)
    }

    /// Roteiro de teste (`clip "texto"`): define o texto do sistema e o envia na hora.
    pub fn force_set(&self, text: &str) -> bool {
        if !self.host.set_text(text) {
            return false;
        }
        *self.last() = Some(text.to_string());
        self.send_up(text);
        true
    }

    /// Texto atual do sistema (para conferência no roteiro).
    pub fn host_text(&self) -> Option<String> {
        self.host.get_text()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;

    #[derive(Default)]
    struct Falsa(Mutex<Option<String>>);

    impl HostClipboard for Falsa {
        fn get_text(&self) -> Option<String> {
            self.0.lock().unwrap().clone()
        }
        fn set_text(&self, text: &str) -> bool {
            *self.0.lock().unwrap() = Some(text.to_string());
            true
        }
    }

    fn montar() -> (Arc<Falsa>, Arc<ClipboardSync>, mpsc::Receiver<Message>) {
        let host = Arc::new(Falsa::default());
        let (tx, rx) = mpsc::channel();
        (host.clone(), ClipboardSync::new(host, tx), rx)
    }

    fn texto(m: Message) -> String {
        match m {
            Message::ClipboardUp { mime, data } => {
                assert_eq!(mime, MIME_TEXT);
                String::from_utf8(data).unwrap()
            }
            outra => panic!("esperava ClipboardUp, veio {outra:?}"),
        }
    }

    #[test]
    fn envia_so_quando_mudou() {
        let (host, sync, rx) = montar();
        assert!(!sync.on_focus(), "área vazia não envia");
        host.set_text("pistick-teste-123");
        assert!(sync.on_focus());
        assert_eq!(texto(rx.try_recv().unwrap()), "pistick-teste-123");
        assert!(!sync.on_focus(), "mesmo texto: nada a enviar");
        assert!(rx.try_recv().is_err());
        host.set_text("outro");
        assert!(sync.on_focus());
        assert_eq!(texto(rx.try_recv().unwrap()), "outro");
    }

    #[test]
    fn nao_devolve_o_que_acabou_de_receber() {
        let (host, sync, rx) = montar();
        assert_eq!(sync.from_server(MIME_TEXT, "vindo do Pi ç".as_bytes()).as_deref(), Some("vindo do Pi ç"));
        assert_eq!(host.get_text().as_deref(), Some("vindo do Pi ç"));
        assert!(!sync.on_focus(), "eco: o texto recebido não volta ao servidor");
        assert!(rx.try_recv().is_err());
        // Repetição idêntica do servidor também não mexe no sistema.
        assert_eq!(sync.from_server(MIME_TEXT, "vindo do Pi ç".as_bytes()), None);
        // Tipo MIME desconhecido é ignorado.
        assert_eq!(sync.from_server("image/png", b"x"), None);
    }

    #[test]
    fn texto_grande_demais_nao_e_enviado() {
        let (host, sync, rx) = montar();
        host.set_text(&"a".repeat(MAX_CLIPBOARD + 1));
        assert!(!sync.on_focus());
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn force_set_envia_na_hora() {
        let (host, sync, rx) = montar();
        assert!(sync.force_set("pistick-teste-123"));
        assert_eq!(host.get_text().as_deref(), Some("pistick-teste-123"));
        assert_eq!(texto(rx.try_recv().unwrap()), "pistick-teste-123");
        assert!(!sync.on_focus());
    }
}
