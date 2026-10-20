//! Selective content index (ADR-017).
//!
//! Off unless the user enables it. Only files under the configured roots,
//! with the configured extensions, below the size limit, and outside every
//! excluded or sensitive location are read. Text comes from the isolated
//! preview helper (an [`Extractor`]); a file that cannot be extracted is
//! recorded with an explicit state and reason and never affects the
//! filename index.
//!
//! Storage is a separate SQLite database (`content.db`) with an FTS5 table,
//! so content data can be deleted on its own. Each file row keeps the size
//! and mtime it was extracted at; a change re-extracts it and a vanished
//! file is deleted on the next sync.

use crate::governor::Governor;
use flint_fuzzy::engine::ContentMatch;
use flint_fuzzy::fold::fold_str;
use flint_fuzzy::session::ContentProvider;
use flint_fuzzy::table::{flags, EntryId, NameTable, NO_TIME};
use flint_preview::{Extracted, Limits};
use rusqlite::{params, Connection, OptionalExtension};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};

/// Per-file extraction state, stored and shown in diagnostics.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum State {
    Indexed,
    /// Indexed, but only the first part (size/char limit).
    Truncated,
    /// The format has no extractable text here (reason kept).
    Unsupported,
    /// Larger than the size limit; not read.
    TooLarge,
    /// The helper failed, crashed or timed out on it (reason kept).
    Failed,
}

impl State {
    pub fn as_str(&self) -> &'static str {
        match self {
            State::Indexed => "indexed",
            State::Truncated => "truncated",
            State::Unsupported => "unsupported",
            State::TooLarge => "too_large",
            State::Failed => "failed",
        }
    }
    fn parse(s: &str) -> State {
        match s {
            "indexed" => State::Indexed,
            "truncated" => State::Truncated,
            "unsupported" => State::Unsupported,
            "too_large" => State::TooLarge,
            _ => State::Failed,
        }
    }
}

#[derive(Clone, Debug)]
pub struct ContentConfig {
    /// Folders whose files are content-indexed.
    pub roots: Vec<String>,
    /// Lowercase extensions without the dot.
    pub extensions: Vec<String>,
    /// Never read (user exclusions plus built-in sensitive locations).
    pub excluded: Vec<String>,
    pub max_file_bytes: u64,
    pub limits: Limits,
}

/// Locations never content-indexed whatever the roots say: credentials,
/// keys, browser profiles and application data.
pub const SENSITIVE: &[&str] = &["\\appdata\\", "\\.ssh\\", "\\.gnupg\\", "\\.aws\\", "\\.azure\\", "\\.kube\\", "\\.docker\\", "\\microsoft\\credentials\\", "\\microsoft\\protect\\", "/.ssh/", "/.gnupg/", "/.aws/"];

impl ContentConfig {
    pub fn default_extensions() -> Vec<String> {
        ["txt", "md", "markdown", "rst", "csv", "tsv", "log", "json", "xml", "yaml", "yml", "toml", "ini", "html", "htm", "docx", "xlsx", "pptx", "odt", "ods", "odp", "epub", "tex"].iter().map(|s| s.to_string()).collect()
    }

    fn excluded(&self, folded_path: &str) -> bool {
        SENSITIVE.iter().any(|s| folded_path.contains(s)) || self.excluded.iter().any(|e| folded_path.starts_with(&fold_str(e)))
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Candidate {
    pub path: String,
    pub size: u64,
    pub mtime: i64,
}

/// Files in the name table eligible for content indexing.
pub fn candidates(t: &NameTable, cfg: &ContentConfig) -> Vec<Candidate> {
    let roots: Vec<String> = cfg.roots.iter().map(|r| {
        let f = fold_str(r);
        if f.ends_with(['\\', '/']) { f } else { format!("{f}{}", t.sep) }
    }).collect();
    let exts: HashSet<&str> = cfg.extensions.iter().map(String::as_str).collect();
    let mut out = Vec::new();
    for id in 0..t.len() as EntryId {
        if !t.is_live(id) {
            continue;
        }
        let m = t.meta(id);
        if m.flags & (flags::DIR | flags::REPARSE) != 0 {
            continue;
        }
        // Cheap test first: extension of the folded name.
        let name = t.folded(id);
        let Some(dot) = name.rfind('.') else { continue };
        if !exts.contains(&name[dot + 1..]) {
            continue;
        }
        let folded_path = format!("{}{}{}", t.parent_path_folded(id), t.sep, name);
        if !roots.iter().any(|r| folded_path.starts_with(r.as_str())) || cfg.excluded(&folded_path) {
            continue;
        }
        out.push(Candidate { path: t.path(id), size: m.size, mtime: if m.mtime == NO_TIME { 0 } else { m.mtime } });
    }
    out
}

/// Text extraction backend (the preview helper in production).
pub trait Extractor {
    /// Outer error: the helper failed (crash/timeout/unavailable); inner:
    /// the file could not be read or parsed.
    fn extract(&mut self, path: &str, limits: &Limits) -> Result<Result<Extracted, String>, String>;
}

impl Extractor for flint_preview::client::HostClient {
    fn extract(&mut self, path: &str, limits: &Limits) -> Result<Result<Extracted, String>, String> {
        flint_preview::client::HostClient::extract(self, path, limits).map_err(|e| e.to_string())
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ContentStats {
    pub files: u64,
    pub by_state: Vec<(String, u64)>,
    /// Candidates not yet extracted (or changed since).
    pub pending: u64,
    pub last_sync_complete: bool,
}

pub struct ContentStore {
    conn: Connection,
}

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS files(path TEXT PRIMARY KEY, size INTEGER NOT NULL, mtime INTEGER NOT NULL, state TEXT NOT NULL, detail TEXT NOT NULL DEFAULT '');
CREATE VIRTUAL TABLE IF NOT EXISTS body USING fts5(path UNINDEXED, text, tokenize = 'unicode61 remove_diacritics 2');
";

impl ContentStore {
    pub fn open(path: &Path) -> Result<Self, String> {
        let conn = Connection::open(path).map_err(|e| e.to_string())?;
        conn.pragma_update(None, "journal_mode", "WAL").map_err(|e| e.to_string())?;
        conn.pragma_update(None, "synchronous", "NORMAL").map_err(|e| e.to_string())?;
        conn.busy_timeout(std::time::Duration::from_secs(5)).map_err(|e| e.to_string())?;
        conn.execute_batch(SCHEMA).map_err(|e| e.to_string())?;
        Ok(ContentStore { conn })
    }

    fn known(&self) -> Result<HashMap<String, (u64, i64)>, String> {
        let mut st = self.conn.prepare("SELECT path, size, mtime FROM files").map_err(|e| e.to_string())?;
        let rows = st.query_map([], |r| Ok((r.get::<_, String>(0)?, (r.get::<_, i64>(1)? as u64, r.get::<_, i64>(2)?)))).map_err(|e| e.to_string())?;
        rows.collect::<Result<_, _>>().map_err(|e| e.to_string())
    }

    fn put(&mut self, c: &Candidate, state: &State, detail: &str, text: Option<&str>) -> Result<(), String> {
        let tx = self.conn.transaction().map_err(|e| e.to_string())?;
        tx.execute("DELETE FROM body WHERE path = ?1", [&c.path]).map_err(|e| e.to_string())?;
        tx.execute("INSERT OR REPLACE INTO files(path, size, mtime, state, detail) VALUES (?1, ?2, ?3, ?4, ?5)", params![c.path, c.size as i64, c.mtime, state.as_str(), detail]).map_err(|e| e.to_string())?;
        if let Some(t) = text {
            tx.execute("INSERT INTO body(path, text) VALUES (?1, ?2)", params![c.path, t]).map_err(|e| e.to_string())?;
        }
        tx.commit().map_err(|e| e.to_string())
    }

    fn remove(&mut self, paths: &[String]) -> Result<(), String> {
        let tx = self.conn.transaction().map_err(|e| e.to_string())?;
        for p in paths {
            tx.execute("DELETE FROM body WHERE path = ?1", [p]).map_err(|e| e.to_string())?;
            tx.execute("DELETE FROM files WHERE path = ?1", [p]).map_err(|e| e.to_string())?;
        }
        tx.commit().map_err(|e| e.to_string())
    }

    pub fn state_of(&self, path: &str) -> Result<Option<(State, String)>, String> {
        self.conn.query_row("SELECT state, detail FROM files WHERE path = ?1", [path], |r| Ok((State::parse(&r.get::<_, String>(0)?), r.get(1)?))).optional().map_err(|e| e.to_string())
    }

    pub fn stats(&self) -> Result<ContentStats, String> {
        let mut st = self.conn.prepare("SELECT state, COUNT(*) FROM files GROUP BY state ORDER BY state").map_err(|e| e.to_string())?;
        let by_state: Vec<(String, u64)> = st.query_map([], |r| Ok((r.get(0)?, r.get::<_, i64>(1)? as u64))).map_err(|e| e.to_string())?.collect::<Result<_, _>>().map_err(|e| e.to_string())?;
        Ok(ContentStats { files: by_state.iter().map(|s| s.1).sum(), by_state, ..Default::default() })
    }

    /// Paths whose text matches `text` (FTS5, all words, diacritics and
    /// case ignored). Returns (paths, more_than_limit).
    pub fn search(&self, text: &str, limit: usize) -> Result<(Vec<String>, bool), String> {
        let q = fts_query(text);
        if q.is_empty() {
            return Ok((Vec::new(), false));
        }
        let mut st = self.conn.prepare("SELECT path FROM body WHERE body MATCH ?1 LIMIT ?2").map_err(|e| e.to_string())?;
        let rows: Vec<String> = st.query_map(params![q, limit as i64 + 1], |r| r.get(0)).map_err(|e| e.to_string())?.collect::<Result<_, _>>().map_err(|e| e.to_string())?;
        let more = rows.len() > limit;
        Ok((rows.into_iter().take(limit).collect(), more))
    }
}

/// User text to an FTS5 query: every word must occur (as a prefix for the
/// last word, which may still be being typed); quotes and operators in the
/// input are never interpreted as FTS syntax.
pub fn fts_query(text: &str) -> String {
    let words: Vec<String> = text.split(|c: char| !c.is_alphanumeric()).filter(|w| !w.is_empty()).map(|w| format!("\"{}\"", w.replace('"', ""))).collect();
    match words.split_last() {
        Some((last, rest)) => rest.iter().cloned().chain(std::iter::once(format!("{last}*"))).collect::<Vec<_>>().join(" "),
        None => String::new(),
    }
}

/// One pass: extract new/changed candidates, drop vanished ones. Returns
/// the number of candidates still pending (0 when complete). Holds a scan
/// permit and paces like the filename indexer.
pub fn sync(store: &mut ContentStore, cands: &[Candidate], cfg: &ContentConfig, ex: &mut dyn Extractor, gov: Option<&Governor>, stop: &AtomicBool, mut on_progress: impl FnMut()) -> Result<u64, String> {
    let _permit = match gov {
        Some(g) => Some(g.acquire_scan(stop, &mut |_| {}).ok_or("stopped")?),
        None => None,
    };
    let known = store.known()?;
    let live: HashSet<&str> = cands.iter().map(|c| c.path.as_str()).collect();
    let gone: Vec<String> = known.keys().filter(|p| !live.contains(p.as_str())).cloned().collect();
    store.remove(&gone)?;
    let todo: Vec<&Candidate> = cands.iter().filter(|c| known.get(&c.path) != Some(&(c.size, c.mtime))).collect();
    let mut left = todo.len() as u64;
    for c in todo {
        if stop.load(Ordering::Relaxed) {
            return Ok(left);
        }
        if let Some(g) = gov {
            g.pace(stop);
        }
        if c.size > cfg.max_file_bytes {
            store.put(c, &State::TooLarge, &format!("{} MB exceeds the {} MB limit", c.size >> 20, cfg.max_file_bytes >> 20), None)?;
        } else {
            match ex.extract(&c.path, &cfg.limits) {
                Ok(Ok(Extracted::Text { text, truncated })) => store.put(c, if truncated { &State::Truncated } else { &State::Indexed }, "", Some(&text))?,
                Ok(Ok(Extracted::Unsupported(why))) => store.put(c, &State::Unsupported, &why, None)?,
                Ok(Err(e)) | Err(e) => store.put(c, &State::Failed, &e, None)?,
            }
        }
        left -= 1;
        on_progress();
    }
    Ok(left)
}

/// Matches table entries against a set of content-search result paths.
pub struct PathSet {
    names: HashSet<String>,
    paths: HashSet<String>,
}

impl PathSet {
    pub fn new(paths: &[String]) -> Self {
        let paths: HashSet<String> = paths.iter().map(|p| fold_str(p)).collect();
        let names = paths.iter().map(|p| p.rsplit(['\\', '/']).next().unwrap_or(p).to_owned()).collect();
        PathSet { names, paths }
    }
}

impl ContentMatch for PathSet {
    fn matches(&self, t: &NameTable, id: EntryId) -> bool {
        // Name first: almost every slot is rejected without building a path.
        self.names.contains(t.folded(id)) && self.paths.contains(&format!("{}{}{}", t.parent_path_folded(id), t.sep, t.folded(id)))
    }
}

/// Shared content-index state for searching (read connection) and the
/// status line.
pub struct ContentIndex {
    read: Mutex<ContentStore>,
    generation: AtomicU64,
    pending: AtomicU64,
    enabled_note: Option<String>,
    /// The last content query had more hits than `max_hits`.
    capped: AtomicBool,
    /// Most result paths one `content:` filter considers.
    pub max_hits: usize,
}

impl ContentIndex {
    pub fn open(path: &Path) -> Result<Arc<Self>, String> {
        Ok(Arc::new(ContentIndex { read: Mutex::new(ContentStore::open(path)?), generation: AtomicU64::new(1), pending: AtomicU64::new(u64::MAX), enabled_note: None, capped: AtomicBool::new(false), max_hits: 20_000 }))
    }

    /// For when content indexing is off: every content query says so.
    pub fn disabled(why: &str) -> Arc<Self> {
        let conn = ContentStore::open(Path::new(":memory:")).expect("in-memory database");
        Arc::new(ContentIndex { read: Mutex::new(conn), generation: AtomicU64::new(1), pending: AtomicU64::new(0), enabled_note: Some(why.to_owned()), capped: AtomicBool::new(false), max_hits: 0 })
    }

    pub fn stats(&self) -> Result<ContentStats, String> {
        let mut s = self.read.lock().unwrap().stats()?;
        let p = self.pending.load(Ordering::Relaxed);
        s.pending = if p == u64::MAX { 0 } else { p };
        s.last_sync_complete = p == 0;
        Ok(s)
    }
}

impl ContentProvider for ContentIndex {
    fn matcher(&self, text: &str) -> Result<Box<dyn ContentMatch + Send>, String> {
        if let Some(why) = &self.enabled_note {
            return Err(why.clone());
        }
        let (paths, more) = self.read.lock().unwrap().search(text, self.max_hits)?;
        self.capped.store(more, Ordering::Relaxed);
        Ok(Box::new(PathSet::new(&paths)))
    }
    fn generation(&self) -> u64 {
        self.generation.load(Ordering::Relaxed)
    }
    fn note(&self) -> Option<String> {
        if self.enabled_note.is_some() {
            return None;
        }
        let mut parts = Vec::new();
        match self.pending.load(Ordering::Relaxed) {
            0 => {}
            u64::MAX => parts.push("content index: first pass not finished".to_owned()),
            n => parts.push(format!("content index: {n} file(s) not yet read")),
        }
        if self.capped.load(Ordering::Relaxed) {
            parts.push(format!("only the first {} content matches were used", self.max_hits));
        }
        (!parts.is_empty()).then(|| parts.join(", "))
    }
}

/// What the background content indexer works with.
pub struct ContentJob {
    pub index: Arc<ContentIndex>,
    pub db: PathBuf,
    pub table: Arc<RwLock<NameTable>>,
    pub cfg: ContentConfig,
    pub extractor: Box<dyn Extractor + Send>,
    pub governor: Arc<Governor>,
    /// Re-sync interval (it also re-syncs when woken).
    pub every: std::time::Duration,
    pub on_change: Arc<dyn Fn() + Send + Sync>,
}

/// Background content indexing: a thread that re-syncs when woken and
/// periodically, at background priority under the governor.
pub struct ContentService {
    stop: Arc<AtomicBool>,
    wake: std::sync::mpsc::Sender<()>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl ContentService {
    pub fn start(job: ContentJob) -> Self {
        let ContentJob { index, db, table, cfg, extractor: mut ex, governor: gov, every, on_change } = job;
        let stop = Arc::new(AtomicBool::new(false));
        let (wake, rx) = std::sync::mpsc::channel();
        let s2 = stop.clone();
        let handle = std::thread::Builder::new()
            .name("flint-content".into())
            .spawn(move || {
                gov.setup_thread();
                let mut store = match ContentStore::open(&db) {
                    Ok(s) => s,
                    Err(_) => return,
                };
                // Re-sync when woken, every `every`, or when the filename
                // index changed (checked cheaply; at most once a minute), so
                // the first pass after start-up sees the loaded index.
                const CHECK: std::time::Duration = std::time::Duration::from_secs(15);
                const MIN_GAP: std::time::Duration = std::time::Duration::from_secs(60);
                let mut last: Option<(u64, std::time::Instant)> = None;
                loop {
                    let generation = table.read().unwrap().generation();
                    let due = match last {
                        None => true,
                        Some((g, at)) => at.elapsed() >= every || (g != generation && at.elapsed() >= MIN_GAP),
                    };
                    if !due {
                        match rx.recv_timeout(CHECK) {
                            Ok(()) => last = None,
                            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
                            Err(_) => return,
                        }
                        if s2.load(Ordering::Relaxed) {
                            return;
                        }
                        continue;
                    }
                    last = Some((generation, std::time::Instant::now()));
                    let cands = candidates(&table.read().unwrap(), &cfg);
                    let mut since = 0;
                    let r = sync(&mut store, &cands, &cfg, ex.as_mut(), Some(&gov), &s2, || {
                        since += 1;
                        // Make new text searchable in batches.
                        if since % 50 == 0 {
                            index.generation.fetch_add(1, Ordering::Relaxed);
                        }
                    });
                    if let Ok(left) = r {
                        index.pending.store(left, Ordering::Relaxed);
                    }
                    index.generation.fetch_add(1, Ordering::Relaxed);
                    on_change();
                    if s2.load(Ordering::Relaxed) {
                        return;
                    }
                }
            })
            .expect("spawn content indexer");
        ContentService { stop, wake, handle: Some(handle) }
    }

    pub fn wake(&self) {
        let _ = self.wake.send(());
    }
}

impl Drop for ContentService {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        let _ = self.wake.send(());
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use flint_fuzzy::engine::{search_with, CancelToken, Options};
    use flint_fuzzy::query::parse;
    use flint_fuzzy::table::{Meta, NO_PARENT};

    /// Extracts from an in-memory map; `None` simulates a helper crash.
    struct FakeEx(HashMap<String, Option<Result<Extracted, String>>>, Vec<String>);

    impl Extractor for FakeEx {
        fn extract(&mut self, path: &str, _: &Limits) -> Result<Result<Extracted, String>, String> {
            self.1.push(path.to_owned());
            match self.0.get(path) {
                Some(Some(r)) => Ok(r.clone()),
                Some(None) => Err("the preview helper failed on this file (exited)".into()),
                None => Ok(Err("not found".into())),
            }
        }
    }

    fn text(s: &str) -> Option<Result<Extracted, String>> {
        Some(Ok(Extracted::Text { text: s.into(), truncated: false }))
    }

    fn setup() -> (NameTable, ContentConfig) {
        let mut t = NameTable::new();
        let c = t.push(NO_PARENT, "C:", Meta::dir(0));
        let users = t.push(c, "Users", Meta::dir(0));
        let me = t.push(users, "me", Meta::dir(0));
        let docs = t.push(me, "Documents", Meta::dir(0));
        let ad = t.push(me, "AppData", Meta::dir(0));
        t.push(docs, "Budget 2024.xlsx", Meta::file(0, 100, 10));
        t.push(docs, "notes.md", Meta::file(0, 50, 10));
        t.push(docs, "Résumé.docx", Meta::file(0, 70, 10));
        t.push(docs, "photo.jpg", Meta::file(0, 900, 10));
        t.push(docs, "huge.log", Meta::file(0, 900 << 20, 10));
        t.push(docs, "broken.docx", Meta::file(0, 70, 10));
        t.push(docs, "scan.pdf", Meta::file(0, 70, 10));
        t.push(ad, "secrets.txt", Meta::file(0, 10, 10));
        t.push(c, "outside.txt", Meta::file(0, 10, 10));
        let cfg = ContentConfig { roots: vec!["C:\\Users\\me".into()], extensions: ContentConfig::default_extensions().into_iter().chain(["pdf".to_string()]).collect(), excluded: vec![], max_file_bytes: 64 << 20, limits: Limits::default() };
        (t, cfg)
    }

    fn store() -> ContentStore {
        ContentStore::open(Path::new(":memory:")).unwrap()
    }

    #[test]
    fn selects_only_roots_extensions_and_never_sensitive_locations() {
        let (t, mut cfg) = setup();
        let names: Vec<String> = candidates(&t, &cfg).iter().map(|c| c.path.rsplit('\\').next().unwrap().to_owned()).collect();
        assert_eq!(names, ["Budget 2024.xlsx", "notes.md", "Résumé.docx", "huge.log", "broken.docx", "scan.pdf"], "no jpg, nothing outside the root, nothing in AppData");
        cfg.excluded = vec!["c:\\users\\ME\\documents".into()];
        assert!(candidates(&t, &cfg).is_empty(), "user exclusions apply (case-insensitive)");
    }

    #[test]
    fn extraction_states_are_explicit_and_failures_are_contained() {
        let (t, cfg) = setup();
        let p = |n: &str| format!("C:\\Users\\me\\Documents\\{n}");
        let mut ex = FakeEx(
            [
                (p("Budget 2024.xlsx"), text("Quarterly totals and forecast")),
                (p("notes.md"), Some(Ok(Extracted::Text { text: "meeting notes about the café".into(), truncated: true }))),
                (p("Résumé.docx"), text("Curriculum vitae: Rust engineer")),
                (p("broken.docx"), None),
                (p("scan.pdf"), Some(Ok(Extracted::Unsupported("PDF text extraction is not available yet".into())))),
            ]
            .into_iter()
            .collect(),
            vec![],
        );
        let mut s = store();
        let cands = candidates(&t, &cfg);
        let left = sync(&mut s, &cands, &cfg, &mut ex, None, &AtomicBool::new(false), || {}).unwrap();
        assert_eq!(left, 0);
        let state = |n: &str| s.state_of(&p(n)).unwrap().unwrap();
        assert_eq!(state("Budget 2024.xlsx").0, State::Indexed);
        assert_eq!(state("notes.md").0, State::Truncated);
        assert_eq!(state("huge.log").0, State::TooLarge);
        assert!(!ex.1.contains(&p("huge.log")), "oversized files are never read");
        let (st, why) = state("broken.docx");
        assert!(st == State::Failed && why.contains("helper failed"), "a helper crash is one failed file: {why}");
        assert_eq!(state("scan.pdf"), (State::Unsupported, "PDF text extraction is not available yet".into()));
        // Search: words in any order, diacritics folded, prefix on the last.
        assert_eq!(s.search("forecast quarterly", 10).unwrap().0, vec![p("Budget 2024.xlsx")]);
        assert_eq!(s.search("cafe", 10).unwrap().0, vec![p("notes.md")]);
        assert_eq!(s.search("engin", 10).unwrap().0, vec![p("Résumé.docx")]);
        assert!(s.search("\" OR * NEAR(", 10).is_ok(), "FTS syntax in user input is inert");
        // Second pass: nothing changed, nothing re-read.
        ex.1.clear();
        sync(&mut s, &cands, &cfg, &mut ex, None, &AtomicBool::new(false), || {}).unwrap();
        assert!(ex.1.is_empty());
        // A modified file is re-read; a vanished one is dropped.
        let mut changed = cands.clone();
        changed[0].mtime = 99;
        changed.retain(|c| !c.path.ends_with("notes.md"));
        ex.0.insert(p("Budget 2024.xlsx"), text("revised numbers"));
        sync(&mut s, &changed, &cfg, &mut ex, None, &AtomicBool::new(false), || {}).unwrap();
        assert_eq!(ex.1, vec![p("Budget 2024.xlsx")]);
        assert!(s.search("forecast", 10).unwrap().0.is_empty() && s.search("revised", 10).unwrap().0.len() == 1, "old text replaced");
        assert_eq!(s.state_of(&p("notes.md")).unwrap(), None);
        assert!(s.search("meeting", 10).unwrap().0.is_empty());
    }

    #[test]
    fn content_filter_finds_files_by_their_text_in_the_engine() {
        let (t, cfg) = setup();
        let p = |n: &str| format!("C:\\Users\\me\\Documents\\{n}");
        let mut ex = FakeEx([(p("Résumé.docx"), text("Curriculum vitae: Rust engineer")), (p("notes.md"), text("rust meetup"))].into_iter().collect(), vec![]);
        let mut s = store();
        sync(&mut s, &candidates(&t, &cfg), &cfg, &mut ex, None, &AtomicBool::new(false), || {}).unwrap();
        let hits = PathSet::new(&s.search("rust", 100).unwrap().0);
        let run = |q: &str| {
            let r = search_with(&t, &parse(q, 0), &Options::default(), Some(&hits), &CancelToken::new(), |_| {});
            (r.status.stop, r.hits.iter().map(|h| t.name(h.id).to_owned()).collect::<Vec<_>>())
        };
        let (_, mut names) = run("content:rust");
        names.sort();
        assert_eq!(names, ["Résumé.docx", "notes.md"]);
        assert_eq!(run("resume content:rust").1[0], "Résumé.docx", "name text and content combine; the name match ranks first");
        let unsupported = search_with(&t, &parse("content:rust", 0), &Options::default(), None, &CancelToken::new(), |_| {});
        assert_eq!(unsupported.status.stop, flint_fuzzy::engine::Stop::Unsupported, "without an index content: is reported, never ignored");
    }

    #[test]
    fn stopping_mid_sync_leaves_the_rest_pending() {
        let (t, cfg) = setup();
        let mut ex = FakeEx(HashMap::new(), vec![]);
        let stop = AtomicBool::new(false);
        let mut n = 0;
        let left = sync(&mut store(), &candidates(&t, &cfg), &cfg, &mut ex, None, &stop, || {
            n += 1;
            if n == 2 {
                stop.store(true, Ordering::Relaxed);
            }
        }).unwrap();
        assert_eq!(left, 4);
    }

    #[test]
    fn query_text_never_reaches_fts_as_syntax() {
        assert_eq!(fts_query("rust engineer"), "\"rust\" \"engineer\"*");
        assert_eq!(fts_query("a\"b OR c"), "\"a\" \"b\" \"OR\" \"c\"*");
        assert_eq!(fts_query("  ** "), "");
    }
}
