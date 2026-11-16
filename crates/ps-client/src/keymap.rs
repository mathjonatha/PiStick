//! Conversão das teclas físicas do winit para códigos evdev (`linux/input-event-codes.h`).
//!
//! Usa a tecla física (posição no teclado), não o caractere: o layout (por exemplo ABNT2)
//! é resolvido no servidor, pelo compositor, como em qualquer cliente Wayland.

use winit::event::MouseButton;
use winit::keyboard::KeyCode;

/// Botões de mouse evdev usados pelo protocolo.
pub const BTN_LEFT: u32 = 0x110;
pub const BTN_RIGHT: u32 = 0x111;
pub const BTN_MIDDLE: u32 = 0x112;
pub const BTN_SIDE: u32 = 0x113;
pub const BTN_EXTRA: u32 = 0x114;

use KeyCode::*;

/// Tabela tecla física → código evdev.
const TABLE: &[(KeyCode, u32)] = &[
    (Escape, 1),
    (Digit1, 2),
    (Digit2, 3),
    (Digit3, 4),
    (Digit4, 5),
    (Digit5, 6),
    (Digit6, 7),
    (Digit7, 8),
    (Digit8, 9),
    (Digit9, 10),
    (Digit0, 11),
    (Minus, 12),
    (Equal, 13),
    (Backspace, 14),
    (Tab, 15),
    (KeyQ, 16),
    (KeyW, 17),
    (KeyE, 18),
    (KeyR, 19),
    (KeyT, 20),
    (KeyY, 21),
    (KeyU, 22),
    (KeyI, 23),
    (KeyO, 24),
    (KeyP, 25),
    (BracketLeft, 26),
    (BracketRight, 27),
    (Enter, 28),
    (ControlLeft, 29),
    (KeyA, 30),
    (KeyS, 31),
    (KeyD, 32),
    (KeyF, 33),
    (KeyG, 34),
    (KeyH, 35),
    (KeyJ, 36),
    (KeyK, 37),
    (KeyL, 38),
    (Semicolon, 39),
    (Quote, 40),
    (Backquote, 41),
    (ShiftLeft, 42),
    (Backslash, 43),
    (KeyZ, 44),
    (KeyX, 45),
    (KeyC, 46),
    (KeyV, 47),
    (KeyB, 48),
    (KeyN, 49),
    (KeyM, 50),
    (Comma, 51),
    (Period, 52),
    (Slash, 53),
    (ShiftRight, 54),
    (NumpadMultiply, 55),
    (AltLeft, 56),
    (Space, 57),
    (CapsLock, 58),
    (F1, 59),
    (F2, 60),
    (F3, 61),
    (F4, 62),
    (F5, 63),
    (F6, 64),
    (F7, 65),
    (F8, 66),
    (F9, 67),
    (F10, 68),
    (NumLock, 69),
    (ScrollLock, 70),
    (Numpad7, 71),
    (Numpad8, 72),
    (Numpad9, 73),
    (NumpadSubtract, 74),
    (Numpad4, 75),
    (Numpad5, 76),
    (Numpad6, 77),
    (NumpadAdd, 78),
    (Numpad1, 79),
    (Numpad2, 80),
    (Numpad3, 81),
    (Numpad0, 82),
    (NumpadDecimal, 83),
    (IntlBackslash, 86), // KEY_102ND
    (F11, 87),
    (F12, 88),
    (IntlRo, 89), // KEY_RO: a tecla extra do ABNT2 ao lado do shift direito
    (NumpadEnter, 96),
    (ControlRight, 97),
    (NumpadDivide, 98),
    (PrintScreen, 99),
    (AltRight, 100),
    (Home, 102),
    (ArrowUp, 103),
    (PageUp, 104),
    (ArrowLeft, 105),
    (ArrowRight, 106),
    (End, 107),
    (ArrowDown, 108),
    (PageDown, 109),
    (Insert, 110),
    (Delete, 111),
    (NumpadEqual, 117),
    (Pause, 119),
    (NumpadComma, 121), // KEY_KPCOMMA: o ponto do teclado numérico ABNT2
    (IntlYen, 124),
    (SuperLeft, 125),
    (SuperRight, 126),
    (ContextMenu, 127),
];

/// Código evdev da tecla física, ou `None` se não mapeada.
pub fn keycode_to_evdev(code: KeyCode) -> Option<u32> {
    TABLE.iter().find(|(k, _)| *k == code).map(|(_, v)| *v)
}

/// Código evdev do botão do mouse, ou `None` para botões extras sem mapeamento.
pub fn button_to_evdev(b: MouseButton) -> Option<u32> {
    match b {
        MouseButton::Left => Some(BTN_LEFT),
        MouseButton::Right => Some(BTN_RIGHT),
        MouseButton::Middle => Some(BTN_MIDDLE),
        MouseButton::Back => Some(BTN_SIDE),
        MouseButton::Forward => Some(BTN_EXTRA),
        MouseButton::Other(_) => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn teclas_conhecidas() {
        let casos = [
            (KeyA, 30),
            (KeyQ, 16),
            (KeyZ, 44),
            (KeyM, 50),
            (Digit1, 2),
            (Digit0, 11),
            (Enter, 28),
            (Escape, 1),
            (Backspace, 14),
            (Tab, 15),
            (Space, 57),
            (ShiftLeft, 42),
            (ShiftRight, 54),
            (ControlLeft, 29),
            (ControlRight, 97),
            (AltLeft, 56),
            (AltRight, 100),
            (SuperLeft, 125),
            (ArrowUp, 103),
            (ArrowDown, 108),
            (ArrowLeft, 105),
            (ArrowRight, 106),
            (F1, 59),
            (F10, 68),
            (F11, 87),
            (F12, 88),
            (IntlRo, 89),
        ];
        for (k, v) in casos {
            assert_eq!(keycode_to_evdev(k), Some(v), "{k:?}");
        }
    }

    #[test]
    fn codigos_evdev_sao_unicos_e_teclas_tambem() {
        let mut vistos = std::collections::HashSet::new();
        for (k, v) in TABLE {
            assert!(vistos.insert(*v), "código evdev repetido: {v} ({k:?})");
        }
        for (i, (k, _)) in TABLE.iter().enumerate() {
            assert!(TABLE[..i].iter().all(|(o, _)| o != k), "tecla repetida: {k:?}");
        }
    }

    #[test]
    fn cobertura_exigida() {
        // letras, números, modificadores, setas, Enter, Esc, Backspace, Tab, espaço, F1-F12
        let letras = [
            KeyA, KeyB, KeyC, KeyD, KeyE, KeyF, KeyG, KeyH, KeyI, KeyJ, KeyK, KeyL, KeyM, KeyN, KeyO, KeyP, KeyQ, KeyR, KeyS, KeyT,
            KeyU, KeyV, KeyW, KeyX, KeyY, KeyZ,
        ];
        let numeros = [Digit0, Digit1, Digit2, Digit3, Digit4, Digit5, Digit6, Digit7, Digit8, Digit9];
        let fn_ = [F1, F2, F3, F4, F5, F6, F7, F8, F9, F10, F11, F12];
        let resto = [ShiftLeft, ShiftRight, ControlLeft, ControlRight, AltLeft, AltRight, SuperLeft, SuperRight, ArrowUp, ArrowDown, ArrowLeft, ArrowRight, Enter, Escape, Backspace, Tab, Space];
        for k in letras.iter().chain(&numeros).chain(&fn_).chain(&resto) {
            assert!(keycode_to_evdev(*k).is_some(), "{k:?} sem mapeamento");
        }
    }

    #[test]
    fn botoes() {
        assert_eq!(button_to_evdev(MouseButton::Left), Some(0x110));
        assert_eq!(button_to_evdev(MouseButton::Right), Some(0x111));
        assert_eq!(button_to_evdev(MouseButton::Middle), Some(0x112));
        assert_eq!(button_to_evdev(MouseButton::Other(9)), None);
    }
}
