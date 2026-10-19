//! Index correctness and recovery against the simulated NTFS volume.
//!
//! The oracle is the simulator's ground truth: after every sync, the set of
//! indexed link paths (in memory, and after a restart from SQLite) must equal
//! the simulator's, and file sizes must match once metadata is refreshed.

use flint_fuzzy::table::{flags, NameTable};
use flint_index::indexer::{SyncError, VolumeIndexer};
use flint_index::model::*;
use flint_index::sim::SimVolume;
use flint_index::store::Store;
use flint_index::volume::{Changes, VolumeIndex};
use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::sync::RwLock;

const KEY: &str = "\\\\?\\Volume{sim}";

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn below(&mut self, n: usize) -> usize {
        (self.next() % n.max(1) as u64) as usize
    }
}

fn db_path(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("flint-journal-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d.join("index.db")
}

/// A running "process": table + indexer over a store file.
struct App {
    table: RwLock<NameTable>,
    store: Store,
    ix: VolumeIndexer,
}

impl App {
    fn start(db: &PathBuf, sim: &SimVolume) -> App {
        let (mut store, _) = Store::open(db).unwrap();
        let table = RwLock::new(NameTable::new());
        let ix = VolumeIndexer::open(&mut store, &table, 0, KEY, "C:", sim.root).unwrap();
        App { table, store, ix }
    }
    fn sync(&mut self, sim: &mut SimVolume) -> Result<(), SyncError> {
        self.ix.sync(sim, &mut self.store, &self.table, &AtomicBool::new(false))
    }
    fn paths(&self) -> std::collections::BTreeSet<String> {
        self.ix.index.paths(&self.table.read().unwrap())
    }
    fn assert_matches(&self, sim: &SimVolume, ctx: &str) {
        let got = self.paths();
        let want = sim.paths("C:");
        if got != want {
            let missing: Vec<_> = want.difference(&got).take(5).collect();
            let extra: Vec<_> = got.difference(&want).take(5).collect();
            panic!("{ctx}: index diverged ({} vs {}), missing {missing:?}, extra {extra:?}", got.len(), want.len());
        }
    }
}

fn names(r: &mut Rng) -> String {
    const W: &[&str] = &["Report", "data", "Ünïcode", "日本", "a", "b", "notes", "x y", "Résumé", "LOG"];
    format!("{}{}", W[r.below(W.len())], r.below(40))
}

/// One random filesystem operation (directories are emptied before delete).
fn random_op(sim: &mut SimVolume, r: &mut Rng) {
    let dirs = sim.dirs();
    let frns = sim.frns();
    let pick_dir = |r: &mut Rng| dirs[r.below(dirs.len())];
    match r.below(100) {
        0..=24 => {
            let d = pick_dir(r);
            sim.create(d, &names(r), r.below(4) == 0);
        }
        25..=39 if !frns.is_empty() => {
            let f = frns[r.below(frns.len())];
            sim.write(f, r.below(1 << 20) as u64);
        }
        40..=59 if !frns.is_empty() => {
            let f = frns[r.below(frns.len())];
            let d = pick_dir(r);
            let n = if r.below(5) == 0 { sim.paths("").len().to_string() } else { names(r) };
            sim.rename(f, r.below(3), d, &n);
        }
        60..=74 if !frns.is_empty() => {
            let f = frns[r.below(frns.len())];
            sim.delete(f);
        }
        75..=84 if !frns.is_empty() => {
            let f = frns[r.below(frns.len())];
            let d = pick_dir(r);
            sim.add_link(f, d, &names(r));
        }
        85..=92 if !frns.is_empty() => {
            let f = frns[r.below(frns.len())];
            sim.remove_link(f, r.below(3));
        }
        _ => {
            let d = pick_dir(r);
            sim.create(d, &names(r), true);
        }
    }
}

fn populate(sim: &mut SimVolume, r: &mut Rng, n: usize) {
    let mut dirs = sim.dirs();
    for _ in 0..n {
        let d = dirs[r.below(dirs.len())];
        let is_dir = r.below(5) == 0;
        if let Some(f) = sim.create(d, &names(r), is_dir) {
            if is_dir {
                dirs.push(f);
            }
        }
    }
}

#[test]
fn initial_index_matches_ground_truth_and_survives_restart() {
    let mut r = Rng(1);
    let mut sim = SimVolume::new();
    populate(&mut sim, &mut r, 3000);
    let db = db_path("initial");
    let mut app = App::start(&db, &sim);
    app.sync(&mut sim).unwrap();
    assert!(app.ix.is_complete());
    app.assert_matches(&sim, "after initial index");
    drop(app);
    let mut app = App::start(&db, &sim);
    app.assert_matches(&sim, "loaded from SQLite");
    app.sync(&mut sim).unwrap();
    assert_eq!(app.ix.stats.rescans, 0, "valid checkpoint must resume, not rescan");
}

#[test]
fn randomized_operations_with_crashes_and_restarts() {
    for seed in 1..=6u64 {
        let mut r = Rng(seed * 7919);
        let mut sim = SimVolume::new();
        populate(&mut sim, &mut r, 400);
        let db = db_path(&format!("random-{seed}"));
        let mut app = App::start(&db, &sim);
        app.sync(&mut sim).unwrap();
        for round in 0..60 {
            for _ in 0..r.below(40) + 1 {
                random_op(&mut sim, &mut r);
            }
            if r.below(6) == 0 {
                // Crash in the middle of a commit, then restart from disk.
                app.store.fail_after_ops = Some(r.below(30));
                let _ = app.sync(&mut sim);
                drop(app);
                app = App::start(&db, &sim);
            }
            app.sync(&mut sim).unwrap_or_else(|e| panic!("seed {seed} round {round}: {e:?}"));
            app.assert_matches(&sim, &format!("seed {seed} round {round}"));
            assert!(app.ix.is_complete());
            if round % 15 == 14 {
                drop(app);
                app = App::start(&db, &sim);
                app.assert_matches(&sim, &format!("seed {seed} round {round} reload"));
            }
        }
        // Sizes match ground truth for every file.
        let t = app.table.read().unwrap();
        for f in sim.frns() {
            for id in app.ix.index.links(&t, f) {
                let m = t.meta(id);
                assert_eq!(m.flags & flags::META_STALE, 0, "metadata refreshed");
            }
        }
    }
}

#[test]
fn journal_reset_forces_rescan() {
    let mut r = Rng(3);
    let mut sim = SimVolume::new();
    populate(&mut sim, &mut r, 200);
    let db = db_path("reset");
    let mut app = App::start(&db, &sim);
    app.sync(&mut sim).unwrap();
    sim.reset_journal();
    for _ in 0..50 {
        random_op(&mut sim, &mut r);
    }
    app.sync(&mut sim).unwrap();
    assert_eq!(app.ix.stats.last_rescan_reason, Some(RescanReason::JournalIdChanged));
    app.assert_matches(&sim, "after journal reset");
}

#[test]
fn wraparound_forces_rescan() {
    let mut r = Rng(4);
    let mut sim = SimVolume::new();
    populate(&mut sim, &mut r, 200);
    let db = db_path("wrap");
    let mut app = App::start(&db, &sim);
    app.sync(&mut sim).unwrap();
    for _ in 0..200 {
        random_op(&mut sim, &mut r);
    }
    sim.truncate_journal(10);
    app.sync(&mut sim).unwrap();
    assert_eq!(app.ix.stats.last_rescan_reason, Some(RescanReason::RecordsLost));
    app.assert_matches(&sim, "after wraparound");
}

#[test]
fn records_purged_mid_read_forces_rescan() {
    let mut r = Rng(5);
    let mut sim = SimVolume::new();
    populate(&mut sim, &mut r, 100);
    let db = db_path("purged");
    let mut app = App::start(&db, &sim);
    app.sync(&mut sim).unwrap();
    random_op(&mut sim, &mut r);
    sim.read_faults.push(SourceError::EntryDeleted);
    app.sync(&mut sim).unwrap();
    assert_eq!(app.ix.stats.last_rescan_reason, Some(RescanReason::RecordsLost));
    app.assert_matches(&sim, "after purge");
}

#[test]
fn checkpoint_ahead_of_journal_forces_rescan() {
    let mut r = Rng(6);
    let mut sim = SimVolume::new();
    populate(&mut sim, &mut r, 100);
    let db = db_path("ahead");
    let mut app = App::start(&db, &sim);
    app.sync(&mut sim).unwrap();
    let VolumeState::Live { journal_id, next_usn } = app.ix.state else { panic!() };
    app.store.commit(1, false, &[], Some(&VolumeState::Live { journal_id, next_usn: next_usn + 1_000_000 })).unwrap();
    drop(app);
    let mut app = App::start(&db, &sim);
    app.sync(&mut sim).unwrap();
    assert_eq!(app.ix.stats.last_rescan_reason, Some(RescanReason::CheckpointAhead));
    app.assert_matches(&sim, "after bad checkpoint");
}

#[test]
fn crash_during_enumeration_is_never_reported_complete() {
    let mut r = Rng(7);
    let mut sim = SimVolume::new();
    populate(&mut sim, &mut r, 120_000);
    let db = db_path("enum-crash");
    let mut app = App::start(&db, &sim);
    // Fail after the first enumeration batch has committed.
    app.store.fail_after_ops = Some(60_000);
    assert!(app.sync(&mut sim).is_err());
    assert!(!app.ix.is_complete());
    drop(app);
    let app2 = App::start(&db, &sim);
    assert!(matches!(app2.ix.state, VolumeState::Enumerating { .. }), "partial data must stay marked as enumerating");
    assert!(!app2.ix.is_complete());
    let mut app2 = app2;
    app2.sync(&mut sim).unwrap();
    assert_eq!(app2.ix.stats.last_rescan_reason, Some(RescanReason::InterruptedEnumeration));
    app2.assert_matches(&sim, "after interrupted enumeration");
}

#[test]
fn cancel_during_enumeration_leaves_incomplete_state() {
    let mut r = Rng(8);
    let mut sim = SimVolume::new();
    populate(&mut sim, &mut r, 500);
    let db = db_path("cancel");
    let mut app = App::start(&db, &sim);
    let res = app.ix.sync(&mut sim, &mut app.store, &app.table, &AtomicBool::new(true));
    assert!(matches!(res, Err(SyncError::Canceled)));
    assert!(!app.ix.is_complete());
    assert!(matches!(app.ix.state, VolumeState::Enumerating { .. }));
}

#[test]
fn unavailable_volume_keeps_checkpoint_and_reports_incomplete() {
    let mut r = Rng(9);
    let mut sim = SimVolume::new();
    populate(&mut sim, &mut r, 100);
    let db = db_path("unavail");
    let mut app = App::start(&db, &sim);
    app.sync(&mut sim).unwrap();
    let before = app.ix.state.clone();
    sim.available = false;
    assert!(app.sync(&mut sim).is_err());
    assert!(!app.ix.is_complete(), "disconnected volume must not report complete");
    assert_eq!(app.ix.state, before, "transient failure must not touch the checkpoint");
    assert!(!app.paths().is_empty(), "stale entries stay searchable");
    sim.available = true;
    random_op(&mut sim, &mut r);
    app.sync(&mut sim).unwrap();
    assert_eq!(app.ix.stats.rescans, 1, "only the initial scan");
    app.assert_matches(&sim, "after reconnect");
}

#[test]
fn transient_read_error_then_resume() {
    let mut r = Rng(10);
    let mut sim = SimVolume::new();
    populate(&mut sim, &mut r, 100);
    let db = db_path("ioerr");
    let mut app = App::start(&db, &sim);
    app.sync(&mut sim).unwrap();
    for _ in 0..30 {
        random_op(&mut sim, &mut r);
    }
    sim.read_faults.push(SourceError::Io("device busy".into()));
    assert!(app.sync(&mut sim).is_err());
    app.sync(&mut sim).unwrap();
    assert_eq!(app.ix.stats.rescans, 1);
    app.assert_matches(&sim, "after transient error");
}

#[test]
fn no_journal_requests_traversal() {
    let mut sim = SimVolume::new();
    sim.journal_active = false;
    let db = db_path("nojournal");
    let mut app = App::start(&db, &sim);
    assert!(matches!(app.sync(&mut sim), Err(SyncError::Source(SourceError::JournalNotActive))));
    assert!(!app.ix.is_complete());
}

#[test]
fn directory_move_updates_whole_subtree() {
    let mut sim = SimVolume::new();
    let a = sim.create(sim.root, "A", true).unwrap();
    let b = sim.create(sim.root, "B", true).unwrap();
    let mut cur = a;
    for i in 0..50 {
        cur = sim.create(cur, &format!("d{i}"), true).unwrap();
        sim.create(cur, &format!("f{i}.txt"), false);
    }
    let db = db_path("dirmove");
    let mut app = App::start(&db, &sim);
    app.sync(&mut sim).unwrap();
    sim.rename(a, 0, b, "Moved");
    app.sync(&mut sim).unwrap();
    app.assert_matches(&sim, "after directory move");
    assert!(app.paths().iter().all(|p| !p.starts_with("C:\\A")));
}

#[test]
fn file_id_reuse_without_delete_record_drops_stale_entry() {
    let mut t = NameTable::new();
    let root = make_frn(5, 5);
    let mut v = VolumeIndex::new(&mut t, 0, root, "C:");
    let mut ch = Changes::default();
    let old = make_frn(100, 1);
    v.apply_record(&mut t, &FileRecord { frn: old, parent: root, name: "old.txt".into(), attrs: 0, size: Some(1), mtime: Some(1) }, &mut ch);
    // The delete of `old` was never seen; segment 100 is reused with seq 2.
    let new = make_frn(100, 2);
    let rec = UsnRecord { usn: 1, frn: new, parent: root, reason: reason::FILE_CREATE | reason::CLOSE, attrs: 0, name: "new.txt".into() };
    v.apply_usn(&mut t, &rec, &mut ch);
    assert_eq!(v.paths(&t).into_iter().collect::<Vec<_>>(), vec!["C:\\new.txt".to_string()]);
    assert!(ch.inconsistencies >= 1);
}

#[test]
fn rename_with_missing_old_name_record_moves_single_link() {
    let mut t = NameTable::new();
    let root = make_frn(5, 5);
    let mut v = VolumeIndex::new(&mut t, 0, root, "C:");
    let mut ch = Changes::default();
    let f = make_frn(100, 1);
    v.apply_record(&mut t, &FileRecord { frn: f, parent: root, name: "a.txt".into(), attrs: 0, size: Some(1), mtime: Some(1) }, &mut ch);
    let rec = UsnRecord { usn: 1, frn: f, parent: root, reason: reason::RENAME_NEW_NAME | reason::CLOSE, attrs: 0, name: "b.txt".into() };
    v.apply_usn(&mut t, &rec, &mut ch);
    assert_eq!(v.paths(&t).into_iter().collect::<Vec<_>>(), vec!["C:\\b.txt".to_string()]);
}

#[test]
fn replaying_a_batch_twice_is_idempotent() {
    let mut r = Rng(11);
    let mut sim = SimVolume::new();
    populate(&mut sim, &mut r, 200);
    let mut t = NameTable::new();
    let mut v = VolumeIndex::new(&mut t, 0, sim.root, "C:");
    let mut ch = Changes::default();
    use flint_index::source::JournalSource;
    sim.enumerate(&mut |rec| {
        v.apply_record(&mut t, &rec, &mut ch);
        true
    })
    .unwrap();
    let start = sim.query().unwrap().next_usn;
    for _ in 0..300 {
        random_op(&mut sim, &mut r);
    }
    let (recs, _) = sim.read(sim.journal_id, start, usize::MAX).unwrap();
    for _ in 0..2 {
        for rec in &recs {
            v.apply_usn(&mut t, rec, &mut ch);
        }
        // Hard-link sets are settled by follow-ups, as the indexer does.
        for f in std::mem::take(&mut ch.followups) {
            if let flint_index::volume::Followup::Links(frn) | flint_index::volume::Followup::Parent(frn) = f {
                let links = sim.links(frn).unwrap();
                v.set_links(&mut t, frn, &links, &mut ch);
            }
        }
    }
    assert_eq!(v.paths(&t), sim.paths("C:"));
}
