//! Live-search session: debouncing, cancellation of obsolete searches,
//! stale-response suppression and a small in-memory result cache.
//!
//! Every submitted query gets a strictly increasing generation number.
//! Submitting cancels the in-flight search immediately; the worker only
//! delivers events whose generation is still the latest, so a slow old search
//! can never overwrite newer results. Nothing here is persisted (search
//! history is off by default and lives elsewhere if enabled).

use crate::engine::{search_with, CancelToken, ContentMatch, Options, SearchResult};
use crate::query::Filter;
use crate::query::{parse_with, Clock, UtcOffset};
use crate::table::NameTable;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{channel, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex, RwLock};
use std::thread::JoinHandle;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

#[derive(Clone, Debug)]
pub enum Event {
    /// Ranked snapshot after a stage; more may follow.
    Partial { generation: u64, result: SearchResult },
    Final { generation: u64, result: SearchResult, cached: bool },
}

impl Event {
    pub fn generation(&self) -> u64 {
        match self {
            Event::Partial { generation, .. } | Event::Final { generation, .. } => *generation,
        }
    }
}

enum Msg {
    Query { generation: u64, raw: String, opts: Options },
    ClearCache,
    Stop,
}

pub struct Session {
    tx: Sender<Msg>,
    latest: Arc<AtomicU64>,
    inflight: Arc<Mutex<CancelToken>>,
    worker: Option<JoinHandle<()>>,
}

const CACHE_SLOTS: usize = 16;

/// (query text, options, table generation, hour, content generation).
type CacheKey = (String, Options, u64, i64, u64);

/// Answers `content:` filters from a content index.
pub trait ContentProvider: Send + Sync {
    /// Matcher for one `content:` filter's text.
    fn matcher(&self, text: &str) -> Result<Box<dyn ContentMatch + Send>, String>;
    /// Changes whenever the indexed content changes (part of the cache key).
    fn generation(&self) -> u64;
    /// Coverage caveat for the status line ("content index: 40 files pending").
    fn note(&self) -> Option<String>;
}

/// Several `content:` filters must all match.
struct AllOf(Vec<Box<dyn ContentMatch + Send>>);

impl ContentMatch for AllOf {
    fn matches(&self, t: &NameTable, id: crate::table::EntryId) -> bool {
        self.0.iter().all(|m| m.matches(t, id))
    }
}

pub struct SessionConfig {
    pub debounce: Duration,
    /// Time zone for date filters (`|_| 0` = UTC).
    pub offset: Arc<UtcOffset>,
    pub content: Option<Arc<dyn ContentProvider>>,
}

impl Session {
    /// `sink` is called on the worker thread; it should post to the UI thread.
    /// Dates in queries are interpreted in UTC.
    pub fn new(table: Arc<RwLock<NameTable>>, debounce: Duration, sink: impl Fn(Event) + Send + 'static) -> Self {
        Self::with_time_zone(table, debounce, Arc::new(|_| 0), sink)
    }

    /// Like [`new`](Self::new), with dates in queries interpreted in the
    /// zone given by `offset` (normally the user's local zone).
    pub fn with_time_zone(table: Arc<RwLock<NameTable>>, debounce: Duration, offset: Arc<UtcOffset>, sink: impl Fn(Event) + Send + 'static) -> Self {
        Self::start(table, SessionConfig { debounce, offset, content: None }, sink)
    }

    /// The general constructor: time zone and an optional content index.
    pub fn start(table: Arc<RwLock<NameTable>>, cfg: SessionConfig, sink: impl Fn(Event) + Send + 'static) -> Self {
        let SessionConfig { debounce, offset, content } = cfg;
        let (tx, rx) = channel::<Msg>();
        let latest = Arc::new(AtomicU64::new(0));
        let inflight = Arc::new(Mutex::new(CancelToken::new()));
        let (latest_w, inflight_w) = (latest.clone(), inflight.clone());
        let worker = std::thread::Builder::new()
            .name("flint-search".into())
            .spawn(move || {
                // Keyed by text, options, table generation and the hour, so
                // relative dates ("today") never outlive their meaning.
                let mut cache: Vec<(CacheKey, SearchResult)> = Vec::new();
                let mut pending: Option<Msg> = None;
                loop {
                    let msg = match pending.take() {
                        Some(m) => m,
                        None => match rx.recv() {
                            Ok(m) => m,
                            Err(_) => return,
                        },
                    };
                    let (generation, raw, opts) = match msg {
                        Msg::Stop => return,
                        Msg::ClearCache => {
                            cache.clear();
                            continue;
                        }
                        Msg::Query { generation, raw, opts } => (generation, raw, opts),
                    };
                    // Debounce: absorb newer queries arriving within the window.
                    let mut cur = (generation, raw, opts);
                    loop {
                        match rx.recv_timeout(debounce) {
                            Ok(Msg::Query { generation, raw, opts }) => cur = (generation, raw, opts),
                            Ok(Msg::ClearCache) => cache.clear(),
                            Ok(Msg::Stop) | Err(RecvTimeoutError::Disconnected) => return,
                            Err(RecvTimeoutError::Timeout) => break,
                        }
                    }
                    let (generation, raw, opts) = cur;
                    if generation != latest_w.load(Ordering::SeqCst) {
                        continue;
                    }
                    let cancel = inflight_w.lock().unwrap().clone();
                    let t = table.read().unwrap();
                    let now = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs() as i64);
                    let content_gen = content.as_ref().map_or(0, |c| c.generation());
                    let key = (raw.clone(), opts.clone(), t.generation(), now / 3600, content_gen);
                    if let Some(pos) = cache.iter().position(|(k, _)| *k == key) {
                        let entry = cache.remove(pos);
                        let result = entry.1.clone();
                        cache.push(entry);
                        sink(Event::Final { generation, result, cached: true });
                        continue;
                    }
                    let q = parse_with(&raw, &Clock { now, offset: &*offset });
                    // content: filters, answered by the content index if any.
                    let mut notes = Vec::new();
                    let wanted: Vec<&str> = q.filters.iter().filter_map(|f| if let Filter::Content(s) = f { Some(s.as_str()) } else { None }).collect();
                    let matcher: Option<AllOf> = match (&content, wanted.is_empty()) {
                        (_, true) | (None, _) => None,
                        (Some(c), false) => {
                            notes.extend(c.note());
                            match wanted.iter().map(|w| c.matcher(w)).collect::<Result<Vec<_>, _>>() {
                                Ok(ms) => Some(AllOf(ms)),
                                Err(e) => {
                                    notes.push(format!("content search failed: {e}"));
                                    None
                                }
                            }
                        }
                    };
                    let cm = matcher.as_ref().map(|m| m as &dyn ContentMatch);
                    let mut result = search_with(&t, &q, &opts, cm, &cancel, |partial| {
                        if latest_w.load(Ordering::SeqCst) == generation {
                            sink(Event::Partial { generation, result: partial.clone() });
                        }
                    });
                    drop(t);
                    result.status.notes.extend(notes);
                    if cancel.is_canceled() || latest_w.load(Ordering::SeqCst) != generation {
                        continue;
                    }
                    if cache.len() >= CACHE_SLOTS {
                        cache.remove(0);
                    }
                    cache.push((key, result.clone()));
                    sink(Event::Final { generation, result, cached: false });
                    // Anything that queued while we searched (a newer query, a
                    // cache clear or Stop) is handled next; never dropped.
                    pending = rx.try_recv().ok();
                }
            })
            .expect("spawn search worker");
        Session { tx, latest, inflight, worker: Some(worker) }
    }

    /// Submits a query, cancelling any in-flight search. Returns its generation.
    pub fn submit(&self, raw: &str, opts: Options) -> u64 {
        let generation = self.latest.fetch_add(1, Ordering::SeqCst) + 1;
        {
            let mut inflight = self.inflight.lock().unwrap();
            inflight.cancel();
            *inflight = CancelToken::new();
        }
        let _ = self.tx.send(Msg::Query { generation, raw: raw.to_owned(), opts });
        generation
    }

    /// True if events for `generation` should still be displayed.
    pub fn is_current(&self, generation: u64) -> bool {
        self.latest.load(Ordering::SeqCst) == generation
    }

    pub fn clear_cache(&self) {
        let _ = self.tx.send(Msg::ClearCache);
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        self.inflight.lock().unwrap().cancel();
        let _ = self.tx.send(Msg::Stop);
        if let Some(w) = self.worker.take() {
            let _ = w.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::table::{Meta, NO_PARENT};
    use std::sync::mpsc;

    fn big_table() -> Arc<RwLock<NameTable>> {
        let mut t = NameTable::new();
        let root = t.push(NO_PARENT, "C:", Meta::dir(0));
        for i in 0..200_000 {
            t.push(root, &format!("file_{i}_report_{}.txt", i % 97), Meta::file(0, i, 0));
        }
        Arc::new(RwLock::new(t))
    }

    #[test]
    fn rapid_typing_delivers_only_the_latest_final() {
        let table = big_table();
        let (tx, rx) = mpsc::channel();
        let s = Session::new(table, Duration::from_millis(5), move |e| tx.send(e).unwrap());
        let mut last = 0;
        for q in ["r", "re", "rep", "repo", "repor", "report_4"] {
            last = s.submit(q, Options { exhaustive: true, enough: usize::MAX, ..Default::default() });
        }
        let mut finals = Vec::new();
        while let Ok(e) = rx.recv_timeout(Duration::from_secs(10)) {
            if let Event::Final { generation, .. } = e {
                finals.push(generation);
                if generation == last {
                    break;
                }
            }
        }
        assert_eq!(finals, vec![last], "stale finals must be suppressed");
    }

    /// Regression: a Stop arriving while a search ran used to be swallowed,
    /// hanging `Drop`.
    #[test]
    fn drop_during_search_does_not_hang() {
        for _ in 0..20 {
            let table = big_table();
            let s = Session::new(table, Duration::from_millis(0), |_| {});
            s.submit("report", Options { exhaustive: true, enough: usize::MAX, ..Default::default() });
            std::thread::sleep(Duration::from_millis(2));
            drop(s);
        }
    }

    #[test]
    fn repeated_query_is_served_from_cache_until_index_changes() {
        let table = big_table();
        let (tx, rx) = mpsc::channel();
        let s = Session::new(table.clone(), Duration::from_millis(1), move |e| tx.send(e).unwrap());
        let wait_final = || loop {
            if let Event::Final { cached, result, .. } = rx.recv_timeout(Duration::from_secs(10)).unwrap() {
                return (cached, result);
            }
        };
        s.submit("report_5", Options::default());
        assert!(!wait_final().0);
        s.submit("report_5", Options::default());
        assert!(wait_final().0);
        table.write().unwrap().push(NO_PARENT, "report_5_new", Meta::file(0, 0, 0));
        s.submit("report_5", Options::default());
        let (cached, result) = wait_final();
        assert!(!cached, "index generation change must invalidate the cache");
        assert!(result.hits.iter().any(|h| h.tier >= crate::engine::tier::PREFIX));
    }
}
