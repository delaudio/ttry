use std::io::Write;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use crate::{Error, Result};

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Key {
    Text(String),
    Enter,
    Escape,
    Tab,
    Backspace,
    Delete,
    ArrowUp,
    ArrowDown,
    ArrowLeft,
    ArrowRight,
    Home,
    End,
    PageUp,
    PageDown,
    Function(u8),
    Modified {
        ctrl: bool,
        alt: bool,
        shift: bool,
        key: Box<Key>,
    },
}

impl Key {
    pub fn parse(expression: &str) -> Result<Self> {
        let original_expression = expression;
        if expression.is_empty() {
            return Err(Error::InvalidKey(expression.into()));
        }
        match expression {
            "\t" => return Ok(Key::Tab),
            "\n" | "\r" => return Ok(Key::Enter),
            _ => {}
        }
        if expression != " " && expression.trim().is_empty() {
            return Err(Error::InvalidKey(expression.into()));
        }
        if expression == " " {
            return Ok(Key::Text(expression.into()));
        }
        let expression = expression.trim();
        if expression == "+" {
            return Ok(Key::Text(expression.into()));
        }
        if !expression.contains('+')
            && expression.chars().count() == 1
            && expression.chars().next().is_some_and(|ch| !ch.is_control())
        {
            return Ok(Key::Text(expression.into()));
        }
        let original_parts: Vec<&str> = expression.trim().split('+').collect();
        if original_parts.iter().any(|part| part.is_empty()) {
            return Err(Error::InvalidKey(original_expression.into()));
        }
        let normalized_parts: Vec<String> = original_parts
            .iter()
            .map(|part| part.to_ascii_lowercase())
            .collect();
        let parts: Vec<&str> = normalized_parts.iter().map(String::as_str).collect();
        let original_key = original_parts.last().copied().unwrap_or(expression);
        let (modifiers, key_name) = parts.split_at(parts.len() - 1);
        if modifiers
            .iter()
            .any(|part| !matches!(*part, "ctrl" | "control" | "alt" | "option" | "shift"))
        {
            return Err(Error::InvalidKey(expression.into()));
        }
        let ctrl_count = modifiers
            .iter()
            .filter(|part| matches!(**part, "ctrl" | "control"))
            .count();
        let alt_count = modifiers
            .iter()
            .filter(|part| matches!(**part, "alt" | "option"))
            .count();
        let shift_count = modifiers.iter().filter(|part| **part == "shift").count();
        if ctrl_count > 1 || alt_count > 1 || shift_count > 1 {
            return Err(Error::InvalidKey(expression.into()));
        }
        let ctrl = ctrl_count == 1;
        let alt = alt_count == 1;
        let shift = shift_count == 1;
        let base = match key_name[0] {
            "enter" | "return" => Key::Enter,
            "escape" | "esc" => Key::Escape,
            "tab" => Key::Tab,
            "backspace" => Key::Backspace,
            "delete" | "del" => Key::Delete,
            "arrowup" | "up" => Key::ArrowUp,
            "arrowdown" | "down" => Key::ArrowDown,
            "arrowleft" | "left" => Key::ArrowLeft,
            "arrowright" | "right" => Key::ArrowRight,
            "home" => Key::Home,
            "end" => Key::End,
            "pageup" | "pgup" => Key::PageUp,
            "pagedown" | "pgdn" => Key::PageDown,
            name if name.starts_with('f') && name[1..].parse::<u8>().is_ok() => {
                let number = name[1..].parse::<u8>().unwrap();
                if !(1..=12).contains(&number) {
                    return Err(Error::UnsupportedKey(
                        original_expression.into(),
                        "only F1 through F12 are supported".into(),
                    ));
                }
                Key::Function(number)
            }
            name if name.chars().count() == 1 => Key::Text(original_key.into()),
            _ => return Err(Error::InvalidKey(original_expression.into())),
        };
        if ctrl || alt || shift {
            let modified = Key::Modified {
                ctrl,
                alt,
                shift,
                key: Box::new(base),
            };
            match modified.encode() {
                Err(Error::UnsupportedKey(_, reason)) => {
                    Err(Error::UnsupportedKey(original_expression.into(), reason))
                }
                Err(error) => Err(error),
                Ok(_) => Ok(modified),
            }
        } else {
            Ok(base)
        }
    }

    pub fn encode(&self) -> Result<Vec<u8>> {
        match self {
            Key::Text(text) => Ok(text.as_bytes().to_vec()),
            Key::Enter => Ok(vec![b'\r']),
            Key::Escape => Ok(vec![0x1b]),
            Key::Tab => Ok(vec![b'\t']),
            Key::Backspace => Ok(vec![0x7f]),
            Key::Delete => Ok(b"\x1b[3~".to_vec()),
            Key::ArrowUp => Ok(b"\x1b[A".to_vec()),
            Key::ArrowDown => Ok(b"\x1b[B".to_vec()),
            Key::ArrowRight => Ok(b"\x1b[C".to_vec()),
            Key::ArrowLeft => Ok(b"\x1b[D".to_vec()),
            Key::Home => Ok(b"\x1b[H".to_vec()),
            Key::End => Ok(b"\x1b[F".to_vec()),
            Key::PageUp => Ok(b"\x1b[5~".to_vec()),
            Key::PageDown => Ok(b"\x1b[6~".to_vec()),
            Key::Function(number) => {
                let sequence = match number {
                    1 => "\x1bOP",
                    2 => "\x1bOQ",
                    3 => "\x1bOR",
                    4 => "\x1bOS",
                    5 => "\x1b[15~",
                    6 => "\x1b[17~",
                    7 => "\x1b[18~",
                    8 => "\x1b[19~",
                    9 => "\x1b[20~",
                    10 => "\x1b[21~",
                    11 => "\x1b[23~",
                    12 => "\x1b[24~",
                    _ => {
                        return Err(Error::UnsupportedKey(
                            format!("F{number}"),
                            "only F1 through F12 are supported".into(),
                        ))
                    }
                };
                Ok(sequence.as_bytes().to_vec())
            }
            Key::Modified {
                ctrl,
                alt,
                shift,
                key,
            } => encode_modified(*ctrl, *alt, *shift, key),
        }
    }
}

fn encode_modified(ctrl: bool, alt: bool, shift: bool, key: &Key) -> Result<Vec<u8>> {
    if shift && matches!(key, Key::Tab) && !ctrl && !alt {
        return Ok(b"\x1b[Z".to_vec());
    }
    if ctrl {
        if let Key::Text(text) = key {
            let mut chars = text.chars();
            if let (Some(ch), None) = (chars.next(), chars.next()) {
                if !ch.is_ascii() {
                    return Err(Error::UnsupportedKey(
                        format!("{key:?}"),
                        "Ctrl combinations are only supported for ASCII letters".into(),
                    ));
                }
                let upper = ch.to_ascii_uppercase();
                if upper.is_ascii_uppercase() {
                    if shift {
                        return Err(Error::UnsupportedKey(
                            format!("{key:?}"),
                            "Ctrl+Shift letters have no distinct portable terminal encoding".into(),
                        ));
                    }
                    let mut bytes = vec![(upper as u8) & 0x1f];
                    if alt {
                        bytes.insert(0, 0x1b);
                    }
                    return Ok(bytes);
                }
            }
        }
    }
    if !ctrl && shift {
        if let Key::Text(text) = key {
            let mut chars = text.chars();
            if let (Some(ch), None) = (chars.next(), chars.next()) {
                if ch.is_ascii_alphabetic() {
                    if alt {
                        return Err(Error::UnsupportedKey(
                            format!("{key:?}"),
                            "Alt+Shift letters have no distinct portable terminal encoding".into(),
                        ));
                    }
                    return Ok(ch.to_ascii_uppercase().to_string().into_bytes());
                }
            }
        }
    }
    let navigation_code = match key {
        Key::ArrowUp => Some('A'),
        Key::ArrowDown => Some('B'),
        Key::ArrowRight => Some('C'),
        Key::ArrowLeft => Some('D'),
        Key::Home => Some('H'),
        Key::End => Some('F'),
        _ => None,
    };
    if let Some(code) = navigation_code {
        let modifier = 1 + shift as u8 + (alt as u8 * 2) + (ctrl as u8 * 4);
        return Ok(format!("\x1b[1;{modifier}{code}").into_bytes());
    }
    if alt && !ctrl && !shift {
        let mut bytes = vec![0x1b];
        bytes.extend(key.encode()?);
        return Ok(bytes);
    }
    Err(Error::UnsupportedKey(
        format!("{key:?}"),
        "this modifier combination has no portable terminal encoding".into(),
    ))
}

#[derive(Clone)]
pub struct Keyboard {
    writer: Arc<Mutex<Box<dyn Write + Send>>>,
}

impl std::fmt::Debug for Keyboard {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Keyboard").finish_non_exhaustive()
    }
}

impl Keyboard {
    pub(crate) fn new(writer: Arc<Mutex<Box<dyn Write + Send>>>) -> Self {
        Self { writer }
    }
    /// Presses one key expression.
    ///
    /// Shifted printable characters are keyboard-layout dependent, so callers
    /// pass the resulting character directly (`"!"`, `"A"`, or `"alt+A"`)
    /// instead of expressions such as `"shift+1"` or `"alt+shift+a"`.
    pub fn press(&self, expression: &str) -> Result<()> {
        self.write_all(&Key::parse(expression)?.encode()?)
    }
    pub fn paste(&self, text: &str) -> Result<()> {
        self.write_all(text.as_bytes())
    }
    pub fn type_text(&self, text: &str, delay: Option<Duration>) -> Result<()> {
        for ch in text.chars() {
            self.write_all(ch.to_string().as_bytes())?;
            if let Some(delay) = delay {
                if !delay.is_zero() {
                    thread::sleep(delay);
                }
            }
        }
        Ok(())
    }
    fn write_all(&self, bytes: &[u8]) -> Result<()> {
        let mut writer = self.writer.lock().expect("keyboard lock poisoned");
        writer.write_all(bytes)?;
        writer.flush()?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn basic_and_navigation_encodings() {
        let cases = [
            ("a", b"a".as_slice()),
            ("enter", b"\r"),
            (" enter", b"\r"),
            ("enter ", b"\r"),
            ("escape", b"\x1b"),
            ("delete", b"\x1b[3~"),
            ("up", b"\x1b[A"),
            (" up ", b"\x1b[A"),
            ("f12", b"\x1b[24~"),
        ];
        for (input, expected) in cases {
            assert_eq!(Key::parse(input).unwrap().encode().unwrap(), expected);
        }
    }

    #[test]
    fn plus_and_space_are_printable_keys() {
        assert_eq!(Key::parse("+").unwrap().encode().unwrap(), b"+");
        assert_eq!(Key::parse(" ").unwrap().encode().unwrap(), b" ");
        assert_eq!(Key::parse("\t").unwrap().encode().unwrap(), b"\t");
        assert_eq!(Key::parse("\n").unwrap().encode().unwrap(), b"\r");
        assert_eq!(Key::parse("\r").unwrap().encode().unwrap(), b"\r");
        assert!(Key::parse("hello").is_err());
        assert!(Key::parse("  ").is_err());
        assert!(Key::parse("\t ").is_err());
    }
    #[test]
    fn modifier_encodings() {
        assert_eq!(Key::parse("ctrl+c").unwrap().encode().unwrap(), vec![3]);
        assert_eq!(Key::parse(" ctrl+c").unwrap().encode().unwrap(), vec![3]);
        assert_eq!(Key::parse("ctrl+c ").unwrap().encode().unwrap(), vec![3]);
        assert_eq!(Key::parse("alt+x").unwrap().encode().unwrap(), b"\x1bx");
        assert_eq!(
            Key::parse("alt+up").unwrap().encode().unwrap(),
            b"\x1b[1;3A"
        );
        assert_eq!(
            Key::parse("alt+left").unwrap().encode().unwrap(),
            b"\x1b[1;3D"
        );
        assert_eq!(
            Key::parse("alt+home").unwrap().encode().unwrap(),
            b"\x1b[1;3H"
        );
        assert_eq!(
            Key::parse("shift+tab").unwrap().encode().unwrap(),
            b"\x1b[Z"
        );
        assert_eq!(Key::parse("A").unwrap().encode().unwrap(), b"A");
        assert_eq!(Key::parse("shift+a").unwrap().encode().unwrap(), b"A");
        assert_eq!(Key::parse("alt+A").unwrap().encode().unwrap(), b"\x1bA");
        assert!(matches!(
            Key::parse("alt+shift+a"),
            Err(Error::UnsupportedKey(expression, _)) if expression == "alt+shift+a"
        ));
        assert!(Key::parse("hyper+x").is_err());
        assert!(Key::parse("ctrl+control+c").is_err());
        assert!(Key::parse("alt+option+x").is_err());
        assert!(Key::parse("shift+shift+a").is_err());
        assert!(matches!(Key::parse("ctrl+"), Err(Error::InvalidKey(_))));
        assert!(matches!(Key::parse("ctrl++"), Err(Error::InvalidKey(_))));
        assert!(matches!(
            Key::parse("ctrl+shift+f1"),
            Err(Error::UnsupportedKey(expression, _)) if expression == "ctrl+shift+f1"
        ));
        assert!(matches!(
            Key::parse("ctrl+shift+c"),
            Err(Error::UnsupportedKey(expression, _)) if expression == "ctrl+shift+c"
        ));
        assert!(matches!(
            Key::parse("ctrl+é"),
            Err(Error::UnsupportedKey(expression, reason))
                if expression == "ctrl+é"
                    && reason == "Ctrl combinations are only supported for ASCII letters"
        ));
        assert!(matches!(
            Key::parse("shift+1"),
            Err(Error::UnsupportedKey(expression, _)) if expression == "shift+1"
        ));
        assert!(matches!(
            Key::parse("alt+shift+/"),
            Err(Error::UnsupportedKey(expression, _)) if expression == "alt+shift+/"
        ));
        assert_eq!(Key::parse("!").unwrap().encode().unwrap(), b"!");
    }
}
