//! Raw `$MFT` FILE-record parsing (platform-independent, bounds-checked).
//!
//! Produces, per in-use file: every hard-link name (all `$FILE_NAME`
//! attributes except DOS 8.3 aliases), parent references, attributes,
//! modification time (`$STANDARD_INFORMATION`) and size (unnamed `$DATA`).
//! Attributes moved to extension records (files with an `$ATTRIBUTE_LIST`)
//! are merged by [`Assembler`] using each extension record's base reference,
//! so the attribute list itself never needs parsing.
//!
//! Every multi-sector record is verified with its update sequence array; a
//! torn or corrupt record is rejected individually and counted.

use crate::records::filetime_to_unix;
use flint_index::model::{attr, make_frn, FileRecord};
use std::collections::HashMap;

pub const ATTR_STANDARD_INFORMATION: u32 = 0x10;
pub const ATTR_ATTRIBUTE_LIST: u32 = 0x20;
pub const ATTR_FILE_NAME: u32 = 0x30;
pub const ATTR_DATA: u32 = 0x80;
const ATTR_END: u32 = 0xFFFF_FFFF;

pub const NS_POSIX: u8 = 0;
pub const NS_WIN32: u8 = 1;
pub const NS_DOS: u8 = 2;
pub const NS_WIN32_DOS: u8 = 3;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MftError {
    TooShort,
    BadSignature,
    BadFixup,
    BadAttribute { offset: usize },
    BadRun,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MftName {
    pub parent: u64,
    pub name: String,
    pub namespace: u8,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct MftEntry {
    pub segment: u64,
    pub sequence: u16,
    /// Base record segment for extension records (0 for base records).
    pub base: u64,
    pub is_dir: bool,
    pub names: Vec<MftName>,
    /// `$STANDARD_INFORMATION` file attributes.
    pub attrs: Option<u32>,
    pub mtime: Option<i64>,
    /// Unnamed `$DATA` logical size (first extent only carries it).
    pub size: Option<u64>,
    pub has_attr_list: bool,
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

/// Verifies and undoes the update sequence fixups in place: the last two
/// bytes of every sector must equal the update sequence number and are
/// replaced with the saved originals.
pub fn apply_fixups(rec: &mut [u8], sector: usize) -> Result<(), MftError> {
    let usa_off = u16_at(rec, 4).ok_or(MftError::TooShort)? as usize;
    let usa_count = u16_at(rec, 6).ok_or(MftError::TooShort)? as usize;
    if sector < 2 || usa_count == 0 || (usa_count - 1) * sector > rec.len() || usa_off + usa_count * 2 > rec.len() {
        return Err(MftError::BadFixup);
    }
    let usn = u16_at(rec, usa_off).unwrap();
    for i in 1..usa_count {
        let end = i * sector - 2;
        if u16_at(rec, end) != Some(usn) {
            return Err(MftError::BadFixup);
        }
        let saved = [rec[usa_off + 2 * i], rec[usa_off + 2 * i + 1]];
        rec[end..end + 2].copy_from_slice(&saved);
    }
    Ok(())
}

/// Parses one FILE record (fixups applied here). Returns `None` for records
/// that are unused or not FILE records (free slots are normal in an MFT).
pub fn parse_record(rec: &mut [u8], segment: u64, sector: usize) -> Result<Option<MftEntry>, MftError> {
    if rec.len() < 48 {
        return Err(MftError::TooShort);
    }
    if &rec[0..4] != b"FILE" {
        // Zeroed/never-used slots have no signature.
        return if rec[0..4] == [0; 4] { Ok(None) } else { Err(MftError::BadSignature) };
    }
    apply_fixups(rec, sector)?;
    let rec = &*rec;
    let flags = u16_at(rec, 0x16).unwrap();
    if flags & 0x01 == 0 {
        return Ok(None);
    }
    let used = (u32_at(rec, 0x18).unwrap() as usize).min(rec.len());
    let mut e = MftEntry {
        segment,
        sequence: u16_at(rec, 0x10).unwrap(),
        base: u64_at(rec, 0x20).unwrap() & 0x0000_FFFF_FFFF_FFFF,
        is_dir: flags & 0x02 != 0,
        ..Default::default()
    };
    let mut off = u16_at(rec, 0x14).unwrap() as usize;
    let mut guard = 0;
    while off + 8 <= used && guard < 256 {
        guard += 1;
        let ty = u32_at(rec, off).unwrap();
        if ty == ATTR_END {
            break;
        }
        let len = u32_at(rec, off + 4).unwrap() as usize;
        if len < 16 || off + len > used || !len.is_multiple_of(8) {
            return Err(MftError::BadAttribute { offset: off });
        }
        let a = &rec[off..off + len];
        let non_resident = a[8] != 0;
        let name_len = a[9] as usize;
        let resident_value = || -> Option<&[u8]> {
            if non_resident {
                return None;
            }
            let vl = u32_at(a, 0x10)? as usize;
            let vo = u16_at(a, 0x14)? as usize;
            a.get(vo..vo.checked_add(vl)?)
        };
        match ty {
            ATTR_STANDARD_INFORMATION => {
                let v = resident_value().ok_or(MftError::BadAttribute { offset: off })?;
                e.mtime = u64_at(v, 0x08).map(|t| filetime_to_unix(t as i64));
                e.attrs = u32_at(v, 0x20);
            }
            ATTR_ATTRIBUTE_LIST => e.has_attr_list = true,
            ATTR_FILE_NAME => {
                let v = resident_value().ok_or(MftError::BadAttribute { offset: off })?;
                let parent = u64_at(v, 0).ok_or(MftError::BadAttribute { offset: off })?;
                let n = *v.get(0x40).ok_or(MftError::BadAttribute { offset: off })? as usize;
                let ns = *v.get(0x41).ok_or(MftError::BadAttribute { offset: off })?;
                let raw = v.get(0x42..0x42 + 2 * n).ok_or(MftError::BadAttribute { offset: off })?;
                let units: Vec<u16> = (0..raw.len() / 2).map(|i| u16::from_le_bytes([raw[2 * i], raw[2 * i + 1]])).collect();
                e.names.push(MftName { parent, name: String::from_utf16_lossy(&units), namespace: ns });
            }
            ATTR_DATA if name_len == 0 => {
                if non_resident {
                    // Only the first extent (starting VCN 0) carries sizes.
                    if u64_at(a, 0x10) == Some(0) {
                        e.size = u64_at(a, 0x30);
                    }
                } else {
                    e.size = u32_at(a, 0x10).map(|v| v as u64);
                }
            }
            _ => {}
        }
        off += len;
    }
    Ok(Some(e))
}

/// Decodes a non-resident attribute's mapping pairs into (LCN, clusters)
/// extents; sparse runs have `None` as LCN.
pub fn decode_runs(runs: &[u8]) -> Result<Vec<(Option<i64>, u64)>, MftError> {
    let mut out = Vec::new();
    let (mut i, mut lcn) = (0usize, 0i64);
    while i < runs.len() {
        let h = runs[i];
        if h == 0 {
            return Ok(out);
        }
        let (ls, os) = ((h & 0x0F) as usize, (h >> 4) as usize);
        if ls == 0 || ls > 8 || os > 8 || i + 1 + ls + os > runs.len() {
            return Err(MftError::BadRun);
        }
        let mut len = 0u64;
        for k in 0..ls {
            len |= (runs[i + 1 + k] as u64) << (8 * k);
        }
        if os == 0 {
            out.push((None, len));
        } else {
            let mut d = 0i64;
            for k in 0..os {
                d |= (runs[i + 1 + ls + k] as i64) << (8 * k);
            }
            // Sign-extend the variable-width delta.
            let shift = 64 - 8 * os as u32;
            d = (d << shift) >> shift;
            lcn = lcn.checked_add(d).ok_or(MftError::BadRun)?;
            if lcn < 0 {
                return Err(MftError::BadRun);
            }
            out.push((Some(lcn), len));
        }
        i += 1 + ls + os;
    }
    Err(MftError::BadRun)
}

/// Data runs of the unnamed `$DATA` attribute (used for record 0, `$MFT`).
pub fn data_runs_of(rec: &mut [u8], sector: usize) -> Result<Vec<(Option<i64>, u64)>, MftError> {
    if rec.len() < 48 || &rec[0..4] != b"FILE" {
        return Err(MftError::BadSignature);
    }
    apply_fixups(rec, sector)?;
    let used = (u32_at(rec, 0x18).unwrap() as usize).min(rec.len());
    let mut off = u16_at(rec, 0x14).unwrap() as usize;
    while off + 8 <= used {
        let ty = u32_at(rec, off).unwrap();
        let len = u32_at(rec, off + 4).unwrap() as usize;
        if ty == ATTR_END || len < 16 || off + len > used {
            break;
        }
        let a = &rec[off..off + len];
        if ty == ATTR_DATA && a[9] == 0 && a[8] != 0 {
            let ro = u16_at(a, 0x20).ok_or(MftError::BadRun)? as usize;
            return decode_runs(a.get(ro..).ok_or(MftError::BadRun)?);
        }
        off += len;
    }
    Err(MftError::BadRun)
}

#[derive(Default)]
struct Partial {
    sequence: Option<u16>,
    is_dir: bool,
    names: Vec<MftName>,
    attrs: Option<u32>,
    mtime: Option<i64>,
    size: Option<u64>,
}

/// Merges base and extension records and emits [`FileRecord`]s, one per
/// hard-link name. Records without an attribute list are emitted
/// immediately; the rest wait for [`finish`](Self::finish).
#[derive(Default)]
pub struct Assembler {
    pending: HashMap<u64, Partial>,
    pub rejected: u64,
}

fn emit(segment: u64, p: &Partial, out: &mut dyn FnMut(FileRecord)) {
    let Some(seq) = p.sequence else { return };
    // DOS 8.3 aliases duplicate a Win32 name; keep them only if nothing else exists.
    let has_long = p.names.iter().any(|n| n.namespace != NS_DOS);
    let mut seen: Vec<(u64, &str)> = Vec::new();
    for n in &p.names {
        if has_long && n.namespace == NS_DOS {
            continue;
        }
        if seen.contains(&(n.parent, n.name.as_str())) {
            continue;
        }
        seen.push((n.parent, n.name.as_str()));
        let mut attrs = p.attrs.unwrap_or(0);
        if p.is_dir {
            attrs |= attr::DIRECTORY;
        }
        out(FileRecord {
            frn: make_frn(segment, seq),
            parent: n.parent,
            name: n.name.clone(),
            attrs,
            size: if p.is_dir { None } else { Some(p.size.unwrap_or(0)) },
            mtime: p.mtime,
        });
    }
}

impl Assembler {
    pub fn add(&mut self, e: MftEntry, out: &mut dyn FnMut(FileRecord)) {
        if e.base == 0 && !e.has_attr_list && !self.pending.contains_key(&e.segment) {
            let p = Partial { sequence: Some(e.sequence), is_dir: e.is_dir, names: e.names, attrs: e.attrs, mtime: e.mtime, size: e.size };
            emit(e.segment, &p, out);
            return;
        }
        let key = if e.base == 0 { e.segment } else { e.base };
        let p = self.pending.entry(key).or_default();
        if e.base == 0 {
            p.sequence = Some(e.sequence);
            p.is_dir = e.is_dir;
        }
        p.names.extend(e.names);
        p.attrs = p.attrs.or(e.attrs);
        p.mtime = p.mtime.or(e.mtime);
        p.size = p.size.or(e.size);
    }

    /// Emits files that had extension records. Extension records whose base
    /// never appeared (freed meanwhile) are dropped.
    pub fn finish(self, out: &mut dyn FnMut(FileRecord)) {
        let mut keys: Vec<u64> = self.pending.keys().copied().collect();
        keys.sort_unstable();
        for k in keys {
            emit(k, &self.pending[&k], out);
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub const SECTOR: usize = 512;

    /// Builds a 1024-byte FILE record as NTFS lays it out, fixups applied.
    pub struct RecBuilder {
        attrs: Vec<u8>,
        flags: u16,
        sequence: u16,
        base: u64,
    }

    fn attr_header(ty: u32, body: &[u8], non_resident: bool, name_len: u8) -> Vec<u8> {
        let mut a = vec![0u8; 0x18];
        a[0..4].copy_from_slice(&ty.to_le_bytes());
        a[8] = non_resident as u8;
        a[9] = name_len;
        if !non_resident {
            a[0x10..0x14].copy_from_slice(&(body.len() as u32).to_le_bytes());
            a[0x14..0x16].copy_from_slice(&0x18u16.to_le_bytes());
        }
        a.extend_from_slice(body);
        while !a.len().is_multiple_of(8) {
            a.push(0);
        }
        let len = a.len() as u32;
        a[4..8].copy_from_slice(&len.to_le_bytes());
        a
    }

    impl RecBuilder {
        pub fn new(sequence: u16, dir: bool) -> Self {
            RecBuilder { attrs: Vec::new(), flags: 1 | if dir { 2 } else { 0 }, sequence, base: 0 }
        }
        pub fn extension_of(mut self, base: u64) -> Self {
            self.base = base;
            self
        }
        pub fn free(mut self) -> Self {
            self.flags = 0;
            self
        }
        pub fn std_info(mut self, mtime_ft: u64, attrs: u32) -> Self {
            let mut v = vec![0u8; 0x48];
            v[0x08..0x10].copy_from_slice(&mtime_ft.to_le_bytes());
            v[0x20..0x24].copy_from_slice(&attrs.to_le_bytes());
            self.attrs.extend(attr_header(ATTR_STANDARD_INFORMATION, &v, false, 0));
            self
        }
        pub fn file_name(mut self, parent: u64, name: &str, ns: u8) -> Self {
            let units: Vec<u16> = name.encode_utf16().collect();
            let mut v = vec![0u8; 0x42];
            v[0..8].copy_from_slice(&parent.to_le_bytes());
            v[0x40] = units.len() as u8;
            v[0x41] = ns;
            for u in units {
                v.extend_from_slice(&u.to_le_bytes());
            }
            self.attrs.extend(attr_header(ATTR_FILE_NAME, &v, false, 0));
            self
        }
        pub fn resident_data(mut self, len: usize) -> Self {
            self.attrs.extend(attr_header(ATTR_DATA, &vec![7u8; len], false, 0));
            self
        }
        pub fn nonresident_data(mut self, size: u64, runs: &[u8]) -> Self {
            let mut a = vec![0u8; 0x40];
            a[0..4].copy_from_slice(&ATTR_DATA.to_le_bytes());
            a[8] = 1;
            a[0x20..0x22].copy_from_slice(&0x40u16.to_le_bytes());
            a[0x30..0x38].copy_from_slice(&size.to_le_bytes());
            a.extend_from_slice(runs);
            while !a.len().is_multiple_of(8) {
                a.push(0);
            }
            let len = a.len() as u32;
            a[4..8].copy_from_slice(&len.to_le_bytes());
            self.attrs.extend(a);
            self
        }
        pub fn attr_list(mut self) -> Self {
            self.attrs.extend(attr_header(ATTR_ATTRIBUTE_LIST, &[0u8; 8], false, 0));
            self
        }
        pub fn build(self) -> Vec<u8> {
            let mut r = vec![0u8; 1024];
            r[0..4].copy_from_slice(b"FILE");
            r[4..6].copy_from_slice(&0x30u16.to_le_bytes()); // USA offset
            r[6..8].copy_from_slice(&3u16.to_le_bytes()); // USN + 2 sectors
            r[0x10..0x12].copy_from_slice(&self.sequence.to_le_bytes());
            r[0x14..0x16].copy_from_slice(&0x38u16.to_le_bytes());
            r[0x16..0x18].copy_from_slice(&self.flags.to_le_bytes());
            r[0x1C..0x20].copy_from_slice(&1024u32.to_le_bytes());
            r[0x20..0x28].copy_from_slice(&self.base.to_le_bytes());
            let end = 0x38 + self.attrs.len();
            assert!(end + 8 <= 1024 - 2, "record too full for test builder");
            r[0x38..end].copy_from_slice(&self.attrs);
            r[end..end + 4].copy_from_slice(&ATTR_END.to_le_bytes());
            r[0x18..0x1C].copy_from_slice(&((end + 8) as u32).to_le_bytes());
            // Fixups: save sector tails into the USA, stamp the USN.
            let usn: u16 = 0x0042;
            r[0x30..0x32].copy_from_slice(&usn.to_le_bytes());
            for i in 1..3 {
                let e = i * SECTOR - 2;
                let (a, b) = (r[e], r[e + 1]);
                r[0x30 + 2 * i] = a;
                r[0x30 + 2 * i + 1] = b;
                r[e..e + 2].copy_from_slice(&usn.to_le_bytes());
            }
            r
        }
    }

    const ROOT: u64 = 0x0005_0000_0000_0005;

    fn collect(records: Vec<(u64, Vec<u8>)>) -> (Vec<FileRecord>, u64) {
        let mut asm = Assembler::default();
        let mut out = Vec::new();
        for (seg, mut r) in records {
            match parse_record(&mut r, seg, SECTOR) {
                Ok(Some(e)) => asm.add(e, &mut |f| out.push(f)),
                Ok(None) => {}
                Err(_) => asm.rejected += 1,
            }
        }
        let rejected = asm.rejected;
        asm.finish(&mut |f| out.push(f));
        (out, rejected)
    }

    #[test]
    fn parses_names_links_sizes_times() {
        let ft = 133_000_000_000_000_000u64;
        let recs = vec![
            (64, RecBuilder::new(3, true).std_info(ft, 0x10).file_name(ROOT, "Users", NS_WIN32_DOS).build()),
            // A file with a long name, its DOS alias, and a second hard link.
            (65, RecBuilder::new(7, false).std_info(ft, 0x20).file_name(make_frn(64, 3), "Quarterly Report.docx", NS_WIN32).file_name(make_frn(64, 3), "QUARTE~1.DOC", NS_DOS).file_name(ROOT, "link.docx", NS_POSIX).nonresident_data(123_456_789, &[0x11, 0x10, 0x20, 0x00]).build()),
            (66, RecBuilder::new(1, false).std_info(ft, 0).file_name(ROOT, "tiny.txt", NS_WIN32_DOS).resident_data(42).build()),
            (67, RecBuilder::new(1, false).free().build()),
            (68, vec![0u8; 1024]),
        ];
        let (out, rejected) = collect(recs);
        assert_eq!(rejected, 0);
        let names: Vec<&str> = out.iter().map(|r| r.name.as_str()).collect();
        assert_eq!(names, vec!["Users", "Quarterly Report.docx", "link.docx", "tiny.txt"]);
        let report = &out[1];
        assert_eq!(report.frn, make_frn(65, 7));
        assert_eq!(report.size, Some(123_456_789));
        assert_eq!(report.mtime, Some(filetime_to_unix(ft as i64)));
        assert_eq!(out[2].frn, report.frn, "both links share the FRN");
        assert_eq!(out[3].size, Some(42));
        assert_eq!(out[0].attrs & attr::DIRECTORY, attr::DIRECTORY);
        assert_eq!(out[0].size, None);
    }

    #[test]
    fn merges_extension_records_in_any_order() {
        let ft = 133_000_000_000_000_000u64;
        let recs = vec![
            // Extension record arrives before its base.
            (80, RecBuilder::new(2, false).extension_of(81).file_name(ROOT, "second name.txt", NS_WIN32).nonresident_data(999, &[0x11, 0x01, 0x05, 0x00]).build()),
            (81, RecBuilder::new(9, false).std_info(ft, 0x20).attr_list().file_name(ROOT, "first name.txt", NS_WIN32).build()),
            // Orphan extension (base freed): dropped.
            (82, RecBuilder::new(1, false).extension_of(4000).file_name(ROOT, "ghost", NS_WIN32).build()),
        ];
        let (out, _) = collect(recs);
        assert_eq!(out.len(), 2);
        assert!(out.iter().all(|r| r.frn == make_frn(81, 9) && r.size == Some(999)));
    }

    #[test]
    fn torn_record_is_rejected() {
        let mut r = RecBuilder::new(1, false).file_name(ROOT, "x", NS_WIN32).build();
        r[SECTOR - 2] ^= 0xFF; // sector tail no longer matches the USN
        assert_eq!(parse_record(&mut r, 70, SECTOR), Err(MftError::BadFixup));
    }

    #[test]
    fn data_runs() {
        // 0x21: 1-byte length, 2-byte offset; then a sparse run; then a
        // negative relative offset.
        let runs = [0x21, 0x10, 0x00, 0x01, 0x01, 0x05, 0x11, 0x08, 0xF0, 0x00];
        assert_eq!(decode_runs(&runs).unwrap(), vec![(Some(256), 16), (None, 5), (Some(240), 8)]);
        assert_eq!(decode_runs(&[0x21, 0x10]), Err(MftError::BadRun));
        assert_eq!(decode_runs(&[0x11, 0x01, 0x80]), Err(MftError::BadRun), "negative LCN");
        let mut mft0 = RecBuilder::new(1, false).nonresident_data(1 << 20, &[0x31, 0x40, 0x00, 0x00, 0x0C, 0x00]).build();
        assert_eq!(data_runs_of(&mut mft0, SECTOR).unwrap(), vec![(Some(0x0C_0000), 0x40)]);
    }

    #[test]
    fn mutated_records_never_panic() {
        let good = RecBuilder::new(7, false).std_info(1, 0x20).file_name(ROOT, "Résumé.pdf", NS_WIN32).file_name(ROOT, "RSUM~1.PDF", NS_DOS).nonresident_data(5, &[0x11, 0x01, 0x05, 0x00]).build();
        let mut x = 0xdead_beefu64;
        for _ in 0..100_000 {
            let mut r = good.clone();
            for _ in 0..1 + (x % 4) {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                let i = (x as usize) % r.len();
                r[i] = (x >> 40) as u8;
            }
            let _ = parse_record(&mut r, 1, SECTOR);
            let _ = data_runs_of(&mut good.clone(), SECTOR);
            let cut = (x as usize >> 3) % 1024;
            let _ = parse_record(&mut r[..cut], 1, SECTOR);
        }
    }
}
