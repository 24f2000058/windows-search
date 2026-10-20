//! Format detection and the built-in backends. All of them read through
//! [`Limits`] and return bounded output; none executes anything.
//!
//! | Kind | Preview | Extract (content index) |
//! |---|---|---|
//! | text, source, CSV, JSON, XML, logs | decoded text | text |
//! | HTML | text | text without markup/scripts |
//! | DOCX, XLSX, PPTX, ODT/ODS/ODP, EPUB | text | text (bounded inflate) |
//! | ZIP, JAR | listing + bomb warnings | unsupported |
//! | PNG, JPEG, GIF, BMP, WebP | dimensions (+ shell thumbnail on Windows) | unsupported |
//! | PDF | unsupported (stated) | unsupported (stated) |
//! | programs | unsupported, never run | unsupported |
//! | other binary | hex of the first bytes | unsupported |

use crate::zip;
use crate::{Extracted, Limits, Preview};
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Text,
    Html,
    Office(Office),
    Zip,
    Image(&'static str),
    Pdf,
    Program,
    Binary,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Office {
    Docx,
    Xlsx,
    Pptx,
    OpenDocument,
    Epub,
}

impl Office {
    fn label(self) -> &'static str {
        match self {
            Office::Docx => "Word document",
            Office::Xlsx => "Excel workbook",
            Office::Pptx => "PowerPoint presentation",
            Office::OpenDocument => "OpenDocument file",
            Office::Epub => "EPUB book",
        }
    }
}

fn ext(path: &str) -> String {
    let name = path.rsplit(['\\', '/']).next().unwrap_or(path);
    match name.rfind('.') {
        Some(i) if i > 0 => name[i + 1..].to_ascii_lowercase(),
        _ => String::new(),
    }
}

const TEXT_EXT: &[&str] = &[
    "txt", "md", "markdown", "rst", "log", "csv", "tsv", "json", "jsonl", "xml", "yaml", "yml", "toml", "ini", "cfg", "conf", "properties", "rs", "c", "h", "cpp", "hpp", "cc", "cs", "java", "kt", "py", "js", "ts",
    "tsx", "jsx", "go", "rb", "php", "swift", "sh", "ps1", "psm1", "bat", "cmd", "lua", "sql", "css", "scss", "less", "vue", "svg", "tex", "r", "m", "pl", "gitignore", "editorconfig", "reg", "srt", "vtt",
];

/// Decides by magic bytes first, extension second (extensions lie).
pub fn sniff(path: &str, head: &[u8]) -> Kind {
    let e = ext(path);
    let starts = |m: &[u8]| head.starts_with(m);
    if starts(b"\x89PNG\r\n\x1a\n") {
        return Kind::Image("PNG");
    }
    if starts(b"\xFF\xD8\xFF") {
        return Kind::Image("JPEG");
    }
    if starts(b"GIF87a") || starts(b"GIF89a") {
        return Kind::Image("GIF");
    }
    if starts(b"BM") && head.len() >= 26 && matches!(e.as_str(), "bmp" | "dib") {
        return Kind::Image("BMP");
    }
    if head.len() >= 12 && &head[0..4] == b"RIFF" && &head[8..12] == b"WEBP" {
        return Kind::Image("WebP");
    }
    if starts(b"%PDF-") {
        return Kind::Pdf;
    }
    if starts(b"MZ") || starts(b"\x7FELF") {
        return Kind::Program;
    }
    if starts(b"PK\x03\x04") || starts(b"PK\x05\x06") {
        return match e.as_str() {
            "docx" | "docm" => Kind::Office(Office::Docx),
            "xlsx" | "xlsm" => Kind::Office(Office::Xlsx),
            "pptx" | "pptm" => Kind::Office(Office::Pptx),
            "odt" | "ods" | "odp" => Kind::Office(Office::OpenDocument),
            "epub" => Kind::Office(Office::Epub),
            _ => Kind::Zip,
        };
    }
    if matches!(e.as_str(), "exe" | "dll" | "sys" | "scr" | "com" | "msi") {
        return Kind::Program;
    }
    if matches!(e.as_str(), "html" | "htm" | "xhtml") && looks_textual(head) {
        return Kind::Html;
    }
    if looks_textual(head) {
        return Kind::Text;
    }
    let _ = TEXT_EXT; // extension alone never makes binary data "text"
    Kind::Binary
}

/// BOM, valid UTF-8, or few control characters.
fn looks_textual(head: &[u8]) -> bool {
    if head.starts_with(&[0xEF, 0xBB, 0xBF]) || head.starts_with(&[0xFF, 0xFE]) || head.starts_with(&[0xFE, 0xFF]) {
        return true;
    }
    if head.contains(&0) {
        return false;
    }
    let controls = head.iter().filter(|&&b| b < 0x20 && !matches!(b, b'\n' | b'\r' | b'\t' | 0x0C)).count();
    controls * 50 <= head.len()
}

/// Windows-1252 bytes 0x80–0x9F; the rest of the range is Latin-1.
const CP1252: [char; 32] = [
    '€', '\u{81}', '‚', 'ƒ', '„', '…', '†', '‡', 'ˆ', '‰', 'Š', '‹', 'Œ', '\u{8D}', 'Ž', '\u{8F}', '\u{90}', '‘', '’', '“', '”', '•', '–', '—', '˜', '™', 'š', '›', 'œ', '\u{9D}', 'ž', 'Ÿ',
];

/// Decodes text, returning (text, encoding name). `cut` says the bytes are a
/// prefix of the file, so an incomplete final character is dropped.
pub fn decode(bytes: &[u8], cut: bool) -> (String, &'static str) {
    if let Some(b) = bytes.strip_prefix(&[0xEF, 0xBB, 0xBF]) {
        return (String::from_utf8_lossy(utf8_prefix(b, cut)).into_owned(), "UTF-8 (BOM)");
    }
    for (bom, le, name) in [(&[0xFF, 0xFE][..], true, "UTF-16 LE"), (&[0xFE, 0xFF][..], false, "UTF-16 BE")] {
        if let Some(b) = bytes.strip_prefix(bom) {
            let units: Vec<u16> = b.chunks_exact(2).map(|c| if le { u16::from_le_bytes([c[0], c[1]]) } else { u16::from_be_bytes([c[0], c[1]]) }).collect();
            return (String::from_utf16_lossy(&units), name);
        }
    }
    let p = utf8_prefix(bytes, cut);
    if let Ok(s) = std::str::from_utf8(p) {
        return (s.to_owned(), "UTF-8");
    }
    (bytes.iter().map(|&b| if (0x80..0xA0).contains(&b) { CP1252[(b - 0x80) as usize] } else { b as char }).collect(), "Windows-1252")
}

/// Drops an incomplete UTF-8 sequence at the end of a cut-off buffer.
fn utf8_prefix(b: &[u8], cut: bool) -> &[u8] {
    if !cut {
        return b;
    }
    match std::str::from_utf8(b) {
        Err(e) if e.error_len().is_none() => &b[..e.valid_up_to()],
        _ => b,
    }
}

fn cap_chars(s: String, max: usize) -> (String, bool) {
    match s.char_indices().nth(max) {
        Some((i, _)) => (s[..i].to_owned(), true),
        None => (s, false),
    }
}

/// Reads at most `limit` bytes; returns (bytes, file_size).
fn read_head(f: &mut File, limit: u64) -> std::io::Result<(Vec<u8>, u64)> {
    let size = f.metadata()?.len();
    let mut buf = Vec::with_capacity(size.min(limit) as usize);
    f.take(limit).read_to_end(&mut buf)?;
    Ok((buf, size))
}

/// Text from XML/HTML markup: tags removed, entities decoded, paragraph
/// and line-break elements become newlines, `script`/`style` skipped.
pub fn markup_text(src: &str, max_chars: usize) -> (String, bool) {
    const BREAK: &[&str] = &["p", "w:p", "text:p", "text:h", "a:p", "br", "div", "li", "tr", "h1", "h2", "h3", "h4", "h5", "h6", "row", "si", "text:list-item", "table:table-row"];
    const SPACE: &[&str] = &["w:tab", "text:tab", "text:s", "td", "th", "c", "table:table-cell", "a:br", "w:br"];
    let mut out = String::new();
    let mut chars = 0usize;
    let mut rest = src;
    let mut skip_until: Option<&'static str> = None;
    let push = |out: &mut String, chars: &mut usize, s: &str| -> bool {
        for c in s.chars() {
            if *chars >= max_chars {
                return false;
            }
            let c = if c.is_whitespace() && c != '\n' { ' ' } else { c };
            if c == ' ' && (out.ends_with(' ') || out.ends_with('\n') || out.is_empty()) {
                continue;
            }
            if c == '\n' && (out.ends_with("\n\n") || out.is_empty()) {
                continue;
            }
            if c == '\n' && out.ends_with(' ') {
                out.pop();
                *chars -= 1;
            }
            out.push(c);
            *chars += 1;
        }
        true
    };
    while !rest.is_empty() {
        let Some(lt) = rest.find('<') else {
            if skip_until.is_none() && !push(&mut out, &mut chars, &decode_entities(rest)) {
                return (out, true);
            }
            break;
        };
        if skip_until.is_none() && !push(&mut out, &mut chars, &decode_entities(&rest[..lt])) {
            return (out, true);
        }
        rest = &rest[lt..];
        if rest.starts_with("<!--") {
            rest = rest.find("-->").map_or("", |i| &rest[i + 3..]);
            continue;
        }
        let Some(gt) = rest.find('>') else { break };
        let tag = &rest[1..gt];
        rest = &rest[gt + 1..];
        let closing = tag.starts_with('/');
        let name = tag.trim_start_matches('/').split(|c: char| c.is_whitespace() || c == '/').next().unwrap_or("").to_ascii_lowercase();
        if let Some(end) = skip_until {
            if closing && name == end {
                skip_until = None;
            }
            continue;
        }
        if !closing && (name == "script" || name == "style") && !tag.ends_with('/') {
            skip_until = Some(if name == "script" { "script" } else { "style" });
            continue;
        }
        let sep = if BREAK.contains(&name.as_str()) && (closing || name == "br") {
            "\n"
        } else if SPACE.contains(&name.as_str()) {
            " "
        } else {
            ""
        };
        if !sep.is_empty() && !push(&mut out, &mut chars, sep) {
            return (out, true);
        }
    }
    (out.trim_end().to_owned(), false)
}

fn decode_entities(s: &str) -> String {
    if !s.contains('&') {
        return s.to_owned();
    }
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(i) = rest.find('&') {
        out.push_str(&rest[..i]);
        rest = &rest[i..];
        let end = rest[..rest.len().min(12)].find(';');
        let decoded = end.and_then(|e| {
            let ent = &rest[1..e];
            let c = match ent {
                "amp" => Some('&'),
                "lt" => Some('<'),
                "gt" => Some('>'),
                "quot" => Some('"'),
                "apos" => Some('\''),
                "nbsp" => Some(' '),
                _ if ent.starts_with("#x") || ent.starts_with("#X") => u32::from_str_radix(&ent[2..], 16).ok().and_then(char::from_u32),
                _ if ent.starts_with('#') => ent[1..].parse().ok().and_then(char::from_u32),
                _ => None,
            };
            c.map(|c| (c, e))
        });
        match decoded {
            Some((c, e)) => {
                out.push(c);
                rest = &rest[e + 1..];
            }
            None => {
                out.push('&');
                rest = &rest[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

/// Text of an Office/ODF/EPUB package (bounded inflate of the text parts).
pub fn office_text(f: &mut File, kind: Office, limits: &Limits) -> Result<(String, bool), String> {
    let d = zip::read_directory(f, 20_000)?;
    let mut parts: Vec<&zip::Member> = d
        .members
        .iter()
        .filter(|m| {
            let n = m.name.as_str();
            match kind {
                Office::Docx => n == "word/document.xml",
                Office::Xlsx => n == "xl/sharedStrings.xml",
                Office::Pptx => n.starts_with("ppt/slides/slide") && n.ends_with(".xml"),
                Office::OpenDocument => n == "content.xml",
                Office::Epub => n.ends_with(".xhtml") || n.ends_with(".html") || n.ends_with(".htm"),
            }
        })
        .collect();
    if parts.is_empty() {
        return Err(format!("no text part found in this {}", kind.label()));
    }
    // Slides and chapters in numeric order (slide2 before slide10).
    parts.sort_by_key(|m| {
        let digits: String = m.name.chars().filter(|c| c.is_ascii_digit()).collect();
        (digits.len(), digits, m.name.clone())
    });
    let mut text = String::new();
    let mut truncated = false;
    let mut budget = limits.max_inflate;
    for m in parts {
        let left = limits.max_chars.saturating_sub(text.chars().count());
        if left == 0 || budget == 0 {
            truncated = true;
            break;
        }
        let part_limits = Limits { max_inflate: budget, ..limits.clone() };
        let (xml, cut) = zip::read_member(f, m, &part_limits)?;
        budget = budget.saturating_sub(xml.len());
        let (t, more) = markup_text(&String::from_utf8_lossy(&xml), left);
        if !text.is_empty() && !t.is_empty() {
            text.push_str("\n\n");
        }
        text.push_str(&t);
        truncated |= cut || more;
    }
    Ok((text, truncated))
}

/// Image dimensions from the header (no pixel decoding).
pub fn image_size(format: &str, h: &[u8]) -> Option<(u32, u32)> {
    let be32 = |o: usize| h.get(o..o + 4).map(|b| u32::from_be_bytes(b.try_into().unwrap()));
    let le16 = |o: usize| h.get(o..o + 2).map(|b| u16::from_le_bytes([b[0], b[1]]) as u32);
    match format {
        "PNG" => Some((be32(16)?, be32(20)?)),
        "GIF" => Some((le16(6)?, le16(8)?)),
        "BMP" => {
            let w = i32::from_le_bytes(h.get(18..22)?.try_into().ok()?);
            let hh = i32::from_le_bytes(h.get(22..26)?.try_into().ok()?);
            Some((w.unsigned_abs(), hh.unsigned_abs()))
        }
        "JPEG" => {
            // Walk markers to the first start-of-frame.
            let mut i = 2;
            while i + 9 < h.len() {
                if h[i] != 0xFF {
                    return None;
                }
                let m = h[i + 1];
                let len = u16::from_be_bytes([h[i + 2], h[i + 3]]) as usize;
                if matches!(m, 0xC0..=0xCF) && !matches!(m, 0xC4 | 0xC8 | 0xCC) {
                    return Some((u16::from_be_bytes([h[i + 7], h[i + 8]]) as u32, u16::from_be_bytes([h[i + 5], h[i + 6]]) as u32));
                }
                if len < 2 {
                    return None;
                }
                i += 2 + len;
            }
            None
        }
        "WebP" => match h.get(12..16)? {
            b"VP8X" => {
                let w = 1 + (h.get(24)?.to_owned() as u32 | (*h.get(25)? as u32) << 8 | (*h.get(26)? as u32) << 16);
                let hh = 1 + (h.get(27)?.to_owned() as u32 | (*h.get(28)? as u32) << 8 | (*h.get(29)? as u32) << 16);
                Some((w, hh))
            }
            b"VP8L" => {
                let b = h.get(21..25)?;
                let v = u32::from_le_bytes(b.try_into().ok()?);
                Some((1 + (v & 0x3FFF), 1 + ((v >> 14) & 0x3FFF)))
            }
            b"VP8 " => Some((le16(26)? & 0x3FFF, le16(28)? & 0x3FFF)),
            _ => None,
        },
        _ => None,
    }
}

/// Platform thumbnail provider (the shell on Windows; none elsewhere).
pub type ThumbFn = fn(&str, u32) -> Option<crate::Thumbnail>;

pub fn preview(path: &str, limits: &Limits, thumb: Option<ThumbFn>) -> Result<Preview, String> {
    let mut f = File::open(path).map_err(|e| e.to_string())?;
    if f.metadata().map_err(|e| e.to_string())?.is_dir() {
        return Err("is a folder".into());
    }
    let (head, size) = read_head(&mut f, 64 * 1024).map_err(|e| e.to_string())?;
    match sniff(path, &head) {
        Kind::Text | Kind::Html => {
            f.seek(SeekFrom::Start(0)).map_err(|e| e.to_string())?;
            let (bytes, size) = read_head(&mut f, limits.max_read).map_err(|e| e.to_string())?;
            let cut = (bytes.len() as u64) < size;
            let (text, enc) = decode(&bytes, cut);
            let (text, more) = if sniff(path, &head) == Kind::Html { markup_text(&text, limits.max_chars) } else { cap_chars(text, limits.max_chars) };
            Ok(Preview::Text { text, encoding: enc.into(), truncated: cut || more })
        }
        Kind::Office(k) => {
            let (text, truncated) = office_text(&mut f, k, limits)?;
            Ok(Preview::Text { text, encoding: format!("{} (text only)", k.label()), truncated })
        }
        Kind::Zip => {
            let (entries, total_entries, total_size, warnings) = zip::list(&mut f, limits)?;
            Ok(Preview::Archive { format: "ZIP".into(), entries, total_entries, total_size, warnings })
        }
        Kind::Image(fmt) => {
            let (width, height) = image_size(fmt, &head).unwrap_or((0, 0));
            let thumbnail = if limits.thumb > 0 { thumb.and_then(|t| t(path, limits.thumb)) } else { None };
            Ok(Preview::Image { format: fmt.into(), width, height, thumbnail })
        }
        Kind::Pdf => Ok(Preview::Unsupported { kind: "PDF document".into(), reason: "PDF preview is not available yet".into() }),
        Kind::Program => Ok(Preview::Unsupported { kind: "Program".into(), reason: "programs are never run or previewed".into() }),
        Kind::Binary => Ok(Preview::Hex { offset_bytes: head[..head.len().min(4096)].to_vec(), file_size: size }),
    }
}

pub fn extract(path: &str, limits: &Limits) -> Result<Extracted, String> {
    let mut f = File::open(path).map_err(|e| e.to_string())?;
    let (head, _) = read_head(&mut f, 64 * 1024).map_err(|e| e.to_string())?;
    match sniff(path, &head) {
        k @ (Kind::Text | Kind::Html) => {
            f.seek(SeekFrom::Start(0)).map_err(|e| e.to_string())?;
            let (bytes, size) = read_head(&mut f, limits.max_read).map_err(|e| e.to_string())?;
            let cut = (bytes.len() as u64) < size;
            let (text, _) = decode(&bytes, cut);
            let (text, more) = if k == Kind::Html { markup_text(&text, limits.max_chars) } else { cap_chars(text, limits.max_chars) };
            Ok(Extracted::Text { text, truncated: cut || more })
        }
        Kind::Office(k) => office_text(&mut f, k, limits).map(|(text, truncated)| Extracted::Text { text, truncated }),
        Kind::Pdf => Ok(Extracted::Unsupported("PDF text extraction is not available yet".into())),
        Kind::Zip => Ok(Extracted::Unsupported("archive contents are not indexed".into())),
        Kind::Image(_) => Ok(Extracted::Unsupported("image (no text)".into())),
        Kind::Program => Ok(Extracted::Unsupported("program".into())),
        Kind::Binary => Ok(Extracted::Unsupported("binary file".into())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::zip::build::{zip, Entry};

    fn tmp(name: &str, data: &[u8]) -> String {
        let d = std::env::temp_dir().join(format!("flint-preview-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        let p = d.join(name);
        std::fs::write(&p, data).unwrap();
        p.to_string_lossy().into_owned()
    }

    #[test]
    fn decodes_encodings_and_cut_utf8() {
        assert_eq!(decode("héllo".as_bytes(), false), ("héllo".into(), "UTF-8"));
        let cut = &"hé".as_bytes()[..2];
        assert_eq!(decode(cut, true), ("h".into(), "UTF-8"), "a split character is dropped, not mangled");
        let mut u16le = vec![0xFF, 0xFE];
        u16le.extend("ab€".encode_utf16().flat_map(|u| u.to_le_bytes()));
        assert_eq!(decode(&u16le, false), ("ab€".into(), "UTF-16 LE"));
        assert_eq!(decode(&[0x80, b'x', 0xE9], false), ("€xé".into(), "Windows-1252"));
    }

    #[test]
    fn sniffing_trusts_magic_over_extension() {
        assert_eq!(sniff("a.txt", b"\x89PNG\r\n\x1a\n...."), Kind::Image("PNG"));
        assert_eq!(sniff("readme.txt", b"MZ\x90\x00"), Kind::Program, "an exe renamed .txt is still a program");
        assert_eq!(sniff("x.jpg", b"plain words"), Kind::Text);
        assert_eq!(sniff("x.docx", b"PK\x03\x04"), Kind::Office(Office::Docx));
        assert_eq!(sniff("x.bin", b"\x00\x01\x02\x03"), Kind::Binary);
        assert_eq!(sniff("tool.exe", b"text?"), Kind::Program);
        assert_eq!(sniff("page.html", b"<html><body>hi"), Kind::Html);
    }

    #[test]
    fn markup_to_text() {
        let (t, _) = markup_text("<w:p><w:r><w:t>Hello</w:t></w:r><w:tab/><w:r><w:t>world &amp; co</w:t></w:r></w:p><w:p><w:t>Second</w:t></w:p>", 1000);
        assert_eq!(t, "Hello world & co\nSecond");
        let (t, _) = markup_text("<html><head><style>body{x}</style><script>alert('x')</script></head><body><p>A&#233;&#x41;</p><!-- c --><br>B</body></html>", 1000);
        assert_eq!(t, "AéA\n\nB", "paragraph end then <br>: a blank line");
        let (t, more) = markup_text("<p>abcdef</p>", 3);
        assert_eq!((t.as_str(), more), ("abc", true));
    }

    #[test]
    fn previews_and_extracts_each_kind() {
        let l = Limits::default();
        let p = tmp("notes.md", b"# Title\nline");
        assert_eq!(preview(&p, &l, None).unwrap(), Preview::Text { text: "# Title\nline".into(), encoding: "UTF-8".into(), truncated: false });
        let big = tmp("big.log", &b"0123456789".repeat(1000));
        let small = Limits { max_read: 100, ..Default::default() };
        assert!(matches!(preview(&big, &small, None).unwrap(), Preview::Text { truncated: true, ref text, .. } if text.len() == 100));
        let docx = tmp("cv.docx", &zip(&[Entry { name: "[Content_Types].xml", data: b"<x/>", deflate: false }, Entry { name: "word/document.xml", data: b"<w:document><w:body><w:p><w:t>Curriculum vitae</w:t></w:p></w:body></w:document>", deflate: true }]));
        assert_eq!(extract(&docx, &l).unwrap(), Extracted::Text { text: "Curriculum vitae".into(), truncated: false });
        assert!(matches!(preview(&docx, &l, None).unwrap(), Preview::Text { ref encoding, .. } if encoding.starts_with("Word document")));
        let pptx = tmp("deck.pptx", &zip(&[Entry { name: "ppt/slides/slide10.xml", data: b"<a:p><a:t>ten</a:t></a:p>", deflate: true }, Entry { name: "ppt/slides/slide2.xml", data: b"<a:p><a:t>two</a:t></a:p>", deflate: true }]));
        assert_eq!(extract(&pptx, &l).unwrap(), Extracted::Text { text: "two\n\nten".into(), truncated: false }, "slides in numeric order");
        let png = tmp("p.png", &[b"\x89PNG\r\n\x1a\n\x00\x00\x00\x0DIHDR".as_slice(), &640u32.to_be_bytes(), &480u32.to_be_bytes(), &[8, 6, 0, 0, 0]].concat());
        assert_eq!(preview(&png, &l, None).unwrap(), Preview::Image { format: "PNG".into(), width: 640, height: 480, thumbnail: None });
        let pdf = tmp("a.pdf", b"%PDF-1.7\n...");
        assert!(matches!(preview(&pdf, &l, None).unwrap(), Preview::Unsupported { .. }));
        assert!(matches!(extract(&pdf, &l).unwrap(), Extracted::Unsupported(ref r) if r.contains("PDF")));
        let exe = tmp("setup.exe", b"MZ\x90\x00\x03");
        assert!(matches!(preview(&exe, &l, None).unwrap(), Preview::Unsupported { ref reason, .. } if reason.contains("never run")));
        let bin = tmp("data.bin", &[0, 1, 2, 3, 255]);
        assert_eq!(preview(&bin, &l, None).unwrap(), Preview::Hex { offset_bytes: vec![0, 1, 2, 3, 255], file_size: 5 });
        let z = tmp("a.zip", &zip(&[Entry { name: "x.txt", data: b"x", deflate: false }]));
        assert!(matches!(preview(&z, &l, None).unwrap(), Preview::Archive { total_entries: 1, .. }));
        assert!(preview("/definitely/missing/file", &l, None).is_err());
    }

    #[test]
    fn office_bomb_is_refused_and_huge_parts_are_bounded() {
        let zeros = vec![b' '; 32 << 20];
        let bomb = tmp("bomb.docx", &zip(&[Entry { name: "word/document.xml", data: &zeros, deflate: true }]));
        let e = extract(&bomb, &Limits::default()).unwrap_err();
        assert!(e.contains("zip bomb"), "{e}");
        // A genuinely large (low-ratio) part inflates only up to the limit.
        let mut seed = 7u32;
        let noisy: Vec<u8> = (0..(2 << 20)).map(|_| {
            seed ^= seed << 13;
            seed ^= seed >> 17;
            seed ^= seed << 5;
            b'a' + (seed % 26) as u8
        }).collect();
        let doc = [b"<w:p><w:t>".as_slice(), &noisy, b"</w:t></w:p>"].concat();
        let big = tmp("big.docx", &zip(&[Entry { name: "word/document.xml", data: &doc, deflate: true }]));
        let l = Limits { max_inflate: 1 << 20, max_chars: 10_000, ..Default::default() };
        match extract(&big, &l).unwrap() {
            Extracted::Text { text, truncated } => assert!(truncated && text.chars().count() <= 10_000),
            o => panic!("{o:?}"),
        }
    }

    #[test]
    fn malformed_inputs_never_panic() {
        let l = Limits::default();
        let mut seed = 99u32;
        let mut rnd = || {
            seed ^= seed << 13;
            seed ^= seed >> 17;
            seed ^= seed << 5;
            seed
        };
        let seeds: [&[u8]; 6] = [b"\x89PNG\r\n\x1a\n", b"\xFF\xD8\xFF", b"GIF89a", b"RIFF\0\0\0\0WEBPVP8X", b"PK\x03\x04", b"<html><p>"];
        for (k, s) in seeds.iter().enumerate() {
            for i in 0..200 {
                let mut data = s.to_vec();
                let n = (rnd() % 600) as usize;
                data.extend((0..n).map(|_| rnd() as u8));
                for name in ["f.png", "f.docx", "f.html", "f.bin"] {
                    let p = tmp(&format!("fuzz-{k}-{i}-{name}"), &data);
                    let _ = preview(&p, &l, None);
                    let _ = extract(&p, &l);
                    let _ = std::fs::remove_file(&p);
                }
            }
        }
    }
}
