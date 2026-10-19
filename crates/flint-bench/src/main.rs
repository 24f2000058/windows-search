//! Reproducible benchmarks.
//!
//! ```text
//! flint-bench engine  [--n 1000000] [--seed 1] [--queries 200] [--threads N]
//! flint-bench dump    [--n 1000000] [--seed 1] --out names.txt
//! flint-bench kernels --corpus names.txt   (single-threaded; compare with bench/cpp)
//! flint-bench index   [--n 1000000]        (SQLite persist/load timings)
//! ```

mod corpus;

use corpus::Rng;
use flint_fuzzy::engine::{search, CancelToken, Intensity, Options};
use flint_fuzzy::fold::fold_str;
use flint_fuzzy::kernels::{osa_substring, subsequence_positions, Pattern};
use flint_fuzzy::query::parse;
use flint_fuzzy::table::NameTable;
use std::time::Instant;

fn arg(args: &[String], key: &str, default: &str) -> String {
    args.iter().position(|a| a == key).and_then(|i| args.get(i + 1)).cloned().unwrap_or_else(|| default.to_owned())
}

fn rss_kb() -> u64 {
    std::fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|s| s.lines().find(|l| l.starts_with("VmRSS:")).and_then(|l| l.split_whitespace().nth(1)?.parse().ok()))
        .unwrap_or(0)
}

fn pct(v: &mut [f64], p: f64) -> f64 {
    v.sort_by(|a, b| a.partial_cmp(b).unwrap());
    if v.is_empty() {
        return 0.0;
    }
    v[((v.len() - 1) as f64 * p).round() as usize]
}

fn pick_name(t: &NameTable, r: &mut Rng) -> String {
    loop {
        let id = r.below(t.len()) as u32;
        if t.is_live(id) && t.name(id).chars().count() >= 6 {
            return t.name(id).to_owned();
        }
    }
}

fn typo(s: &str, r: &mut Rng, edits: usize) -> String {
    let mut c: Vec<char> = s.chars().collect();
    for _ in 0..edits {
        if c.len() < 3 {
            break;
        }
        let i = 1 + r.below(c.len() - 2);
        match r.below(4) {
            0 => c[i] = (b'a' + r.below(26) as u8) as char,
            1 => {
                c.remove(i);
            }
            2 => c.insert(i, (b'a' + r.below(26) as u8) as char),
            _ => c.swap(i, i - 1),
        }
    }
    c.into_iter().collect()
}

fn stem(s: &str) -> &str {
    s.rsplit_once('.').map_or(s, |(a, _)| if a.is_empty() { s } else { a })
}

fn make_queries(t: &NameTable, class: &str, n: usize, seed: u64) -> Vec<String> {
    let mut r = Rng(seed);
    (0..n)
        .map(|_| {
            let name = pick_name(t, &mut r);
            let st: Vec<char> = stem(&name).chars().collect();
            match class {
                "exact" => name,
                "prefix" => st[..st.len().min(3 + r.below(4))].iter().collect(),
                "substring" => {
                    let a = r.below(st.len().saturating_sub(3).max(1));
                    st[a..(a + 4).min(st.len())].iter().collect()
                }
                "typo1" => typo(stem(&name), &mut r, 1),
                "typo2" => typo(stem(&name), &mut r, 2),
                "scattered" => {
                    let mut idx: Vec<usize> = (0..st.len()).filter(|_| r.chance(35)).collect();
                    if idx.len() < 3 {
                        idx = vec![0, st.len() / 2, st.len() - 1];
                    }
                    idx.truncate(5);
                    idx.iter().map(|&i| st[i]).collect()
                }
                "multi" => {
                    let w: Vec<&str> = stem(&name).split(['_', '-', ' ', '.']).filter(|w| w.len() > 2).collect();
                    if w.len() >= 2 { format!("{} {}", w[1], w[0]) } else { stem(&name).to_owned() }
                }
                "path" => {
                    let id = loop {
                        let id = r.below(t.len()) as u32;
                        if t.parent(id) != u32::MAX {
                            break id;
                        }
                    };
                    let p = t.name(t.parent(id));
                    let n: String = t.name(id).chars().take(4).collect();
                    format!("{}\\{}", p.chars().take(4).collect::<String>(), n)
                }
                "nonsense" => (0..4 + r.below(5)).map(|_| (b'a' + r.below(26) as u8) as char).collect(),
                "onechar" => ((b'a' + r.below(26) as u8) as char).to_string(),
                _ => unreachable!(),
            }
        })
        .collect()
}

fn engine_bench(args: &[String]) {
    let n: usize = arg(args, "--n", "1000000").parse().unwrap();
    let seed: u64 = arg(args, "--seed", "1").parse().unwrap();
    let nq: usize = arg(args, "--queries", "200").parse().unwrap();
    if let Ok(th) = arg(args, "--threads", "0").parse::<usize>() {
        if th > 0 {
            rayon::ThreadPoolBuilder::new().num_threads(th).build_global().unwrap();
        }
    }
    let rss0 = rss_kb();
    let t0 = Instant::now();
    let t = corpus::build(n, seed);
    let build = t0.elapsed();
    let t1 = Instant::now();
    let _ = t.chain_masks();
    let chain = t1.elapsed();
    let rss1 = rss_kb();
    println!("corpus: {} entries, build {:.2}s, chain-mask {:.1} ms", t.len(), build.as_secs_f64(), chain.as_secs_f64() * 1e3);
    println!("memory: table heap ≈ {:.1} MB, RSS delta {:.1} MB, threads {}", t.heap_bytes() as f64 / 1e6, (rss1 - rss0) as f64 / 1024.0, rayon::current_num_threads());
    println!();
    println!("{:<10} {:>6} {:>9} {:>9} {:>9} {:>9} {:>11} {:>9}", "class", "n", "p50 ms", "p95 ms", "p99 ms", "max ms", "first p95", "hits avg");
    let configs: [(&str, Options); 2] = [("default", Options::default()), ("exhaustive", Options { exhaustive: true, through: Some(flint_fuzzy::engine::Stage::Exhaustive), ..Default::default() })];
    for (label, opts) in &configs {
        println!("-- options: {label}");
        let classes: &[&str] = if *label == "default" {
            &["exact", "prefix", "substring", "typo1", "typo2", "scattered", "multi", "path", "nonsense", "onechar"]
        } else {
            &["typo1", "nonsense"]
        };
        for class in classes {
            let qs = make_queries(&t, class, nq, seed ^ 0xabc);
            // Warm-up
            for q in qs.iter().take(5) {
                search(&t, &parse(q, 0), opts, &CancelToken::new(), |_| {});
            }
            let (mut total, mut first, mut hits) = (Vec::new(), Vec::new(), 0usize);
            for q in &qs {
                let pq = parse(q, 0);
                let s = Instant::now();
                let mut first_t = None;
                let res = search(&t, &pq, opts, &CancelToken::new(), |_| {
                    first_t.get_or_insert_with(|| s.elapsed().as_secs_f64() * 1e3);
                });
                total.push(s.elapsed().as_secs_f64() * 1e3);
                first.push(first_t.unwrap_or(0.0));
                hits += res.hits.len();
            }
            println!(
                "{:<10} {:>6} {:>9.2} {:>9.2} {:>9.2} {:>9.2} {:>11.2} {:>9.0}",
                class,
                qs.len(),
                pct(&mut total, 0.5),
                pct(&mut total, 0.95),
                pct(&mut total, 0.99),
                pct(&mut total, 1.0),
                pct(&mut first, 0.95),
                hits as f64 / qs.len() as f64
            );
        }
    }
    // Highlight cost for a page of results.
    let q = parse("reprot", 0);
    let res = search(&t, &q, &Options::default(), &CancelToken::new(), |_| {});
    let s = Instant::now();
    for h in res.hits.iter().take(30) {
        std::hint::black_box(flint_fuzzy::engine::highlight(&t, &q, Intensity::Normal, h));
        std::hint::black_box(t.parent_path(h.id));
    }
    println!("\nhighlight+path for 30 visible rows: {:.3} ms", s.elapsed().as_secs_f64() * 1e3);
}

fn dump(args: &[String]) {
    let n: usize = arg(args, "--n", "1000000").parse().unwrap();
    let seed: u64 = arg(args, "--seed", "1").parse().unwrap();
    let out = arg(args, "--out", "names.txt");
    let t = corpus::build(n, seed);
    let mut s = String::new();
    for id in 0..t.len() as u32 {
        s.push_str(t.folded(id));
        s.push('\n');
    }
    std::fs::write(&out, s).unwrap();
    let qs: Vec<String> = ["typo1", "scattered", "nonsense"].iter().flat_map(|c| make_queries(&t, c, 50, 7)).map(|q| fold_str(&q)).collect();
    std::fs::write(format!("{out}.queries"), qs.join("\n") + "\n").unwrap();
    println!("wrote {} names to {out} and {} queries to {out}.queries", t.len(), qs.len());
}

/// Single-threaded kernel throughput, mirrored by bench/cpp/kernels.cpp.
fn kernels(args: &[String]) {
    let path = arg(args, "--corpus", "names.txt");
    let text = std::fs::read_to_string(&path).unwrap();
    let names: Vec<Vec<char>> = text.lines().map(|l| l.chars().collect()).collect();
    let queries: Vec<String> = std::fs::read_to_string(format!("{path}.queries")).unwrap().lines().map(str::to_owned).collect();
    let s = Instant::now();
    let (mut osa_hits, mut sub_hits) = (0u64, 0u64);
    for q in &queries {
        let p = Pattern::new(q);
        let qc: Vec<char> = q.chars().collect();
        let k = Intensity::Normal.max_edits(qc.len());
        for n in &names {
            if osa_substring(&p, n, k).is_some() {
                osa_hits += 1;
            }
            if subsequence_positions(&qc, n).is_some() {
                sub_hits += 1;
            }
        }
    }
    let el = s.elapsed().as_secs_f64();
    let cells = (queries.len() * names.len()) as f64;
    println!("rust kernels: {} queries x {} names: {:.3}s, {:.1} ns/name-pair, osa_hits={osa_hits} sub_hits={sub_hits}", queries.len(), names.len(), el, el * 1e9 / cells);
}

/// Per-stage timing breakdown for a few queries (diagnostics).
fn stages(args: &[String]) {
    let n: usize = arg(args, "--n", "1000000").parse().unwrap();
    let t = corpus::build(n, 1);
    let _ = t.chain_masks();
    let only = arg(args, "--q", "");
    for q in ["report", "reprot", "qxzvbn", "mgp", "a"] {
        if !only.is_empty() && q != only {
            continue;
        }
        for _ in 0..3 {
            search(&t, &parse(q, 0), &Options::default(), &CancelToken::new(), |_| {});
        }
        let r = search(&t, &parse(q, 0), &Options::default(), &CancelToken::new(), |_| {});
        let parts: Vec<String> = r.status.stages.iter().map(|s| format!("{:?}={}us/{}", s.stage, s.micros, s.matched)).collect();
        println!("{q:>8}: total {}us  {}", r.status.elapsed_micros, parts.join("  "));
    }
}

/// Raw scan floors: what a pass costs before any matching work.
fn floor(args: &[String]) {
    use rayon::prelude::*;
    let n: usize = arg(args, "--n", "1000000").parse().unwrap();
    let t = corpus::build(n, 1);
    let qm = flint_fuzzy::fold::mask_of("qxzvbn");
    for label in ["mask-1t", "mask-par", "live+folded-eq-1t", "live+folded-eq-par"] {
        let mut best = f64::MAX;
        for _ in 0..5 {
            let s = Instant::now();
            let c: usize = match label {
                "mask-1t" => (0..t.len() as u32).filter(|&i| t.mask(i) & qm == qm).count(),
                "mask-par" => (0..t.len() as u32).into_par_iter().filter(|&i| t.mask(i) & qm == qm).count(),
                "live+folded-eq-1t" => (0..t.len() as u32).filter(|&i| t.is_live(i) && t.folded(i) == "qxzvbn").count(),
                _ => (0..t.len() as u32).into_par_iter().filter(|&i| t.is_live(i) && t.folded(i) == "qxzvbn").count(),
            };
            std::hint::black_box(c);
            best = best.min(s.elapsed().as_secs_f64() * 1e3);
        }
        println!("{label:>20}: {best:.2} ms");
    }
}

/// Initial index into SQLite, restart load, and incremental journal batch.
fn index_bench(args: &[String]) {
    use flint_index::indexer::VolumeIndexer;
    use flint_index::sim::SimVolume;
    use flint_index::store::Store;
    use std::sync::atomic::AtomicBool;
    use std::sync::RwLock;
    let n: usize = arg(args, "--n", "1000000").parse().unwrap();
    let dir = std::env::temp_dir().join(format!("flint-index-bench-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let db = dir.join("index.db");
    let mut r = Rng(42);
    let mut sim = SimVolume::new();
    let mut dirs = vec![sim.root];
    let s = Instant::now();
    for _ in 0..n {
        let d = dirs[if r.chance(70) && dirs.len() > 50 { dirs.len() - 1 - r.below(50) } else { r.below(dirs.len()) }];
        let is_dir = r.chance(10);
        if let Some(f) = sim.create(d, &corpus::file_name(&mut r), is_dir) {
            if is_dir {
                dirs.push(f);
            }
        }
    }
    println!("simulated volume: {} links in {:.2}s", sim.paths("C:").len(), s.elapsed().as_secs_f64());
    let (mut store, _) = Store::open(&db).unwrap();
    let table = RwLock::new(NameTable::new());
    let mut ix = VolumeIndexer::open(&mut store, &table, 0, "bench", "C:", sim.root).unwrap();
    let s = Instant::now();
    ix.sync(&mut sim, &mut store, &table, &AtomicBool::new(false)).unwrap();
    println!("initial index (enumerate + SQLite write): {:.2}s, live={}", s.elapsed().as_secs_f64(), ix.is_complete());
    drop((ix, table, store));
    let size: u64 = std::fs::read_dir(&dir).unwrap().map(|e| e.unwrap().metadata().unwrap().len()).sum();
    println!("database size (incl. WAL): {:.1} MB", size as f64 / 1e6);
    let rss0 = rss_kb();
    let s = Instant::now();
    let (mut store, _) = Store::open(&db).unwrap();
    let table = RwLock::new(NameTable::new());
    let mut ix = VolumeIndexer::open(&mut store, &table, 0, "bench", "C:", sim.root).unwrap();
    println!("restart load from SQLite: {:.2}s ({} slots), RSS +{:.1} MB", s.elapsed().as_secs_f64(), table.read().unwrap().live_count(), (rss_kb() - rss0) as f64 / 1024.0);
    {
        // Breakdown: SQLite read alone vs rebuilding the in-memory index.
        let (st, _) = Store::open(&db).unwrap();
        let s = Instant::now();
        let mut recs = Vec::new();
        st.load(1, |r| recs.push(r)).unwrap();
        let read = s.elapsed();
        let s = Instant::now();
        let mut t = NameTable::new();
        let mut v = flint_index::volume::VolumeIndex::new(&mut t, 0, sim.root, "C:");
        let mut ch = flint_index::volume::Changes::default();
        for r in &recs {
            v.apply_record(&mut t, r, &mut ch);
        }
        println!("  breakdown: SQLite read {:.2}s, index rebuild {:.2}s", read.as_secs_f64(), s.elapsed().as_secs_f64());
    }
    {
        let snaps = dir.join("snapshots");
        let s = Instant::now();
        assert!(ix.write_snapshot(&store, &table, &snaps).unwrap());
        let write = s.elapsed();
        let snap_bytes = std::fs::metadata(flint_index::snapshot::path_for(&snaps, "bench")).unwrap().len();
        let s = Instant::now();
        drop(Store::open(&db).unwrap());
        println!("  Store::open alone: {:.3}s", s.elapsed().as_secs_f64());
        let s = Instant::now();
        let (mut st2, _) = Store::open(&db).unwrap();
        let t2 = RwLock::new(NameTable::new());
        let (_, src) = VolumeIndexer::open_with_snapshot(&mut st2, &t2, 0, "bench", "C:", sim.root, Some(&snaps)).unwrap();
        println!(
            "snapshot: write {:.2}s, {:.1} MB; restart load {:.3}s via {src:?} ({} slots)",
            write.as_secs_f64(),
            snap_bytes as f64 / 1e6,
            s.elapsed().as_secs_f64(),
            t2.read().unwrap().live_count()
        );
    }
    let frns = sim.frns();
    for i in 0..10_000 {
        let f = frns[(i * 7919) % frns.len()];
        sim.write(f, i as u64);
    }
    let s = Instant::now();
    ix.sync(&mut sim, &mut store, &table, &AtomicBool::new(false)).unwrap();
    println!("apply 10,000 file changes (20,000 USN records) incl. commit: {:.1} ms, rescans={}", s.elapsed().as_secs_f64() * 1e3, ix.stats.rescans);
    let _ = std::fs::remove_dir_all(&dir);
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    match args.get(1).map(String::as_str) {
        Some("engine") => engine_bench(&args),
        Some("dump") => dump(&args),
        Some("kernels") => kernels(&args),
        Some("stages") => stages(&args),
        Some("floor") => floor(&args),
        Some("index") => index_bench(&args),
        _ => eprintln!("usage: flint-bench engine|dump|kernels [options]"),
    }
}
