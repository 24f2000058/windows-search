//! Directory-traversal fallback for volumes without a usable change journal
//! (FAT/exFAT, network shares, ReFS until a native adapter exists, or when the
//! journal cannot be opened without elevation).
//!
//! * Iterative (explicit stack), so deep trees cannot overflow the stack.
//! * Never follows symlinks, junctions or other reparse points: they are
//!   indexed as entries but not descended into, which also rules out loops.
//! * Every unreadable directory is recorded; coverage is reported complete
//!   only if nothing was skipped and the walk was not canceled.
//! * Identity is synthetic (traversal order), so each run is a full rescan;
//!   there is no incremental update without a journal.

use crate::model::*;
use crate::source::JournalSource;
use std::ffi::OsString;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

#[derive(Clone, Debug)]
pub struct DirItem {
    pub name: OsString,
    pub is_dir: bool,
    /// Symlink, junction or other reparse point: never descended into.
    pub is_link: bool,
    pub hidden: bool,
    pub size: u64,
    pub mtime: Option<i64>,
}

/// Filesystem access used by the walker (real or fault-injecting).
pub trait Fs {
    fn list(&self, dir: &Path) -> io::Result<Vec<io::Result<DirItem>>>;
}

impl<T: Fs + ?Sized> Fs for Box<T> {
    fn list(&self, dir: &Path) -> io::Result<Vec<io::Result<DirItem>>> {
        (**self).list(dir)
    }
}

pub struct RealFs;

impl Fs for RealFs {
    fn list(&self, dir: &Path) -> io::Result<Vec<io::Result<DirItem>>> {
        Ok(std::fs::read_dir(dir)?
            .map(|e| {
                let e = e?;
                // `DirEntry::metadata` does not traverse symlinks.
                let md = e.metadata()?;
                let ft = md.file_type();
                #[cfg(windows)]
                let (is_link, hidden) = {
                    use std::os::windows::fs::MetadataExt;
                    let a = md.file_attributes();
                    (ft.is_symlink() || a & attr::REPARSE_POINT != 0, a & attr::HIDDEN != 0)
                };
                #[cfg(not(windows))]
                let (is_link, hidden) = (ft.is_symlink(), e.file_name().to_string_lossy().starts_with('.'));
                let mtime = md.modified().ok().and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok()).map(|d| d.as_secs() as i64);
                Ok(DirItem { name: e.file_name(), is_dir: ft.is_dir(), is_link, hidden, size: if ft.is_dir() { 0 } else { md.len() }, mtime })
            })
            .collect())
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct TraversalReport {
    pub entries: u64,
    /// Directories that could not be listed, with the error.
    pub skipped: Vec<(PathBuf, String)>,
    /// Individual entries that could not be read.
    pub entry_errors: u64,
    /// Reparse points/symlinks indexed but not descended into.
    pub links_not_followed: u64,
    /// Names that are not valid Unicode; indexed lossily and flagged, since
    /// operations must not target the lossy spelling.
    pub lossy_names: u64,
    pub depth_limited: u64,
    pub canceled: bool,
}

impl TraversalReport {
    pub fn complete(&self) -> bool {
        !self.canceled && self.skipped.is_empty() && self.entry_errors == 0 && self.depth_limited == 0
    }
}

pub const MAX_DEPTH: usize = 512;

/// A [`JournalSource`] that enumerates by walking `root`. It has no journal:
/// `query`/`read` report [`SourceError::JournalNotActive`].
pub struct TraversalSource<F: Fs = RealFs> {
    pub root: PathBuf,
    pub root_frn: Frn,
    pub fs: F,
    pub report: TraversalReport,
    pub cancel: Arc<AtomicBool>,
}

impl<F: Fs> TraversalSource<F> {
    pub fn new(root: PathBuf, fs: F) -> Self {
        TraversalSource { root, root_frn: make_frn(1, 1), fs, report: TraversalReport::default(), cancel: Arc::new(AtomicBool::new(false)) }
    }

    /// Walks the tree, emitting records parents-first with synthetic FRNs.
    pub fn walk(&mut self, sink: &mut dyn FnMut(FileRecord) -> bool) {
        self.report = TraversalReport::default();
        let mut next_seg = 2u64;
        let root = self.root.clone();
        self.walk_from(root, self.root_frn, 0, &mut next_seg, sink);
    }

    /// Walks the subtree under `dir` (whose identity is `dir_frn`), emitting
    /// its descendants and allocating synthetic FRNs from `next_seg`. The
    /// report accumulates (call [`walk`](Self::walk) to reset it).
    pub fn walk_from(&mut self, dir: PathBuf, dir_frn: Frn, depth: usize, next_seg: &mut u64, sink: &mut dyn FnMut(FileRecord) -> bool) {
        let mut stack: Vec<(PathBuf, Frn, usize)> = vec![(dir, dir_frn, depth)];
        while let Some((dir, dir_frn, depth)) = stack.pop() {
            if self.cancel.load(Ordering::Relaxed) {
                self.report.canceled = true;
                return;
            }
            let items = match self.fs.list(&dir) {
                Ok(v) => v,
                Err(e) => {
                    self.report.skipped.push((dir, e.to_string()));
                    continue;
                }
            };
            for item in items {
                let item = match item {
                    Ok(i) => i,
                    Err(_) => {
                        self.report.entry_errors += 1;
                        continue;
                    }
                };
                let name = match item.name.to_str() {
                    Some(s) => s.to_owned(),
                    None => {
                        self.report.lossy_names += 1;
                        item.name.to_string_lossy().into_owned()
                    }
                };
                *next_seg += 1;
                let frn = make_frn(*next_seg, 1);
                let mut attrs = 0;
                if item.is_dir {
                    attrs |= attr::DIRECTORY;
                }
                if item.is_link {
                    attrs |= attr::REPARSE_POINT;
                    self.report.links_not_followed += 1;
                }
                if item.hidden {
                    attrs |= attr::HIDDEN;
                }
                let rec = FileRecord { frn, parent: dir_frn, name: name.clone(), attrs, size: (!item.is_dir).then_some(item.size), mtime: item.mtime.or(Some(0)) };
                self.report.entries += 1;
                if !sink(rec) {
                    self.report.canceled = true;
                    return;
                }
                if item.is_dir && !item.is_link {
                    if depth + 1 >= MAX_DEPTH {
                        self.report.depth_limited += 1;
                    } else {
                        stack.push((dir.join(&item.name), frn, depth + 1));
                    }
                }
            }
        }
    }
}

impl<F: Fs> JournalSource for TraversalSource<F> {
    fn query(&mut self) -> Result<JournalInfo, SourceError> {
        Err(SourceError::JournalNotActive)
    }
    fn enumerate(&mut self, sink: &mut dyn FnMut(FileRecord) -> bool) -> Result<(), SourceError> {
        self.walk(sink);
        Ok(())
    }
    fn read(&mut self, _: u64, _: i64, _: usize) -> Result<(Vec<UsnRecord>, i64), SourceError> {
        Err(SourceError::JournalNotActive)
    }
    fn links(&mut self, _: Frn) -> Result<Vec<FileRecord>, SourceError> {
        Err(SourceError::Io("traversal source cannot resolve links".into()))
    }
    fn meta(&mut self, _: Frn) -> Result<Option<(u32, u64, i64)>, SourceError> {
        Err(SourceError::Io("traversal source cannot resolve metadata".into()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::{BTreeSet, HashMap, HashSet};

    /// In-memory filesystem with injectable failures.
    struct FakeFs {
        dirs: HashMap<PathBuf, Vec<DirItem>>,
        deny: HashSet<PathBuf>,
        bad_entry: HashSet<PathBuf>,
    }

    impl Fs for FakeFs {
        fn list(&self, dir: &Path) -> io::Result<Vec<io::Result<DirItem>>> {
            if self.deny.contains(dir) {
                return Err(io::Error::new(io::ErrorKind::PermissionDenied, "access denied"));
            }
            let items = self.dirs.get(dir).ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "vanished"))?;
            Ok(items.iter().map(|i| if self.bad_entry.contains(&dir.join(&i.name)) { Err(io::Error::other("bad entry")) } else { Ok(i.clone()) }).collect())
        }
    }

    fn item(name: &str, is_dir: bool) -> DirItem {
        DirItem { name: name.into(), is_dir, is_link: false, hidden: false, size: 1, mtime: Some(1) }
    }

    fn fake() -> FakeFs {
        let mut dirs = HashMap::new();
        dirs.insert(PathBuf::from("/r"), vec![item("a", true), item("b", true), item("f.txt", false)]);
        dirs.insert(PathBuf::from("/r/a"), vec![item("x.rs", false), DirItem { is_link: true, ..item("loop", true) }]);
        dirs.insert(PathBuf::from("/r/b"), vec![item("y.md", false)]);
        FakeFs { dirs, deny: HashSet::new(), bad_entry: HashSet::new() }
    }

    fn names(src: &mut TraversalSource<FakeFs>) -> BTreeSet<String> {
        let mut out = BTreeSet::new();
        src.walk(&mut |r| {
            out.insert(r.name);
            true
        });
        out
    }

    #[test]
    fn complete_walk_does_not_follow_links() {
        let mut s = TraversalSource::new("/r".into(), fake());
        let n = names(&mut s);
        assert_eq!(n.len(), 6);
        assert!(s.report.complete());
        assert_eq!(s.report.links_not_followed, 1);
    }

    #[test]
    fn denied_directory_makes_coverage_incomplete() {
        let mut fs = fake();
        fs.deny.insert("/r/b".into());
        let mut s = TraversalSource::new("/r".into(), fs);
        let n = names(&mut s);
        assert!(!n.contains("y.md"));
        assert!(!s.report.complete());
        assert_eq!(s.report.skipped.len(), 1);
    }

    #[test]
    fn vanished_directory_and_bad_entries_are_reported() {
        let mut fs = fake();
        fs.dirs.remove(&PathBuf::from("/r/a"));
        fs.bad_entry.insert("/r/f.txt".into());
        let mut s = TraversalSource::new("/r".into(), fs);
        names(&mut s);
        assert_eq!(s.report.skipped.len(), 1);
        assert_eq!(s.report.entry_errors, 1);
        assert!(!s.report.complete());
    }

    #[test]
    fn cancellation_is_reported() {
        let mut s = TraversalSource::new("/r".into(), fake());
        s.cancel.store(true, Ordering::Relaxed);
        names(&mut s);
        assert!(s.report.canceled && !s.report.complete());
    }

    #[cfg(unix)]
    #[test]
    fn real_fs_symlink_loop_terminates() {
        let d = std::env::temp_dir().join(format!("flint-walk-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(d.join("sub/deeper")).unwrap();
        std::fs::write(d.join("sub/deeper/file.txt"), b"x").unwrap();
        std::os::unix::fs::symlink(&d, d.join("sub/loop")).unwrap();
        let mut s = TraversalSource::new(d.clone(), RealFs);
        let mut n = 0;
        s.walk(&mut |_| {
            n += 1;
            true
        });
        assert_eq!(n, 4, "sub, deeper, file.txt, loop (not followed)");
        assert_eq!(s.report.links_not_followed, 1);
        assert!(s.report.complete());
        let _ = std::fs::remove_dir_all(&d);
    }
}
