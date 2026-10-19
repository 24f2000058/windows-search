//! Per-volume startup snapshots: a binary copy of the volume's name-table
//! columns, loaded with a bulk append instead of re-applying every record.
//!
//! A snapshot is only used if its `commit_seq` equals the store's for that
//! volume, i.e. it mirrors exactly the committed SQLite state. Any mismatch,
//! corruption (checksum), truncation or invalid content falls back to the
//! SQLite load; SQLite remains the source of truth and snapshots are
//! disposable caches.
//!
//! Layout (little-endian):
//! `magic "FLSNAP\0\x01" | u64 commit_seq | u64 root_frn | u32 key_len | key |
//!  u64 arena_len | arena | u64 n | n × slot(43 bytes) | u64 fnv1a(all prior)`

use flint_fuzzy::table::{Chunk, ChunkSlot};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

const MAGIC: &[u8; 8] = b"FLSNAP\0\x01";
const SLOT_BYTES: usize = 4 + 2 + 4 + 2 + 4 + 8 + 8 + 1 + 8;

#[derive(Debug, PartialEq, Eq)]
pub enum SnapshotError {
    Io(String),
    BadMagic,
    Truncated,
    Checksum,
    /// Valid file, but for another volume or another committed state.
    Stale { snapshot_seq: u64, store_seq: u64 },
    WrongVolume,
}

pub struct Snapshot {
    pub commit_seq: u64,
    pub root_frn: u64,
    pub key: String,
    pub chunk: Chunk,
}

fn fnv1a(data: &[u8]) -> u64 {
    data.iter().fold(0xcbf2_9ce4_8422_2325u64, |h, &b| (h ^ b as u64).wrapping_mul(0x100_0000_01b3))
}

/// File name for a volume key (keys contain `\\?\Volume{...}` characters).
pub fn path_for(dir: &Path, key: &str) -> PathBuf {
    dir.join(format!("{:016x}.snap", fnv1a(key.as_bytes())))
}

pub fn encode(s: &Snapshot) -> Vec<u8> {
    let c = &s.chunk;
    let mut b = Vec::with_capacity(64 + s.key.len() + c.arena.len() + c.slots.len() * SLOT_BYTES);
    b.extend_from_slice(MAGIC);
    b.extend_from_slice(&s.commit_seq.to_le_bytes());
    b.extend_from_slice(&s.root_frn.to_le_bytes());
    b.extend_from_slice(&(s.key.len() as u32).to_le_bytes());
    b.extend_from_slice(s.key.as_bytes());
    b.extend_from_slice(&(c.arena.len() as u64).to_le_bytes());
    b.extend_from_slice(&c.arena);
    b.extend_from_slice(&(c.slots.len() as u64).to_le_bytes());
    for x in &c.slots {
        b.extend_from_slice(&x.name_off.to_le_bytes());
        b.extend_from_slice(&x.name_len.to_le_bytes());
        b.extend_from_slice(&x.folded_off.to_le_bytes());
        b.extend_from_slice(&x.folded_len.to_le_bytes());
        b.extend_from_slice(&x.parent.to_le_bytes());
        b.extend_from_slice(&x.size.to_le_bytes());
        b.extend_from_slice(&x.mtime.to_le_bytes());
        b.push(x.flags);
        b.extend_from_slice(&x.key.to_le_bytes());
    }
    let h = fnv1a(&b);
    b.extend_from_slice(&h.to_le_bytes());
    b
}

struct Reader<'a> {
    b: &'a [u8],
    at: usize,
}

impl<'a> Reader<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8], SnapshotError> {
        let end = self.at.checked_add(n).ok_or(SnapshotError::Truncated)?;
        let s = self.b.get(self.at..end).ok_or(SnapshotError::Truncated)?;
        self.at = end;
        Ok(s)
    }
    fn u64(&mut self) -> Result<u64, SnapshotError> {
        Ok(u64::from_le_bytes(self.take(8)?.try_into().unwrap()))
    }
    fn u32(&mut self) -> Result<u32, SnapshotError> {
        Ok(u32::from_le_bytes(self.take(4)?.try_into().unwrap()))
    }
    fn u16(&mut self) -> Result<u16, SnapshotError> {
        Ok(u16::from_le_bytes(self.take(2)?.try_into().unwrap()))
    }
}

/// Decodes and integrity-checks a snapshot. Content validity (spans, UTF-8,
/// parents) is enforced again by `NameTable::import_chunk`.
pub fn decode(b: &[u8]) -> Result<Snapshot, SnapshotError> {
    if b.len() < MAGIC.len() + 8 {
        return Err(SnapshotError::Truncated);
    }
    if &b[..8] != MAGIC {
        return Err(SnapshotError::BadMagic);
    }
    let (body, tail) = b.split_at(b.len() - 8);
    if fnv1a(body) != u64::from_le_bytes(tail.try_into().unwrap()) {
        return Err(SnapshotError::Checksum);
    }
    let mut r = Reader { b: body, at: 8 };
    let commit_seq = r.u64()?;
    let root_frn = r.u64()?;
    let kl = r.u32()? as usize;
    let key = String::from_utf8(r.take(kl)?.to_vec()).map_err(|_| SnapshotError::Truncated)?;
    let al = r.u64()? as usize;
    let arena = r.take(al)?.to_vec();
    let n = r.u64()? as usize;
    if n.checked_mul(SLOT_BYTES).is_none_or(|need| need > body.len() - r.at) {
        return Err(SnapshotError::Truncated);
    }
    let mut slots = Vec::with_capacity(n);
    for _ in 0..n {
        slots.push(ChunkSlot {
            name_off: r.u32()?,
            name_len: r.u16()?,
            folded_off: r.u32()?,
            folded_len: r.u16()?,
            parent: r.u32()?,
            size: r.u64()?,
            mtime: r.u64()? as i64,
            flags: r.take(1)?[0],
            key: r.u64()?,
        });
    }
    if r.at != body.len() {
        return Err(SnapshotError::Truncated);
    }
    Ok(Snapshot { commit_seq, root_frn, key, chunk: Chunk { arena, slots } })
}

/// Writes atomically (temp file + rename) so a crash never leaves a torn
/// snapshot under the real name.
pub fn write(path: &Path, s: &Snapshot) -> io::Result<()> {
    if let Some(d) = path.parent() {
        std::fs::create_dir_all(d)?;
    }
    let tmp = path.with_extension("snap.tmp");
    {
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(&encode(s))?;
        f.sync_all()?;
    }
    std::fs::rename(tmp, path)
}

/// Reads a snapshot and checks it belongs to `key` at `store_seq`.
pub fn read(path: &Path, key: &str, store_seq: u64) -> Result<Snapshot, SnapshotError> {
    let b = std::fs::read(path).map_err(|e| SnapshotError::Io(e.to_string()))?;
    let s = decode(&b)?;
    if s.key != key {
        return Err(SnapshotError::WrongVolume);
    }
    if s.commit_seq != store_seq {
        return Err(SnapshotError::Stale { snapshot_seq: s.commit_seq, store_seq });
    }
    Ok(s)
}

#[cfg(test)]
mod tests {
    use super::*;
    use flint_fuzzy::table::{Meta, NameTable, NO_PARENT};

    fn sample() -> Snapshot {
        let mut t = NameTable::new();
        let r = t.push_keyed(NO_PARENT, "C:", Meta::dir(0), 5);
        let d = t.push_keyed(r, "Données", Meta::dir(0), 6);
        t.push_keyed(d, "a.txt", Meta::file(0, 3, 4), 7);
        Snapshot { commit_seq: 42, root_frn: 5, key: "vol".into(), chunk: t.export_volume(0) }
    }

    #[test]
    fn round_trip() {
        let s = sample();
        let d = decode(&encode(&s)).unwrap();
        assert_eq!((d.commit_seq, d.root_frn, d.key.as_str()), (42, 5, "vol"));
        assert_eq!(d.chunk, s.chunk);
    }

    #[test]
    fn corruption_is_detected_and_never_panics() {
        let good = encode(&sample());
        let mut x = 7u64;
        for i in 0..20_000 {
            let mut b = good.clone();
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            let pos = (x as usize) % b.len();
            b[pos] ^= 1 + (x >> 32) as u8 % 255;
            assert!(decode(&b).is_err(), "single-byte flip at {pos} must be detected");
            let cut = (x as usize >> 8) % b.len();
            let _ = decode(&good[..cut]);
            if i == 0 {
                assert!(decode(&good[..cut]).is_err());
            }
        }
    }
}
