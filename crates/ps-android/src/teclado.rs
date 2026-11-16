
use ps_client::autotype::strokes_for;
use ps_proto::Message;

pub const KEY_ESC: u32 = 1;
pub const KEY_BACKSPACE: u32 = 14;
pub const KEY_TAB: u32 = 15;
pub const KEY_ENTER: u32 = 28;
pub const KEY_LEFTCTRL: u32 = 29;
pub const KEY_LEFTSHIFT: u32 = 42;
pub const KEY_LEFTALT: u32 = 56;
pub const KEY_HOME: u32 = 102;
pub const KEY_UP: u32 = 103;
pub const KEY_LEFT: u32 = 105;
pub const KEY_RIGHT: u32 = 106;
pub const KEY_END: u32 = 107;
pub const KEY_DOWN: u32 = 108;
pub const KEY_DELETE: u32 = 111;

pub fn evdev_de_android(key_code: i32) -> Option<u32> {
    const LETRAS: [u32; 26] = [30, 48, 46, 32, 18, 33, 34, 35, 23, 36, 37, 38, 50, 49, 24, 25, 16, 19, 31, 20, 22, 47, 17, 45, 21, 44];
    Some(match key_code {
        29..=54 => LETRAS[(key_code - 29) as usize],
        7 => 11,
        8..=16 => (key_code - 8 + 2) as u32,
        66 | 160 => KEY_ENTER,
        67 => KEY_BACKSPACE,
        61 => KEY_TAB,
        111 => KEY_ESC,
        62 => 57,
        19 => KEY_UP,
        20 => KEY_DOWN,
        21 => KEY_LEFT,
        22 => KEY_RIGHT,
        122 => KEY_HOME,
        123 => KEY_END,
        92 => 104,
        93 => 109,
        112 => KEY_DELETE,
        131..=140 => (key_code - 131 + 59) as u32,
        141 => 87,
        142 => 88,
        59 => KEY_LEFTSHIFT,
        60 => 54,
        113 => KEY_LEFTCTRL,
        114 => 97,
        57 => KEY_LEFTALT,
        58 => 100,
        _ => return None,
    })
}

pub const PAUSA_TECLA_MS: u32 = 20;
const MAX_TECLAS: usize = 4096;

#[derive(Debug, Clone, PartialEq)]
pub enum Passo {
    Msg(Message),
    PausaMs(u32),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mod {
    Ctrl,
    Alt,
}

#[derive(Debug, Default)]
pub struct Teclado {
    ctrl: bool,
    alt: bool,
    descartados: u32,
    soltar: Option<(u32, bool, bool)>,
}

fn tecla(win: u32, keycode: u32, pressed: bool) -> Passo {
    Passo::Msg(Message::Key { win, keycode, pressed })
}

impl Teclado {
    pub fn new() -> Teclado {
        Teclado::default()
    }

    pub fn alternar(&mut self, m: Mod) -> bool {
        let v = match m {
            Mod::Ctrl => &mut self.ctrl,
            Mod::Alt => &mut self.alt,
        };
        *v = !*v;
        *v
    }

    pub fn grudados(&self) -> (bool, bool) {
        (self.ctrl, self.alt)
    }

    pub fn descartados_e_zerar(&mut self) -> u32 {
        std::mem::take(&mut self.descartados)
    }

    fn com_modificadores(&mut self, win: u32, corpo: Vec<Passo>) -> Vec<Passo> {
        let (ctrl, alt) = (self.ctrl, self.alt);
        self.ctrl = false;
        self.alt = false;
        let mut v = Vec::with_capacity(corpo.len() + 4);
        if ctrl {
            v.push(tecla(win, KEY_LEFTCTRL, true));
        }
        if alt {
            v.push(tecla(win, KEY_LEFTALT, true));
        }
        v.extend(corpo);
        if alt {
            v.push(tecla(win, KEY_LEFTALT, false));
        }
        if ctrl {
            v.push(tecla(win, KEY_LEFTCTRL, false));
        }
        v
    }

    fn bater(win: u32, code: u32, shift: bool) -> Vec<Passo> {
        let mut v = Vec::with_capacity(5);
        if shift {
            v.push(tecla(win, KEY_LEFTSHIFT, true));
        }
        v.push(tecla(win, code, true));
        v.push(Passo::PausaMs(PAUSA_TECLA_MS));
        v.push(tecla(win, code, false));
        if shift {
            v.push(tecla(win, KEY_LEFTSHIFT, false));
        }
        v
    }

    pub fn texto(&mut self, win: u32, texto: &str) -> Vec<Passo> {
        let mut out = Vec::new();
        let mut primeiro = true;
        let mut teclas = 0;
        for c in texto.chars() {
            let Some(batidas) = strokes_for(c) else {
                self.descartados += 1;
                continue;
            };
            teclas += batidas.len();
            if teclas > MAX_TECLAS {
                self.descartados += 1;
                continue;
            }
            let mut corpo = Vec::new();
            for b in &batidas {
                corpo.extend(Self::bater(win, b.code, b.shift));
            }
            if primeiro {
                out.extend(self.com_modificadores(win, corpo));
                primeiro = false;
            } else {
                out.extend(corpo);
            }
        }
        out
    }

    pub fn apagar(&mut self, win: u32, antes: u32, depois: u32) -> Vec<Passo> {
        let mut out = Vec::new();
        for _ in 0..antes.min(MAX_TECLAS as u32) {
            out.extend(Self::bater(win, KEY_BACKSPACE, false));
        }
        for _ in 0..depois.min(MAX_TECLAS as u32) {
            out.extend(Self::bater(win, KEY_DELETE, false));
        }
        out
    }

    pub fn tecla_extra(&mut self, win: u32, code: u32) -> Vec<Passo> {
        if !(1..=255).contains(&code) {
            return Vec::new();
        }
        let corpo = Self::bater(win, code, false);
        self.com_modificadores(win, corpo)
    }

    pub fn tecla_fisica(&mut self, win: u32, scan_code: i32, apertada: bool) -> Vec<Passo> {
        if !(1..=255).contains(&scan_code) {
            return Vec::new();
        }
        let code = scan_code as u32;
        let modificadora = matches!(code, 29 | 97 | 42 | 54 | 56 | 100);
        if apertada && !modificadora && (self.ctrl || self.alt) {
            let (ctrl, alt) = (self.ctrl, self.alt);
            self.ctrl = false;
            self.alt = false;
            self.soltar = Some((code, ctrl, alt));
            let mut v = Vec::with_capacity(3);
            if ctrl {
                v.push(tecla(win, KEY_LEFTCTRL, true));
            }
            if alt {
                v.push(tecla(win, KEY_LEFTALT, true));
            }
            v.push(tecla(win, code, true));
            return v;
        }
        let mut v = vec![tecla(win, code, apertada)];
        if !apertada {
            if let Some((c, ctrl, alt)) = self.soltar {
                if c == code {
                    self.soltar = None;
                    if alt {
                        v.push(tecla(win, KEY_LEFTALT, false));
                    }
                    if ctrl {
                        v.push(tecla(win, KEY_LEFTCTRL, false));
                    }
                }
            }
        }
        v
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn teclas(v: &[Passo]) -> Vec<(u32, bool)> {
        v.iter()
            .filter_map(|p| match p {
                Passo::Msg(Message::Key { keycode, pressed, .. }) => Some((*keycode, *pressed)),
                _ => None,
            })
            .collect()
    }

    fn bate(code: u32) -> Vec<(u32, bool)> {
        vec![(code, true), (code, false)]
    }

    #[test]
    fn letra_minuscula_e_maiuscula() {
        let mut t = Teclado::new();
        assert_eq!(teclas(&t.texto(1, "e")), bate(18));
        assert_eq!(teclas(&t.texto(1, "E")), vec![(KEY_LEFTSHIFT, true), (18, true), (18, false), (KEY_LEFTSHIFT, false)]);
    }

    #[test]
    fn texto_com_acentos_vira_teclas_mortas_do_abnt2() {
        let mut t = Teclado::new();
        assert_eq!(teclas(&t.texto(7, "ã")), [bate(40), bate(30)].concat());
        assert_eq!(teclas(&t.texto(7, "ç")), bate(39));
        assert_eq!(teclas(&t.texto(7, "Ç")), vec![(KEY_LEFTSHIFT, true), (39, true), (39, false), (KEY_LEFTSHIFT, false)]);
        assert_eq!(teclas(&t.texto(7, "Ã")), [bate(40), vec![(KEY_LEFTSHIFT, true), (30, true), (30, false), (KEY_LEFTSHIFT, false)]].concat());
        let v = t.texto(7, "echo ação ç ÃÇ");
        assert!(!v.is_empty());
        assert_eq!(t.descartados_e_zerar(), 0);
        assert!(v.iter().all(|p| match p {
            Passo::Msg(Message::Key { win, .. }) => *win == 7,
            _ => true,
        }));
    }

    #[test]
    fn enter_e_espaco() {
        let mut t = Teclado::new();
        assert_eq!(teclas(&t.texto(1, "\n")), bate(KEY_ENTER));
        assert_eq!(teclas(&t.texto(1, " ")), bate(57));
    }

    #[test]
    fn caractere_sem_batida_e_descartado_e_contado() {
        let mut t = Teclado::new();
        let v = t.texto(1, "a😀b€c");
        assert_eq!(teclas(&v), [bate(30), bate(48), bate(46)].concat(), "só a, b e c");
        assert_eq!(t.descartados_e_zerar(), 2);
        assert_eq!(t.descartados_e_zerar(), 0, "o contador zera");
        assert!(t.texto(1, "").is_empty());
    }

    #[test]
    fn ctrl_grudento_vale_para_a_proxima_tecla_e_se_solta_sozinho() {
        let mut t = Teclado::new();
        assert!(t.alternar(Mod::Ctrl));
        assert_eq!(t.grudados(), (true, false));
        assert_eq!(teclas(&t.texto(1, "c")), vec![(KEY_LEFTCTRL, true), (46, true), (46, false), (KEY_LEFTCTRL, false)]);
        assert_eq!(t.grudados(), (false, false), "consumido");
        assert_eq!(teclas(&t.texto(1, "l")), bate(38));
        t.alternar(Mod::Ctrl);
        assert_eq!(teclas(&t.texto(1, "l")), vec![(KEY_LEFTCTRL, true), (38, true), (38, false), (KEY_LEFTCTRL, false)]);
    }

    #[test]
    fn ctrl_so_vale_para_o_primeiro_caractere_do_texto() {
        let mut t = Teclado::new();
        t.alternar(Mod::Ctrl);
        let v = teclas(&t.texto(1, "ab"));
        assert_eq!(v, vec![(KEY_LEFTCTRL, true), (30, true), (30, false), (KEY_LEFTCTRL, false), (48, true), (48, false)]);
    }

    #[test]
    fn alt_e_ctrl_juntos_e_desligar_pelo_segundo_toque() {
        let mut t = Teclado::new();
        t.alternar(Mod::Ctrl);
        t.alternar(Mod::Alt);
        let v = teclas(&t.tecla_extra(1, KEY_TAB));
        assert_eq!(v, vec![(KEY_LEFTCTRL, true), (KEY_LEFTALT, true), (KEY_TAB, true), (KEY_TAB, false), (KEY_LEFTALT, false), (KEY_LEFTCTRL, false)]);
        assert!(t.alternar(Mod::Alt));
        assert!(!t.alternar(Mod::Alt), "o segundo toque desliga");
        assert_eq!(teclas(&t.tecla_extra(1, KEY_ESC)), bate(KEY_ESC));
        t.alternar(Mod::Ctrl);
        t.alternar(Mod::Ctrl);
        assert_eq!(teclas(&t.texto(1, "x")), bate(45));
    }

    #[test]
    fn modificador_sem_caractere_com_tecla_continua_ligado() {
        let mut t = Teclado::new();
        t.alternar(Mod::Ctrl);
        assert!(t.texto(1, "😀").is_empty());
        assert_eq!(t.grudados(), (true, false), "nada foi digitado: o Ctrl espera");
    }

    #[test]
    fn delete_surrounding_text_vira_backspace_e_delete() {
        let mut t = Teclado::new();
        let v = teclas(&t.apagar(1, 2, 1));
        assert_eq!(v, [bate(KEY_BACKSPACE), bate(KEY_BACKSPACE), bate(KEY_DELETE)].concat());
        assert!(t.apagar(1, 0, 0).is_empty());
        assert!(teclas(&t.apagar(1, u32::MAX, 0)).len() <= 2 * MAX_TECLAS);
    }

    #[test]
    fn teclas_extras_validam_o_codigo() {
        let mut t = Teclado::new();
        for code in [KEY_UP, KEY_DOWN, KEY_LEFT, KEY_RIGHT, KEY_HOME, KEY_END] {
            assert_eq!(teclas(&t.tecla_extra(1, code)), bate(code));
        }
        assert!(t.tecla_extra(1, 0).is_empty());
        assert!(t.tecla_extra(1, 9999).is_empty());
    }

    #[test]
    fn teclado_fisico_usa_o_scan_code_direto() {
        let mut t = Teclado::new();
        assert_eq!(teclas(&t.tecla_fisica(9, 30, true)), vec![(30, true)]);
        assert_eq!(teclas(&t.tecla_fisica(9, 30, false)), vec![(30, false)]);
        assert!(t.tecla_fisica(9, 0, true).is_empty(), "scanCode 0: tecla sem código de hardware");
        assert!(t.tecla_fisica(9, -1, true).is_empty());
        assert!(t.tecla_fisica(9, 4000, true).is_empty());
    }

    #[test]
    fn grudento_vale_para_tecla_fisica_e_por_keycode() {
        for code in [46, evdev_de_android(31).unwrap()] {
            let mut t = Teclado::new();
            t.alternar(Mod::Ctrl);
            assert_eq!(teclas(&t.tecla_fisica(1, KEY_LEFTSHIFT as i32, true)), vec![(KEY_LEFTSHIFT, true)]);
            assert_eq!(t.grudados(), (true, false));
            assert_eq!(teclas(&t.tecla_fisica(1, KEY_LEFTSHIFT as i32, false)), vec![(KEY_LEFTSHIFT, false)]);
            assert_eq!(teclas(&t.tecla_fisica(1, code as i32, true)), vec![(KEY_LEFTCTRL, true), (code, true)]);
            assert_eq!(t.grudados(), (false, false));
            assert_eq!(teclas(&t.tecla_fisica(1, code as i32, false)), vec![(code, false), (KEY_LEFTCTRL, false)]);
            assert_eq!(teclas(&t.tecla_fisica(1, code as i32, true)), vec![(code, true)]);
        }
        let mut t = Teclado::new();
        t.alternar(Mod::Ctrl);
        t.alternar(Mod::Alt);
        assert_eq!(teclas(&t.tecla_fisica(1, 46, true)), vec![(KEY_LEFTCTRL, true), (KEY_LEFTALT, true), (46, true)]);
        assert_eq!(teclas(&t.tecla_fisica(1, 46, false)), vec![(46, false), (KEY_LEFTALT, false), (KEY_LEFTCTRL, false)]);
    }

    #[test]
    fn keycode_do_android_vira_evdev() {
        assert_eq!(evdev_de_android(29), Some(30), "A");
        assert_eq!(evdev_de_android(54), Some(44), "Z");
        assert_eq!(evdev_de_android(48), Some(20), "T");
        assert_eq!(evdev_de_android(7), Some(11), "0");
        assert_eq!(evdev_de_android(8), Some(2), "1");
        assert_eq!(evdev_de_android(16), Some(10), "9");
        assert_eq!(evdev_de_android(66), Some(KEY_ENTER));
        assert_eq!(evdev_de_android(67), Some(KEY_BACKSPACE));
        assert_eq!(evdev_de_android(61), Some(KEY_TAB));
        assert_eq!(evdev_de_android(111), Some(KEY_ESC));
        assert_eq!(evdev_de_android(62), Some(57));
        assert_eq!(evdev_de_android(19), Some(KEY_UP));
        assert_eq!(evdev_de_android(22), Some(KEY_RIGHT));
        assert_eq!(evdev_de_android(92), Some(104));
        assert_eq!(evdev_de_android(93), Some(109));
        assert_eq!(evdev_de_android(112), Some(KEY_DELETE));
        assert_eq!(evdev_de_android(131), Some(59), "F1");
        assert_eq!(evdev_de_android(140), Some(68), "F10");
        assert_eq!(evdev_de_android(141), Some(87), "F11");
        assert_eq!(evdev_de_android(142), Some(88), "F12");
        assert_eq!(evdev_de_android(59), Some(KEY_LEFTSHIFT));
        assert_eq!(evdev_de_android(113), Some(KEY_LEFTCTRL));
        assert_eq!(evdev_de_android(58), Some(100));
        for k in [4, 3, 24, 25, 26, 69, 74, 0, -1] {
            assert_eq!(evdev_de_android(k), None, "keyCode {k}");
        }
        let mut vistos: Vec<u32> = (29..=54).filter_map(evdev_de_android).collect();
        vistos.sort_unstable();
        vistos.dedup();
        assert_eq!(vistos.len(), 26);
    }

    #[test]
    fn texto_enorme_e_limitado() {
        let mut t = Teclado::new();
        let v = t.texto(1, &"a".repeat(MAX_TECLAS * 3));
        assert!(teclas(&v).len() <= MAX_TECLAS * 2);
        assert!(t.descartados_e_zerar() > 0);
    }
}
