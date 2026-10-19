//! Hotkey strings such as `Ctrl+Space` or `Ctrl+Alt+F`, parsed into
//! modifier flags and a Win32 virtual-key code (values from winuser.h, so
//! this module needs no platform bindings and is testable anywhere).

use std::fmt;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Hotkey {
    pub ctrl: bool,
    pub alt: bool,
    pub shift: bool,
    pub win: bool,
    pub vk: u16,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HotkeyError {
    Empty,
    UnknownKey(String),
    DuplicateModifier(String),
    NoKey,
    /// Only modifiers, or a plain character key without a modifier, would
    /// steal ordinary typing.
    NeedsModifier,
    /// Combinations Windows reserves or that break basic navigation.
    Reserved(String),
}

impl fmt::Display for HotkeyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            HotkeyError::Empty => write!(f, "hotkey is empty"),
            HotkeyError::UnknownKey(k) => write!(f, "unknown key `{k}`"),
            HotkeyError::DuplicateModifier(m) => write!(f, "modifier `{m}` given twice"),
            HotkeyError::NoKey => write!(f, "hotkey has modifiers but no key"),
            HotkeyError::NeedsModifier => write!(f, "hotkey needs Ctrl, Alt or Win (except F-keys)"),
            HotkeyError::Reserved(s) => write!(f, "`{s}` is reserved by Windows"),
        }
    }
}

const NAMED: &[(&str, u16)] = &[
    ("space", 0x20),
    ("tab", 0x09),
    ("enter", 0x0D),
    ("return", 0x0D),
    ("esc", 0x1B),
    ("escape", 0x1B),
    ("backspace", 0x08),
    ("insert", 0x2D),
    ("delete", 0x2E),
    ("home", 0x24),
    ("end", 0x23),
    ("pageup", 0x21),
    ("pagedown", 0x22),
    ("left", 0x25),
    ("up", 0x26),
    ("right", 0x27),
    ("down", 0x28),
    ("`", 0xC0),
    ("backquote", 0xC0),
    ("-", 0xBD),
    ("=", 0xBB),
    ("[", 0xDB),
    ("]", 0xDD),
    (";", 0xBA),
    ("'", 0xDE),
    (",", 0xBC),
    (".", 0xBE),
    ("/", 0xBF),
    ("\\", 0xDC),
    ("pause", 0x13),
    ("scrolllock", 0x91),
];

fn key_code(name: &str) -> Option<u16> {
    let l = name.to_ascii_lowercase();
    if let Some((_, v)) = NAMED.iter().find(|(n, _)| *n == l) {
        return Some(*v);
    }
    let b = l.as_bytes();
    if b.len() == 1 && (b[0].is_ascii_lowercase() || b[0].is_ascii_digit()) {
        return Some(b[0].to_ascii_uppercase() as u16);
    }
    if let Some(n) = l.strip_prefix('f').and_then(|n| n.parse::<u16>().ok()) {
        if (1..=24).contains(&n) {
            return Some(0x70 + n - 1);
        }
    }
    None
}

fn key_name(vk: u16) -> String {
    match vk {
        0x30..=0x39 | 0x41..=0x5A => (vk as u8 as char).to_string(),
        0x70..=0x87 => format!("F{}", vk - 0x70 + 1),
        _ => NAMED.iter().find(|(_, v)| *v == vk).map_or_else(|| format!("VK{vk:#04x}"), |(n, _)| {
            let mut c = n.chars();
            c.next().map_or(String::new(), |f| f.to_uppercase().collect::<String>() + c.as_str())
        }),
    }
}

impl Hotkey {
    pub fn parse(s: &str) -> Result<Hotkey, HotkeyError> {
        let s = s.trim();
        if s.is_empty() {
            return Err(HotkeyError::Empty);
        }
        let mut hk = Hotkey { ctrl: false, alt: false, shift: false, win: false, vk: 0 };
        let mut key = None;
        // Split on '+', but allow the '+' key itself only as "Plus".
        for part in s.split('+').map(str::trim) {
            let flag = match part.to_ascii_lowercase().as_str() {
                "ctrl" | "control" => Some(&mut hk.ctrl),
                "alt" => Some(&mut hk.alt),
                "shift" => Some(&mut hk.shift),
                "win" | "super" | "meta" => Some(&mut hk.win),
                _ => None,
            };
            match flag {
                Some(f) if *f => return Err(HotkeyError::DuplicateModifier(part.to_owned())),
                Some(f) => *f = true,
                None => {
                    if key.is_some() {
                        return Err(HotkeyError::UnknownKey(part.to_owned()));
                    }
                    let k = if part.eq_ignore_ascii_case("plus") { Some(0xBB) } else { key_code(part) };
                    key = Some(k.ok_or_else(|| HotkeyError::UnknownKey(part.to_owned()))?);
                }
            }
        }
        hk.vk = key.ok_or(HotkeyError::NoKey)?;
        let fkey = (0x70..=0x87).contains(&hk.vk);
        if !(hk.ctrl || hk.alt || hk.win || fkey) {
            return Err(HotkeyError::NeedsModifier);
        }
        let reserved = [
            (Hotkey { ctrl: false, alt: true, shift: false, win: false, vk: 0x09 }, "Alt+Tab"),
            (Hotkey { ctrl: false, alt: true, shift: false, win: false, vk: 0x73 }, "Alt+F4"),
            (Hotkey { ctrl: true, alt: true, shift: false, win: false, vk: 0x2E }, "Ctrl+Alt+Delete"),
            (Hotkey { ctrl: false, alt: false, shift: false, win: true, vk: 0x4C }, "Win+L"),
            (Hotkey { ctrl: true, alt: false, shift: true, win: false, vk: 0x1B }, "Ctrl+Shift+Esc"),
        ];
        if let Some((_, name)) = reserved.iter().find(|(r, _)| *r == hk) {
            return Err(HotkeyError::Reserved((*name).to_owned()));
        }
        Ok(hk)
    }

    /// `MOD_*` flags for `RegisterHotKey` (without `MOD_NOREPEAT`).
    pub fn win32_modifiers(&self) -> u32 {
        (self.alt as u32) | (self.ctrl as u32) << 1 | (self.shift as u32) << 2 | (self.win as u32) << 3
    }
}

impl fmt::Display for Hotkey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut parts = Vec::new();
        for (on, n) in [(self.ctrl, "Ctrl"), (self.alt, "Alt"), (self.shift, "Shift"), (self.win, "Win")] {
            if on {
                parts.push(n.to_owned());
            }
        }
        parts.push(key_name(self.vk));
        write!(f, "{}", parts.join("+"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_and_round_trips() {
        for (s, canon) in [("Ctrl+Space", "Ctrl+Space"), ("ctrl + alt + k", "Ctrl+Alt+K"), ("Win+Shift+F1", "Shift+Win+F1"), ("F9", "F9"), ("Alt+`", "Alt+`"), ("Ctrl+Plus", "Ctrl+=")] {
            let h = Hotkey::parse(s).unwrap_or_else(|e| panic!("{s}: {e}"));
            assert_eq!(h.to_string(), canon);
            assert_eq!(Hotkey::parse(&h.to_string()).unwrap(), h);
        }
        let h = Hotkey::parse("Ctrl+Space").unwrap();
        assert_eq!((h.vk, h.win32_modifiers()), (0x20, 0x2));
    }

    #[test]
    fn rejects_bad_hotkeys() {
        assert_eq!(Hotkey::parse(""), Err(HotkeyError::Empty));
        assert_eq!(Hotkey::parse("Ctrl+Alt"), Err(HotkeyError::NoKey));
        assert_eq!(Hotkey::parse("K"), Err(HotkeyError::NeedsModifier));
        assert_eq!(Hotkey::parse("Shift+K"), Err(HotkeyError::NeedsModifier));
        assert!(matches!(Hotkey::parse("Ctrl+Ctrl+K"), Err(HotkeyError::DuplicateModifier(_))));
        assert!(matches!(Hotkey::parse("Ctrl+Banana"), Err(HotkeyError::UnknownKey(_))));
        assert!(matches!(Hotkey::parse("Ctrl+A+B"), Err(HotkeyError::UnknownKey(_))));
        assert!(matches!(Hotkey::parse("Alt+F4"), Err(HotkeyError::Reserved(_))));
        assert!(matches!(Hotkey::parse("Win+L"), Err(HotkeyError::Reserved(_))));
    }
}
