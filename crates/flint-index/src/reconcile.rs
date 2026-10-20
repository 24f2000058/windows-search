//! Incremental reconciliation of journal-less (traversal) volumes.
//!
//! Given directories reported as changed (by a watcher, or a user action),
//! re-list just those directories, diff them against the index and apply
//! the difference: new entries (newly appeared directories are walked in
//! full), removed entries (with their subtrees), type changes (file ↔
//! directory, link flag) and size/mtime/attribute changes. Filesystem I/O
//! happens without holding the table lock; only the final apply takes it.

use crate::indexer::{SyncError, VolumeIndexer};
use crate::model::{attr, make_frn, segment, FileRecord, Frn, VolumeState};
use crate::store::Store;
use crate::traverse::{DirItem, Fs, TraversalSource};
use crate::volume::Changes;
use flint_fuzzy::table::{flags, EntryId, NameTable, NO_PARENT};
use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::sync::RwLock;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ReconcileReport {
    pub dirs: usize,
    pub added: u64,
    pub removed: u64,
    pub updated: u64,
    /// Directories that could not be listed (coverage becomes incomplete).
    pub unreadable: Vec<(PathBuf, String)>,
}

struct Child {
    id: EntryId,
    frn: Frn,
    is_dir: bool,
    is_link: bool,
    hidden: bool,
    size: u64,
    mtime: i64,
}

fn attrs_for(i: &DirItem) -> u32 {
    let mut a = 0;
    if i.is_dir {
        a |= attr::DIRECTORY;
    }
    if i.is_link {
        a |= attr::REPARSE_POINT;
    }
    if i.hidden {
        a |= attr::HIDDEN;
    }
    a
}

/// Volume-relative path components, ignoring empty/`.` parts.
fn components(rel: &Path) -> Vec<String> {
    rel.components().filter_map(|c| match c {
        std::path::Component::Normal(s) => Some(s.to_string_lossy().into_owned()),
        _ => None,
    }).collect()
}

impl VolumeIndexer {
    fn next_synthetic(&mut self, t: &NameTable) -> u64 {
        if self.synthetic_next == 0 {
            let v = self.index.volume;
            let max = (0..t.len() as EntryId).filter(|&i| t.is_live(i) && t.volume(i) == v).map(|i| segment(t.key(i))).max().unwrap_or(1);
            self.synthetic_next = max + 1;
        }
        self.synthetic_next
    }

    /// Reconciles `dirs` (relative to `src.root`; empty path = root).
    pub fn reconcile<F: Fs>(&mut self, src: &mut TraversalSource<F>, dirs: &[PathBuf], store: &mut Store, table: &RwLock<NameTable>) -> Result<ReconcileReport, SyncError> {
        let mut report = ReconcileReport::default();
        // 1. Resolve each changed directory to the nearest indexed ancestor
        //    (a directory that is itself new is handled by its parent).
        let (targets, mut children, mut next) = {
            let t = table.read().unwrap();
            let v = self.index.volume;
            let mut by_name: HashMap<(EntryId, String), EntryId> = HashMap::new();
            let mut kids: HashMap<EntryId, Vec<EntryId>> = HashMap::new();
            for id in 0..t.len() as EntryId {
                if t.is_live(id) && t.volume(id) == v && t.parent(id) != NO_PARENT {
                    by_name.insert((t.parent(id), t.name(id).to_lowercase()), id);
                    kids.entry(t.parent(id)).or_default().push(id);
                }
            }
            let mut targets: BTreeSet<(Vec<String>, EntryId)> = BTreeSet::new();
            for d in dirs {
                let comps = components(d);
                let (mut cur, mut depth) = (self.index.root(), 0);
                for c in &comps {
                    match by_name.get(&(cur, c.to_lowercase())) {
                        Some(&id) if t.flags(id) & flags::DIR != 0 => {
                            cur = id;
                            depth += 1;
                        }
                        _ => break,
                    }
                }
                targets.insert((comps[..depth].to_vec(), cur));
            }
            let mut children: HashMap<EntryId, HashMap<String, Child>> = HashMap::new();
            for (_, dir) in &targets {
                let m = children.entry(*dir).or_default();
                for &c in kids.get(dir).map(Vec::as_slice).unwrap_or(&[]) {
                    let meta = t.meta(c);
                    m.insert(t.name(c).to_owned(), Child {
                        id: c,
                        frn: t.key(c),
                        is_dir: meta.flags & flags::DIR != 0,
                        is_link: meta.flags & flags::REPARSE != 0,
                        hidden: meta.flags & flags::HIDDEN != 0,
                        size: meta.size,
                        mtime: meta.mtime,
                    });
                }
            }
            let next = self.next_synthetic(&t);
            (targets, children, next)
        };
        // 2. List and diff without holding the table lock.
        let mut adds: Vec<FileRecord> = Vec::new();
        let mut removes: Vec<Frn> = Vec::new();
        let mut updates: Vec<(Frn, u32, u64, i64)> = Vec::new();
        for (comps, dir) in &targets {
            report.dirs += 1;
            let abs = comps.iter().fold(src.root.clone(), |p, c| p.join(c));
            if src.is_excluded(&abs) {
                continue;
            }
            let dir_frn = table.read().unwrap().key(*dir);
            let items = match src.fs.list(&abs) {
                Ok(v) => v,
                Err(e) => {
                    // A vanished directory is removed by its parent's pass;
                    // anything else leaves coverage incomplete.
                    if e.kind() != std::io::ErrorKind::NotFound {
                        report.unreadable.push((abs, e.to_string()));
                    }
                    continue;
                }
            };
            let mut existing = children.remove(dir).unwrap_or_default();
            for item in items.into_iter().flatten() {
                let name = item.name.to_string_lossy().into_owned();
                let a = attrs_for(&item);
                let mtime = item.mtime.unwrap_or(0);
                match existing.remove(&name) {
                    Some(c) if c.is_dir == item.is_dir && c.is_link == item.is_link => {
                        if !c.is_dir && (c.size != item.size || c.mtime != mtime || c.hidden != item.hidden) {
                            updates.push((c.frn, a, item.size, mtime));
                        }
                        let _ = c.id;
                    }
                    other => {
                        if let Some(c) = other {
                            removes.push(c.frn);
                        }
                        next += 1;
                        let frn = make_frn(next, 1);
                        adds.push(FileRecord { frn, parent: dir_frn, name, attrs: a, size: (!item.is_dir).then_some(item.size), mtime: Some(mtime) });
                        if item.is_dir && !item.is_link && !src.is_excluded(&abs.join(&item.name)) {
                            src.walk_from(abs.join(&item.name), frn, comps.len() + 1, &mut next, &mut |r| {
                                adds.push(r);
                                true
                            });
                        }
                    }
                }
            }
            // Names no longer present (case changes arrive as remove + add).
            removes.extend(existing.into_values().map(|c| c.frn));
        }
        report.unreadable.extend(std::mem::take(&mut src.report.skipped));
        // 3. Apply atomically: memory, then one store transaction.
        let mut ch = Changes::default();
        {
            let mut t = table.write().unwrap();
            for frn in &removes {
                self.index.remove_file(&mut t, *frn, &mut ch);
            }
            for r in &adds {
                self.index.apply_record(&mut t, r, &mut ch);
            }
            for &(frn, a, size, mtime) in &updates {
                self.index.set_meta(&mut t, frn, a, size, mtime, &mut ch);
            }
        }
        self.synthetic_next = next;
        report.added = adds.len() as u64;
        report.removed = removes.len() as u64;
        report.updated = updates.len() as u64;
        let state = if report.unreadable.is_empty() {
            None
        } else {
            let skipped = match &self.state {
                VolumeState::Traversal { skipped, .. } => skipped + report.unreadable.len() as u64,
                _ => report.unreadable.len() as u64,
            };
            Some(VolumeState::Traversal { complete: false, skipped })
        };
        if !ch.ops.is_empty() || state.is_some() {
            let r = store.commit(self.db_id, false, &ch.ops, state.as_ref());
            self.consistent = r.is_ok();
            r?;
        }
        if let Some(s) = state {
            self.state = s;
        }
        Ok(report)
    }
}

/// `FILE_NOTIFY_INFORMATION` actions (winnt.h).
pub mod action {
    pub const ADDED: u32 = 1;
    pub const REMOVED: u32 = 2;
    pub const MODIFIED: u32 = 3;
    pub const RENAMED_OLD: u32 = 4;
    pub const RENAMED_NEW: u32 = 5;
}

/// Parses a `ReadDirectoryChangesW` buffer into (action, relative path).
/// Bounds-checked; stops at the first malformed entry.
pub fn parse_notify_buffer(b: &[u8]) -> Vec<(u32, PathBuf)> {
    let mut out = Vec::new();
    let mut off = 0usize;
    while let Some(h) = b.get(off..off + 12) {
        let next = u32::from_le_bytes(h[0..4].try_into().unwrap()) as usize;
        let act = u32::from_le_bytes(h[4..8].try_into().unwrap());
        let len = u32::from_le_bytes(h[8..12].try_into().unwrap()) as usize;
        let Some(name) = b.get(off + 12..off + 12 + len).filter(|_| len.is_multiple_of(2)) else { break };
        let units: Vec<u16> = (0..name.len() / 2).map(|i| u16::from_le_bytes([name[2 * i], name[2 * i + 1]])).collect();
        out.push((act, PathBuf::from(String::from_utf16_lossy(&units).replace('\\', "/"))));
        if next == 0 {
            break;
        }
        off = match off.checked_add(next) {
            Some(o) if next >= 12 => o,
            _ => break,
        };
    }
    out
}

/// Directories to reconcile for a batch of events: the parent of each
/// changed path (a parent re-list sees additions, removals, renames and
/// metadata changes of its children). Deduplicated.
pub fn dirs_for_events(events: &[(u32, PathBuf)]) -> Vec<PathBuf> {
    let set: BTreeSet<PathBuf> = events.iter().map(|(_, p)| p.parent().map(Path::to_path_buf).unwrap_or_default()).collect();
    set.into_iter().collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(next: u32, act: u32, name: &str) -> Vec<u8> {
        let units: Vec<u16> = name.encode_utf16().collect();
        let mut b = Vec::new();
        b.extend_from_slice(&next.to_le_bytes());
        b.extend_from_slice(&act.to_le_bytes());
        b.extend_from_slice(&((units.len() * 2) as u32).to_le_bytes());
        for u in units {
            b.extend_from_slice(&u.to_le_bytes());
        }
        while !b.len().is_multiple_of(4) {
            b.push(0);
        }
        b
    }

    #[test]
    fn parses_notify_buffers_and_derives_dirs() {
        let a = entry(0, action::RENAMED_NEW, "Docs\\Résumé.pdf");
        let len_first = entry(0, action::ADDED, "top.txt").len() as u32;
        let mut buf = entry(len_first, action::ADDED, "top.txt");
        buf.extend(a);
        let ev = parse_notify_buffer(&buf);
        assert_eq!(ev, vec![(action::ADDED, PathBuf::from("top.txt")), (action::RENAMED_NEW, PathBuf::from("Docs/Résumé.pdf"))]);
        assert_eq!(dirs_for_events(&ev), vec![PathBuf::from(""), PathBuf::from("Docs")]);
        // Malformed / truncated buffers never panic.
        for cut in 0..buf.len() {
            let _ = parse_notify_buffer(&buf[..cut]);
        }
        let mut bad = buf.clone();
        bad[0] = 3; // next offset smaller than a header
        assert_eq!(parse_notify_buffer(&bad).len(), 1);
    }
}
