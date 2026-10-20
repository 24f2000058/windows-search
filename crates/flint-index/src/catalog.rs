//! Multi-volume orchestration.
//!
//! One worker thread per volume, each with its own SQLite connection, so a
//! slow or hung volume (network share, failing USB disk) never delays
//! another. A worker loads its volume (snapshot or SQLite), then loops:
//! sync, bounded metadata refresh, periodic snapshot, wait. Failures move the
//! volume to `Waiting` with exponential backoff and a fresh backend on retry,
//! which also covers unplug/reconnect. Removing a volume takes its entries
//! out of search (they could not be opened) but keeps them in the store, so
//! re-arrival is fast.

use crate::governor::{Governor, GovernorOptions};
use crate::indexer::{Stats, SyncError, VolumeIndexer};
use crate::model::{Frn, SourceError, VolumeState};
use crate::source::JournalSource;
use crate::store::{Store, StoreError};
use crate::traverse::{Fs, TraversalSource};
use flint_fuzzy::table::NameTable;
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{channel, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex, RwLock};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

pub enum Backend {
    Journal(Box<dyn JournalSource + Send>),
    /// Periodic full traversal (no change notifications available).
    Traversal(TraversalSource<Box<dyn Fs + Send>>),
    /// Traversal once, then incremental reconciliation driven by a change
    /// feed (e.g. `ReadDirectoryChangesW`). The feed must be started before
    /// the traversal so no change is missed in between.
    Watched(TraversalSource<Box<dyn Fs + Send>>, Box<dyn ChangeFeed>),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FeedEvent {
    Idle,
    /// Volume-relative directories whose contents changed.
    Dirs(Vec<PathBuf>),
    /// Notifications were lost (buffer overflow): a full traversal is needed.
    Overflow,
    Failed(String),
}

pub trait ChangeFeed: Send {
    /// Waits up to `timeout` for changes.
    fn poll(&mut self, timeout: Duration) -> FeedEvent;
}

/// Opens a volume's backend. Called again after every failure. `journal_failed`
/// tells the opener the journal is unusable (no journal, no privilege) so it
/// can return a traversal backend instead.
pub type Opener = Box<dyn FnMut(bool) -> Result<Backend, SourceError> + Send>;

#[derive(Clone, Debug)]
pub struct VolumeSpec {
    /// Stable identity (e.g. `\\?\Volume{GUID}\`), the store key.
    pub key: String,
    /// Display root (e.g. `C:`).
    pub root_name: String,
    pub root_frn: Frn,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Phase {
    Loading,
    Indexing,
    Live,
    Traversed { complete: bool },
    Waiting(String),
    Stopped,
}

#[derive(Clone, Debug)]
pub struct VolumeStatus {
    pub key: String,
    pub root_name: String,
    pub volume: u16,
    pub phase: Phase,
    /// Entries seen by the running enumeration.
    pub progress: u64,
    pub load_source: Option<String>,
    pub stale_meta: usize,
    pub state: VolumeState,
    pub stats: Stats,
    pub snapshots_written: u64,
    /// Waiting for another volume's scan (one heavy scan runs at a time).
    pub queued: bool,
}

#[derive(Clone, Debug)]
pub struct CatalogOptions {
    /// Journal poll interval while live.
    pub poll: Duration,
    pub backoff_min: Duration,
    pub backoff_max: Duration,
    pub snapshot_every: Duration,
    /// Metadata refreshes per worker iteration.
    pub meta_batch: usize,
    /// Re-traverse journal-less volumes this often (`None`: only on wake).
    pub traversal_rescan: Option<Duration>,
}

impl Default for CatalogOptions {
    fn default() -> Self {
        CatalogOptions {
            poll: Duration::from_secs(1),
            backoff_min: Duration::from_secs(1),
            backoff_max: Duration::from_secs(60),
            snapshot_every: Duration::from_secs(600),
            meta_batch: 2000,
            traversal_rescan: Some(Duration::from_secs(1800)),
        }
    }
}

/// Aggregated index coverage, for the UI and diagnostics.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Coverage {
    pub complete: bool,
    pub loading: bool,
    pub problems: Vec<String>,
}

struct Worker {
    volume: u16,
    status: Arc<Mutex<VolumeStatus>>,
    progress: Arc<Mutex<Option<Arc<AtomicU64>>>>,
    queued: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
    wake: Sender<()>,
    handle: JoinHandle<()>,
}

pub type OnChange = Arc<dyn Fn() + Send + Sync>;

pub struct Catalog {
    table: Arc<RwLock<NameTable>>,
    store_path: PathBuf,
    snapshot_dir: Option<PathBuf>,
    opts: CatalogOptions,
    on_change: OnChange,
    workers: Mutex<BTreeMap<String, Worker>>,
    next_volume: Mutex<u16>,
    governor: Arc<Governor>,
}

impl Catalog {
    /// Opens (and migrates) the store once up front, so workers never race
    /// on schema creation.
    pub fn new(table: Arc<RwLock<NameTable>>, store_path: PathBuf, snapshot_dir: Option<PathBuf>, opts: CatalogOptions, on_change: OnChange) -> Result<Self, StoreError> {
        drop(Store::open(&store_path)?);
        Ok(Catalog { table, store_path, snapshot_dir, opts, on_change, workers: Mutex::new(BTreeMap::new()), next_volume: Mutex::new(1), governor: Governor::new(GovernorOptions::default(), None) })
    }

    /// Replaces the resource governor; affects volumes added afterwards.
    pub fn with_governor(mut self, g: Arc<Governor>) -> Self {
        self.governor = g;
        self
    }

    pub fn governor(&self) -> &Arc<Governor> {
        &self.governor
    }

    /// Starts indexing a volume (no-op if already present). Returns its id.
    pub fn add(&self, spec: VolumeSpec, opener: Opener) -> u16 {
        let mut workers = self.workers.lock().unwrap();
        if let Some(w) = workers.get(&spec.key) {
            return w.volume;
        }
        let volume = {
            let mut n = self.next_volume.lock().unwrap();
            let v = *n;
            *n = n.wrapping_add(1).max(1);
            v
        };
        let status = Arc::new(Mutex::new(VolumeStatus {
            key: spec.key.clone(),
            root_name: spec.root_name.clone(),
            volume,
            phase: Phase::Loading,
            progress: 0,
            load_source: None,
            stale_meta: 0,
            state: VolumeState::NotIndexed,
            stats: Stats::default(),
            snapshots_written: 0,
            queued: false,
        }));
        let stop = Arc::new(AtomicBool::new(false));
        let progress = Arc::new(Mutex::new(None));
        let queued = Arc::new(AtomicBool::new(false));
        let (wake, wake_rx) = channel();
        let ctx = WorkerCtx {
            spec: spec.clone(),
            volume,
            opener,
            table: self.table.clone(),
            store_path: self.store_path.clone(),
            snapshot_dir: self.snapshot_dir.clone(),
            opts: self.opts.clone(),
            status: status.clone(),
            progress: progress.clone(),
            stop: stop.clone(),
            wake_rx,
            on_change: self.on_change.clone(),
            governor: self.governor.clone(),
            queued: queued.clone(),
        };
        let handle = std::thread::Builder::new().name(format!("flint-vol-{}", spec.root_name)).spawn(move || ctx.run()).expect("spawn volume worker");
        workers.insert(spec.key, Worker { volume, status, progress, queued, stop, wake, handle });
        (self.on_change)();
        volume
    }

    /// Stops a volume's worker and removes its entries from search. Its
    /// store rows (and snapshot) are kept for a fast return.
    pub fn remove(&self, key: &str) -> bool {
        let Some(w) = self.workers.lock().unwrap().remove(key) else { return false };
        w.stop.store(true, Ordering::SeqCst);
        let _ = w.wake.send(());
        let _ = w.handle.join();
        let mut t = self.table.write().unwrap();
        for id in 0..t.len() as u32 {
            if t.is_live(id) && t.volume(id) == w.volume {
                t.remove(id);
            }
        }
        drop(t);
        (self.on_change)();
        true
    }

    /// Prompts a worker to sync now (e.g. after a change notification).
    pub fn wake(&self, key: &str) {
        if let Some(w) = self.workers.lock().unwrap().get(key) {
            let _ = w.wake.send(());
        }
    }

    pub fn keys(&self) -> Vec<String> {
        self.workers.lock().unwrap().keys().cloned().collect()
    }

    pub fn statuses(&self) -> Vec<VolumeStatus> {
        self.workers
            .lock()
            .unwrap()
            .values()
            .map(|w| {
                let mut s = w.status.lock().unwrap().clone();
                if let Some(p) = w.progress.lock().unwrap().as_ref() {
                    s.progress = p.load(Ordering::Relaxed);
                }
                s.queued = w.queued.load(Ordering::Relaxed);
                s
            })
            .collect()
    }

    /// True if the volume's entry keys are real file IDs (journal-indexed
    /// NTFS), which file operations compare with the live file. Traversal
    /// volumes use synthetic keys.
    pub fn has_file_ids(&self, volume: u16) -> bool {
        self.workers.lock().unwrap().values().find(|w| w.volume == volume).is_some_and(|w| matches!(w.status.lock().unwrap().state, VolumeState::Live { .. } | VolumeState::Enumerating { .. }))
    }

    pub fn coverage(&self) -> Coverage {
        let st = self.statuses();
        let mut c = Coverage { complete: !st.is_empty(), loading: false, problems: Vec::new() };
        if st.is_empty() {
            c.problems.push("no volumes configured".into());
        }
        for s in &st {
            let problem = match &s.phase {
                Phase::Live => None,
                Phase::Traversed { complete: true } => None,
                Phase::Loading => {
                    c.loading = true;
                    Some(format!("{} loading index", s.root_name))
                }
                Phase::Indexing if s.queued => Some(format!("{} queued (one volume is indexed at a time)", s.root_name)),
                Phase::Indexing => Some(format!("{} indexing ({} entries so far)", s.root_name, s.progress)),
                Phase::Traversed { complete: false } => Some(format!("{} partially indexed (unreadable folders)", s.root_name)),
                Phase::Waiting(why) => Some(format!("{} unavailable: {why}", s.root_name)),
                Phase::Stopped => Some(format!("{} stopped", s.root_name)),
            };
            if let Some(p) = problem {
                c.complete = false;
                c.problems.push(p);
            }
        }
        c
    }

    /// Stops all workers (each writes a snapshot if it has unsaved state).
    pub fn shutdown(&self) {
        let workers: Vec<Worker> = std::mem::take(&mut *self.workers.lock().unwrap()).into_values().collect();
        for w in &workers {
            w.stop.store(true, Ordering::SeqCst);
            let _ = w.wake.send(());
        }
        for w in workers {
            let _ = w.handle.join();
        }
    }
}

impl Drop for Catalog {
    fn drop(&mut self) {
        self.shutdown();
    }
}

struct WorkerCtx {
    spec: VolumeSpec,
    volume: u16,
    opener: Opener,
    table: Arc<RwLock<NameTable>>,
    store_path: PathBuf,
    snapshot_dir: Option<PathBuf>,
    opts: CatalogOptions,
    status: Arc<Mutex<VolumeStatus>>,
    progress: Arc<Mutex<Option<Arc<AtomicU64>>>>,
    stop: Arc<AtomicBool>,
    wake_rx: Receiver<()>,
    on_change: OnChange,
    governor: Arc<Governor>,
    queued: Arc<AtomicBool>,
}

impl WorkerCtx {
    fn set(&self, f: impl FnOnce(&mut VolumeStatus)) {
        let changed = {
            let mut s = self.status.lock().unwrap();
            let before = (s.phase.clone(), s.stale_meta, s.snapshots_written);
            f(&mut s);
            before != (s.phase.clone(), s.stale_meta, s.snapshots_written)
        };
        if changed {
            (self.on_change)();
        }
    }

    /// Sleeps up to `d`, returning early on wake or stop. Returns false if
    /// the worker should exit.
    fn wait(&self, d: Duration) -> bool {
        if self.stop.load(Ordering::SeqCst) {
            return false;
        }
        match self.wake_rx.recv_timeout(d) {
            Ok(()) | Err(RecvTimeoutError::Timeout) => !self.stop.load(Ordering::SeqCst),
            Err(RecvTimeoutError::Disconnected) => false,
        }
    }

    fn run(mut self) {
        self.governor.setup_thread();
        let mut backoff = self.opts.backoff_min;
        // Store: retried with backoff (e.g. disk temporarily full/locked).
        let mut store = loop {
            match Store::open(&self.store_path) {
                Ok((s, _)) => break s,
                Err(e) => {
                    self.set(|s| s.phase = Phase::Waiting(format!("index database: {e}")));
                    if !self.wait(backoff) {
                        self.set(|s| s.phase = Phase::Stopped);
                        return;
                    }
                    backoff = (backoff * 2).min(self.opts.backoff_max);
                }
            }
        };
        let (mut ix, load) = match VolumeIndexer::open_with_snapshot(&mut store, &self.table, self.volume, &self.spec.key, &self.spec.root_name, self.spec.root_frn, self.snapshot_dir.as_deref()) {
            Ok(x) => x,
            Err(e) => {
                self.set(|s| s.phase = Phase::Waiting(format!("loading index: {e}")));
                return;
            }
        };
        ix.governor = Some(self.governor.clone());
        ix.queued = self.queued.clone();
        {
            // Loading from SQLite grows the columns by doubling.
            let mut t = self.table.write().unwrap();
            if t.slack_bytes() > t.heap_bytes() / 8 {
                t.shrink_to_fit();
            }
        }
        *self.progress.lock().unwrap() = Some(ix.progress.clone());
        self.set(|s| {
            s.load_source = Some(format!("{load:?}"));
            s.state = ix.state.clone();
            // Loaded data is searchable now; it becomes Live after the first sync.
            s.phase = Phase::Indexing;
        });
        let mut backend: Option<Backend> = None;
        let mut journal_failed = false;
        let mut last_seq = store.commit_seq_for(&self.spec.key).unwrap_or(0);
        let mut snapshot_seq = if matches!(load, crate::indexer::LoadSource::Snapshot) { last_seq } else { u64::MAX };
        let mut last_snapshot = Instant::now();
        let mut traversed_at: Option<Instant> = None;
        backoff = self.opts.backoff_min;
        loop {
            if self.stop.load(Ordering::SeqCst) {
                break;
            }
            if backend.is_none() {
                match (self.opener)(journal_failed) {
                    Ok(b) => backend = Some(b),
                    Err(e) => {
                        self.set(|s| s.phase = Phase::Waiting(format!("{e:?}")));
                        if !self.wait(backoff) {
                            break;
                        }
                        backoff = (backoff * 2).min(self.opts.backoff_max);
                        continue;
                    }
                }
            }
            let next_wait = match backend.as_mut().unwrap() {
                Backend::Journal(src) => {
                    let res = ix.sync(src.as_mut(), &mut store, &self.table, &self.stop);
                    match res {
                        Ok(()) => {
                            backoff = self.opts.backoff_min;
                            let stale = if self.opts.meta_batch > 0 { ix.refresh_stale_meta(src.as_mut(), &mut store, &self.table, self.opts.meta_batch).unwrap_or(0) } else { 0 };
                            self.set(|s| {
                                s.phase = Phase::Live;
                                s.stale_meta = stale;
                                s.state = ix.state.clone();
                                s.stats = ix.stats.clone();
                            });
                            // More metadata to refresh: come back soon.
                            if stale > 0 { Duration::from_millis(50) } else { self.opts.poll }
                        }
                        Err(SyncError::Canceled) => break,
                        Err(SyncError::Source(SourceError::JournalNotActive | SourceError::AccessDenied)) => {
                            journal_failed = true;
                            backend = None;
                            Duration::ZERO
                        }
                        Err(e) => {
                            backend = None;
                            self.set(|s| {
                                s.phase = Phase::Waiting(format!("{e:?}"));
                                s.stats = ix.stats.clone();
                            });
                            let w = backoff;
                            backoff = (backoff * 2).min(self.opts.backoff_max);
                            w
                        }
                    }
                }
                Backend::Traversal(src) => {
                    let due = traversed_at.is_none_or(|t| self.opts.traversal_rescan.is_some_and(|every| t.elapsed() >= every));
                    if due {
                        src.cancel = self.stop.clone();
                        self.set(|s| s.phase = Phase::Indexing);
                        match ix.sync_traversal(src, &mut store, &self.table, &self.stop) {
                            Ok(rep) => {
                                traversed_at = Some(Instant::now());
                                backoff = self.opts.backoff_min;
                                self.set(|s| {
                                    s.phase = Phase::Traversed { complete: rep.complete() };
                                    s.state = ix.state.clone();
                                    s.stats = ix.stats.clone();
                                });
                            }
                            Err(SyncError::Canceled) => break,
                            Err(e) => {
                                backend = None;
                                self.set(|s| s.phase = Phase::Waiting(format!("{e:?}")));
                            }
                        }
                    }
                    self.opts.traversal_rescan.unwrap_or(Duration::from_secs(3600)).min(Duration::from_secs(60))
                }
                Backend::Watched(src, feed) => {
                    if traversed_at.is_none() {
                        src.cancel = self.stop.clone();
                        self.set(|s| s.phase = Phase::Indexing);
                        match ix.sync_traversal(src, &mut store, &self.table, &self.stop) {
                            Ok(rep) => {
                                traversed_at = Some(Instant::now());
                                self.set(|s| {
                                    s.phase = Phase::Traversed { complete: rep.complete() };
                                    s.state = ix.state.clone();
                                    s.stats = ix.stats.clone();
                                });
                            }
                            Err(SyncError::Canceled) => break,
                            Err(e) => {
                                backend = None;
                                self.set(|s| s.phase = Phase::Waiting(format!("{e:?}")));
                            }
                        }
                        Duration::ZERO
                    } else {
                        // The feed's own wait paces this loop.
                        match feed.poll(self.opts.poll) {
                            FeedEvent::Idle => {}
                            FeedEvent::Dirs(dirs) => match ix.reconcile(src, &dirs, &mut store, &self.table) {
                                Ok(_) => self.set(|s| {
                                    s.phase = Phase::Traversed { complete: ix.state.is_complete() };
                                    s.state = ix.state.clone();
                                    s.stats = ix.stats.clone();
                                }),
                                Err(e) => {
                                    backend = None;
                                    self.set(|s| s.phase = Phase::Waiting(format!("{e:?}")));
                                }
                            },
                            FeedEvent::Overflow => traversed_at = None,
                            FeedEvent::Failed(why) => {
                                backend = None;
                                self.set(|s| s.phase = Phase::Waiting(format!("change notifications failed: {why}")));
                            }
                        }
                        Duration::ZERO
                    }
                }
            };
            last_seq = store.commit_seq_for(&self.spec.key).unwrap_or(last_seq);
            if let Some(dir) = &self.snapshot_dir {
                if last_seq != snapshot_seq && last_snapshot.elapsed() >= self.opts.snapshot_every && matches!(ix.write_snapshot(&store, &self.table, dir), Ok(true)) {
                    snapshot_seq = last_seq;
                    last_snapshot = Instant::now();
                    self.set(|s| s.snapshots_written += 1);
                }
            }
            if !next_wait.is_zero() && !self.wait(next_wait) {
                break;
            }
        }
        if let Some(dir) = &self.snapshot_dir {
            let seq = store.commit_seq_for(&self.spec.key).unwrap_or(0);
            if seq != snapshot_seq && matches!(ix.write_snapshot(&store, &self.table, dir), Ok(true)) {
                self.set(|s| s.snapshots_written += 1);
            }
        }
        self.set(|s| s.phase = Phase::Stopped);
    }
}
