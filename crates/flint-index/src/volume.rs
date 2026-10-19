//! Per-volume mapping between file reference numbers and name-table slots,
//! and the state machine that applies enumeration and USN records.
//!
//! Every mutation also yields [`StoreOp`]s (so the persistent store mirrors the
//! in-memory index inside the same transaction as the checkpoint) and
//! [`Followup`]s: questions only the live filesystem can answer (current hard
//! links, size/mtime, an unknown parent). Application is idempotent, which is
//! what makes replay-after-crash from the last committed checkpoint safe.

#![allow(clippy::too_many_arguments)] // internal helpers mirror record fields

use crate::model::*;
use flint_fuzzy::table::{flags, EntryId, Meta, NameTable, NO_PARENT, NO_TIME};
use std::collections::HashMap;

const NONE: EntryId = u32::MAX;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StoreOp {
    PutLink { frn: Frn, parent: Frn, name: String, attrs: u32, size: Option<u64>, mtime: Option<i64> },
    DelLink { frn: Frn, parent: Frn, name: String },
    DelFile { frn: Frn },
    SetMeta { frn: Frn, attrs: u32, size: Option<u64>, mtime: Option<i64> },
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Followup {
    /// Re-read size/mtime/attributes.
    Meta(Frn),
    /// Re-read the full set of hard links.
    Links(Frn),
    /// A record referenced a parent directory the index does not know.
    Parent(Frn),
}

#[derive(Default, Debug)]
pub struct Changes {
    pub ops: Vec<StoreOp>,
    pub followups: Vec<Followup>,
    /// Situations that imply missed or contradictory records (e.g. a reused
    /// file ID whose deletion we never saw). Reported in diagnostics.
    pub inconsistencies: u64,
}

impl Changes {
    pub fn is_empty(&self) -> bool {
        self.ops.is_empty() && self.followups.is_empty()
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum LinkMode {
    /// Enumeration / link refresh: an unknown name is an additional link.
    Add,
    /// Journal: an unknown name for a single-link file means we missed the
    /// rename's old-name record, so move the existing link.
    MoveIfSingle,
}

pub fn table_flags(attrs: u32) -> u8 {
    let mut f = 0;
    if attrs & attr::DIRECTORY != 0 {
        f |= flags::DIR;
    }
    if attrs & attr::HIDDEN != 0 {
        f |= flags::HIDDEN;
    }
    if attrs & attr::SYSTEM != 0 {
        f |= flags::SYSTEM;
    }
    if attrs & attr::REPARSE_POINT != 0 {
        f |= flags::REPARSE;
    }
    f
}

fn attrs_of(fl: u8) -> u32 {
    let mut a = 0;
    if fl & flags::DIR != 0 {
        a |= attr::DIRECTORY;
    }
    if fl & flags::HIDDEN != 0 {
        a |= attr::HIDDEN;
    }
    if fl & flags::SYSTEM != 0 {
        a |= attr::SYSTEM;
    }
    if fl & flags::REPARSE != 0 {
        a |= attr::REPARSE_POINT;
    }
    a
}

pub struct VolumeIndex {
    pub volume: u16,
    pub root_frn: Frn,
    root_id: EntryId,
    by_segment: Vec<EntryId>,
    extra_links: HashMap<u64, Vec<EntryId>>,
    /// Slots whose parent directory is not (yet) known, keyed by parent FRN.
    orphans: HashMap<Frn, Vec<EntryId>>,
    pending_rename: HashMap<Frn, (Frn, String)>,
}

impl VolumeIndex {
    /// Creates the volume and its root slot (displayed as `root_name`, e.g. `C:`).
    pub fn new(t: &mut NameTable, volume: u16, root_frn: Frn, root_name: &str) -> Self {
        let root_id = t.push_keyed(NO_PARENT, root_name, Meta::dir(volume), root_frn);
        let mut v = VolumeIndex { volume, root_frn, root_id, by_segment: Vec::new(), extra_links: HashMap::new(), orphans: HashMap::new(), pending_rename: HashMap::new() };
        v.register(segment(root_frn), root_id, true);
        v
    }

    /// Rebuilds the FRN mapping for slots `base..base+n` just imported from a
    /// snapshot. Fails (caller falls back to SQLite) unless exactly one root
    /// with `root_frn` exists and every other slot has a parent.
    pub fn adopt(t: &NameTable, volume: u16, root_frn: Frn, base: EntryId, n: usize) -> Result<Self, String> {
        let mut root_id = None;
        let mut v = VolumeIndex { volume, root_frn, root_id: NONE, by_segment: Vec::new(), extra_links: HashMap::new(), orphans: HashMap::new(), pending_rename: HashMap::new() };
        for id in base..base + n as EntryId {
            if t.parent(id) == NO_PARENT {
                if t.key(id) != root_frn || root_id.is_some() {
                    return Err(format!("unexpected parentless slot {id}"));
                }
                root_id = Some(id);
            }
            let seg = segment(t.key(id));
            let primary = v.by_segment.get(seg as usize).is_none_or(|&p| p == NONE);
            v.register(seg, id, primary);
        }
        v.root_id = root_id.ok_or("snapshot has no root")?;
        let r = v.root_id;
        v.register(segment(root_frn), r, true);
        Ok(v)
    }

    pub fn root(&self) -> EntryId {
        self.root_id
    }

    /// Parent FRNs that orphaned slots are waiting for.
    pub fn orphan_parents(&self) -> Vec<Frn> {
        let mut v: Vec<Frn> = self.orphans.iter().filter(|(_, k)| !k.is_empty()).map(|(p, _)| *p).collect();
        v.sort_unstable();
        v
    }

    /// FRNs of files whose size/mtime are not known.
    pub fn stale_meta(&self, t: &NameTable) -> Vec<Frn> {
        let mut v: Vec<Frn> = (0..t.len() as EntryId)
            .filter(|&id| t.is_live(id) && t.volume(id) == self.volume && t.flags(id) & flags::META_STALE != 0)
            .map(|id| t.key(id))
            .collect();
        v.sort_unstable();
        v.dedup();
        v
    }

    pub fn orphan_count(&self) -> usize {
        self.orphans.values().map(Vec::len).sum()
    }

    fn register(&mut self, seg: u64, id: EntryId, primary: bool) {
        let s = seg as usize;
        if s >= self.by_segment.len() {
            self.by_segment.resize(s + 1, NONE);
        }
        if primary || self.by_segment[s] == NONE {
            self.by_segment[s] = id;
        } else {
            self.extra_links.entry(seg).or_default().push(id);
        }
    }

    fn unregister(&mut self, seg: u64, id: EntryId) {
        let s = seg as usize;
        let extra = self.extra_links.get_mut(&seg);
        if s < self.by_segment.len() && self.by_segment[s] == id {
            // Promote an extra link to primary, if any.
            self.by_segment[s] = match extra {
                Some(v) if !v.is_empty() => v.remove(0),
                _ => NONE,
            };
        } else if let Some(v) = extra {
            v.retain(|&e| e != id);
        }
        if self.extra_links.get(&seg).is_some_and(Vec::is_empty) {
            self.extra_links.remove(&seg);
        }
    }

    /// All live slots of `frn` (exact sequence match).
    pub fn links(&self, t: &NameTable, frn: Frn) -> Vec<EntryId> {
        let seg = segment(frn);
        let mut v = Vec::new();
        if let Some(&p) = self.by_segment.get(seg as usize) {
            if p != NONE && t.is_live(p) && t.key(p) == frn {
                v.push(p);
                if let Some(x) = self.extra_links.get(&seg) {
                    v.extend(x.iter().copied().filter(|&e| t.is_live(e) && t.key(e) == frn));
                }
            }
        }
        v
    }

    /// The live slot currently occupying `frn`'s segment, with whatever
    /// sequence number it has.
    fn occupant(&self, t: &NameTable, frn: Frn) -> Option<EntryId> {
        match self.by_segment.get(segment(frn) as usize) {
            Some(&p) if p != NONE && t.is_live(p) => Some(p),
            _ => None,
        }
    }

    fn dir_entry(&self, t: &NameTable, frn: Frn) -> Option<EntryId> {
        if frn == self.root_frn {
            return Some(self.root_id);
        }
        self.links(t, frn).first().copied().filter(|&e| t.flags(e) & flags::DIR != 0)
    }

    fn parent_frn_of(&self, t: &NameTable, id: EntryId) -> Frn {
        match t.parent(id) {
            NO_PARENT => self.orphans.iter().find(|(_, v)| v.contains(&id)).map_or(0, |(k, _)| *k),
            p => t.key(p),
        }
    }

    fn find_link(&self, t: &NameTable, frn: Frn, parent: Frn, name: &str) -> Option<EntryId> {
        let links = self.links(t, frn);
        let same_parent = |&&e: &&EntryId| self.parent_frn_of(t, e) == parent;
        links
            .iter()
            .filter(same_parent)
            .find(|&&e| t.name(e) == name)
            .or_else(|| links.iter().filter(same_parent).find(|&&e| t.name(e).to_lowercase() == name.to_lowercase()))
            .copied()
    }

    fn meta_of(&self, t: &NameTable, id: EntryId) -> (Option<u64>, Option<i64>) {
        let m = t.meta(id);
        if m.flags & flags::META_STALE != 0 {
            (None, None)
        } else {
            (Some(m.size), (m.mtime != NO_TIME).then_some(m.mtime))
        }
    }

    fn make_meta(&self, attrs: u32, size: Option<u64>, mtime: Option<i64>) -> Meta {
        let mut f = table_flags(attrs);
        if attrs & attr::DIRECTORY == 0 && (size.is_none() || mtime.is_none()) {
            f |= flags::META_STALE;
        }
        Meta { size: size.unwrap_or(0), mtime: mtime.unwrap_or(NO_TIME), flags: f, volume: self.volume }
    }

    fn attach(&mut self, t: &mut NameTable, id: EntryId, parent: Frn, ch: &mut Changes) {
        match self.dir_entry(t, parent) {
            Some(p) => {
                let name = t.name(id).to_owned();
                t.rename(id, p, &name);
            }
            None => {
                let name = t.name(id).to_owned();
                t.rename(id, NO_PARENT, &name);
                self.orphans.entry(parent).or_default().push(id);
                ch.followups.push(Followup::Parent(parent));
            }
        }
    }

    fn adopt_orphans(&mut self, t: &mut NameTable, dir_frn: Frn, dir_id: EntryId) {
        if let Some(kids) = self.orphans.remove(&dir_frn) {
            for k in kids {
                if t.is_live(k) {
                    let name = t.name(k).to_owned();
                    t.rename(k, dir_id, &name);
                }
            }
        }
    }

    fn new_link(&mut self, t: &mut NameTable, frn: Frn, parent: Frn, name: &str, attrs: u32, size: Option<u64>, mtime: Option<i64>, ch: &mut Changes) -> EntryId {
        let meta = self.make_meta(attrs, size, mtime);
        let id = t.push_keyed(NO_PARENT, name, meta, frn);
        let primary = self.links(t, frn).iter().all(|&e| e == id);
        self.register(segment(frn), id, primary);
        self.attach(t, id, parent, ch);
        if attrs & attr::DIRECTORY != 0 {
            self.adopt_orphans(t, frn, id);
        }
        if meta.flags & flags::META_STALE != 0 {
            ch.followups.push(Followup::Meta(frn));
        }
        ch.ops.push(StoreOp::PutLink { frn, parent, name: name.to_owned(), attrs, size, mtime });
        id
    }

    fn drop_slot(&mut self, t: &mut NameTable, id: EntryId) {
        let frn = t.key(id);
        t.remove(id);
        self.unregister(segment(frn), id);
        for v in self.orphans.values_mut() {
            v.retain(|&e| e != id);
        }
    }

    /// Removes a slot; if it is a directory that still has children in the
    /// index, they are removed too (NTFS cannot delete a non-empty directory,
    /// so that means we missed their deletions).
    fn remove_slot_tree(&mut self, t: &mut NameTable, id: EntryId, ch: &mut Changes) {
        if t.flags(id) & flags::DIR != 0 {
            let mut stack = t.children_of(id);
            while let Some(c) = stack.pop() {
                ch.inconsistencies += 1;
                if t.flags(c) & flags::DIR != 0 {
                    stack.extend(t.children_of(c));
                }
                let cfrn = t.key(c);
                ch.ops.push(StoreOp::DelLink { frn: cfrn, parent: t.key(t.parent(c)), name: t.name(c).to_owned() });
                self.drop_slot(t, c);
            }
        }
        self.drop_slot(t, id);
    }

    /// Removes every link of `frn`.
    pub fn remove_file(&mut self, t: &mut NameTable, frn: Frn, ch: &mut Changes) {
        if frn == self.root_frn {
            return;
        }
        let links = self.links(t, frn);
        for id in links {
            self.remove_slot_tree(t, id, ch);
        }
        self.pending_rename.remove(&frn);
        ch.ops.push(StoreOp::DelFile { frn });
    }

    pub fn remove_link(&mut self, t: &mut NameTable, frn: Frn, parent: Frn, name: &str, ch: &mut Changes) {
        if let Some(id) = self.find_link(t, frn, parent, name) {
            let n = t.name(id).to_owned();
            self.remove_slot_tree(t, id, ch);
            ch.ops.push(StoreOp::DelLink { frn, parent, name: n });
        }
    }

    fn upsert(&mut self, t: &mut NameTable, frn: Frn, parent: Frn, name: &str, attrs: u32, size: Option<u64>, mtime: Option<i64>, mode: LinkMode, ch: &mut Changes) {
        if frn == self.root_frn {
            return;
        }
        if let Some(occ) = self.occupant(t, frn) {
            if t.key(occ) != frn {
                // Same MFT segment, different sequence: the old file was
                // deleted and its record reused. Drop the stale entry.
                ch.inconsistencies += 1;
                let old = t.key(occ);
                self.remove_file(t, old, ch);
            }
        }
        let links = self.links(t, frn);
        if links.is_empty() {
            self.new_link(t, frn, parent, name, attrs, size, mtime, ch);
            return;
        }
        if let Some(id) = self.find_link(t, frn, parent, name) {
            let (os, om) = self.meta_of(t, id);
            let (size, mtime) = (size.or(os), mtime.or(om));
            let meta = self.make_meta(attrs, size, mtime);
            let changed = t.meta(id) != meta || t.name(id) != name;
            if t.name(id) != name {
                // Case-only rename.
                let p = t.parent(id);
                t.rename(id, p, name);
            }
            for &l in &links {
                t.set_meta(l, meta);
            }
            if changed {
                ch.ops.push(StoreOp::PutLink { frn, parent, name: name.to_owned(), attrs, size, mtime });
            }
            return;
        }
        match (mode, links.as_slice()) {
            (LinkMode::MoveIfSingle, [only]) => {
                let id = *only;
                let old_parent = self.parent_frn_of(t, id);
                let old_name = t.name(id).to_owned();
                self.move_slot(t, id, frn, old_parent, &old_name, parent, name, attrs, ch);
            }
            (LinkMode::MoveIfSingle, _) => {
                let (s, m) = self.meta_of(t, links[0]);
                self.new_link(t, frn, parent, name, attrs, size.or(s), mtime.or(m), ch);
                ch.followups.push(Followup::Links(frn));
            }
            (LinkMode::Add, _) => {
                let (s, m) = self.meta_of(t, links[0]);
                self.new_link(t, frn, parent, name, attrs, size.or(s), mtime.or(m), ch);
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn move_slot(&mut self, t: &mut NameTable, id: EntryId, frn: Frn, old_parent: Frn, old_name: &str, parent: Frn, name: &str, attrs: u32, ch: &mut Changes) {
        for v in self.orphans.values_mut() {
            v.retain(|&e| e != id);
        }
        let pid = t.parent(id);
        t.rename(id, pid, name);
        self.attach(t, id, parent, ch);
        let (size, mtime) = self.meta_of(t, id);
        let meta = self.make_meta(attrs, size, mtime);
        t.set_meta(id, meta);
        ch.ops.push(StoreOp::DelLink { frn, parent: old_parent, name: old_name.to_owned() });
        ch.ops.push(StoreOp::PutLink { frn, parent, name: name.to_owned(), attrs, size, mtime });
    }

    /// Applies one enumerated link (initial index or link refresh).
    pub fn apply_record(&mut self, t: &mut NameTable, r: &FileRecord, ch: &mut Changes) {
        self.upsert(t, r.frn, r.parent, &r.name, r.attrs, r.size, r.mtime, LinkMode::Add, ch);
    }

    /// Applies one change-journal record.
    pub fn apply_usn(&mut self, t: &mut NameTable, r: &UsnRecord, ch: &mut Changes) {
        use reason::*;
        if r.frn == self.root_frn {
            return;
        }
        let close = r.reason & CLOSE != 0;
        if r.reason & RENAME_OLD_NAME != 0 {
            self.pending_rename.insert(r.frn, (r.parent, r.name.clone()));
            return;
        }
        if r.reason & FILE_DELETE != 0 {
            if close {
                if r.reason & HARD_LINK_CHANGE != 0 {
                    // One name removed; other links we may not know about can
                    // remain, so let the adapter report the true link set.
                    self.remove_link(t, r.frn, r.parent, &r.name, ch);
                    ch.followups.push(Followup::Links(r.frn));
                } else {
                    self.remove_file(t, r.frn, ch);
                }
            }
            return;
        }
        if r.reason & RENAME_NEW_NAME != 0 {
            if let Some((op, on)) = self.pending_rename.remove(&r.frn) {
                if let Some(id) = self.find_link(t, r.frn, op, &on) {
                    let old_name = t.name(id).to_owned();
                    self.move_slot(t, id, r.frn, op, &old_name, r.parent, &r.name, r.attrs, ch);
                }
            }
            self.upsert(t, r.frn, r.parent, &r.name, r.attrs, None, None, LinkMode::MoveIfSingle, ch);
        } else if r.reason & HARD_LINK_CHANGE != 0 {
            // Creation and removal of a non-last link look alike; add the name
            // if unknown and let the adapter re-read the true link set.
            self.upsert(t, r.frn, r.parent, &r.name, r.attrs, None, None, LinkMode::Add, ch);
            ch.followups.push(Followup::Links(r.frn));
        } else {
            self.upsert(t, r.frn, r.parent, &r.name, r.attrs, None, None, LinkMode::MoveIfSingle, ch);
        }
        if r.reason & reason::META != 0 && r.attrs & attr::DIRECTORY == 0 {
            for id in self.links(t, r.frn) {
                let mut m = t.meta(id);
                if m.flags & flags::META_STALE == 0 {
                    m.flags |= flags::META_STALE;
                    t.set_meta(id, m);
                }
            }
            ch.followups.push(Followup::Meta(r.frn));
        }
    }

    /// Resolution of [`Followup::Meta`].
    pub fn set_meta(&mut self, t: &mut NameTable, frn: Frn, attrs: u32, size: u64, mtime: i64, ch: &mut Changes) {
        let links = self.links(t, frn);
        if links.is_empty() {
            return;
        }
        let meta = self.make_meta(attrs, Some(size), Some(mtime));
        for &l in &links {
            t.set_meta(l, meta);
        }
        ch.ops.push(StoreOp::SetMeta { frn, attrs, size: Some(size), mtime: Some(mtime) });
    }

    /// Resolution of [`Followup::Links`]: makes the link set exactly `actual`
    /// (empty means the file no longer exists).
    pub fn set_links(&mut self, t: &mut NameTable, frn: Frn, actual: &[FileRecord], ch: &mut Changes) {
        if actual.is_empty() {
            self.remove_file(t, frn, ch);
            return;
        }
        for id in self.links(t, frn) {
            let p = self.parent_frn_of(t, id);
            let n = t.name(id).to_owned();
            if !actual.iter().any(|r| r.parent == p && r.name.to_lowercase() == n.to_lowercase()) {
                self.remove_slot_tree(t, id, ch);
                ch.ops.push(StoreOp::DelLink { frn, parent: p, name: n });
            }
        }
        for r in actual {
            self.apply_record(t, r, ch);
        }
    }

    /// Every live link path in this volume (test oracle / diagnostics).
    pub fn paths(&self, t: &NameTable) -> std::collections::BTreeSet<String> {
        (0..t.len() as EntryId).filter(|&id| t.is_live(id) && t.volume(id) == self.volume && id != self.root_id).map(|id| t.path(id)).collect()
    }

    /// Drops every slot of this volume except the root (before a rescan).
    pub fn clear(&mut self, t: &mut NameTable) {
        for id in 0..t.len() as EntryId {
            if t.is_live(id) && t.volume(id) == self.volume && id != self.root_id {
                t.remove(id);
            }
        }
        self.by_segment.clear();
        self.extra_links.clear();
        self.orphans.clear();
        self.pending_rename.clear();
        let root = self.root_id;
        self.register(segment(self.root_frn), root, true);
    }

    /// Stored attributes for a slot (used when persisting).
    pub fn attrs(&self, t: &NameTable, id: EntryId) -> u32 {
        attrs_of(t.flags(id))
    }
}
