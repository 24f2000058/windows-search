//! Columnar, memory-compact name table that the search engine scans.
//!
//! Each slot is one *name link* (a file with several hard links has several
//! slots). Paths are not stored; they are reconstructed from parent links, so
//! renaming or moving a directory is O(1) regardless of subtree size.

use crate::fold::{fold_str, mask_of};
use std::sync::{Arc, Mutex};

pub type EntryId = u32;
pub const NO_PARENT: EntryId = u32::MAX;
/// Guard against corrupt parent cycles when walking ancestors.
pub const MAX_DEPTH: usize = 2048;

pub mod flags {
    pub const DIR: u8 = 1;
    pub const HIDDEN: u8 = 2;
    pub const SYSTEM: u8 = 4;
    pub const REPARSE: u8 = 8;
    /// Size/mtime not yet refreshed after a change (USN records carry neither).
    pub const META_STALE: u8 = 16;
    pub const DEAD: u8 = 128;
}

/// Unknown modification time.
pub const NO_TIME: i64 = i64::MIN;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Meta {
    pub size: u64,
    /// Unix seconds (UTC), or [`NO_TIME`].
    pub mtime: i64,
    pub flags: u8,
    pub volume: u16,
}

impl Meta {
    pub fn dir(volume: u16) -> Self {
        Meta { size: 0, mtime: NO_TIME, flags: flags::DIR, volume }
    }
    pub fn file(volume: u16, size: u64, mtime: i64) -> Self {
        Meta { size, mtime, flags: 0, volume }
    }
}

#[derive(Default)]
pub struct NameTable {
    /// UTF-8 name bytes. Invariant: every (offset, len) span stored in this
    /// table covers exactly one whole string pushed by `intern`.
    arena: Vec<u8>,
    name_off: Vec<u32>,
    name_len: Vec<u16>,
    folded_off: Vec<u32>,
    folded_len: Vec<u16>,
    parent: Vec<EntryId>,
    mask: Vec<u64>,
    size: Vec<u64>,
    mtime: Vec<i64>,
    flags: Vec<u8>,
    volume: Vec<u16>,
    live: usize,
    dead_bytes: usize,
    generation: u64,
    chain_cache: Mutex<Option<Arc<Vec<u64>>>>,
    dir_cache: Mutex<Option<Arc<DirIndex>>>,
    /// Path separator used when rendering paths.
    pub sep: char,
}

impl NameTable {
    pub fn new() -> Self {
        NameTable { sep: '\\', ..Default::default() }
    }

    /// Number of slots (including dead ones); valid ids are `0..len()`.
    pub fn len(&self) -> usize {
        self.parent.len()
    }
    pub fn is_empty(&self) -> bool {
        self.live == 0
    }
    pub fn live_count(&self) -> usize {
        self.live
    }
    /// Incremented on every mutation; used to invalidate result caches.
    pub fn generation(&self) -> u64 {
        self.generation
    }

    fn intern(&mut self, name: &str) -> (u32, u16, u32, u16) {
        assert!(name.len() <= u16::MAX as usize, "name too long");
        assert!(self.arena.len() + 2 * name.len() < u32::MAX as usize, "name arena full");
        let off = self.arena.len() as u32;
        self.arena.extend_from_slice(name.as_bytes());
        let folded = fold_str(name);
        if folded == name {
            (off, name.len() as u16, off, name.len() as u16)
        } else {
            let foff = self.arena.len() as u32;
            self.arena.extend_from_slice(folded.as_bytes());
            (off, name.len() as u16, foff, folded.len() as u16)
        }
    }

    fn touch(&mut self, structural: bool) {
        self.generation += 1;
        if structural {
            *self.chain_cache.get_mut().unwrap() = None;
            *self.dir_cache.get_mut().unwrap() = None;
        }
    }

    pub fn push(&mut self, parent: EntryId, name: &str, meta: Meta) -> EntryId {
        let id = self.parent.len() as EntryId;
        assert!(id != NO_PARENT, "table full");
        let (no, nl, fo, fl) = self.intern(name);
        self.name_off.push(no);
        self.name_len.push(nl);
        self.folded_off.push(fo);
        self.folded_len.push(fl);
        self.mask.push(mask_of(self.span(fo, fl)));
        self.parent.push(parent);
        self.size.push(meta.size);
        self.mtime.push(meta.mtime);
        self.flags.push(meta.flags & !flags::DEAD);
        self.volume.push(meta.volume);
        self.live += 1;
        self.touch(true);
        id
    }

    pub fn remove(&mut self, id: EntryId) {
        let i = id as usize;
        if self.flags[i] & flags::DEAD == 0 {
            self.flags[i] |= flags::DEAD;
            self.live -= 1;
            self.dead_bytes += self.name_len[i] as usize;
            self.touch(true);
        }
    }

    /// Renames and/or moves a slot. Descendants follow automatically.
    pub fn rename(&mut self, id: EntryId, parent: EntryId, name: &str) {
        let i = id as usize;
        if self.name(id) != name {
            self.dead_bytes += self.name_len[i] as usize;
            let (no, nl, fo, fl) = self.intern(name);
            self.name_off[i] = no;
            self.name_len[i] = nl;
            self.folded_off[i] = fo;
            self.folded_len[i] = fl;
            self.mask[i] = mask_of(self.span(fo, fl));
        }
        self.parent[i] = parent;
        self.touch(true);
    }

    pub fn set_meta(&mut self, id: EntryId, meta: Meta) {
        let i = id as usize;
        let dead = self.flags[i] & flags::DEAD;
        let dir_changed = (self.flags[i] ^ meta.flags) & flags::DIR != 0;
        self.size[i] = meta.size;
        self.mtime[i] = meta.mtime;
        self.flags[i] = (meta.flags & !flags::DEAD) | dead;
        self.volume[i] = meta.volume;
        self.touch(dir_changed);
    }

    #[inline]
    pub fn is_live(&self, id: EntryId) -> bool {
        self.flags[id as usize] & flags::DEAD == 0
    }
    #[inline]
    fn span(&self, off: u32, len: u16) -> &str {
        let bytes = &self.arena[off as usize..off as usize + len as usize];
        // SAFETY: spans only ever cover whole strings appended by `intern`
        // (see the `arena` invariant), so they are valid UTF-8. Skipping the
        // re-validation/char-boundary checks matters in the scan loop.
        unsafe { std::str::from_utf8_unchecked(bytes) }
    }
    #[inline]
    pub fn name(&self, id: EntryId) -> &str {
        let i = id as usize;
        self.span(self.name_off[i], self.name_len[i])
    }
    #[inline]
    pub fn folded(&self, id: EntryId) -> &str {
        let i = id as usize;
        self.span(self.folded_off[i], self.folded_len[i])
    }
    #[inline]
    pub fn parent(&self, id: EntryId) -> EntryId {
        self.parent[id as usize]
    }
    #[inline]
    pub fn mask(&self, id: EntryId) -> u64 {
        self.mask[id as usize]
    }
    #[inline]
    pub fn meta(&self, id: EntryId) -> Meta {
        let i = id as usize;
        Meta { size: self.size[i], mtime: self.mtime[i], flags: self.flags[i], volume: self.volume[i] }
    }
    #[inline]
    pub fn flags(&self, id: EntryId) -> u8 {
        self.flags[id as usize]
    }

    /// Ancestors from the immediate parent up to the root (cycle-safe).
    pub fn ancestors(&self, id: EntryId) -> impl Iterator<Item = EntryId> + '_ {
        let mut cur = self.parent(id);
        let mut depth = 0;
        std::iter::from_fn(move || {
            if cur == NO_PARENT || depth >= MAX_DEPTH || cur as usize >= self.len() {
                return None;
            }
            depth += 1;
            let r = cur;
            cur = self.parent(cur);
            Some(r)
        })
    }

    /// Full display path of a slot.
    pub fn path(&self, id: EntryId) -> String {
        let mut parts: Vec<&str> = self.ancestors(id).map(|a| self.name(a)).collect();
        parts.reverse();
        parts.push(self.name(id));
        join_path(&parts, self.sep)
    }

    /// Display path of the containing directory ("" for roots).
    pub fn parent_path(&self, id: EntryId) -> String {
        match self.parent(id) {
            NO_PARENT => String::new(),
            p => self.path(p),
        }
    }

    /// Folded path of the containing directory, with the same char count as
    /// [`parent_path`](Self::parent_path).
    pub fn parent_path_folded(&self, id: EntryId) -> String {
        let mut parts: Vec<&str> = self.ancestors(id).map(|a| self.folded(a)).collect();
        parts.reverse();
        join_path(&parts, self.sep)
    }

    /// Per-slot OR of the character masks of the slot and all its ancestors.
    /// Cached; recomputed lazily after structural changes (O(n)).
    pub fn chain_masks(&self) -> Arc<Vec<u64>> {
        let mut cache = self.chain_cache.lock().unwrap();
        if let Some(c) = cache.as_ref() {
            return c.clone();
        }
        let n = self.len();
        let mut chain = vec![0u64; n];
        let mut done = vec![false; n];
        let mut stack = Vec::new();
        for start in 0..n {
            if done[start] {
                continue;
            }
            let mut cur = start as EntryId;
            while cur != NO_PARENT && (cur as usize) < n && !done[cur as usize] && stack.len() < MAX_DEPTH {
                stack.push(cur);
                cur = self.parent(cur);
            }
            let mut acc = if cur != NO_PARENT && (cur as usize) < n && done[cur as usize] { chain[cur as usize] } else { 0 };
            while let Some(e) = stack.pop() {
                acc |= self.mask(e);
                chain[e as usize] = acc;
                done[e as usize] = true;
            }
        }
        let arc = Arc::new(chain);
        *cache = Some(arc.clone());
        arc
    }

    /// Dense directory numbering, cached until the next structural change.
    pub fn dir_index(&self) -> Arc<DirIndex> {
        let mut cache = self.dir_cache.lock().unwrap();
        if let Some(c) = cache.as_ref() {
            return c.clone();
        }
        let n = self.len();
        let mut d = DirIndex { dirs: Vec::new(), ord: vec![NOT_DIR; n], parent_ord: Vec::new(), mask: Vec::new(), names: String::new(), name_end: Vec::new() };
        for id in 0..n {
            if self.flags[id] & (flags::DIR | flags::DEAD) == flags::DIR {
                d.ord[id] = d.dirs.len() as u32;
                d.dirs.push(id as EntryId);
            }
        }
        for &id in &d.dirs {
            let p = self.parent(id);
            d.parent_ord.push(if p == NO_PARENT || p as usize >= n { NOT_DIR } else { d.ord[p as usize] });
            d.mask.push(self.mask(id));
            d.names.push_str(self.folded(id));
            d.name_end.push(d.names.len() as u32);
        }
        let arc = Arc::new(d);
        *cache = Some(arc.clone());
        arc
    }

    /// Drops derived caches (chain masks, directory index) to lower idle
    /// memory; they are rebuilt on demand.
    pub fn trim_caches(&self) {
        *self.chain_cache.lock().unwrap() = None;
        *self.dir_cache.lock().unwrap() = None;
    }

    /// Approximate heap bytes used by the table.
    pub fn heap_bytes(&self) -> usize {
        self.arena.capacity()
            + self.parent.capacity() * (4 + 4 + 2 + 4 + 2 + 8 + 8 + 8 + 1 + 2)
            + self.chain_cache.lock().unwrap().as_ref().map_or(0, |c| c.len() * 8)
            + self.dir_cache.lock().unwrap().as_ref().map_or(0, |d| d.dirs.len() * 20 + d.names.len() + d.ord.len() * 4)
    }

    /// Bytes of names that are no longer referenced (compaction hint).
    pub fn dead_bytes(&self) -> usize {
        self.dead_bytes
    }
}

pub const NOT_DIR: u32 = u32::MAX;

/// Live directories numbered densely (by ascending id), with dense copies of
/// what per-query directory computations read, so they sweep a few MB
/// sequentially instead of missing cache on scattered slot columns.
pub struct DirIndex {
    pub dirs: Vec<EntryId>,
    /// Directory ordinal per slot, or [`NOT_DIR`].
    pub ord: Vec<u32>,
    /// Ordinal of each directory's parent, or [`NOT_DIR`] for roots.
    pub parent_ord: Vec<u32>,
    pub mask: Vec<u64>,
    names: String,
    name_end: Vec<u32>,
}

impl DirIndex {
    /// Folded name of directory ordinal `o`.
    #[inline]
    pub fn folded(&self, o: u32) -> &str {
        let o = o as usize;
        let start = if o == 0 { 0 } else { self.name_end[o - 1] as usize };
        &self.names[start..self.name_end[o] as usize]
    }
}

fn join_path(parts: &[&str], sep: char) -> String {
    let mut s = String::new();
    for (i, p) in parts.iter().enumerate() {
        if i > 0 && !s.ends_with(sep) {
            s.push(sep);
        }
        s.push_str(p);
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths_follow_directory_moves() {
        let mut t = NameTable::new();
        let root = t.push(NO_PARENT, "C:", Meta::dir(0));
        let a = t.push(root, "Projects", Meta::dir(0));
        let b = t.push(root, "Archive", Meta::dir(0));
        let f = t.push(a, "Main.rs", Meta::file(0, 10, 0));
        assert_eq!(t.path(f), "C:\\Projects\\Main.rs");
        assert_eq!(t.folded(f), "main.rs");
        t.rename(a, b, "Old Projects");
        assert_eq!(t.path(f), "C:\\Archive\\Old Projects\\Main.rs");
        assert_eq!(t.parent_path(f).chars().count(), t.parent_path_folded(f).chars().count());
        let chain = t.chain_masks();
        assert_eq!(chain[f as usize] & crate::fold::char_bit('v'), crate::fold::char_bit('v'));
    }

    #[test]
    fn parent_cycles_do_not_hang() {
        let mut t = NameTable::new();
        let a = t.push(NO_PARENT, "a", Meta::dir(0));
        let b = t.push(a, "b", Meta::dir(0));
        t.rename(a, b, "a");
        let _ = t.path(b);
        let _ = t.chain_masks();
    }

    #[test]
    fn remove_tracks_live_count() {
        let mut t = NameTable::new();
        let a = t.push(NO_PARENT, "a", Meta::dir(0));
        t.remove(a);
        t.remove(a);
        assert_eq!(t.live_count(), 0);
        assert!(!t.is_live(a));
    }
}
