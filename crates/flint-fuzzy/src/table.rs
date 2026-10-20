//! Columnar, memory-compact name table that the search engine scans.
//!
//! Each slot is one *name link* (a file with several hard links has several
//! slots). Paths are not stored; they are reconstructed from parent links, so
//! renaming or moving a directory is O(1) regardless of subtree size.

use crate::fold::{fold_str, mask_of};
use std::collections::HashMap;
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

/// Longest name a slot can hold, in UTF-8 bytes. NTFS allows 255 UTF-16
/// units (at most 765 bytes); the top bit of the stored folded length is used
/// as a flag.
pub const MAX_NAME_BYTES: usize = 0x7FFF;
/// Stored folded length flag: the folded name follows the display name in
/// the arena (otherwise it is the display name itself).
const FOLDED_SEPARATE: u16 = 0x8000;
/// Stored times are u32 seconds since 1970 (until 2106); these two values
/// mean "unknown" and "see `wide_times`" respectively.
const T_NONE: u32 = u32::MAX;
const T_WIDE: u32 = u32::MAX - 1;

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

/// Columnar layout, ~43 bytes per slot plus name bytes (see ADR-013). The
/// folded name has no offset column: it is either the display name itself or
/// stored directly after it.
#[derive(Default)]
pub struct NameTable {
    /// UTF-8 name bytes. Invariant: every (offset, len) span stored in this
    /// table covers exactly one whole string pushed by `intern`.
    arena: Vec<u8>,
    name_off: Vec<u32>,
    name_len: Vec<u16>,
    /// Folded byte length, with [`FOLDED_SEPARATE`] set when it differs.
    folded_len: Vec<u16>,
    parent: Vec<EntryId>,
    mask: Vec<u64>,
    size: Vec<u64>,
    mtime: Vec<u32>,
    /// Exact times outside the u32 range (pre-1970 or after 2106); rare.
    wide_times: HashMap<EntryId, i64>,
    flags: Vec<u8>,
    volume: Vec<u16>,
    /// Opaque source identity per slot (e.g. NTFS file reference number).
    key: Vec<u64>,
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

    /// Appends a name (and its folded form when different) to the arena,
    /// returning (offset, name length, stored folded length).
    fn intern(&mut self, name: &str) -> (u32, u16, u16) {
        let folded = fold_str(name);
        self.intern_folded(name, &folded)
    }

    fn intern_folded(&mut self, name: &str, folded: &str) -> (u32, u16, u16) {
        assert!(name.len() <= MAX_NAME_BYTES && folded.len() <= MAX_NAME_BYTES, "name too long");
        assert!(self.arena.len() + name.len() + folded.len() < u32::MAX as usize, "name arena full");
        let off = self.arena.len() as u32;
        self.arena.extend_from_slice(name.as_bytes());
        if folded == name {
            (off, name.len() as u16, name.len() as u16)
        } else {
            self.arena.extend_from_slice(folded.as_bytes());
            (off, name.len() as u16, folded.len() as u16 | FOLDED_SEPARATE)
        }
    }

    #[inline]
    fn folded_span(&self, i: usize) -> (u32, u16) {
        let fl = self.folded_len[i];
        if fl & FOLDED_SEPARATE != 0 {
            (self.name_off[i] + self.name_len[i] as u32, fl & !FOLDED_SEPARATE)
        } else {
            (self.name_off[i], fl)
        }
    }

    fn store_time(&mut self, id: EntryId, t: i64) -> u32 {
        if t == NO_TIME {
            self.wide_times.remove(&id);
            T_NONE
        } else if (0..T_WIDE as i64).contains(&t) {
            self.wide_times.remove(&id);
            t as u32
        } else {
            self.wide_times.insert(id, t);
            T_WIDE
        }
    }

    #[inline]
    fn load_time(&self, i: usize) -> i64 {
        match self.mtime[i] {
            T_NONE => NO_TIME,
            T_WIDE => self.wide_times.get(&(i as EntryId)).copied().unwrap_or(NO_TIME),
            t => t as i64,
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
        self.push_keyed(parent, name, meta, 0)
    }

    /// Like [`push`](Self::push), recording an opaque source key for the slot.
    pub fn push_keyed(&mut self, parent: EntryId, name: &str, meta: Meta, key: u64) -> EntryId {
        let id = self.parent.len() as EntryId;
        assert!(id != NO_PARENT, "table full");
        let (no, nl, fl) = self.intern(name);
        self.name_off.push(no);
        self.name_len.push(nl);
        self.folded_len.push(fl);
        let (fo, fl) = self.folded_span(id as usize);
        self.mask.push(mask_of(self.span(fo, fl)));
        self.parent.push(parent);
        self.size.push(meta.size);
        let t = self.store_time(id, meta.mtime);
        self.mtime.push(t);
        self.flags.push(meta.flags & !flags::DEAD);
        self.volume.push(meta.volume);
        self.key.push(key);
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
            let (no, nl, fl) = self.intern(name);
            self.name_off[i] = no;
            self.name_len[i] = nl;
            self.folded_len[i] = fl;
            let (fo, fl) = self.folded_span(i);
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
        self.mtime[i] = self.store_time(id, meta.mtime);
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
        let (fo, fl) = self.folded_span(id as usize);
        self.span(fo, fl)
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
        Meta { size: self.size[i], mtime: self.load_time(i), flags: self.flags[i], volume: self.volume[i] }
    }
    #[inline]
    pub fn flags(&self, id: EntryId) -> u8 {
        self.flags[id as usize]
    }
    #[inline]
    pub fn key(&self, id: EntryId) -> u64 {
        self.key[id as usize]
    }
    #[inline]
    pub fn volume(&self, id: EntryId) -> u16 {
        self.volume[id as usize]
    }

    /// Live slots whose parent is `id`. O(n): intended for rare repair paths
    /// (e.g. a directory deleted while the index still holds children).
    pub fn children_of(&self, id: EntryId) -> Vec<EntryId> {
        (0..self.len() as EntryId).filter(|&c| self.parent[c as usize] == id && self.is_live(c)).collect()
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

    /// Copies one volume's live slots into a self-contained [`Chunk`] with
    /// ids renumbered `0..n` (parents outside the chunk become `NO_PARENT`).
    pub fn export_volume(&self, volume: u16) -> Chunk {
        let ids: Vec<EntryId> = (0..self.len() as EntryId).filter(|&i| self.is_live(i) && self.volume[i as usize] == volume).collect();
        let mut local = std::collections::HashMap::with_capacity(ids.len());
        for (n, &id) in ids.iter().enumerate() {
            local.insert(id, n as u32);
        }
        let mut c = Chunk { arena: Vec::new(), slots: Vec::with_capacity(ids.len()) };
        for &id in &ids {
            let i = id as usize;
            let name_off = c.arena.len() as u32;
            c.arena.extend_from_slice(self.name(id).as_bytes());
            let (folded_off, folded_len) = if self.folded_len[i] & FOLDED_SEPARATE == 0 {
                (name_off, self.folded_len[i])
            } else {
                let o = c.arena.len() as u32;
                c.arena.extend_from_slice(self.folded(id).as_bytes());
                (o, self.folded_len[i] & !FOLDED_SEPARATE)
            };
            c.slots.push(ChunkSlot {
                name_off,
                name_len: self.name_len[i],
                folded_off,
                folded_len,
                parent: local.get(&self.parent[i]).copied().unwrap_or(NO_PARENT),
                size: self.size[i],
                mtime: self.load_time(i),
                flags: self.flags[i],
                key: self.key[i],
            });
        }
        c
    }

    /// Appends a chunk as `volume`, returning the id of its first slot.
    /// Every span is bounds- and UTF-8-checked first (the table later reads
    /// names without re-validation), so a corrupt chunk is rejected whole and
    /// leaves the table untouched.
    pub fn import_chunk(&mut self, volume: u16, c: &Chunk) -> Result<EntryId, ChunkError> {
        let n = c.slots.len();
        let base = self.len();
        if base + n >= NO_PARENT as usize || self.arena.len() + c.arena.len() >= u32::MAX as usize {
            return Err(ChunkError::TooLarge);
        }
        for (i, s) in c.slots.iter().enumerate() {
            for (off, len) in [(s.name_off, s.name_len), (s.folded_off, s.folded_len)] {
                if len as usize > MAX_NAME_BYTES {
                    return Err(ChunkError::BadSpan(i));
                }
                let bytes = c.arena.get(off as usize..off as usize + len as usize).ok_or(ChunkError::BadSpan(i))?;
                std::str::from_utf8(bytes).map_err(|_| ChunkError::BadUtf8(i))?;
            }
            if s.parent != NO_PARENT && s.parent as usize >= n {
                return Err(ChunkError::BadParent(i));
            }
            if s.flags & flags::DEAD != 0 {
                return Err(ChunkError::DeadSlot(i));
            }
        }
        self.arena.reserve(c.arena.len());
        self.name_off.reserve(n);
        self.name_len.reserve(n);
        self.folded_len.reserve(n);
        self.parent.reserve(n);
        self.size.reserve(n);
        self.mtime.reserve(n);
        self.flags.reserve(n);
        self.volume.reserve(n);
        self.key.reserve(n);
        self.mask.reserve(n);
        let text = |off: u32, len: u16| {
            // SAFETY: every span was bounds- and UTF-8-checked above.
            unsafe { std::str::from_utf8_unchecked(&c.arena[off as usize..off as usize + len as usize]) }
        };
        for (k, s) in c.slots.iter().enumerate() {
            let id = (base + k) as EntryId;
            // Re-interned (not bulk-copied) so the folded name sits directly
            // after the display name, which is what the layout relies on.
            let (no, nl, fl) = self.intern_folded(text(s.name_off, s.name_len), text(s.folded_off, s.folded_len));
            self.name_off.push(no);
            self.name_len.push(nl);
            self.folded_len.push(fl);
            self.parent.push(if s.parent == NO_PARENT { NO_PARENT } else { base as EntryId + s.parent });
            self.size.push(s.size);
            let t = self.store_time(id, s.mtime);
            self.mtime.push(t);
            self.flags.push(s.flags);
            self.volume.push(volume);
            self.key.push(s.key);
            // Recomputed rather than trusted: a wrong mask would silently
            // hide matches.
            let (fo, fl) = self.folded_span(id as usize);
            let m = mask_of(self.span(fo, fl));
            self.mask.push(m);
        }
        self.live += n;
        self.touch(true);
        Ok(base as EntryId)
    }

    /// Bytes per slot in the fixed-width columns.
    pub const SLOT_BYTES: usize = 4 + 2 + 2 + 4 + 8 + 8 + 4 + 1 + 2 + 8;

    /// Approximate heap bytes used by the table (by capacity, which is what
    /// Windows commits).
    pub fn heap_bytes(&self) -> usize {
        self.arena.capacity()
            + self.parent.capacity() * Self::SLOT_BYTES
            + self.wide_times.capacity() * 16
            + self.chain_cache.lock().unwrap().as_ref().map_or(0, |c| c.len() * 8)
            + self.dir_cache.lock().unwrap().as_ref().map_or(0, |d| d.dirs.len() * 20 + d.names.len() + d.ord.len() * 4)
    }

    /// Releases spare capacity left by `Vec` growth (up to half of each
    /// column right after a large scan). Called once a bulk load settles;
    /// costs one copy of each column.
    pub fn shrink_to_fit(&mut self) {
        self.arena.shrink_to_fit();
        self.name_off.shrink_to_fit();
        self.name_len.shrink_to_fit();
        self.folded_len.shrink_to_fit();
        self.parent.shrink_to_fit();
        self.mask.shrink_to_fit();
        self.size.shrink_to_fit();
        self.mtime.shrink_to_fit();
        self.wide_times.shrink_to_fit();
        self.flags.shrink_to_fit();
        self.volume.shrink_to_fit();
        self.key.shrink_to_fit();
    }

    /// Spare capacity in bytes (what [`shrink_to_fit`](Self::shrink_to_fit) would release).
    pub fn slack_bytes(&self) -> usize {
        (self.arena.capacity() - self.arena.len()) + (self.parent.capacity() - self.parent.len()) * Self::SLOT_BYTES
    }

    /// Bytes of names that are no longer referenced (compaction hint).
    pub fn dead_bytes(&self) -> usize {
        self.dead_bytes
    }
}

/// One volume's slots in portable form (see [`NameTable::export_volume`]).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Chunk {
    pub arena: Vec<u8>,
    pub slots: Vec<ChunkSlot>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ChunkSlot {
    pub name_off: u32,
    pub name_len: u16,
    pub folded_off: u32,
    pub folded_len: u16,
    /// Index within the chunk, or `NO_PARENT`.
    pub parent: u32,
    pub size: u64,
    pub mtime: i64,
    pub flags: u8,
    pub key: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ChunkError {
    TooLarge,
    BadSpan(usize),
    BadUtf8(usize),
    BadParent(usize),
    DeadSlot(usize),
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
    fn chunk_round_trip_and_rejection() {
        let mut t = NameTable::new();
        let other = t.push(NO_PARENT, "D:", Meta::dir(1));
        let root = t.push_keyed(NO_PARENT, "C:", Meta::dir(0), 5);
        let d = t.push_keyed(root, "Résumés", Meta::dir(0), 6);
        let f = t.push_keyed(d, "CV.pdf", Meta::file(0, 9, 7), 7);
        let gone = t.push_keyed(d, "old.txt", Meta::file(0, 1, 1), 8);
        t.remove(gone);
        t.push(other, "elsewhere.txt", Meta::file(1, 1, 1));
        let c = t.export_volume(0);
        assert_eq!(c.slots.len(), 3);
        let mut u = NameTable::new();
        u.push(NO_PARENT, "X:", Meta::dir(3));
        let base = u.import_chunk(0, &c).unwrap();
        let f2 = base + 2;
        assert_eq!(u.path(f2), t.path(f));
        assert_eq!(u.folded(f2), "cv.pdf");
        assert_eq!(u.mask(f2), t.mask(f));
        assert_eq!(u.meta(f2), t.meta(f));
        assert_eq!(u.key(f2), 7);
        let before = u.len();
        let mut bad = c.clone();
        bad.arena[0] = 0xff;
        assert_eq!(u.import_chunk(0, &bad), Err(ChunkError::BadUtf8(0)));
        let mut bad = c.clone();
        bad.slots[1].parent = 99;
        assert_eq!(u.import_chunk(0, &bad), Err(ChunkError::BadParent(1)));
        let mut bad = c;
        bad.slots[2].name_len = 9999;
        assert_eq!(u.import_chunk(0, &bad), Err(ChunkError::BadSpan(2)));
        assert_eq!(u.len(), before, "rejected chunks leave the table untouched");
    }

    #[test]
    fn times_are_exact_across_the_compact_range() {
        let mut t = NameTable::new();
        let r = t.push(NO_PARENT, "C:", Meta::dir(0));
        for mtime in [0, 1, 1_700_000_000, T_WIDE as i64 - 1, T_WIDE as i64, u32::MAX as i64, -1, -11_644_473_600, i64::MAX, NO_TIME] {
            let f = t.push(r, "f", Meta::file(0, 1, mtime));
            assert_eq!(t.meta(f).mtime, mtime);
            t.set_meta(f, Meta::file(0, 1, 5));
            assert_eq!(t.meta(f).mtime, 5);
            t.set_meta(f, Meta::file(0, 1, mtime));
            assert_eq!(t.meta(f).mtime, mtime, "set_meta round trip");
            let c = t.export_volume(0);
            let mut u = NameTable::new();
            let base = u.import_chunk(0, &c).unwrap();
            assert!((0..c.slots.len() as EntryId).any(|k| u.meta(base + k).mtime == mtime), "chunk round trip {mtime}");
        }
    }

    #[test]
    fn folded_names_survive_renames_and_shrinking() {
        let mut t = NameTable::new();
        let r = t.push(NO_PARENT, "C:", Meta::dir(0));
        let a = t.push(r, "lower.txt", Meta::file(0, 1, 1));
        let b = t.push(r, "Ünïcode.TXT", Meta::file(0, 1, 1));
        assert_eq!(t.folded(a), "lower.txt");
        assert_eq!(t.folded(b), fold_str("Ünïcode.TXT"));
        t.rename(a, r, "Now Upper");
        t.rename(b, r, "now lower");
        assert_eq!((t.name(a), t.folded(a)), ("Now Upper", "now upper"));
        assert_eq!((t.name(b), t.folded(b)), ("now lower", "now lower"));
        for i in 0..1000 {
            t.push(r, &format!("F{i}"), Meta::file(0, 1, 1));
        }
        let before = t.heap_bytes();
        t.shrink_to_fit();
        assert_eq!(t.slack_bytes(), 0);
        assert!(t.heap_bytes() <= before);
        assert_eq!(t.folded(a), "now upper");
        assert_eq!(t.path(b), "C:\\now lower");
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
