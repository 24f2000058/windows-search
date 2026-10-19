//! Per-volume orchestration: checkpoint validation, (re)enumeration, journal
//! catch-up, follow-up resolution and atomic persistence.
//!
//! Invariants:
//! * The persisted checkpoint never runs ahead of persisted entries: each
//!   batch's ops and its checkpoint commit in one transaction.
//! * `Enumerating` is persisted *before* enumeration starts, so a crash or
//!   cancel at any point forces a rescan rather than trusting partial data.
//! * Transient problems (volume unplugged, journal being deleted) never
//!   overwrite the stored checkpoint; they are reported as runtime status.

use crate::model::*;
use crate::snapshot;
use crate::source::JournalSource;
use crate::traverse::{Fs, TraversalReport, TraversalSource};
use crate::store::{Store, StoreError};
use crate::volume::{Changes, Followup, VolumeIndex};
use flint_fuzzy::table::NameTable;
use std::collections::HashSet;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::RwLock;

pub const ENUM_BATCH: usize = 50_000;
pub const READ_BATCH: usize = 4096;
const MAX_RESCANS_PER_SYNC: u32 = 3;
const MAX_FOLLOWUP_ROUNDS: usize = 100_000;

#[derive(Debug)]
pub enum SyncError {
    Store(StoreError),
    Source(SourceError),
    Canceled,
    /// Rescans kept being invalidated (e.g. journal wrapping faster than we
    /// can enumerate).
    RescanLoop,
}

impl From<StoreError> for SyncError {
    fn from(e: StoreError) -> Self {
        SyncError::Store(e)
    }
}
impl From<SourceError> for SyncError {
    fn from(e: SourceError) -> Self {
        SyncError::Source(e)
    }
}

#[derive(Clone, Debug, Default)]
pub struct Stats {
    pub rescans: u64,
    pub last_rescan_reason: Option<RescanReason>,
    pub records_applied: u64,
    pub batches_committed: u64,
    pub inconsistencies: u64,
    pub followup_failures: u64,
    pub last_error: Option<String>,
}

pub struct VolumeIndexer {
    pub key: String,
    pub root_name: String,
    db_id: i64,
    pub index: VolumeIndex,
    /// Persisted state (mirrors the store).
    pub state: VolumeState,
    /// Transient condition preventing the index from being current.
    pub runtime: Option<String>,
    pub stats: Stats,
    /// Enumeration progress (links seen) while enumerating.
    pub progress: u64,
    /// In-memory state is known to equal the committed store state (false
    /// after a failed sync until the next successful one). Snapshots are only
    /// written while true.
    consistent: bool,
}

/// Where a volume's entries came from at startup.
#[derive(Debug, PartialEq, Eq)]
pub enum LoadSource {
    Snapshot,
    /// SQLite; `why` explains why no snapshot was used (None = none existed
    /// or snapshots disabled).
    Store { why: Option<String> },
}

impl VolumeIndexer {
    /// Loads a volume's persisted entries into `table`.
    pub fn open(store: &mut Store, table: &RwLock<NameTable>, volume: u16, key: &str, root_name: &str, root_frn: Frn) -> Result<Self, StoreError> {
        Self::open_with_snapshot(store, table, volume, key, root_name, root_frn, None).map(|(ix, _)| ix)
    }

    /// Like [`open`](Self::open), but loads from a matching snapshot in
    /// `snapshot_dir` when one exists (falling back to SQLite otherwise).
    pub fn open_with_snapshot(store: &mut Store, table: &RwLock<NameTable>, volume: u16, key: &str, root_name: &str, root_frn: Frn, snapshot_dir: Option<&Path>) -> Result<(Self, LoadSource), StoreError> {
        let (db_id, state) = store.volume(key, root_name)?;
        let mut why = None;
        if let Some(dir) = snapshot_dir {
            let path = snapshot::path_for(dir, key);
            if path.exists() {
                match snapshot::read(&path, key, store.commit_seq(db_id)?) {
                    Ok(snap) if snap.root_frn == root_frn => {
                        let mut t = table.write().unwrap();
                        let n = snap.chunk.slots.len();
                        match t.import_chunk(volume, &snap.chunk) {
                            Ok(base) => match VolumeIndex::adopt(&t, volume, root_frn, base, n) {
                                Ok(index) => {
                                    drop(t);
                                    let ix = VolumeIndexer { key: key.to_owned(), root_name: root_name.to_owned(), db_id, index, state, runtime: None, stats: Stats::default(), progress: 0, consistent: true };
                                    return Ok((ix, LoadSource::Snapshot));
                                }
                                Err(e) => {
                                    // Undo the import before falling back.
                                    for id in base..base + n as u32 {
                                        t.remove(id);
                                    }
                                    why = Some(format!("snapshot rejected: {e}"));
                                }
                            },
                            Err(e) => why = Some(format!("snapshot rejected: {e:?}")),
                        }
                    }
                    Ok(_) => why = Some("snapshot root mismatch".into()),
                    Err(e) => why = Some(format!("{e:?}")),
                }
            }
        }
        let mut t = table.write().unwrap();
        let mut index = VolumeIndex::new(&mut t, volume, root_frn, root_name);
        let mut ch = Changes::default();
        store.load(db_id, |r| index.apply_record(&mut t, &r, &mut ch))?;
        drop(t);
        let mut stats = Stats::default();
        // A consistent store yields no orphans; any are repaired on sync.
        stats.inconsistencies += index.orphan_count() as u64;
        let ix = VolumeIndexer { key: key.to_owned(), root_name: root_name.to_owned(), db_id, index, state, runtime: None, stats, progress: 0, consistent: true };
        Ok((ix, LoadSource::Store { why }))
    }

    /// Writes a startup snapshot if the in-memory state provably mirrors the
    /// committed store. Returns whether one was written.
    pub fn write_snapshot(&self, store: &Store, table: &RwLock<NameTable>, dir: &Path) -> Result<bool, SyncError> {
        if !self.consistent || self.index.orphan_count() > 0 {
            return Ok(false);
        }
        let seq = store.commit_seq(self.db_id)?;
        let chunk = table.read().unwrap().export_volume(self.index.volume);
        let snap = snapshot::Snapshot { commit_seq: seq, root_frn: self.index.root_frn, key: self.key.clone(), chunk };
        snapshot::write(&snapshot::path_for(dir, &self.key), &snap).map_err(|e| SyncError::Source(SourceError::Io(e.to_string())))?;
        Ok(true)
    }

    /// True only if the volume is journal-current with no transient problem.
    pub fn is_complete(&self) -> bool {
        self.runtime.is_none() && self.state.is_complete()
    }

    fn persist_state(&mut self, store: &mut Store, clear: bool, state: VolumeState) -> Result<(), StoreError> {
        store.commit(self.db_id, clear, &[], Some(&state))?;
        self.state = state;
        Ok(())
    }

    /// Brings the volume up to date. Safe to call repeatedly.
    pub fn sync(&mut self, src: &mut dyn JournalSource, store: &mut Store, table: &RwLock<NameTable>, cancel: &AtomicBool) -> Result<(), SyncError> {
        let r = self.sync_inner(src, store, table, cancel);
        self.consistent = r.is_ok();
        match &r {
            Ok(()) => {
                self.runtime = None;
                self.stats.last_error = None;
            }
            Err(e) => {
                self.stats.last_error = Some(format!("{e:?}"));
                if !matches!(e, SyncError::Canceled) {
                    self.runtime = Some(format!("{e:?}"));
                } else {
                    self.runtime = Some("indexing canceled".into());
                }
            }
        }
        r
    }

    fn sync_inner(&mut self, src: &mut dyn JournalSource, store: &mut Store, table: &RwLock<NameTable>, cancel: &AtomicBool) -> Result<(), SyncError> {
        let mut rescans = 0;
        loop {
            match validate(&self.state, &src.query()) {
                Decision::Resume { journal_id, from_usn } => match self.catch_up(src, store, table, cancel, journal_id, from_usn) {
                    Ok(()) => return Ok(()),
                    Err(SyncError::Source(SourceError::EntryDeleted)) => self.persist_state(store, false, VolumeState::NeedsRescan(RescanReason::RecordsLost))?,
                    Err(SyncError::Source(SourceError::JournalIdMismatch)) => self.persist_state(store, false, VolumeState::NeedsRescan(RescanReason::JournalIdChanged))?,
                    Err(e) => return Err(e),
                },
                Decision::Rescan(reason) => {
                    rescans += 1;
                    if rescans > MAX_RESCANS_PER_SYNC {
                        return Err(SyncError::RescanLoop);
                    }
                    self.rescan(src, store, table, cancel, reason)?;
                }
                Decision::Traverse => return Err(SyncError::Source(SourceError::JournalNotActive)),
                Decision::Wait(msg) => return Err(SyncError::Source(SourceError::Unavailable(msg))),
            }
        }
    }

    fn rescan(&mut self, src: &mut dyn JournalSource, store: &mut Store, table: &RwLock<NameTable>, cancel: &AtomicBool, reason: RescanReason) -> Result<(), SyncError> {
        self.stats.rescans += 1;
        self.stats.last_rescan_reason = Some(reason);
        let q = src.query()?;
        self.persist_state(store, true, VolumeState::Enumerating { journal_id: q.journal_id, start_usn: q.next_usn })?;
        self.enumerate_into(src, store, table, cancel)?;
        // Replay changes made while enumerating; the first committed batch
        // flips the state to Live.
        self.catch_up(src, store, table, cancel, q.journal_id, q.next_usn)
    }

    /// Clears the volume and streams a full enumeration into table + store
    /// in batches, then resolves parents still missing.
    fn enumerate_into(&mut self, src: &mut dyn JournalSource, store: &mut Store, table: &RwLock<NameTable>, cancel: &AtomicBool) -> Result<(), SyncError> {
        self.index.clear(&mut table.write().unwrap());
        self.progress = 0;

        let mut batch: Vec<FileRecord> = Vec::with_capacity(ENUM_BATCH);
        let mut failure: Option<SyncError> = None;
        let mut canceled = false;
        let db_id = self.db_id;
        let (index, stats, progress) = (&mut self.index, &mut self.stats, &mut self.progress);
        let mut flush = |batch: &mut Vec<FileRecord>, store: &mut Store| -> Result<(), SyncError> {
            let mut ch = Changes::default();
            {
                let mut t = table.write().unwrap();
                for r in batch.iter() {
                    index.apply_record(&mut t, r, &mut ch);
                }
            }
            // Parent/meta follow-ups are resolved after enumeration, when
            // all parents have been seen.
            store.commit(db_id, false, &ch.ops, None)?;
            stats.inconsistencies += ch.inconsistencies;
            stats.batches_committed += 1;
            *progress += batch.len() as u64;
            batch.clear();
            Ok(())
        };
        let r = src.enumerate(&mut |rec| {
            if cancel.load(Ordering::Relaxed) {
                canceled = true;
                return false;
            }
            batch.push(rec);
            if batch.len() >= ENUM_BATCH {
                if let Err(e) = flush(&mut batch, store) {
                    failure = Some(e);
                    return false;
                }
            }
            true
        });
        if let Some(e) = failure {
            return Err(e);
        }
        r?;
        if canceled {
            return Err(SyncError::Canceled);
        }
        flush(&mut batch, store)?;
        // Orphans left after a full enumeration point at missing parents.
        let mut ch = Changes::default();
        ch.followups.extend(self.index.orphan_parents().into_iter().map(Followup::Parent));
        self.resolve_followups(src, table, &mut ch);
        store.commit(self.db_id, false, &ch.ops, None)?;
        Ok(())
    }

    /// Full re-traversal for a journal-less volume. Coverage is persisted as
    /// complete only if nothing was skipped and the walk was not canceled.
    pub fn sync_traversal<F: Fs>(&mut self, src: &mut TraversalSource<F>, store: &mut Store, table: &RwLock<NameTable>, cancel: &AtomicBool) -> Result<TraversalReport, SyncError> {
        self.stats.rescans += 1;
        self.persist_state(store, true, VolumeState::Traversal { complete: false, skipped: 0 })?;
        let r = self.enumerate_into(src, store, table, cancel);
        let report = src.report.clone();
        self.consistent = r.is_ok();
        if let Err(e) = r {
            self.runtime = Some(format!("{e:?}"));
            return Err(e);
        }
        let skipped = report.skipped.len() as u64 + report.entry_errors + report.depth_limited;
        self.persist_state(store, false, VolumeState::Traversal { complete: report.complete(), skipped })?;
        self.runtime = (!report.complete()).then(|| format!("traversal incomplete: {skipped} unreadable, canceled={}", report.canceled));
        Ok(report)
    }

    fn catch_up(&mut self, src: &mut dyn JournalSource, store: &mut Store, table: &RwLock<NameTable>, cancel: &AtomicBool, journal_id: u64, mut from: i64) -> Result<(), SyncError> {
        loop {
            if cancel.load(Ordering::Relaxed) {
                return Err(SyncError::Canceled);
            }
            let (recs, next) = src.read(journal_id, from, READ_BATCH)?;
            let mut ch = Changes::default();
            {
                let mut t = table.write().unwrap();
                for r in &recs {
                    self.index.apply_usn(&mut t, r, &mut ch);
                }
            }
            self.resolve_followups(src, table, &mut ch);
            let live = VolumeState::Live { journal_id, next_usn: next };
            store.commit(self.db_id, false, &ch.ops, Some(&live))?;
            self.state = live;
            self.stats.records_applied += recs.len() as u64;
            self.stats.batches_committed += 1;
            self.stats.inconsistencies += ch.inconsistencies;
            if recs.is_empty() || next <= from {
                return Ok(());
            }
            from = next;
        }
    }

    /// Re-reads size/mtime for up to `limit` slots whose metadata is stale
    /// (e.g. after an enumeration source that provides names only). Meant for
    /// a low-priority background loop; returns how many remain stale.
    pub fn refresh_stale_meta(&mut self, src: &mut dyn JournalSource, store: &mut Store, table: &RwLock<NameTable>, limit: usize) -> Result<usize, SyncError> {
        let (todo, remaining) = {
            let t = table.read().unwrap();
            let stale = self.index.stale_meta(&t);
            let n = stale.len();
            (stale.into_iter().take(limit).collect::<Vec<_>>(), n.saturating_sub(limit))
        };
        let mut ch = Changes::default();
        ch.followups.extend(todo.into_iter().map(Followup::Meta));
        self.resolve_followups(src, table, &mut ch);
        store.commit(self.db_id, false, &ch.ops, None)?;
        Ok(remaining)
    }

    /// Answers follow-ups against the live source, appending resulting ops.
    fn resolve_followups(&mut self, src: &mut dyn JournalSource, table: &RwLock<NameTable>, ch: &mut Changes) {
        let mut seen: HashSet<Followup> = HashSet::new();
        let mut queue: Vec<Followup> = std::mem::take(&mut ch.followups);
        queue.sort_unstable();
        queue.dedup();
        let mut rounds = 0;
        while let Some(f) = queue.pop() {
            rounds += 1;
            if rounds > MAX_FOLLOWUP_ROUNDS {
                self.stats.followup_failures += queue.len() as u64;
                break;
            }
            if !seen.insert(f.clone()) {
                continue;
            }
            let mut sub = Changes::default();
            match f {
                Followup::Meta(frn) => match src.meta(frn) {
                    Ok(Some((a, size, mtime))) => self.index.set_meta(&mut table.write().unwrap(), frn, a, size, mtime, &mut sub),
                    // Gone: its deletion record will follow in the journal.
                    Ok(None) => {}
                    Err(_) => self.stats.followup_failures += 1,
                },
                Followup::Links(frn) | Followup::Parent(frn) => match src.links(frn) {
                    Ok(recs) => {
                        if recs.is_empty() && matches!(f, Followup::Parent(_)) {
                            // Parent really does not exist: children stay
                            // orphaned until reconciliation.
                            self.stats.inconsistencies += 1;
                        }
                        self.index.set_links(&mut table.write().unwrap(), frn, &recs, &mut sub)
                    }
                    Err(_) => self.stats.followup_failures += 1,
                },
            }
            ch.ops.append(&mut sub.ops);
            ch.inconsistencies += sub.inconsistencies;
            queue.extend(sub.followups.into_iter().filter(|x| !seen.contains(x)));
        }
    }
}
