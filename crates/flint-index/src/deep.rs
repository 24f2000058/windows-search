//! Deep search: a bounded, cancellable walk of locations the index does
//! not cover (ADR-018). Started only by an explicit user request.
//!
//! * Walks breadth-first, so shallow matches are found before the limits
//!   bite, and stops at an entry, time or depth limit. The report says
//!   which limit stopped it, so a partial walk is never presented as a full
//!   one.
//! * Never enters excluded or sensitive locations, and never follows links
//!   (same rules as indexing).
//! * Ranks with the normal engine over a temporary table of what it saw.
//!   Only the hits are kept, inserted into the shared table under
//!   [`DEEP_VOLUME`] so the UI can show and act on them like other results,
//!   and removed again by [`remove_deep`].

use crate::traverse::Fs;
use flint_fuzzy::engine::{search, CancelToken, Options};
use flint_fuzzy::query::Query;
use flint_fuzzy::table::{flags, EntryId, Meta, NameTable, NO_PARENT, NO_TIME};
use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

/// Volume id of deep-search hits in the shared table.
pub const DEEP_VOLUME: u16 = u16::MAX;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DeepLimits {
    pub max_entries: usize,
    pub max_time: Duration,
    pub max_depth: usize,
}

impl Default for DeepLimits {
    fn default() -> Self {
        DeepLimits { max_entries: 300_000, max_time: Duration::from_secs(10), max_depth: 64 }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Stopped {
    EntryLimit,
    TimeLimit,
    Canceled,
}

impl Stopped {
    pub fn label(self) -> &'static str {
        match self {
            Stopped::EntryLimit => "entry limit reached",
            Stopped::TimeLimit => "time limit reached",
            Stopped::Canceled => "canceled",
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct DeepReport {
    pub visited: u64,
    pub unreadable: Vec<(PathBuf, String)>,
    pub excluded: u64,
    pub depth_limited: u64,
    pub stopped: Option<Stopped>,
    pub elapsed: Duration,
}

impl DeepReport {
    /// Every scope was walked to the end (nothing unreadable or cut off).
    pub fn complete(&self) -> bool {
        self.stopped.is_none() && self.unreadable.is_empty() && self.depth_limited == 0
    }
    pub fn summary(&self) -> String {
        let mut s = format!("deep search: {} entries examined", self.visited);
        if let Some(st) = self.stopped {
            s.push_str(&format!(", stopped early ({})", st.label()));
        }
        if !self.unreadable.is_empty() {
            s.push_str(&format!(", {} folder(s) unreadable", self.unreadable.len()));
        }
        if self.depth_limited > 0 {
            s.push_str(&format!(", {} folder(s) too deep", self.depth_limited));
        }
        if self.complete() {
            s.push_str(", all searched locations walked");
        }
        s
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DeepHit {
    pub path: PathBuf,
    pub is_dir: bool,
    pub size: u64,
    pub mtime: i64,
    pub tier: u8,
}

fn excluded(path: &Path, excluded: &[String]) -> bool {
    crate::content::is_excluded(&path.to_string_lossy(), excluded)
}

/// Walks `scopes` within `limits` and returns the entries matching `q`.
pub fn deep_search(fs: &dyn Fs, scopes: &[PathBuf], exclude: &[String], q: &Query, opts: &Options, limits: &DeepLimits, cancel: &AtomicBool) -> (Vec<DeepHit>, DeepReport) {
    let t0 = Instant::now();
    let mut rep = DeepReport::default();
    let mut t = NameTable::new();
    t.sep = std::path::MAIN_SEPARATOR;
    // (dir path, its slot, depth)
    let mut queue: VecDeque<(PathBuf, EntryId, usize)> = VecDeque::new();
    for s in scopes {
        if excluded(s, exclude) {
            rep.excluded += 1;
            continue;
        }
        let id = t.push(NO_PARENT, &s.to_string_lossy(), Meta::dir(0));
        queue.push_back((s.clone(), id, 0));
    }
    'walk: while let Some((dir, id, depth)) = queue.pop_front() {
        if cancel.load(Ordering::Relaxed) {
            rep.stopped = Some(Stopped::Canceled);
            break;
        }
        if t0.elapsed() >= limits.max_time {
            rep.stopped = Some(Stopped::TimeLimit);
            break;
        }
        let items = match fs.list(&dir) {
            Ok(v) => v,
            Err(e) => {
                rep.unreadable.push((dir, e.to_string()));
                continue;
            }
        };
        for item in items.into_iter().flatten() {
            if rep.visited as usize >= limits.max_entries {
                rep.stopped = Some(Stopped::EntryLimit);
                break 'walk;
            }
            let path = dir.join(&item.name);
            if item.is_dir && excluded(&path, exclude) {
                rep.excluded += 1;
                continue;
            }
            rep.visited += 1;
            let mut f = if item.is_dir { flags::DIR } else { 0 };
            if item.is_link {
                f |= flags::REPARSE;
            }
            let child = t.push(id, &item.name.to_string_lossy(), Meta { size: item.size, mtime: item.mtime.unwrap_or(NO_TIME), flags: f, volume: 0 });
            if item.is_dir && !item.is_link {
                if depth + 1 >= limits.max_depth {
                    rep.depth_limited += 1;
                } else {
                    queue.push_back((path, child, depth + 1));
                }
            }
        }
    }
    let r = search(&t, q, opts, &CancelToken::new(), |_| {});
    let hits = r
        .hits
        .iter()
        .filter(|h| t.parent(h.id) != NO_PARENT) // scope roots themselves are not results
        .map(|h| {
            let m = t.meta(h.id);
            DeepHit { path: PathBuf::from(t.path(h.id)), is_dir: m.flags & flags::DIR != 0, size: m.size, mtime: m.mtime, tier: h.tier }
        })
        .collect();
    rep.elapsed = t0.elapsed();
    (hits, rep)
}

/// Adds hits to the shared table under [`DEEP_VOLUME`] (each as its parent
/// folder path plus its name, so paths render in full).
pub fn insert_hits(t: &mut NameTable, hits: &[DeepHit]) {
    remove_deep(t);
    for h in hits {
        let (Some(parent), Some(name)) = (h.path.parent(), h.path.file_name()) else { continue };
        let dir = t.push(NO_PARENT, &parent.to_string_lossy(), Meta::dir(DEEP_VOLUME));
        t.push(dir, &name.to_string_lossy(), Meta { size: h.size, mtime: h.mtime, flags: if h.is_dir { flags::DIR } else { 0 }, volume: DEEP_VOLUME });
    }
}

/// Removes every deep-search entry from the shared table.
pub fn remove_deep(t: &mut NameTable) {
    for id in 0..t.len() as EntryId {
        if t.is_live(id) && t.volume(id) == DEEP_VOLUME {
            t.remove(id);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::traverse::{DirItem, Fs};
    use flint_fuzzy::query::parse;
    use std::collections::HashMap;
    use std::io;

    /// A synthetic tree: every directory has `fanout` subdirectories and a
    /// few files, `depth` levels deep; some directories are unreadable.
    struct Tree {
        fanout: usize,
        depth: usize,
        denied: Vec<String>,
        slow: Duration,
        extra: HashMap<String, Vec<DirItem>>,
    }

    fn item(name: &str, is_dir: bool) -> DirItem {
        DirItem { name: name.into(), is_dir, is_link: false, hidden: false, size: 7, mtime: Some(1_700_000_000) }
    }

    impl Fs for Tree {
        fn list(&self, dir: &Path) -> io::Result<Vec<io::Result<DirItem>>> {
            std::thread::sleep(self.slow);
            let s = dir.to_string_lossy().replace('\\', "/");
            if self.denied.iter().any(|d| s.ends_with(d.as_str())) {
                return Err(io::Error::new(io::ErrorKind::PermissionDenied, "access denied"));
            }
            let level = s.matches('/').count();
            let mut v: Vec<io::Result<DirItem>> = self.extra.get(&s).cloned().unwrap_or_default().into_iter().map(Ok).collect();
            if level < self.depth {
                for i in 0..self.fanout {
                    v.push(Ok(item(&format!("d{i}"), true)));
                }
            }
            v.push(Ok(item(&format!("file-l{level}.txt"), false)));
            Ok(v)
        }
    }

    fn tree(fanout: usize, depth: usize) -> Tree {
        Tree { fanout, depth, denied: vec![], slow: Duration::ZERO, extra: HashMap::new() }
    }

    fn run(fs: &Tree, q: &str, limits: DeepLimits, exclude: &[String]) -> (Vec<DeepHit>, DeepReport) {
        deep_search(fs, &[PathBuf::from("/scope")], exclude, &parse(q, 0), &Options::default(), &limits, &AtomicBool::new(false))
    }

    #[test]
    fn finds_matches_and_reports_a_complete_walk() {
        let mut fs = tree(2, 3);
        fs.extra.insert("/scope/d1/d0".into(), vec![item("Quarterly Report.pdf", false)]);
        let (hits, rep) = run(&fs, "quartrly report", DeepLimits::default(), &[]);
        assert_eq!(hits[0].path, PathBuf::from("/scope/d1/d0/Quarterly Report.pdf"), "fuzzy ranking as in indexed search");
        assert!(rep.complete(), "{rep:?}");
        assert!(rep.summary().contains("all searched locations walked"));
    }

    #[test]
    fn stops_at_limits_and_says_so() {
        let fs = tree(4, 8);
        let (_, rep) = run(&fs, "file", DeepLimits { max_entries: 500, ..Default::default() }, &[]);
        assert_eq!((rep.visited, rep.stopped), (500, Some(Stopped::EntryLimit)));
        assert!(!rep.complete() && rep.summary().contains("entry limit"));
        let mut slow = tree(3, 6);
        slow.slow = Duration::from_millis(2);
        let t = Instant::now();
        let (_, rep) = run(&slow, "file", DeepLimits { max_time: Duration::from_millis(100), ..Default::default() }, &[]);
        assert_eq!(rep.stopped, Some(Stopped::TimeLimit));
        assert!(t.elapsed() < Duration::from_secs(2), "time limit honoured: {:?}", t.elapsed());
        let (_, rep) = run(&tree(1, 10), "file", DeepLimits { max_depth: 3, ..Default::default() }, &[]);
        assert!(rep.depth_limited > 0 && !rep.complete());
    }

    #[test]
    fn breadth_first_finds_shallow_hits_before_the_limit() {
        let mut fs = tree(5, 6);
        fs.extra.insert("/scope".into(), vec![item("needle.txt", false)]);
        let (hits, rep) = run(&fs, "needle", DeepLimits { max_entries: 50, ..Default::default() }, &[]);
        assert_eq!(rep.stopped, Some(Stopped::EntryLimit));
        assert!(hits.iter().any(|h| h.path.ends_with("needle.txt")));
    }

    #[test]
    fn never_enters_excluded_or_sensitive_folders_and_reports_unreadable() {
        let mut fs = tree(2, 3);
        fs.extra.insert("/scope".into(), vec![item(".ssh", true), item("private", true)]);
        fs.extra.insert("/scope/.ssh".into(), vec![item("id_rsa", false)]);
        fs.extra.insert("/scope/private".into(), vec![item("diary.txt", false)]);
        fs.denied.push("/scope/d0".into());
        let (hits, rep) = run(&fs, "id_rsa", DeepLimits::default(), &["/scope/private".into()]);
        assert!(hits.iter().all(|h| !h.path.to_string_lossy().contains(".ssh")), "sensitive folders are never walked");
        let (hits, _) = run(&fs, "diary", DeepLimits::default(), &["/scope/private".into()]);
        assert!(hits.iter().all(|h| !h.path.ends_with("diary.txt")), "excluded folders are never walked");
        assert_eq!(rep.excluded, 2);
        assert_eq!(rep.unreadable.len(), 1);
        assert!(!rep.complete() && rep.summary().contains("1 folder(s) unreadable"));
    }

    #[test]
    fn cancel_stops_promptly() {
        let mut fs = tree(3, 8);
        fs.slow = Duration::from_millis(1);
        let cancel = AtomicBool::new(true);
        let (_, rep) = deep_search(&fs, &[PathBuf::from("/scope")], &[], &parse("x", 0), &Options::default(), &DeepLimits::default(), &cancel);
        assert_eq!((rep.stopped, rep.visited), (Some(Stopped::Canceled), 0));
    }

    #[test]
    fn hits_live_in_the_shared_table_until_removed() {
        let mut t = NameTable::new();
        let r = t.push(NO_PARENT, "C:", Meta::dir(1));
        t.push(r, "indexed.txt", Meta::file(1, 1, 1));
        let hit = DeepHit { path: PathBuf::from("/mnt/usb/photos/beach.jpg"), is_dir: false, size: 3, mtime: 5, tier: 9 };
        insert_hits(&mut t, std::slice::from_ref(&hit));
        let id = (0..t.len() as u32).find(|&i| t.is_live(i) && t.name(i) == "beach.jpg").unwrap();
        assert_eq!((t.path(id), t.volume(id), t.meta(id).size), ("/mnt/usb/photos\\beach.jpg".to_owned(), DEEP_VOLUME, 3));
        remove_deep(&mut t);
        assert_eq!(t.live_count(), 2, "only deep entries removed");
    }
}
