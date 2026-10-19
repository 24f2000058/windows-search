//! A simulated NTFS volume used as a test oracle and benchmark source.
//!
//! It allocates MFT segments with sequence numbers (reusing freed segments
//! with an incremented sequence, as NTFS does), enforces case-insensitive name
//! uniqueness per directory, refuses to delete non-empty directories, and
//! emits USN record sequences shaped like real ones (e.g. a rename produces
//! `RENAME_OLD_NAME`, `RENAME_NEW_NAME`, `RENAME_NEW_NAME|CLOSE`). The journal
//! can be truncated (wraparound) or recreated (new journal ID).

use crate::model::*;
use crate::source::JournalSource;
use std::collections::{BTreeSet, HashMap};

pub const ROOT_SEGMENT: u64 = 5;

#[derive(Clone, Debug)]
struct SimFile {
    frn: Frn,
    links: Vec<(Frn, String)>,
    dir: bool,
    size: u64,
    mtime: i64,
}

pub struct SimVolume {
    pub root: Frn,
    files: HashMap<u64, SimFile>,
    /// (parent, lowercased name) of every link, for O(1) uniqueness checks.
    names: std::collections::HashSet<(Frn, String)>,
    children: HashMap<Frn, usize>,
    seqs: HashMap<u64, u16>,
    free: Vec<u64>,
    next_seg: u64,
    journal: Vec<UsnRecord>,
    pub journal_id: u64,
    lowest_valid: i64,
    next_usn: i64,
    clock: i64,
    pub journal_active: bool,
    pub available: bool,
    /// Fault injection: fail the next N journal reads with this error.
    pub read_faults: Vec<SourceError>,
}

impl Default for SimVolume {
    fn default() -> Self {
        Self::new()
    }
}

impl SimVolume {
    pub fn new() -> Self {
        let root = make_frn(ROOT_SEGMENT, 5);
        let mut files = HashMap::new();
        files.insert(ROOT_SEGMENT, SimFile { frn: root, links: vec![(root, String::new())], dir: true, size: 0, mtime: 0 });
        SimVolume {
            root,
            files,
            names: Default::default(),
            children: HashMap::new(),
            seqs: HashMap::new(),
            free: Vec::new(),
            next_seg: 64,
            journal: Vec::new(),
            journal_id: 0x1d00_0000_0000_0001,
            lowest_valid: 0,
            next_usn: 0,
            clock: 1_700_000_000,
            journal_active: true,
            available: true,
            read_faults: Vec::new(),
        }
    }

    fn emit(&mut self, frn: Frn, parent: Frn, reason: u32, dir: bool, name: &str) {
        if !self.journal_active {
            return;
        }
        let usn = self.next_usn;
        self.next_usn += 64 + 2 * name.len() as i64;
        self.journal.push(UsnRecord { usn, frn, parent, reason, attrs: if dir { attr::DIRECTORY } else { 0 }, name: name.to_owned() });
    }

    fn alloc(&mut self) -> Frn {
        let seg = self.free.pop().unwrap_or_else(|| {
            self.next_seg += 1;
            self.next_seg
        });
        let seq = self.seqs.entry(seg).and_modify(|s| *s = s.wrapping_add(1).max(1)).or_insert(1);
        make_frn(seg, *seq)
    }

    pub fn exists(&self, frn: Frn) -> bool {
        self.files.get(&segment(frn)).is_some_and(|f| f.frn == frn)
    }

    pub fn is_dir(&self, frn: Frn) -> bool {
        self.files.get(&segment(frn)).is_some_and(|f| f.frn == frn && f.dir)
    }

    fn name_taken(&self, parent: Frn, name: &str) -> bool {
        self.names.contains(&(parent, name.to_lowercase()))
    }

    fn has_children(&self, dir: Frn) -> bool {
        self.children.get(&dir).copied().unwrap_or(0) > 0
    }

    fn link_added(&mut self, parent: Frn, name: &str) {
        self.names.insert((parent, name.to_lowercase()));
        *self.children.entry(parent).or_default() += 1;
    }

    fn link_removed(&mut self, parent: Frn, name: &str) {
        self.names.remove(&(parent, name.to_lowercase()));
        if let Some(c) = self.children.get_mut(&parent) {
            *c -= 1;
        }
    }

    pub fn create(&mut self, parent: Frn, name: &str, dir: bool) -> Option<Frn> {
        if !self.is_dir(parent) || self.name_taken(parent, name) || name.is_empty() {
            return None;
        }
        let frn = self.alloc();
        self.clock += 1;
        self.files.insert(segment(frn), SimFile { frn, links: vec![(parent, name.to_owned())], dir, size: 0, mtime: self.clock });
        self.link_added(parent, name);
        self.emit(frn, parent, reason::FILE_CREATE, dir, name);
        self.emit(frn, parent, reason::FILE_CREATE | reason::CLOSE, dir, name);
        Some(frn)
    }

    pub fn write(&mut self, frn: Frn, size: u64) -> bool {
        let Some(f) = self.files.get_mut(&segment(frn)).filter(|f| f.frn == frn && !f.dir) else { return false };
        self.clock += 1;
        f.size = size;
        f.mtime = self.clock;
        let (p, n) = f.links[0].clone();
        self.emit(frn, p, reason::DATA_EXTEND, false, &n);
        self.emit(frn, p, reason::DATA_EXTEND | reason::CLOSE, false, &n);
        true
    }

    /// Deletes a file (all links) or an empty directory.
    pub fn delete(&mut self, frn: Frn) -> bool {
        if frn == self.root || !self.exists(frn) || (self.is_dir(frn) && self.has_children(frn)) {
            return false;
        }
        let f = self.files.remove(&segment(frn)).unwrap();
        for (p, n) in &f.links {
            self.link_removed(*p, n);
        }
        self.free.push(segment(frn));
        let (p, n) = f.links.last().unwrap().clone();
        self.emit(frn, p, reason::FILE_DELETE | reason::CLOSE, f.dir, &n);
        true
    }

    pub fn rename(&mut self, frn: Frn, link: usize, new_parent: Frn, new_name: &str) -> bool {
        if frn == self.root || !self.exists(frn) || !self.is_dir(new_parent) || self.name_taken(new_parent, new_name) || new_name.is_empty() {
            return false;
        }
        // Moving a directory into its own subtree is impossible.
        if self.is_dir(frn) {
            let mut cur = new_parent;
            while cur != self.root {
                if cur == frn {
                    return false;
                }
                cur = self.files[&segment(cur)].links[0].0;
            }
        }
        let f = self.files.get_mut(&segment(frn)).unwrap();
        let link = link % f.links.len();
        let (op, on) = f.links[link].clone();
        f.links[link] = (new_parent, new_name.to_owned());
        let dir = f.dir;
        self.link_removed(op, &on);
        self.link_added(new_parent, new_name);
        self.emit(frn, op, reason::RENAME_OLD_NAME, dir, &on);
        self.emit(frn, new_parent, reason::RENAME_NEW_NAME, dir, new_name);
        self.emit(frn, new_parent, reason::RENAME_NEW_NAME | reason::CLOSE, dir, new_name);
        true
    }

    pub fn add_link(&mut self, frn: Frn, parent: Frn, name: &str) -> bool {
        if !self.exists(frn) || self.is_dir(frn) || !self.is_dir(parent) || self.name_taken(parent, name) || name.is_empty() {
            return false;
        }
        self.files.get_mut(&segment(frn)).unwrap().links.push((parent, name.to_owned()));
        self.link_added(parent, name);
        self.emit(frn, parent, reason::HARD_LINK_CHANGE, false, name);
        self.emit(frn, parent, reason::HARD_LINK_CHANGE | reason::CLOSE, false, name);
        true
    }

    /// Removes one link; removing the last link deletes the file.
    pub fn remove_link(&mut self, frn: Frn, link: usize) -> bool {
        let Some(f) = self.files.get_mut(&segment(frn)).filter(|f| f.frn == frn && !f.dir) else { return false };
        if f.links.len() == 1 {
            return self.delete(frn);
        }
        let (p, n) = f.links.remove(link % f.links.len());
        self.link_removed(p, &n);
        self.emit(frn, p, reason::HARD_LINK_CHANGE | reason::FILE_DELETE | reason::CLOSE, false, &n);
        true
    }

    /// Simulates wraparound: purge all but the newest `keep` records.
    pub fn truncate_journal(&mut self, keep: usize) {
        if self.journal.len() > keep {
            let cut = self.journal.len() - keep;
            self.journal.drain(..cut);
            self.lowest_valid = self.journal.first().map_or(self.next_usn, |r| r.usn);
        }
    }

    /// Simulates `fsutil usn deletejournal` + recreate: new ID, records gone.
    pub fn reset_journal(&mut self) {
        self.journal.clear();
        self.journal_id += 1;
        self.lowest_valid = self.next_usn;
    }

    pub fn frns(&self) -> Vec<Frn> {
        let mut v: Vec<Frn> = self.files.values().map(|f| f.frn).filter(|&f| f != self.root).collect();
        v.sort_unstable();
        v
    }

    pub fn dirs(&self) -> Vec<Frn> {
        let mut v: Vec<Frn> = self.files.values().filter(|f| f.dir).map(|f| f.frn).collect();
        v.sort_unstable();
        v
    }

    pub fn link_count(&self, frn: Frn) -> usize {
        self.files.get(&segment(frn)).filter(|f| f.frn == frn).map_or(0, |f| f.links.len())
    }

    fn path_of(&self, parent: Frn, name: &str, root_name: &str) -> String {
        let mut parts = vec![name.to_owned()];
        let mut cur = parent;
        let mut guard = 0;
        while cur != self.root && guard < 4096 {
            let f = &self.files[&segment(cur)];
            parts.push(f.links[0].1.clone());
            cur = f.links[0].0;
            guard += 1;
        }
        parts.push(root_name.to_owned());
        parts.reverse();
        parts.join("\\")
    }

    /// Ground truth: every link path.
    pub fn paths(&self, root_name: &str) -> BTreeSet<String> {
        self.files.values().filter(|f| f.frn != self.root).flat_map(|f| f.links.iter().map(|(p, n)| self.path_of(*p, n, root_name))).collect()
    }

    fn records_of(&self, f: &SimFile) -> Vec<FileRecord> {
        f.links
            .iter()
            .map(|(p, n)| FileRecord {
                frn: f.frn,
                parent: *p,
                name: n.clone(),
                attrs: if f.dir { attr::DIRECTORY } else { 0 },
                size: (!f.dir).then_some(f.size),
                mtime: Some(f.mtime),
            })
            .collect()
    }
}

impl JournalSource for SimVolume {
    fn query(&mut self) -> Result<JournalInfo, SourceError> {
        if !self.available {
            return Err(SourceError::Unavailable("simulated disconnect".into()));
        }
        if !self.journal_active {
            return Err(SourceError::JournalNotActive);
        }
        Ok(JournalInfo { journal_id: self.journal_id, lowest_valid_usn: self.lowest_valid, first_usn: self.lowest_valid, next_usn: self.next_usn })
    }

    fn enumerate(&mut self, sink: &mut dyn FnMut(FileRecord) -> bool) -> Result<(), SourceError> {
        if !self.available {
            return Err(SourceError::Unavailable("simulated disconnect".into()));
        }
        let mut segs: Vec<u64> = self.files.keys().copied().filter(|&s| s != ROOT_SEGMENT).collect();
        segs.sort_unstable();
        for s in segs {
            for r in self.records_of(&self.files[&s]) {
                if !sink(r) {
                    return Ok(());
                }
            }
        }
        Ok(())
    }

    fn read(&mut self, journal_id: u64, from_usn: i64, max: usize) -> Result<(Vec<UsnRecord>, i64), SourceError> {
        if let Some(e) = (!self.read_faults.is_empty()).then(|| self.read_faults.remove(0)) {
            return Err(e);
        }
        if !self.available {
            return Err(SourceError::Unavailable("simulated disconnect".into()));
        }
        if journal_id != self.journal_id {
            return Err(SourceError::JournalIdMismatch);
        }
        if from_usn < self.lowest_valid {
            return Err(SourceError::EntryDeleted);
        }
        let recs: Vec<UsnRecord> = self.journal.iter().filter(|r| r.usn >= from_usn).take(max).cloned().collect();
        let next = recs.last().map_or(self.next_usn.max(from_usn), |r| r.usn + 1);
        let next = if recs.len() < max { self.next_usn.max(from_usn) } else { next };
        Ok((recs, next))
    }

    fn links(&mut self, frn: Frn) -> Result<Vec<FileRecord>, SourceError> {
        Ok(self.files.get(&segment(frn)).filter(|f| f.frn == frn).map(|f| self.records_of(f)).unwrap_or_default())
    }

    fn meta(&mut self, frn: Frn) -> Result<Option<(u32, u64, i64)>, SourceError> {
        Ok(self.files.get(&segment(frn)).filter(|f| f.frn == frn).map(|f| (if f.dir { attr::DIRECTORY } else { 0 }, f.size, f.mtime)))
    }
}

/// A simulator shared between a test (which mutates it) and an indexer
/// worker (which reads it as a journal source).
#[derive(Clone)]
pub struct SharedSim(pub std::sync::Arc<std::sync::Mutex<SimVolume>>);

impl SharedSim {
    pub fn new(sim: SimVolume) -> Self {
        SharedSim(std::sync::Arc::new(std::sync::Mutex::new(sim)))
    }
    pub fn with<R>(&self, f: impl FnOnce(&mut SimVolume) -> R) -> R {
        f(&mut self.0.lock().unwrap())
    }
}

impl JournalSource for SharedSim {
    fn query(&mut self) -> Result<JournalInfo, SourceError> {
        self.0.lock().unwrap().query()
    }
    fn enumerate(&mut self, sink: &mut dyn FnMut(FileRecord) -> bool) -> Result<(), SourceError> {
        // Snapshot the records first so the lock is not held while indexing.
        let mut recs = Vec::new();
        self.0.lock().unwrap().enumerate(&mut |r| {
            recs.push(r);
            true
        })?;
        for r in recs {
            if !sink(r) {
                break;
            }
        }
        Ok(())
    }
    fn read(&mut self, journal_id: u64, from_usn: i64, max: usize) -> Result<(Vec<UsnRecord>, i64), SourceError> {
        self.0.lock().unwrap().read(journal_id, from_usn, max)
    }
    fn links(&mut self, frn: Frn) -> Result<Vec<FileRecord>, SourceError> {
        self.0.lock().unwrap().links(frn)
    }
    fn meta(&mut self, frn: Frn) -> Result<Option<(u32, u64, i64)>, SourceError> {
        self.0.lock().unwrap().meta(frn)
    }
}
