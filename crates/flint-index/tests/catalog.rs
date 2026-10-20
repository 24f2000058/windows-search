//! Multi-volume catalog: concurrency, availability, removal, shutdown.

use flint_fuzzy::engine::{search, CancelToken, Options};
use flint_fuzzy::query::parse;
use flint_fuzzy::table::NameTable;
use flint_index::catalog::*;
use flint_index::model::SourceError;
use flint_index::sim::{SharedSim, SimVolume};
use flint_index::traverse::{Fs, RealFs, TraversalSource};
use std::collections::BTreeSet;
use std::path::PathBuf;
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

fn opts() -> CatalogOptions {
    CatalogOptions {
        poll: Duration::from_millis(20),
        backoff_min: Duration::from_millis(10),
        backoff_max: Duration::from_millis(80),
        snapshot_every: Duration::from_millis(0),
        meta_batch: 1000,
        traversal_rescan: None,
    }
}

fn tmpdir(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("flint-catalog-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn sim_with(names: &[&str]) -> SharedSim {
    let mut s = SimVolume::new();
    let root = s.root;
    let d = s.create(root, "docs", true).unwrap();
    for n in names {
        s.create(d, n, false).unwrap();
    }
    SharedSim::new(s)
}

fn journal_opener(sim: SharedSim) -> Opener {
    Box::new(move |_| Ok(Backend::Journal(Box::new(sim.clone()))))
}

fn wait_until(what: &str, mut f: impl FnMut() -> bool) {
    let t = Instant::now();
    while !f() {
        assert!(t.elapsed() < Duration::from_secs(20), "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn names(table: &RwLock<NameTable>) -> BTreeSet<String> {
    let t = table.read().unwrap();
    (0..t.len() as u32).filter(|&i| t.is_live(i)).map(|i| t.path(i)).collect()
}

#[test]
fn indexes_several_volumes_concurrently_while_searching() {
    let dir = tmpdir("multi");
    let table = Arc::new(RwLock::new(NameTable::new()));
    let cat = Catalog::new(table.clone(), dir.join("index.db"), Some(dir.join("snaps")), opts(), Arc::new(|| {})).unwrap();
    let c = sim_with(&["report.pdf", "notes.txt"]);
    let d = sim_with(&["photo.jpg"]);
    let tree = dir.join("share");
    std::fs::create_dir_all(tree.join("sub")).unwrap();
    std::fs::write(tree.join("sub/budget.xlsx"), b"x").unwrap();
    cat.add(VolumeSpec { key: "vol-c".into(), root_name: "C:".into(), root_frn: c.with(|s| s.root) }, journal_opener(c.clone()));
    cat.add(VolumeSpec { key: "vol-d".into(), root_name: "D:".into(), root_frn: d.with(|s| s.root) }, journal_opener(d.clone()));
    let t2 = tree.clone();
    let probe = TraversalSource::new(tree.clone(), RealFs);
    cat.add(
        VolumeSpec { key: "share".into(), root_name: "\\\\nas\\share".into(), root_frn: probe.root_frn },
        Box::new(move |_| Ok(Backend::Traversal(TraversalSource::new(t2.clone(), Box::new(RealFs) as Box<dyn Fs + Send>)))),
    );
    // Searches keep working (and never deadlock) while workers index.
    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let (tb, st) = (table.clone(), stop.clone());
    let searcher = std::thread::spawn(move || {
        let mut n = 0;
        while !st.load(std::sync::atomic::Ordering::Relaxed) {
            let t = tb.read().unwrap();
            search(&t, &parse("rep", 0), &Options::default(), &CancelToken::new(), |_| {});
            n += 1;
        }
        n
    });
    wait_until("complete coverage", || cat.coverage().complete);
    stop.store(true, std::sync::atomic::Ordering::Relaxed);
    assert!(searcher.join().unwrap() > 0);
    let all = names(&table);
    for p in ["C:\\docs\\report.pdf", "D:\\docs\\photo.jpg", "\\\\nas\\share\\sub\\budget.xlsx"] {
        assert!(all.contains(p), "missing {p} in {all:?}");
    }
    // Live updates flow through the running worker.
    c.with(|s| {
        let root = s.root;
        s.create(root, "fresh.txt", false).unwrap();
    });
    wait_until("live update", || names(&table).contains("C:\\fresh.txt"));
    cat.shutdown();
}

#[test]
fn unavailable_volume_is_reported_and_recovers() {
    let dir = tmpdir("unavail");
    let table = Arc::new(RwLock::new(NameTable::new()));
    let cat = Catalog::new(table.clone(), dir.join("index.db"), None, opts(), Arc::new(|| {})).unwrap();
    let e = sim_with(&["usb.txt"]);
    e.with(|s| s.available = false);
    cat.add(VolumeSpec { key: "vol-e".into(), root_name: "E:".into(), root_frn: e.with(|s| s.root) }, journal_opener(e.clone()));
    wait_until("waiting phase", || cat.statuses().iter().any(|s| matches!(s.phase, Phase::Waiting(_))));
    let cov = cat.coverage();
    assert!(!cov.complete);
    assert!(cov.problems.iter().any(|p| p.starts_with("E: unavailable")), "{cov:?}");
    e.with(|s| s.available = true);
    wait_until("recovery", || cat.coverage().complete);
    assert!(names(&table).contains("E:\\docs\\usb.txt"));
}

#[test]
fn opener_failure_backs_off_then_journal_failure_falls_back_to_traversal() {
    let dir = tmpdir("fallback");
    let table = Arc::new(RwLock::new(NameTable::new()));
    let cat = Catalog::new(table.clone(), dir.join("index.db"), None, opts(), Arc::new(|| {})).unwrap();
    let tree = dir.join("fat32");
    std::fs::create_dir_all(&tree).unwrap();
    std::fs::write(tree.join("camera.jpg"), b"x").unwrap();
    let mut s = SimVolume::new();
    s.journal_active = false;
    let sim = SharedSim::new(s);
    let mut calls = 0;
    let t2 = tree.clone();
    let opener: Opener = Box::new(move |journal_failed| {
        calls += 1;
        if calls == 1 {
            return Err(SourceError::Unavailable("not mounted yet".into()));
        }
        if journal_failed {
            Ok(Backend::Traversal(TraversalSource::new(t2.clone(), Box::new(RealFs) as Box<dyn Fs + Send>)))
        } else {
            Ok(Backend::Journal(Box::new(sim.clone())))
        }
    });
    let root_frn = TraversalSource::new(tree.clone(), RealFs).root_frn;
    cat.add(VolumeSpec { key: "fat".into(), root_name: "F:".into(), root_frn }, opener);
    wait_until("traversal complete", || cat.statuses().iter().any(|s| s.phase == Phase::Traversed { complete: true }));
    assert!(names(&table).contains("F:\\camera.jpg"));
}

#[test]
fn removal_hides_entries_and_readd_loads_from_snapshot() {
    let dir = tmpdir("remove");
    let table = Arc::new(RwLock::new(NameTable::new()));
    let cat = Catalog::new(table.clone(), dir.join("index.db"), Some(dir.join("snaps")), opts(), Arc::new(|| {})).unwrap();
    let u = sim_with(&["a.txt", "b.txt"]);
    let spec = VolumeSpec { key: "usb".into(), root_name: "U:".into(), root_frn: u.with(|s| s.root) };
    cat.add(spec.clone(), journal_opener(u.clone()));
    wait_until("live", || cat.coverage().complete);
    wait_until("snapshot", || cat.statuses()[0].snapshots_written > 0);
    assert!(cat.remove("usb"));
    assert!(names(&table).iter().all(|p| !p.starts_with("U:")), "unplugged volume must leave search");
    assert!(cat.coverage().problems.contains(&"no volumes configured".to_string()));
    cat.add(spec, journal_opener(u.clone()));
    wait_until("live again", || cat.coverage().complete);
    let st = &cat.statuses()[0];
    assert_eq!(st.load_source.as_deref(), Some("Snapshot"), "{st:?}");
    assert!(names(&table).contains("U:\\docs\\b.txt"));
}

#[test]
fn shutdown_is_prompt_even_mid_indexing() {
    let dir = tmpdir("shutdown");
    let table = Arc::new(RwLock::new(NameTable::new()));
    let cat = Catalog::new(table.clone(), dir.join("index.db"), Some(dir.join("snaps")), opts(), Arc::new(|| {})).unwrap();
    let mut s = SimVolume::new();
    let root = s.root;
    for i in 0..150_000 {
        s.create(root, &format!("f{i}"), false);
    }
    let big = SharedSim::new(s);
    cat.add(VolumeSpec { key: "big".into(), root_name: "B:".into(), root_frn: big.with(|s| s.root) }, journal_opener(big));
    std::thread::sleep(Duration::from_millis(100));
    let t = Instant::now();
    cat.shutdown();
    assert!(t.elapsed() < Duration::from_secs(5), "shutdown took {:?}", t.elapsed());
}

struct FakeFeed(std::sync::mpsc::Receiver<FeedEvent>);
impl ChangeFeed for FakeFeed {
    fn poll(&mut self, timeout: Duration) -> FeedEvent {
        self.0.recv_timeout(timeout).unwrap_or(FeedEvent::Idle)
    }
}

#[test]
fn watched_volume_reconciles_changes_and_recovers_from_overflow() {
    let dir = tmpdir("watched");
    let tree = dir.join("usb");
    std::fs::create_dir_all(tree.join("photos")).unwrap();
    std::fs::write(tree.join("photos/a.jpg"), b"x").unwrap();
    let table = Arc::new(RwLock::new(NameTable::new()));
    let cat = Catalog::new(table.clone(), dir.join("index.db"), None, opts(), Arc::new(|| {})).unwrap();
    let (tx, rx) = std::sync::mpsc::channel();
    let feed = Arc::new(std::sync::Mutex::new(Some(rx)));
    let t2 = tree.clone();
    let root_frn = TraversalSource::new(tree.clone(), RealFs).root_frn;
    cat.add(
        VolumeSpec { key: "exfat".into(), root_name: "X:".into(), root_frn },
        Box::new(move |_| {
            let rx = feed.lock().unwrap().take().ok_or(SourceError::Unavailable("feed used".into()))?;
            Ok(Backend::Watched(TraversalSource::new(t2.clone(), Box::new(RealFs) as Box<dyn Fs + Send>), Box::new(FakeFeed(rx))))
        }),
    );
    wait_until("initial traversal", || names(&table).contains("X:\\photos\\a.jpg"));
    std::fs::write(tree.join("photos/b.jpg"), b"y").unwrap();
    std::fs::remove_file(tree.join("photos/a.jpg")).unwrap();
    tx.send(FeedEvent::Dirs(vec![PathBuf::from("photos")])).unwrap();
    wait_until("incremental update", || {
        let n = names(&table);
        n.contains("X:\\photos\\b.jpg") && !n.contains("X:\\photos\\a.jpg")
    });
    std::fs::write(tree.join("late.txt"), b"z").unwrap();
    tx.send(FeedEvent::Overflow).unwrap();
    wait_until("full rescan after overflow", || names(&table).contains("X:\\late.txt"));
    assert!(cat.coverage().complete, "{:?}", cat.coverage());
}

#[test]
fn heavy_scans_queue_behind_the_governor_and_workers_run_its_thread_setup() {
    use flint_index::governor::{Governor, GovernorOptions};
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    let dir = tmpdir("governor");
    let table = Arc::new(RwLock::new(NameTable::new()));
    let setups = Arc::new(AtomicUsize::new(0));
    let s2 = setups.clone();
    let gov = Governor::new(GovernorOptions::default(), Some(Arc::new(move || {
        s2.fetch_add(1, Ordering::SeqCst);
    })));
    let cat = Catalog::new(table.clone(), dir.join("index.db"), None, opts(), Arc::new(|| {})).unwrap().with_governor(gov.clone());
    // Hold the only scan permit, as another volume's scan would.
    let never = AtomicBool::new(false);
    let permit = gov.acquire_scan(&never, &mut |_| {}).unwrap();
    let c = sim_with(&["report.pdf"]);
    let d = sim_with(&["photo.jpg"]);
    cat.add(VolumeSpec { key: "vol-c".into(), root_name: "C:".into(), root_frn: c.with(|s| s.root) }, journal_opener(c.clone()));
    cat.add(VolumeSpec { key: "vol-d".into(), root_name: "D:".into(), root_frn: d.with(|s| s.root) }, journal_opener(d.clone()));
    wait_until("both volumes queued", || cat.statuses().iter().all(|s| s.queued));
    let cov = cat.coverage();
    assert!(!cov.complete && cov.problems.iter().all(|p| p.contains("queued")), "{cov:?}");
    assert!(!names(&table).iter().any(|n| n.ends_with("report.pdf")), "nothing is scanned while queued");
    drop(permit);
    wait_until("complete coverage", || cat.coverage().complete);
    assert!(names(&table).iter().any(|n| n.ends_with("report.pdf")) && names(&table).iter().any(|n| n.ends_with("photo.jpg")));
    assert_eq!(setups.load(Ordering::SeqCst), 2, "each worker thread runs the setup hook");
    let st = gov.stats();
    assert_eq!((st.scans_running, st.scans_queued), (0, 0));
    cat.shutdown();
}
