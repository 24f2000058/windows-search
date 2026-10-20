//! Defensive ZIP reader: central-directory listing and bounded inflation of
//! single members (for Office/ODF/EPUB text). Archives are never extracted
//! to disk and nested archives are never opened.
//!
//! Zip-bomb defences:
//! * listing reads only the central directory (capped) and never inflates
//! * a member is inflated only if its declared ratio is under the limit, and
//!   inflation stops at `max_inflate` bytes whatever the header claims
//! * overlapping entries (the "quoted overlap" bomb construction), huge
//!   declared totals and nested archives are reported as warnings

use crate::{ArchiveEntry, Limits};
use std::io::{Read, Seek, SeekFrom};

const EOCD_SIG: u32 = 0x0605_4b50;
const CDH_SIG: u32 = 0x0201_4b50;
const LFH_SIG: u32 = 0x0403_4b50;
const MAX_CD_BYTES: u64 = 8 << 20;
const BOMB_TOTAL: u64 = 4 << 30;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Member {
    pub name: String,
    pub method: u16,
    pub compressed: u64,
    pub size: u64,
    pub local_offset: u64,
    pub encrypted: bool,
}

#[derive(Debug, Default)]
pub struct Directory {
    pub members: Vec<Member>,
    /// Entry count declared by the end-of-central-directory record.
    pub declared_entries: u64,
    pub warnings: Vec<String>,
}

fn le16(b: &[u8], o: usize) -> u16 {
    u16::from_le_bytes([b[o], b[o + 1]])
}
fn le32(b: &[u8], o: usize) -> u32 {
    u32::from_le_bytes(b[o..o + 4].try_into().unwrap())
}
fn le64(b: &[u8], o: usize) -> u64 {
    u64::from_le_bytes(b[o..o + 8].try_into().unwrap())
}

fn err(s: &str) -> String {
    format!("not a valid ZIP archive: {s}")
}

/// Reads the central directory (at most `max_members` parsed).
pub fn read_directory<R: Read + Seek>(f: &mut R, max_members: usize) -> Result<Directory, String> {
    let len = f.seek(SeekFrom::End(0)).map_err(|e| e.to_string())?;
    if len < 22 {
        return Err(err("too short"));
    }
    // EOCD is in the last 22 + 65535 (comment) bytes.
    let tail_len = len.min(22 + 65_535);
    f.seek(SeekFrom::Start(len - tail_len)).map_err(|e| e.to_string())?;
    let mut tail = vec![0u8; tail_len as usize];
    f.read_exact(&mut tail).map_err(|e| e.to_string())?;
    let pos = (0..=tail.len() - 22).rev().find(|&i| le32(&tail, i) == EOCD_SIG).ok_or_else(|| err("no end-of-directory record"))?;
    let e = &tail[pos..];
    let mut declared = le16(e, 10) as u64;
    let mut cd_size = le32(e, 12) as u64;
    let mut cd_off = le32(e, 16) as u64;
    let mut d = Directory::default();
    if declared == 0xFFFF || cd_size == 0xFFFF_FFFF || cd_off == 0xFFFF_FFFF {
        // ZIP64: the locator sits just before the EOCD.
        let loc = (len - tail_len) + pos as u64;
        if loc < 20 {
            return Err(err("bad ZIP64 locator"));
        }
        let mut l = [0u8; 20];
        f.seek(SeekFrom::Start(loc - 20)).map_err(|e| e.to_string())?;
        f.read_exact(&mut l).map_err(|e| e.to_string())?;
        if le32(&l, 0) != 0x0706_4b50 {
            return Err(err("missing ZIP64 locator"));
        }
        let rec = le64(&l, 8);
        let mut r = [0u8; 56];
        f.seek(SeekFrom::Start(rec)).map_err(|e| e.to_string())?;
        f.read_exact(&mut r).map_err(|_| err("truncated ZIP64 record"))?;
        if le32(&r, 0) != 0x0606_4b50 {
            return Err(err("bad ZIP64 record"));
        }
        declared = le64(&r, 32);
        cd_size = le64(&r, 40);
        cd_off = le64(&r, 48);
    }
    d.declared_entries = declared;
    if cd_off.checked_add(cd_size).is_none_or(|end| end > len) {
        return Err(err("directory outside the file"));
    }
    if cd_size > MAX_CD_BYTES {
        d.warnings.push(format!("directory is {} MB; only the first {} MB were read", cd_size >> 20, MAX_CD_BYTES >> 20));
    }
    let read = cd_size.min(MAX_CD_BYTES) as usize;
    let mut cd = vec![0u8; read];
    f.seek(SeekFrom::Start(cd_off)).map_err(|e| e.to_string())?;
    f.read_exact(&mut cd).map_err(|e| e.to_string())?;
    let mut i = 0usize;
    while i + 46 <= cd.len() && d.members.len() < max_members {
        if le32(&cd, i) != CDH_SIG {
            d.warnings.push("directory ends early (corrupt entry)".into());
            break;
        }
        let flags = le16(&cd, i + 8);
        let method = le16(&cd, i + 10);
        let mut compressed = le32(&cd, i + 20) as u64;
        let mut size = le32(&cd, i + 24) as u64;
        let (nl, xl, cl) = (le16(&cd, i + 28) as usize, le16(&cd, i + 30) as usize, le16(&cd, i + 32) as usize);
        let mut local_offset = le32(&cd, i + 42) as u64;
        let end = i + 46 + nl + xl + cl;
        if end > cd.len() {
            d.warnings.push("directory entry runs past the directory".into());
            break;
        }
        let name = String::from_utf8_lossy(&cd[i + 46..i + 46 + nl]).into_owned();
        // ZIP64 extra field (0x0001) carries the real values in this order.
        let mut x = i + 46 + nl;
        let xend = x + xl;
        while x + 4 <= xend {
            let (tag, sz) = (le16(&cd, x), le16(&cd, x + 2) as usize);
            let body = x + 4;
            if body + sz > xend {
                break;
            }
            if tag == 1 {
                let mut p = body;
                for v in [&mut size, &mut compressed, &mut local_offset] {
                    if *v == 0xFFFF_FFFF && p + 8 <= body + sz {
                        *v = le64(&cd, p);
                        p += 8;
                    }
                }
            }
            x = body + sz;
        }
        d.members.push(Member { name, method, compressed, size, local_offset, encrypted: flags & 1 != 0 });
        i = end;
    }
    Ok(d)
}

/// Listing with bomb heuristics, for previews.
pub fn list<R: Read + Seek>(f: &mut R, limits: &Limits) -> Result<(Vec<ArchiveEntry>, u64, u64, Vec<String>), String> {
    // Parse more than we list so the heuristics see a larger sample.
    let d = read_directory(f, limits.max_entries.saturating_mul(4).max(1))?;
    let mut warnings = d.warnings.clone();
    let total: u64 = d.members.iter().map(|m| m.size).fold(0u64, |a, b| a.saturating_add(b));
    let packed: u64 = d.members.iter().map(|m| m.compressed).fold(0u64, |a, b| a.saturating_add(b));
    if total > BOMB_TOTAL || (packed > 0 && total / packed.max(1) > limits.max_ratio) {
        warnings.push(format!("expands to {} from {} (ratio {}:1): possible zip bomb; nothing is extracted", human(total), human(packed), total / packed.max(1)));
    }
    let mut spans: Vec<(u64, u64)> = d.members.iter().filter(|m| m.compressed > 0).map(|m| (m.local_offset, m.local_offset.saturating_add(30).saturating_add(m.compressed))).collect();
    spans.sort_unstable();
    if spans.windows(2).any(|w| w[1].0 < w[0].1) {
        warnings.push("entries overlap (a zip-bomb construction); nothing is extracted".into());
    }
    let nested = d.members.iter().filter(|m| is_archive_name(&m.name)).count();
    if nested > 0 {
        warnings.push(format!("{nested} nested archive(s), not opened"));
    }
    if d.members.iter().any(|m| m.encrypted) {
        warnings.push("contains encrypted entries".into());
    }
    if (d.members.len() as u64) < d.declared_entries && d.members.len() >= limits.max_entries {
        // Expected: listing is capped.
    } else if (d.members.len() as u64) != d.declared_entries {
        warnings.push(format!("directory lists {} entries but declares {}", d.members.len(), d.declared_entries));
    }
    let entries = d.members.iter().take(limits.max_entries).map(|m| ArchiveEntry { name: m.name.clone(), size: m.size, compressed: m.compressed, is_dir: m.name.ends_with('/') }).collect();
    Ok((entries, d.declared_entries, total, warnings))
}

fn human(n: u64) -> String {
    match n {
        n if n >= 1 << 30 => format!("{:.1} GB", n as f64 / (1u64 << 30) as f64),
        n if n >= 1 << 20 => format!("{:.1} MB", n as f64 / (1u64 << 20) as f64),
        n if n >= 1 << 10 => format!("{} KB", n.div_ceil(1024)),
        n => format!("{n} B"),
    }
}

pub fn is_archive_name(n: &str) -> bool {
    let l = n.to_ascii_lowercase();
    [".zip", ".7z", ".rar", ".gz", ".tgz", ".tar", ".xz", ".bz2", ".cab", ".iso", ".jar", ".zst"].iter().any(|e| l.ends_with(e))
}

/// Inflates one member, bounded by `limits.max_inflate` and `max_read`.
/// Returns (bytes, truncated).
pub fn read_member<R: Read + Seek>(f: &mut R, m: &Member, limits: &Limits) -> Result<(Vec<u8>, bool), String> {
    if m.encrypted {
        return Err("member is encrypted".into());
    }
    if m.compressed > 0 && m.size / m.compressed.max(1) > limits.max_ratio {
        return Err(format!("member {} declares a {}:1 compression ratio (limit {}:1): possible zip bomb", m.name, m.size / m.compressed, limits.max_ratio));
    }
    if m.compressed > limits.max_read {
        return Err(format!("member {} is {} MB compressed (limit {} MB)", m.name, m.compressed >> 20, limits.max_read >> 20));
    }
    let mut h = [0u8; 30];
    f.seek(SeekFrom::Start(m.local_offset)).map_err(|e| e.to_string())?;
    f.read_exact(&mut h).map_err(|_| err("truncated local header"))?;
    if le32(&h, 0) != LFH_SIG {
        return Err(err("bad local header"));
    }
    let skip = le16(&h, 26) as i64 + le16(&h, 28) as i64;
    f.seek(SeekFrom::Current(skip)).map_err(|e| e.to_string())?;
    let mut data = vec![0u8; m.compressed as usize];
    f.read_exact(&mut data).map_err(|_| err("member data truncated"))?;
    match m.method {
        0 => {
            let truncated = data.len() > limits.max_inflate;
            data.truncate(limits.max_inflate);
            Ok((data, truncated))
        }
        8 => match miniz_oxide::inflate::decompress_to_vec_with_limit(&data, limits.max_inflate) {
            Ok(v) => Ok((v, false)),
            Err(e) if e.status == miniz_oxide::inflate::TINFLStatus::HasMoreOutput => Ok((e.output, true)),
            Err(e) => Err(format!("corrupt compressed data ({:?})", e.status)),
        },
        other => Err(format!("compression method {other} is not supported")),
    }
}

/// Test helper: builds a ZIP in memory (stored or deflated members).
#[cfg(test)]
pub(crate) mod build {
    pub struct Entry<'a> {
        pub name: &'a str,
        pub data: &'a [u8],
        pub deflate: bool,
    }

    pub fn zip(entries: &[Entry]) -> Vec<u8> {
        let mut out = Vec::new();
        let mut cd = Vec::new();
        for e in entries {
            let (method, body) = if e.deflate { (8u16, miniz_oxide::deflate::compress_to_vec(e.data, 6)) } else { (0u16, e.data.to_vec()) };
            let off = out.len() as u32;
            let hdr = |sig: u32, central: bool, v: &mut Vec<u8>| {
                v.extend(sig.to_le_bytes());
                if central {
                    v.extend(20u16.to_le_bytes());
                }
                v.extend(20u16.to_le_bytes());
                v.extend(0u16.to_le_bytes()); // flags
                v.extend(method.to_le_bytes());
                v.extend([0u8; 4]); // time/date
                v.extend(0u32.to_le_bytes()); // crc (not checked)
                v.extend((body.len() as u32).to_le_bytes());
                v.extend((e.data.len() as u32).to_le_bytes());
                v.extend((e.name.len() as u16).to_le_bytes());
                v.extend(0u16.to_le_bytes());
                if central {
                    v.extend([0u8; 6]); // comment len, disk, internal attrs
                    v.extend(0u32.to_le_bytes()); // external attrs
                    v.extend(off.to_le_bytes());
                }
                v.extend(e.name.as_bytes());
            };
            hdr(0x0403_4b50, false, &mut out);
            out.extend(&body);
            hdr(0x0201_4b50, true, &mut cd);
        }
        let cd_off = out.len() as u32;
        out.extend(&cd);
        out.extend(0x0605_4b50u32.to_le_bytes());
        out.extend([0u8; 4]);
        out.extend((entries.len() as u16).to_le_bytes());
        out.extend((entries.len() as u16).to_le_bytes());
        out.extend((cd.len() as u32).to_le_bytes());
        out.extend(cd_off.to_le_bytes());
        out.extend(0u16.to_le_bytes());
        out
    }
}

#[cfg(test)]
mod tests {
    use super::build::{zip, Entry};
    use super::*;
    use std::io::Cursor;

    #[test]
    fn lists_and_reads_members() {
        let z = zip(&[Entry { name: "a.txt", data: b"hello", deflate: false }, Entry { name: "dir/", data: b"", deflate: false }, Entry { name: "b.xml", data: &b"<x>words</x>".repeat(50), deflate: true }]);
        let (entries, n, total, warnings) = list(&mut Cursor::new(&z), &Limits::default()).unwrap();
        assert_eq!(n, 3);
        assert_eq!(entries.iter().map(|e| e.name.as_str()).collect::<Vec<_>>(), ["a.txt", "dir/", "b.xml"]);
        assert!(entries[1].is_dir);
        assert_eq!(total, 5 + 600);
        assert!(warnings.is_empty(), "{warnings:?}");
        let d = read_directory(&mut Cursor::new(&z), 10).unwrap();
        assert_eq!(read_member(&mut Cursor::new(&z), &d.members[0], &Limits::default()).unwrap(), (b"hello".to_vec(), false));
        let (xml, trunc) = read_member(&mut Cursor::new(&z), &d.members[2], &Limits::default()).unwrap();
        assert_eq!((xml.len(), trunc), (600, false));
        let small = Limits { max_inflate: 100, ..Default::default() };
        let (xml, trunc) = read_member(&mut Cursor::new(&z), &d.members[2], &small).unwrap();
        assert_eq!((xml.len(), trunc), (100, true), "inflation stops at the limit");
    }

    #[test]
    fn bombs_are_detected_and_never_inflated() {
        // 64 MiB of zeros deflates to ~64 KiB: a 1000:1 ratio.
        let zeros = vec![0u8; 64 << 20];
        let z = zip(&[Entry { name: "big.bin", data: &zeros, deflate: true }, Entry { name: "inner.zip", data: b"PK", deflate: false }]);
        let (_, _, total, warnings) = list(&mut Cursor::new(&z), &Limits::default()).unwrap();
        assert_eq!(total, (64 << 20) + 2);
        assert!(warnings.iter().any(|w| w.contains("possible zip bomb")), "{warnings:?}");
        assert!(warnings.iter().any(|w| w.contains("nested archive")));
        let d = read_directory(&mut Cursor::new(&z), 10).unwrap();
        let e = read_member(&mut Cursor::new(&z), &d.members[0], &Limits::default()).unwrap_err();
        assert!(e.contains("compression ratio"), "{e}");

        // Overlapping entries: two directory entries pointing at the same data.
        let mut z = zip(&[Entry { name: "a", data: b"aaaaaaaaaa", deflate: false }, Entry { name: "b", data: b"bbbbbbbbbb", deflate: false }]);
        let d = read_directory(&mut Cursor::new(&z), 10).unwrap();
        let second_cd = z.windows(4).rposition(|w| w == 0x0201_4b50u32.to_le_bytes()).unwrap();
        z[second_cd + 42..second_cd + 46].copy_from_slice(&(d.members[0].local_offset as u32).to_le_bytes());
        let (_, _, _, warnings) = list(&mut Cursor::new(&z), &Limits::default()).unwrap();
        assert!(warnings.iter().any(|w| w.contains("overlap")), "{warnings:?}");
    }

    #[test]
    fn malformed_archives_fail_cleanly() {
        let good = zip(&[Entry { name: "a.txt", data: &b"x".repeat(1000), deflate: true }]);
        // Every truncation and many single-byte corruptions: errors or
        // warnings, never a panic or an unbounded allocation.
        for n in 0..good.len() {
            let _ = list(&mut Cursor::new(&good[..n]), &Limits::default());
        }
        let mut seed = 0x1234_5678u32;
        for _ in 0..3000 {
            seed ^= seed << 13;
            seed ^= seed >> 17;
            seed ^= seed << 5;
            let mut z = good.clone();
            let i = seed as usize % z.len();
            z[i] = (seed >> 8) as u8;
            if let Ok(d) = read_directory(&mut Cursor::new(&z), 10) {
                for m in &d.members {
                    let _ = read_member(&mut Cursor::new(&z), m, &Limits::default());
                }
            }
        }
        assert!(list(&mut Cursor::new(b"PK\x05\x06garbage".to_vec()), &Limits::default()).is_err());
        assert!(list(&mut Cursor::new(Vec::new()), &Limits::default()).is_err());
    }
}
