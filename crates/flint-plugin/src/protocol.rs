//! Wire protocol, version 1: length-prefixed JSON over the plugin's
//! stdin/stdout.
//!
//! A frame is a little-endian `u32` byte length followed by one UTF-8 JSON
//! object. Frames above [`MAX_FRAME`] are refused before allocation.
//!
//! Host to plugin: `{"id": 7, "method": "query", "params": {...}}`, methods
//! `hello`, `query`, `action`. Plugin to host: `{"id": 7, "result": {...}}`
//! or `{"id": 7, "error": "message"}`, exactly one reply per request, in
//! order. Everything a plugin returns is clamped ([`Item::clamp`]) before
//! the UI sees it. The full specification is in `docs/plugins.md`.

use serde::{Deserialize, Serialize};
use std::io::{self, Read, Write};

pub const MAX_FRAME: usize = 1 << 20;
/// At most this many results per query are shown.
pub const MAX_ITEMS: usize = 20;
pub const MAX_TEXT: usize = 300;
pub const MAX_ACTION_TEXT: usize = 8 << 10;

pub fn write_frame(w: &mut impl Write, json: &[u8]) -> io::Result<()> {
    if json.len() > MAX_FRAME {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "frame too large"));
    }
    w.write_all(&(json.len() as u32).to_le_bytes())?;
    w.write_all(json)?;
    w.flush()
}

/// One frame; `Ok(None)` at a clean end of stream.
pub fn read_frame(r: &mut impl Read) -> io::Result<Option<Vec<u8>>> {
    let mut len = [0u8; 4];
    match r.read_exact(&mut len) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e),
    }
    let n = u32::from_le_bytes(len) as usize;
    if n > MAX_FRAME {
        return Err(io::Error::new(io::ErrorKind::InvalidData, format!("frame of {n} bytes exceeds the {MAX_FRAME}-byte limit")));
    }
    let mut buf = vec![0u8; n];
    r.read_exact(&mut buf)?;
    Ok(Some(buf))
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Request {
    pub id: u64,
    #[serde(flatten)]
    pub call: Call,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "method", content = "params", rename_all = "snake_case")]
pub enum Call {
    /// First request after start: the host's API version and what the user
    /// granted.
    Hello { api: u32, permissions: Vec<crate::Permission> },
    /// The text after the plugin's prefix.
    Query { text: String },
    /// A result action on the selection (paths only with `selection_paths`).
    Action { action: String, paths: Vec<String> },
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Reply {
    pub id: u64,
    #[serde(default)]
    pub result: Option<serde_json::Value>,
    #[serde(default)]
    pub error: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct HelloResult {
    pub api: u32,
}

/// What pressing Enter on a plugin result does. Performed by Flint, after
/// checking the plugin's permissions; a plugin never acts through Flint
/// in any other way.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ItemAction {
    /// Copy text to the clipboard.
    Copy { text: String },
    /// Open an http(s) URL (needs `open_urls`).
    OpenUrl { url: String },
    /// Replace the search text (e.g. to refine or to run a file search).
    Search { query: String },
    None,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Item {
    pub title: String,
    #[serde(default)]
    pub subtitle: String,
    #[serde(default = "no_action")]
    pub action: ItemAction,
}

fn no_action() -> ItemAction {
    ItemAction::None
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct QueryResult {
    #[serde(default)]
    pub items: Vec<Item>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActionResult {
    #[serde(default)]
    pub message: String,
    /// Text to put on the clipboard.
    #[serde(default)]
    pub copy: Option<String>,
}

/// Printable, single-line, bounded text (no control or bidi-override
/// characters that could disguise what a result says).
pub fn clean(s: &str, max: usize) -> String {
    let mut out = String::new();
    for c in s.chars() {
        let bidi = matches!(c, '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}' | '\u{200E}' | '\u{200F}' | '\u{061C}');
        if bidi {
            continue;
        }
        out.push(if c.is_control() { ' ' } else { c });
        if out.chars().count() >= max {
            out.push('…');
            break;
        }
    }
    out
}

impl Item {
    pub fn clamp(mut self) -> Item {
        self.title = clean(&self.title, MAX_TEXT);
        self.subtitle = clean(&self.subtitle, MAX_TEXT);
        self.action = match self.action {
            // Clipboard text keeps its line breaks but stays bounded.
            ItemAction::Copy { text } => ItemAction::Copy { text: text.chars().take(MAX_ACTION_TEXT).collect() },
            ItemAction::OpenUrl { url } => ItemAction::OpenUrl { url: clean(&url, 2048) },
            ItemAction::Search { query } => ItemAction::Search { query: clean(&query, 512) },
            ItemAction::None => ItemAction::None,
        };
        self
    }
}

impl ActionResult {
    pub fn clamp(self) -> ActionResult {
        ActionResult { message: clean(&self.message, MAX_TEXT), copy: self.copy.map(|t| t.chars().take(MAX_ACTION_TEXT).collect()) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requests_have_the_documented_shape() {
        let r = Request { id: 3, call: Call::Query { text: "now".into() } };
        assert_eq!(serde_json::to_string(&r).unwrap(), r#"{"id":3,"method":"query","params":{"text":"now"}}"#);
        let h = Request { id: 1, call: Call::Hello { api: 1, permissions: vec![crate::Permission::OpenUrls] } };
        assert_eq!(serde_json::to_string(&h).unwrap(), r#"{"id":1,"method":"hello","params":{"api":1,"permissions":["open_urls"]}}"#);
    }

    #[test]
    fn plugin_output_is_clamped() {
        let it = Item { title: format!("a\u{202E}b\nc{}", "x".repeat(1000)), subtitle: "\u{7}bell".into(), action: ItemAction::Copy { text: "y".repeat(100_000) } }.clamp();
        assert!(it.title.starts_with("ab c") && it.title.chars().count() <= MAX_TEXT + 1);
        assert_eq!(it.subtitle, " bell");
        assert!(matches!(it.action, ItemAction::Copy { text } if text.len() == MAX_ACTION_TEXT));
        let parsed: QueryResult = serde_json::from_str(r#"{"items":[{"title":"t"}]}"#).unwrap();
        assert_eq!(parsed.items[0].action, ItemAction::None);
    }

    #[test]
    fn frames_refuse_oversized_lengths() {
        let mut s = Vec::new();
        write_frame(&mut s, b"{}").unwrap();
        assert_eq!(read_frame(&mut &s[..]).unwrap().unwrap(), b"{}");
        assert_eq!(read_frame(&mut &b""[..]).unwrap(), None);
        assert!(read_frame(&mut &u32::MAX.to_le_bytes()[..]).is_err());
        assert!(write_frame(&mut Vec::new(), &vec![b' '; MAX_FRAME + 1]).is_err());
    }
}
