
const KEY_ENTER: u32 = 28;
const KEY_SPACE: u32 = 57;
const KEY_TILDE: u32 = 40;
const KEY_ACUTE: u32 = 26;
const KEY_CCEDILLA: u32 = 39;

#[derive(Debug, PartialEq, Eq)]
pub struct Stroke {
    pub shift: bool,
    pub code: u32,
}

fn letter(c: char) -> Option<u32> {
    const ROWS: [(&str, u32); 3] = [("qwertyuiop", 16), ("asdfghjkl", 30), ("zxcvbnm", 44)];
    let c = c.to_ascii_lowercase();
    ROWS.iter().find_map(|(row, base)| row.find(c).map(|i| base + i as u32))
}

pub fn strokes_for(c: char) -> Option<Vec<Stroke>> {
    let s = |shift, code| Stroke { shift, code };
    if let Some(code) = letter(c) {
        return Some(vec![s(c.is_ascii_uppercase(), code)]);
    }
    let acentuadas: [(char, bool, u32, char); 12] = [
        ('ã', false, KEY_TILDE, 'a'),
        ('õ', false, KEY_TILDE, 'o'),
        ('á', false, KEY_ACUTE, 'a'),
        ('é', false, KEY_ACUTE, 'e'),
        ('í', false, KEY_ACUTE, 'i'),
        ('ó', false, KEY_ACUTE, 'o'),
        ('ú', false, KEY_ACUTE, 'u'),
        ('â', true, KEY_TILDE, 'a'),
        ('ê', true, KEY_TILDE, 'e'),
        ('ô', true, KEY_TILDE, 'o'),
        ('à', true, KEY_ACUTE, 'a'),
        ('Ã', false, KEY_TILDE, 'A'),
    ];
    if let Some((_, shift, dead, base)) = acentuadas.iter().find(|a| a.0 == c) {
        return Some(vec![s(*shift, *dead), s(base.is_ascii_uppercase(), letter(*base)?)]);
    }
    Some(match c {
        'ç' => vec![s(false, KEY_CCEDILLA)],
        'Ç' => vec![s(true, KEY_CCEDILLA)],
        ' ' => vec![s(false, KEY_SPACE)],
        '\n' => vec![s(false, KEY_ENTER)],
        '0' => vec![s(false, 11)],
        '1'..='9' => vec![s(false, 2 + (c as u32 - '1' as u32))],
        '-' => vec![s(false, 12)],
        '=' => vec![s(false, 13)],
        ',' => vec![s(false, 51)],
        '.' => vec![s(false, 52)],
        ';' => vec![s(false, 53)],
        ':' => vec![s(true, 53)],
        '/' => vec![s(false, 89)],
        '?' => vec![s(true, 89)],
        '\\' => vec![s(false, 86)],
        '|' => vec![s(true, 86)],
        '\'' => vec![s(false, 41)],
        '"' => vec![s(true, 41)],
        '[' => vec![s(false, 27)],
        '{' => vec![s(true, 27)],
        ']' => vec![s(false, 43)],
        '}' => vec![s(true, 43)],
        '_' => vec![s(true, 12)],
        '+' => vec![s(true, 13)],
        '!' => vec![s(true, 2)],
        '@' => vec![s(true, 3)],
        '#' => vec![s(true, 4)],
        '$' => vec![s(true, 5)],
        '%' => vec![s(true, 6)],
        '&' => vec![s(true, 8)],
        '*' => vec![s(true, 9)],
        '(' => vec![s(true, 10)],
        ')' => vec![s(true, 11)],
        '<' => vec![s(true, 51)],
        '>' => vec![s(true, 52)],
        _ => return None,
    })
}

pub fn text_to_strokes(text: &str) -> Vec<Stroke> {
    let texto = text.replace("\\n", "\n");
    texto.chars().filter_map(strokes_for).flatten().collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn letras_e_acentos() {
        assert_eq!(strokes_for('e'), Some(vec![Stroke { shift: false, code: 18 }]));
        assert_eq!(strokes_for('ç'), Some(vec![Stroke { shift: false, code: 39 }]));
        assert_eq!(strokes_for('ã'), Some(vec![Stroke { shift: false, code: 40 }, Stroke { shift: false, code: 30 }]));
        assert_eq!(strokes_for('Ç'), Some(vec![Stroke { shift: true, code: 39 }]));
        assert_eq!(text_to_strokes("a\\nb").len(), 3);
    }
}
