//! Isolated, bounded previews and text extraction (ADR-016).
//!
//! Every parser for file contents runs in a separate helper process,
//! `flint-preview-host`, which the launcher talks to over pipes
//! ([`client`]). A malformed or hostile file can therefore crash or hang
//! only the helper: the client kills it on timeout, reports the failure for
//! that one file, and restarts it with backoff. On Windows the helper also
//! runs in a Job Object with a memory limit. Process isolation limits the
//! damage of a parser bug; it is **not** a security sandbox, and nothing
//! claims it is.
//!
//! Limits apply inside the helper as well ([`Limits`]): bytes read per file,
//! output size, archive entry counts, decompressed sizes and compression
//! ratios. Nothing is ever executed, and archives are listed, never
//! extracted to disk.
//!
//! [`scheduler`] tags requests with the selection generation, keeps only
//! the latest, and caches results keyed by file identity. With the viewer
//! disabled, no scheduler and no helper exist.

pub mod backends;
pub mod client;
pub mod host;
pub mod scheduler;
#[cfg(windows)]
pub mod win;
pub mod wire;
pub mod zip;

use wire::{Reader, WireError, Writer};

/// Per-request bounds, enforced inside the helper.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Limits {
    /// Bytes read from the file.
    pub max_read: u64,
    /// Characters of text returned.
    pub max_chars: usize,
    /// Archive entries listed.
    pub max_entries: usize,
    /// Bytes inflated from one archive member (e.g. an Office XML part).
    pub max_inflate: usize,
    /// Declared compression ratio above which members are not inflated.
    pub max_ratio: u64,
    /// Thumbnail edge in pixels (0 = no thumbnail).
    pub thumb: u32,
}

impl Default for Limits {
    fn default() -> Self {
        Limits { max_read: 4 << 20, max_chars: 64 * 1024, max_entries: 1000, max_inflate: 16 << 20, max_ratio: 200, thumb: 256 }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Op {
    /// Something to show in the viewer.
    Preview,
    /// Plain text for the content index.
    Extract,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Request {
    pub id: u64,
    pub op: Op,
    pub path: String,
    pub limits: Limits,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ArchiveEntry {
    pub name: String,
    pub size: u64,
    pub compressed: u64,
    pub is_dir: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Preview {
    Text { text: String, encoding: String, truncated: bool },
    Hex { offset_bytes: Vec<u8>, file_size: u64 },
    Image { format: String, width: u32, height: u32, thumbnail: Option<Thumbnail> },
    Archive { format: String, entries: Vec<ArchiveEntry>, total_entries: u64, total_size: u64, warnings: Vec<String> },
    /// The format is recognised but not previewed (yet); `reason` says why.
    Unsupported { kind: String, reason: String },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Thumbnail {
    pub width: u32,
    pub height: u32,
    /// Top-down 32-bit BGRA rows.
    pub bgra: Vec<u8>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Extracted {
    Text { text: String, truncated: bool },
    /// Not extractable; recorded as such in the content index.
    Unsupported(String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Outcome {
    Preview(Preview),
    Extracted(Extracted),
    /// The file could not be read or parsed.
    Failed(String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Response {
    pub id: u64,
    pub outcome: Outcome,
}

impl Request {
    pub fn encode(&self) -> Vec<u8> {
        let mut w = Writer::default();
        w.u64(self.id).u8(match self.op {
            Op::Preview => 0,
            Op::Extract => 1,
        });
        w.str(&self.path);
        let l = &self.limits;
        w.u64(l.max_read).u64(l.max_chars as u64).u64(l.max_entries as u64).u64(l.max_inflate as u64).u64(l.max_ratio).u32(l.thumb);
        w.buf
    }
    pub fn decode(b: &[u8]) -> Result<Self, WireError> {
        let mut r = Reader::new(b);
        let id = r.u64()?;
        let op = match r.u8()? {
            0 => Op::Preview,
            1 => Op::Extract,
            _ => return Err(WireError("bad op")),
        };
        let path = r.str()?;
        let limits = Limits { max_read: r.u64()?, max_chars: r.u64()? as usize, max_entries: r.u64()? as usize, max_inflate: r.u64()? as usize, max_ratio: r.u64()?, thumb: r.u32()? };
        r.finish()?;
        Ok(Request { id, op, path, limits })
    }
}

impl Response {
    pub fn encode(&self) -> Vec<u8> {
        let mut w = Writer::default();
        w.u64(self.id);
        match &self.outcome {
            Outcome::Failed(e) => {
                w.u8(0).str(e);
            }
            Outcome::Extracted(Extracted::Text { text, truncated }) => {
                w.u8(1).str(text).bool(*truncated);
            }
            Outcome::Extracted(Extracted::Unsupported(r)) => {
                w.u8(2).str(r);
            }
            Outcome::Preview(p) => match p {
                Preview::Text { text, encoding, truncated } => {
                    w.u8(3).str(text).str(encoding).bool(*truncated);
                }
                Preview::Hex { offset_bytes, file_size } => {
                    w.u8(4).bytes(offset_bytes).u64(*file_size);
                }
                Preview::Image { format, width, height, thumbnail } => {
                    w.u8(5).str(format).u32(*width).u32(*height);
                    match thumbnail {
                        Some(t) => w.bool(true).u32(t.width).u32(t.height).bytes(&t.bgra),
                        None => w.bool(false),
                    };
                }
                Preview::Archive { format, entries, total_entries, total_size, warnings } => {
                    w.u8(6).str(format).u32(entries.len() as u32);
                    for e in entries {
                        w.str(&e.name).u64(e.size).u64(e.compressed).bool(e.is_dir);
                    }
                    w.u64(*total_entries).u64(*total_size).u32(warnings.len() as u32);
                    for x in warnings {
                        w.str(x);
                    }
                }
                Preview::Unsupported { kind, reason } => {
                    w.u8(7).str(kind).str(reason);
                }
            },
        }
        w.buf
    }
    pub fn decode(b: &[u8]) -> Result<Self, WireError> {
        let mut r = Reader::new(b);
        let id = r.u64()?;
        let outcome = match r.u8()? {
            0 => Outcome::Failed(r.str()?),
            1 => Outcome::Extracted(Extracted::Text { text: r.str()?, truncated: r.bool()? }),
            2 => Outcome::Extracted(Extracted::Unsupported(r.str()?)),
            3 => Outcome::Preview(Preview::Text { text: r.str()?, encoding: r.str()?, truncated: r.bool()? }),
            4 => Outcome::Preview(Preview::Hex { offset_bytes: r.bytes()?.to_vec(), file_size: r.u64()? }),
            5 => {
                let (format, width, height) = (r.str()?, r.u32()?, r.u32()?);
                let thumbnail = if r.bool()? {
                    let (tw, th) = (r.u32()?, r.u32()?);
                    let bgra = r.bytes()?.to_vec();
                    if (tw as u64) * (th as u64) * 4 != bgra.len() as u64 {
                        return Err(WireError("thumbnail size mismatch"));
                    }
                    Some(Thumbnail { width: tw, height: th, bgra })
                } else {
                    None
                };
                Outcome::Preview(Preview::Image { format, width, height, thumbnail })
            }
            6 => {
                let format = r.str()?;
                let n = r.count(4 + 8 + 8 + 1)?;
                let mut entries = Vec::with_capacity(n);
                for _ in 0..n {
                    entries.push(ArchiveEntry { name: r.str()?, size: r.u64()?, compressed: r.u64()?, is_dir: r.bool()? });
                }
                let (total_entries, total_size) = (r.u64()?, r.u64()?);
                let n = r.count(4)?;
                let mut warnings = Vec::with_capacity(n);
                for _ in 0..n {
                    warnings.push(r.str()?);
                }
                Outcome::Preview(Preview::Archive { format, entries, total_entries, total_size, warnings })
            }
            7 => Outcome::Preview(Preview::Unsupported { kind: r.str()?, reason: r.str()? }),
            _ => return Err(WireError("bad outcome")),
        };
        r.finish()?;
        Ok(Response { id, outcome })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn messages_round_trip() {
        let req = Request { id: 9, op: Op::Extract, path: "C:\\a\\b.docx".into(), limits: Limits::default() };
        assert_eq!(Request::decode(&req.encode()).unwrap(), req);
        let outs = [
            Outcome::Failed("boom".into()),
            Outcome::Extracted(Extracted::Text { text: "hi".into(), truncated: true }),
            Outcome::Extracted(Extracted::Unsupported("pdf".into())),
            Outcome::Preview(Preview::Text { text: "x".into(), encoding: "UTF-8".into(), truncated: false }),
            Outcome::Preview(Preview::Hex { offset_bytes: vec![0, 1, 2], file_size: 3 }),
            Outcome::Preview(Preview::Image { format: "PNG".into(), width: 2, height: 1, thumbnail: Some(Thumbnail { width: 2, height: 1, bgra: vec![0; 8] }) }),
            Outcome::Preview(Preview::Archive { format: "ZIP".into(), entries: vec![ArchiveEntry { name: "a".into(), size: 1, compressed: 1, is_dir: false }], total_entries: 1, total_size: 1, warnings: vec!["w".into()] }),
            Outcome::Preview(Preview::Unsupported { kind: "PDF".into(), reason: "later".into() }),
        ];
        for o in outs {
            let r = Response { id: 3, outcome: o };
            assert_eq!(Response::decode(&r.encode()).unwrap(), r);
        }
        // Truncations of a valid message are errors, never panics.
        let full = Response { id: 1, outcome: Outcome::Preview(Preview::Archive { format: "ZIP".into(), entries: vec![], total_entries: 0, total_size: 0, warnings: vec![] }) }.encode();
        for n in 0..full.len() {
            assert!(Response::decode(&full[..n]).is_err());
        }
    }
}
