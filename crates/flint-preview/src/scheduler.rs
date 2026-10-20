//! Viewer scheduling: latest selection wins.
//!
//! * Every request carries the selection generation. Requests superseded
//!   while waiting are dropped without work; results that arrive after the
//!   selection moved on are dropped without being shown.
//! * Rapid selection changes are debounced, so scrolling through a list
//!   previews only where the user stops.
//! * Results are cached by file identity (path, size, mtime, file id); a
//!   changed file has a different key, so a stale preview is never served.
//!   The cache is bounded by bytes and entries.
//!
//! A `Viewer` exists only while the viewer is enabled; creating none means
//! no thread, no helper process and no file access.

use crate::{Limits, Preview};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{channel, RecvTimeoutError, Sender};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Duration;

/// What a preview depends on. Any change makes a new key.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct FileKey {
    pub path: String,
    pub size: u64,
    pub mtime: i64,
    pub file_id: u64,
}

pub trait Source: Send + 'static {
    fn preview(&mut self, path: &str, limits: &Limits) -> Result<Preview, String>;
}

impl Source for crate::client::HostClient {
    fn preview(&mut self, path: &str, limits: &Limits) -> Result<Preview, String> {
        crate::client::HostClient::preview(self, path, limits)
    }
}

pub type Delivered = Result<Arc<Preview>, String>;

enum Msg {
    Show { generation: u64, key: FileKey },
    Stop,
}

#[derive(Clone, Debug)]
pub struct ViewerOptions {
    pub limits: Limits,
    pub debounce: Duration,
    pub cache_bytes: usize,
    pub cache_entries: usize,
}

impl Default for ViewerOptions {
    fn default() -> Self {
        ViewerOptions { limits: Limits::default(), debounce: Duration::from_millis(60), cache_bytes: 16 << 20, cache_entries: 64 }
    }
}

pub fn preview_bytes(p: &Preview) -> usize {
    match p {
        Preview::Text { text, .. } => text.len(),
        Preview::Hex { offset_bytes, .. } => offset_bytes.len(),
        Preview::Image { thumbnail, .. } => thumbnail.as_ref().map_or(0, |t| t.bgra.len()),
        Preview::Archive { entries, warnings, .. } => entries.iter().map(|e| e.name.len() + 24).sum::<usize>() + warnings.iter().map(String::len).sum::<usize>(),
        Preview::Unsupported { kind, reason } => kind.len() + reason.len(),
    }
}

struct Cache {
    items: Vec<(FileKey, Arc<Preview>, usize)>,
    bytes: usize,
    max_bytes: usize,
    max_entries: usize,
}

impl Cache {
    fn get(&mut self, k: &FileKey) -> Option<Arc<Preview>> {
        let i = self.items.iter().position(|(key, _, _)| key == k)?;
        let item = self.items.remove(i);
        let p = item.1.clone();
        self.items.push(item);
        Some(p)
    }
    fn put(&mut self, k: FileKey, p: Arc<Preview>) {
        let n = preview_bytes(&p);
        if n > self.max_bytes {
            return;
        }
        // One entry per path: an older version of the file is evicted.
        self.items.retain(|(key, _, b)| {
            let keep = key.path != k.path;
            if !keep {
                self.bytes -= b;
            }
            keep
        });
        while !self.items.is_empty() && (self.bytes + n > self.max_bytes || self.items.len() >= self.max_entries) {
            self.bytes -= self.items.remove(0).2;
        }
        self.bytes += n;
        self.items.push((k, p, n));
    }
}

pub struct Viewer {
    tx: Sender<Msg>,
    latest: Arc<AtomicU64>,
    worker: Option<JoinHandle<()>>,
}

impl Viewer {
    /// `sink(generation, result)` runs on the viewer thread for results
    /// still current when they complete.
    pub fn new(mut source: impl Source, opts: ViewerOptions, sink: impl Fn(u64, Delivered) + Send + 'static) -> Self {
        let (tx, rx) = channel::<Msg>();
        let latest = Arc::new(AtomicU64::new(0));
        let latest_w = latest.clone();
        let worker = std::thread::Builder::new()
            .name("flint-viewer".into())
            .spawn(move || {
                let mut cache = Cache { items: Vec::new(), bytes: 0, max_bytes: opts.cache_bytes, max_entries: opts.cache_entries.max(1) };
                loop {
                    let Ok(Msg::Show { mut generation, mut key }) = rx.recv() else { return };
                    // Debounce: take the newest of a burst.
                    loop {
                        match rx.recv_timeout(opts.debounce) {
                            Ok(Msg::Show { generation: g, key: k }) => (generation, key) = (g, k),
                            Ok(Msg::Stop) | Err(RecvTimeoutError::Disconnected) => return,
                            Err(RecvTimeoutError::Timeout) => break,
                        }
                    }
                    if generation != latest_w.load(Ordering::SeqCst) {
                        continue;
                    }
                    let result = match cache.get(&key) {
                        Some(p) => Ok(p),
                        None => source.preview(&key.path, &opts.limits).map(|p| {
                            let p = Arc::new(p);
                            cache.put(key.clone(), p.clone());
                            p
                        }),
                    };
                    if generation == latest_w.load(Ordering::SeqCst) {
                        sink(generation, result);
                    }
                }
            })
            .expect("spawn viewer");
        Viewer { tx, latest, worker: Some(worker) }
    }

    /// Requests a preview for the new selection (`generation` must
    /// increase). Earlier requests become stale immediately.
    pub fn show(&self, generation: u64, key: FileKey) {
        self.latest.store(generation, Ordering::SeqCst);
        let _ = self.tx.send(Msg::Show { generation, key });
    }

    /// The selection no longer has a previewable item: drop pending work.
    pub fn clear(&self, generation: u64) {
        self.latest.store(generation, Ordering::SeqCst);
    }
}

impl Drop for Viewer {
    fn drop(&mut self) {
        let _ = self.tx.send(Msg::Stop);
        if let Some(w) = self.worker.take() {
            let _ = w.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    struct Fake {
        calls: Arc<Mutex<Vec<String>>>,
        delay: Duration,
    }

    impl Source for Fake {
        fn preview(&mut self, path: &str, _: &Limits) -> Result<Preview, String> {
            self.calls.lock().unwrap().push(path.to_owned());
            std::thread::sleep(self.delay);
            if path.ends_with("bad") {
                return Err("cannot read".into());
            }
            Ok(Preview::Text { text: format!("contents of {path}"), encoding: "UTF-8".into(), truncated: false })
        }
    }

    fn key(p: &str, mtime: i64) -> FileKey {
        FileKey { path: p.into(), size: 1, mtime, file_id: 1 }
    }

    type Log<T> = Arc<Mutex<Vec<T>>>;

    fn viewer(delay_ms: u64) -> (Viewer, Log<String>, Log<(u64, Delivered)>) {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let got = Arc::new(Mutex::new(Vec::new()));
        let g2 = got.clone();
        let v = Viewer::new(Fake { calls: calls.clone(), delay: Duration::from_millis(delay_ms) }, ViewerOptions { debounce: Duration::from_millis(30), ..Default::default() }, move |g, r| g2.lock().unwrap().push((g, r)));
        (v, calls, got)
    }

    fn wait(cond: impl Fn() -> bool) {
        let t = std::time::Instant::now();
        while !cond() {
            assert!(t.elapsed() < Duration::from_secs(5), "timed out");
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    fn rapid_selection_previews_only_where_the_user_stops() {
        let (v, calls, got) = viewer(20);
        for g in 1..=200u64 {
            v.show(g, key(&format!("f{g}"), 0));
        }
        wait(|| !got.lock().unwrap().is_empty());
        std::thread::sleep(Duration::from_millis(150));
        let got = got.lock().unwrap();
        assert_eq!(got.len(), 1, "exactly one delivery");
        assert_eq!(got[0].0, 200);
        assert!(calls.lock().unwrap().len() <= 2, "superseded selections cost no work: {:?}", calls.lock().unwrap());
    }

    #[test]
    fn a_result_that_completes_after_the_selection_moved_is_dropped() {
        let (v, calls, got) = viewer(200);
        v.show(1, key("slow", 0));
        wait(|| calls.lock().unwrap().len() == 1);
        v.show(2, key("next", 0)); // selection moves while "slow" is in flight
        wait(|| got.lock().unwrap().iter().any(|(g, _)| *g == 2));
        assert!(got.lock().unwrap().iter().all(|(g, _)| *g == 2), "stale preview never shown");
        v.clear(3);
        v.show(4, key("bad", 0));
        wait(|| got.lock().unwrap().iter().any(|(g, _)| *g == 4));
        assert!(got.lock().unwrap().last().unwrap().1.is_err(), "failures are delivered as such");
    }

    #[test]
    fn cache_serves_unchanged_files_and_misses_changed_ones() {
        let (v, calls, got) = viewer(0);
        v.show(1, key("a", 1));
        wait(|| got.lock().unwrap().len() == 1);
        v.show(2, key("a", 1));
        wait(|| got.lock().unwrap().len() == 2);
        assert_eq!(calls.lock().unwrap().len(), 1, "second view served from cache");
        v.show(3, key("a", 2)); // modified since
        wait(|| got.lock().unwrap().len() == 3);
        assert_eq!(calls.lock().unwrap().len(), 2, "a changed file is previewed again");
        drop(v); // stops and joins
    }

    #[test]
    fn cache_is_bounded() {
        let mut c = Cache { items: Vec::new(), bytes: 0, max_bytes: 100, max_entries: 3 };
        let p = |n: usize| Arc::new(Preview::Text { text: "x".repeat(n), encoding: String::new(), truncated: false });
        c.put(key("a", 0), p(40));
        c.put(key("b", 0), p(40));
        c.put(key("c", 0), p(40));
        assert!(c.bytes <= 100 && c.get(&key("a", 0)).is_none(), "oldest evicted for space");
        c.put(key("b", 1), p(10));
        assert!(c.get(&key("b", 0)).is_none() && c.get(&key("b", 1)).is_some(), "one version per path");
        c.put(key("huge", 0), p(1000));
        assert!(c.get(&key("huge", 0)).is_none(), "oversized results are not cached");
        assert_eq!(c.bytes, c.items.iter().map(|i| i.2).sum::<usize>());
    }
}
