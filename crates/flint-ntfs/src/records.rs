//! Parsing of `USN_RECORD_V2` / `USN_RECORD_V3` buffers as returned by
//! `FSCTL_ENUM_USN_DATA` and `FSCTL_READ_USN_JOURNAL`.
//!
//! Platform-independent and bounds-checked: a malformed or truncated buffer
//! yields an error for that buffer, never a panic or out-of-bounds read.

use flint_index::model::{Frn, UsnRecord};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ParseError {
    Truncated { offset: usize },
    BadLength { offset: usize, len: u32 },
    BadName { offset: usize },
    UnsupportedVersion { offset: usize, major: u16 },
    /// V3 record with a 128-bit file ID that does not fit an NTFS FRN (ReFS).
    WideFileId { offset: usize },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Parsed {
    pub record: UsnRecord,
    /// FILETIME of the change, converted to unix seconds.
    pub timestamp: i64,
    /// The UTF-16 name contained unpaired surrogates and was decoded
    /// lossily; operations must address the file by ID, not by this name.
    pub lossy_name: bool,
}

fn u16_at(b: &[u8], o: usize) -> Option<u16> {
    b.get(o..o + 2).map(|s| u16::from_le_bytes([s[0], s[1]]))
}
fn u32_at(b: &[u8], o: usize) -> Option<u32> {
    b.get(o..o + 4).map(|s| u32::from_le_bytes(s.try_into().unwrap()))
}
fn u64_at(b: &[u8], o: usize) -> Option<u64> {
    b.get(o..o + 8).map(|s| u64::from_le_bytes(s.try_into().unwrap()))
}

/// FILETIME (100 ns since 1601) to unix seconds.
pub fn filetime_to_unix(ft: i64) -> i64 {
    ft.div_euclid(10_000_000) - 11_644_473_600
}

/// Parses one record at the start of `b` (the slice may extend beyond it).
pub fn parse_record(b: &[u8], offset: usize) -> Result<(Parsed, usize), ParseError> {
    let trunc = ParseError::Truncated { offset };
    let len = u32_at(b, 0).ok_or(trunc.clone())?;
    if len < 60 || len as usize > b.len() || len % 8 != 0 {
        return Err(ParseError::BadLength { offset, len });
    }
    let rec = &b[..len as usize];
    let major = u16_at(rec, 4).ok_or(trunc.clone())?;
    let (frn, parent, usn, ts, reason, attrs, name_len, name_off): (Frn, Frn, i64, i64, u32, u32, u16, u16) = match major {
        2 => (
            u64_at(rec, 8).ok_or(trunc.clone())?,
            u64_at(rec, 16).ok_or(trunc.clone())?,
            u64_at(rec, 24).ok_or(trunc.clone())? as i64,
            u64_at(rec, 32).ok_or(trunc.clone())? as i64,
            u32_at(rec, 40).ok_or(trunc.clone())?,
            u32_at(rec, 52).ok_or(trunc.clone())?,
            u16_at(rec, 56).ok_or(trunc.clone())?,
            u16_at(rec, 58).ok_or(trunc.clone())?,
        ),
        3 => {
            if len < 76 {
                return Err(ParseError::BadLength { offset, len });
            }
            let (f_lo, f_hi) = (u64_at(rec, 8).ok_or(trunc.clone())?, u64_at(rec, 16).ok_or(trunc.clone())?);
            let (p_lo, p_hi) = (u64_at(rec, 24).ok_or(trunc.clone())?, u64_at(rec, 32).ok_or(trunc.clone())?);
            if f_hi != 0 || p_hi != 0 {
                return Err(ParseError::WideFileId { offset });
            }
            (
                f_lo,
                p_lo,
                u64_at(rec, 40).ok_or(trunc.clone())? as i64,
                u64_at(rec, 48).ok_or(trunc.clone())? as i64,
                u32_at(rec, 56).ok_or(trunc.clone())?,
                u32_at(rec, 68).ok_or(trunc.clone())?,
                u16_at(rec, 72).ok_or(trunc.clone())?,
                u16_at(rec, 74).ok_or(trunc.clone())?,
            )
        }
        m => return Err(ParseError::UnsupportedVersion { offset, major: m }),
    };
    let (no, nl) = (name_off as usize, name_len as usize);
    if nl % 2 != 0 || no + nl > rec.len() || no < if major == 2 { 60 } else { 76 } {
        return Err(ParseError::BadName { offset });
    }
    let units: Vec<u16> = rec[no..no + nl].chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect();
    let (name, lossy_name) = match String::from_utf16(&units) {
        Ok(s) => (s, false),
        Err(_) => (String::from_utf16_lossy(&units), true),
    };
    Ok((Parsed { record: UsnRecord { usn, frn, parent, reason, attrs, name }, timestamp: filetime_to_unix(ts), lossy_name }, len as usize))
}

/// Parses an output buffer: an 8-byte header (next USN or next start FRN)
/// followed by packed records. Returns the header value and the records;
/// stops at the first malformed record and reports it.
pub fn parse_buffer(buf: &[u8]) -> (Option<u64>, Vec<Parsed>, Option<ParseError>) {
    let Some(header) = u64_at(buf, 0) else { return (None, Vec::new(), Some(ParseError::Truncated { offset: 0 })) };
    let mut out = Vec::new();
    let mut off = 8;
    while off < buf.len() {
        match parse_record(&buf[off..], off) {
            Ok((p, len)) => {
                out.push(p);
                off += len;
            }
            Err(e) => return (Some(header), out, Some(e)),
        }
    }
    (Some(header), out, None)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// Builds a V2 record exactly as the kernel lays it out.
    pub fn v2(frn: u64, parent: u64, usn: i64, reason: u32, attrs: u32, name: &str) -> Vec<u8> {
        let units: Vec<u16> = name.encode_utf16().collect();
        let nl = units.len() * 2;
        let len = (60 + nl).div_ceil(8) * 8;
        let mut b = vec![0u8; len];
        b[0..4].copy_from_slice(&(len as u32).to_le_bytes());
        b[4..6].copy_from_slice(&2u16.to_le_bytes());
        b[8..16].copy_from_slice(&frn.to_le_bytes());
        b[16..24].copy_from_slice(&parent.to_le_bytes());
        b[24..32].copy_from_slice(&usn.to_le_bytes());
        b[32..40].copy_from_slice(&(132_000_000_000_000_000i64).to_le_bytes());
        b[40..44].copy_from_slice(&reason.to_le_bytes());
        b[52..56].copy_from_slice(&attrs.to_le_bytes());
        b[56..58].copy_from_slice(&(nl as u16).to_le_bytes());
        b[58..60].copy_from_slice(&60u16.to_le_bytes());
        for (i, u) in units.iter().enumerate() {
            b[60 + 2 * i..62 + 2 * i].copy_from_slice(&u.to_le_bytes());
        }
        b
    }

    fn v3(frn: u64, frn_hi: u64, parent: u64, usn: i64, name: &str) -> Vec<u8> {
        let units: Vec<u16> = name.encode_utf16().collect();
        let nl = units.len() * 2;
        let len = (76 + nl).div_ceil(8) * 8;
        let mut b = vec![0u8; len];
        b[0..4].copy_from_slice(&(len as u32).to_le_bytes());
        b[4..6].copy_from_slice(&3u16.to_le_bytes());
        b[8..16].copy_from_slice(&frn.to_le_bytes());
        b[16..24].copy_from_slice(&frn_hi.to_le_bytes());
        b[24..32].copy_from_slice(&parent.to_le_bytes());
        b[40..48].copy_from_slice(&usn.to_le_bytes());
        b[56..60].copy_from_slice(&0x100u32.to_le_bytes());
        b[72..74].copy_from_slice(&(nl as u16).to_le_bytes());
        b[74..76].copy_from_slice(&76u16.to_le_bytes());
        for (i, u) in units.iter().enumerate() {
            b[76 + 2 * i..78 + 2 * i].copy_from_slice(&u.to_le_bytes());
        }
        b
    }

    #[test]
    fn parses_v2_and_v3_buffers() {
        let mut buf = 4242u64.to_le_bytes().to_vec();
        buf.extend(v2(0x0001_0000_0000_0064, 0x0005_0000_0000_0005, 100, 0x8000_0100, 0x20, "Résumé 日本.docx"));
        buf.extend(v3(0x0002_0000_0000_0065, 0, 0x0005_0000_0000_0005, 200, "x"));
        let (h, recs, err) = parse_buffer(&buf);
        assert_eq!(h, Some(4242));
        assert!(err.is_none());
        assert_eq!(recs.len(), 2);
        assert_eq!(recs[0].record.name, "Résumé 日本.docx");
        assert_eq!(recs[0].record.frn, 0x0001_0000_0000_0064);
        assert_eq!(recs[0].record.reason, 0x8000_0100);
        assert_eq!(recs[0].timestamp, filetime_to_unix(132_000_000_000_000_000));
        assert_eq!(recs[1].record.usn, 200);
    }

    #[test]
    fn rejects_refs_wide_ids_and_bad_versions() {
        let mut buf = 0u64.to_le_bytes().to_vec();
        buf.extend(v3(1, 7, 5, 1, "refs"));
        assert!(matches!(parse_buffer(&buf).2, Some(ParseError::WideFileId { .. })));
        let mut r = v2(1, 5, 1, 0, 0, "a");
        r[4] = 9;
        let mut buf = 0u64.to_le_bytes().to_vec();
        buf.extend(r);
        assert!(matches!(parse_buffer(&buf).2, Some(ParseError::UnsupportedVersion { major: 9, .. })));
    }

    #[test]
    fn unpaired_surrogate_is_flagged() {
        let mut r = v2(1, 5, 1, 0, 0, "ab");
        r[60..62].copy_from_slice(&0xD800u16.to_le_bytes());
        let (p, _) = parse_record(&r, 0).unwrap();
        assert!(p.lossy_name);
    }

    #[test]
    fn malformed_buffers_never_panic() {
        let mut seed = 0x1234_5678u64;
        let mut next = || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        };
        let good = {
            let mut b = 1u64.to_le_bytes().to_vec();
            b.extend(v2(1, 5, 1, 0, 0, "hello.txt"));
            b.extend(v2(2, 5, 2, 0, 0, "world.txt"));
            b
        };
        for _ in 0..200_000 {
            let mut b = good.clone();
            for _ in 0..(next() % 6) {
                let i = (next() as usize) % b.len();
                b[i] = next() as u8;
            }
            let cut = (next() as usize) % (b.len() + 1);
            let _ = parse_buffer(&b[..cut]);
        }
    }
}
