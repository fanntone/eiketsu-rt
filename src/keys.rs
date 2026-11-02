//! Scancode set 1 → ASCII (what the BIOS INT 9 handler stores next to the scancode).

const NORMAL: &[u8; 0x3a] = b"\0\x1b1234567890-=\x08\tqwertyuiop[]\r\0asdfghjkl;'`\0\\zxcvbnm,./\0*\0 ";
const SHIFT: &[u8; 0x3a] = b"\0\x1b!@#$%^&*()_+\x08\tQWERTYUIOP{}\r\0ASDFGHJKL:\"~\0|ZXCVBNM<>?\0*\0 ";

pub fn ascii(sc: u8, shift: bool, ctrl: bool) -> u8 {
    let i = sc as usize;
    if i >= NORMAL.len() {
        return 0; // function keys, arrows, keypad: extended (ASCII 0)
    }
    let c = if shift { SHIFT[i] } else { NORMAL[i] };
    if ctrl && c.is_ascii_alphabetic() {
        return c.to_ascii_uppercase() - b'@';
    }
    c
}

/// Scancode for a key name used in scripts (`key ENTER`, `key A`, `key F1`, `key UP`).
pub fn by_name(n: &str) -> Option<u16> {
    let n = n.to_ascii_uppercase();
    Some(match n.as_str() {
        "ESC" => 0x01,
        "ENTER" => 0x1c,
        "SPACE" => 0x39,
        "TAB" => 0x0f,
        "BACKSPACE" => 0x0e,
        "UP" => 0x148,
        "DOWN" => 0x150,
        "LEFT" => 0x14b,
        "RIGHT" => 0x14d,
        "HOME" => 0x147,
        "END" => 0x14f,
        "PGUP" => 0x149,
        "PGDN" => 0x151,
        "INS" => 0x152,
        "DEL" => 0x153,
        "SHIFT" => 0x2a,
        "CTRL" => 0x1d,
        "ALT" => 0x38,
        f if f.starts_with('F') && f.len() > 1 => {
            let k: u16 = f[1..].parse().ok()?;
            match k {
                1..=10 => 0x3a + k,
                11 => 0x57,
                12 => 0x58,
                _ => return None,
            }
        }
        s if s.len() == 1 => {
            let c = s.as_bytes()[0].to_ascii_lowercase();
            NORMAL.iter().position(|&x| x == c)? as u16
        }
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn table() {
        assert_eq!(ascii(0x1c, false, false), b'\r');
        assert_eq!(ascii(0x1e, false, false), b'a');
        assert_eq!(ascii(0x1e, true, false), b'A');
        assert_eq!(ascii(0x02, true, false), b'!');
        assert_eq!(ascii(0x48, false, false), 0);
        assert_eq!(by_name("a"), Some(0x1e));
        assert_eq!(by_name("F10"), Some(0x44));
        assert_eq!(by_name("enter"), Some(0x1c));
    }
}
