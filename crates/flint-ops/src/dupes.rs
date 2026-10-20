//! Optional duplicate detection for one selected file: the index proposes
//! candidates of the same size (no I/O), then each is confirmed by a
//! byte-for-byte comparison within a total read budget. Candidates the
//! budget did not reach are reported as unchecked, never as duplicates.

use crate::compare::{compare_files, Comparison};
use flint_fuzzy::table::{flags, EntryId, NameTable};
use std::sync::atomic::AtomicBool;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Limits {
    /// Same-size candidates taken from the index (in index order).
    pub max_candidates: usize,
    /// Total bytes read across all comparisons (both files counted).
    pub max_bytes: u64,
}

impl Default for Limits {
    fn default() -> Self {
        Limits { max_candidates: 200, max_bytes: 2 << 30 }
    }
}

/// Live files with the same size as `id` (excluding `id` and other hard
/// links of the same file). Empty files are not duplicates of anything
/// meaningful and are not proposed. The bool is true if more candidates
/// existed than `max`.
pub fn candidates(t: &NameTable, id: EntryId, max: usize) -> (Vec<EntryId>, bool) {
    let m = t.meta(id);
    if m.flags & flags::DIR != 0 || m.size == 0 || m.flags & flags::META_STALE != 0 {
        return (Vec::new(), false);
    }
    let key = (t.volume(id), t.key(id));
    let mut out = Vec::new();
    let mut more = false;
    for c in 0..t.len() as EntryId {
        if c == id || !t.is_live(c) {
            continue;
        }
        let cm = t.meta(c);
        // Same volume and key = another name of the same file (hard link);
        // key 0 means "no identity" (synthetic tables) and is not compared.
        if cm.size != m.size || cm.flags & (flags::DIR | flags::META_STALE) != 0 || (key.1 != 0 && (t.volume(c), t.key(c)) == key) {
            continue;
        }
        if out.len() == max {
            more = true;
            break;
        }
        out.push(c);
    }
    (out, more)
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DupReport {
    pub identical: Vec<EntryId>,
    pub different: usize,
    /// Not compared because a limit was reached.
    pub unchecked: Vec<EntryId>,
    pub errors: Vec<(EntryId, String)>,
    /// More same-size files existed than `max_candidates`.
    pub truncated: bool,
}

/// Confirms candidates `(id, path, size)` by content. The caller resolves
/// paths while holding the table lock, then releases it before this I/O.
pub fn confirm(original: &str, cands: &[(EntryId, String, u64)], limits: &Limits, cancel: &AtomicBool) -> DupReport {
    let mut r = DupReport::default();
    let mut spent = 0u64;
    for (i, (id, path, size)) in cands.iter().enumerate() {
        if cancel.load(std::sync::atomic::Ordering::Relaxed) || spent + 2 * size > limits.max_bytes {
            r.unchecked.extend(cands[i..].iter().map(|c| c.0));
            break;
        }
        match compare_files(original.as_ref(), path.as_ref(), *size, cancel) {
            Ok(Comparison::Identical) => {
                r.identical.push(*id);
                spent += 2 * size;
            }
            Ok(Comparison::Incomplete { .. }) => r.unchecked.push(*id),
            Ok(Comparison::DiffersAt(off)) => {
                r.different += 1;
                spent += 2 * (off + 1);
            }
            Ok(Comparison::SizeDiffers { .. }) => r.different += 1,
            Err(e) => r.errors.push((*id, e.to_string())),
        }
    }
    r
}

#[cfg(test)]
mod tests {
    use super::*;
    use flint_fuzzy::table::{Meta, NO_PARENT};

    #[test]
    fn proposes_same_size_files_and_confirms_by_content_within_budget() {
        let d = std::env::temp_dir().join(format!("flint-dupes-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        let mut t = NameTable::new();
        t.sep = std::path::MAIN_SEPARATOR;
        let root = t.push(NO_PARENT, d.to_str().unwrap(), Meta::dir(0));
        let mut file = |name: &str, body: &[u8], key: u64| {
            std::fs::write(d.join(name), body).unwrap();
            t.push_keyed(root, name, Meta::file(0, body.len() as u64, 1), key)
        };
        let orig = file("orig.bin", b"0123456789", 1);
        let copy = file("copy.bin", b"0123456789", 2);
        let near = file("near.bin", b"0123456780", 3);
        let link = file("hardlink.bin", b"0123456789", 1); // same key: same file
        let _other = file("other.bin", b"short", 4);
        let _empty1 = file("e1", b"", 5);
        let (c, more) = candidates(&t, orig, 10);
        assert_eq!(c, vec![copy, near], "same size, excluding itself and its own hard link {link}");
        assert!(!more);
        assert_eq!(candidates(&t, orig, 1), (vec![copy], true));
        let empty = t.push(root, "e2", Meta::file(0, 0, 1));
        assert!(candidates(&t, empty, 10).0.is_empty(), "empty files are not proposed");

        let list: Vec<(EntryId, String, u64)> = c.iter().map(|&i| (i, t.path(i), 10)).collect();
        let no = AtomicBool::new(false);
        let r = confirm(&t.path(orig), &list, &Limits::default(), &no);
        assert_eq!((r.identical.clone(), r.different, r.unchecked.len()), (vec![copy], 1, 0));
        let r = confirm(&t.path(orig), &list, &Limits { max_candidates: 10, max_bytes: 25 }, &no);
        assert_eq!((r.identical.clone(), r.unchecked.clone()), (vec![copy], vec![near]), "budget leaves the rest unchecked, not 'duplicate'");
        std::fs::remove_file(d.join("copy.bin")).unwrap();
        let r = confirm(&t.path(orig), &list, &Limits::default(), &no);
        assert_eq!(r.errors.len(), 1, "a vanished candidate is an error, not a duplicate");
        let _ = std::fs::remove_dir_all(&d);
    }
}
