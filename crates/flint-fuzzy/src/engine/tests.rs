use super::*;
use crate::fold::fold_str;
use crate::query::parse;
use crate::table::{Meta, NameTable, NO_PARENT};

fn table() -> (NameTable, Vec<EntryId>) {
    let mut t = NameTable::new();
    let root = t.push(NO_PARENT, "C:", Meta::dir(0));
    let users = t.push(root, "Users", Meta::dir(0));
    let docs = t.push(users, "Documents", Meta::dir(0));
    let proj = t.push(users, "Projects", Meta::dir(0));
    let src = t.push(proj, "src", Meta::dir(0));
    let mut ids = vec![root, users, docs, proj, src];
    for (parent, name, size, mtime) in [
        (docs, "report.pdf", 2_000_000u64, 1_700_000_000i64),
        (docs, "Quarterly Report 2024.docx", 30_000, 1_710_000_000),
        (docs, "reporting_tool.exe", 5_000_000, 1_600_000_000),
        (docs, "Résumé.docx", 10_000, 1_700_000_000),
        (docs, "unrelated.txt", 10, 1_500_000_000),
        (src, "main.rs", 1_000, 1_700_000_000),
        (src, "my_great_project.rs", 1_000, 1_700_000_000),
        (docs, "main.rs", 1_000, 1_700_000_000),
        (docs, "日本語のメモ.txt", 100, 1_700_000_000),
        (docs, "ZebraQuixotic.bin", 100, 1_700_000_000),
    ] {
        ids.push(t.push(parent, name, Meta::file(0, size, mtime)));
    }
    (t, ids)
}

fn run(t: &NameTable, q: &str, opts: &Options) -> SearchResult {
    search(t, &parse(q, 1_710_000_000), opts, &CancelToken::new(), |_| {})
}

fn names(t: &NameTable, r: &SearchResult) -> Vec<String> {
    r.hits.iter().map(|h| t.name(h.id).to_owned()).collect()
}

#[test]
fn exact_beats_prefix_beats_substring() {
    let (t, _) = table();
    let r = run(&t, "report", &Options { enough: usize::MAX, ..Default::default() });
    let n = names(&t, &r);
    assert_eq!(n[0], "report.pdf"); // exact stem
    assert_eq!(n[1], "reporting_tool.exe"); // prefix
    assert_eq!(n[2], "Quarterly Report 2024.docx"); // word start
    assert_eq!(r.hits[0].tier, tier::EXACT_STEM);
    assert_eq!(r.hits[1].tier, tier::PREFIX);
    assert_eq!(r.hits[2].tier, tier::WORD);
}

#[test]
fn typos_match_via_edit_distance() {
    let (t, _) = table();
    for q in ["reprot", "raport", "repport", "reort", "quartely"] {
        let r = run(&t, q, &Options::default());
        let first = &r.hits[0];
        assert_eq!(first.stage, Stage::Fuzzy, "{q}");
        assert!(t.name(first.id).to_lowercase().contains(if q == "quartely" { "quarterly" } else { "report" }), "{q}");
    }
    let r = run(&t, "reprot", &Options::default());
    let h = highlight(&t, &parse("reprot", 0), Intensity::Normal, &r.hits[0]);
    assert!(!h.name.is_empty());
    let name: Vec<char> = t.name(r.hits[0].id).chars().collect();
    assert!(h.name.iter().all(|&p| p < name.len()));
}

#[test]
fn scattered_characters_match_and_highlight() {
    let (t, _) = table();
    let r = run(&t, "mgp", &Options::default());
    assert_eq!(names(&t, &r)[0], "my_great_project.rs");
    assert_eq!(r.hits[0].stage, Stage::Subsequence);
    let h = highlight(&t, &parse("mgp", 0), Intensity::Normal, &r.hits[0]);
    assert_eq!(h.name, vec![0, 3, 9]);
}

#[test]
fn multi_token_and_phrases() {
    let (t, _) = table();
    let r = run(&t, "2024 quarterly", &Options::default());
    assert_eq!(names(&t, &r)[0], "Quarterly Report 2024.docx");
    assert_eq!(r.hits[0].tier, tier::SUBSTRING);
    let r = run(&t, r#""quarterly report""#, &Options::default());
    assert_eq!(r.hits[0].tier, tier::PREFIX);
}

#[test]
fn partial_path_queries() {
    let (t, _) = table();
    let r = run(&t, r"proj\main", &Options::default());
    let paths: Vec<String> = r.hits.iter().map(|h| t.path(h.id)).collect();
    assert_eq!(paths[0], r"C:\Users\Projects\src\main.rs");
    assert!(!paths.contains(&r"C:\Users\Documents\main.rs".to_string()));
    let h = highlight(&t, &parse(r"proj\main", 0), Intensity::Normal, &r.hits[0]);
    let parent: Vec<char> = t.parent_path(r.hits[0].id).chars().collect();
    let hl: String = h.dir.iter().map(|&i| parent[i]).collect();
    assert_eq!(hl.to_lowercase(), "proj");
    // Directory-only query lists contents.
    let r = run(&t, "projects/src/", &Options::default());
    assert_eq!(r.hits.len(), 2);
    // Fuzzy directory segment still matches, one tier lower.
    let r = run(&t, r"projcts\main", &Options::default());
    assert_eq!(t.path(r.hits[0].id), r"C:\Users\Projects\src\main.rs");
    assert_eq!(r.hits[0].tier, tier::EXACT_STEM - 1);
}

#[test]
fn unicode_names() {
    let (t, _) = table();
    let r = run(&t, "resume", &Options::default());
    assert_eq!(names(&t, &r)[0], "Résumé.docx");
    assert_eq!(r.hits[0].tier, tier::EXACT_STEM);
    let r = run(&t, "メモ", &Options::default());
    assert_eq!(names(&t, &r)[0], "日本語のメモ.txt");
    let r = run(&t, "RÉSUMÉ", &Options::default());
    assert_eq!(names(&t, &r)[0], "Résumé.docx");
}

#[test]
fn nonsense_queries_degrade_progressively() {
    let (t, _) = table();
    let q = "qxzbr";
    let strict = run(&t, q, &Options { best_effort: false, ..Default::default() });
    assert!(strict.hits.is_empty());
    assert!(!strict.status.indexed_exhausted);
    let weak = run(&t, q, &Options::default());
    assert!(!weak.hits.is_empty(), "best effort should find weak similarity");
    assert!(weak.hits.iter().all(|h| h.tier <= tier::PATH));
    let all = run(&t, q, &Options { exhaustive: true, enough: usize::MAX, limit: 1000, ..Default::default() });
    assert!(all.status.indexed_exhausted);
    assert_eq!(all.status.total_matched, all.status.considered);
    assert_eq!(all.status.considered, t.live_count());
    // Weak hits outrank the remaining candidates.
    let first_remaining = all.hits.iter().position(|h| h.tier == tier::REMAINING).unwrap();
    assert!(all.hits[..first_remaining].iter().all(|h| h.tier > tier::REMAINING));
}

#[test]
fn enough_stops_early_and_reports_next_stage() {
    let (t, _) = table();
    let r = run(&t, "r", &Options { enough: 1, ..Default::default() });
    assert!(matches!(r.status.stop, Stop::Enough { .. }));
    let forced = run(&t, "r", &Options { enough: 1, through: Some(Stage::Weak), ..Default::default() });
    assert!(forced.status.stages.iter().any(|s| s.stage == Stage::Weak));
}

#[test]
fn filters() {
    let (t, _) = table();
    let r = run(&t, "ext:docx", &Options::default());
    assert_eq!(r.hits.len(), 2);
    assert_eq!(names(&t, &r)[0], "Quarterly Report 2024.docx"); // most recent first
    let r = run(&t, "type:folder", &Options::default());
    assert_eq!(r.hits.len(), 5);
    let r = run(&t, "report size:>1mb", &Options::default());
    assert_eq!(names(&t, &r), vec!["report.pdf", "reporting_tool.exe"]);
    let r = run(&t, "report modified:2024-01-01..", &Options::default());
    assert_eq!(names(&t, &r), vec!["Quarterly Report 2024.docx"]);
    let r = run(&t, "main path:projects", &Options::default());
    assert_eq!(r.hits[0].tier, tier::EXACT_STEM);
    assert!(r.hits.iter().all(|h| t.path(h.id).contains("Projects")));
    // Only weak best-effort extras besides the strong match.
    assert!(r.hits[1..].iter().all(|h| h.tier <= tier::PATH));
    let r = run(&t, "report type:exe", &Options::default());
    assert_eq!(names(&t, &r), vec!["reporting_tool.exe"]);
}

#[test]
fn content_filter_is_reported_not_ignored() {
    let (t, _) = table();
    let r = run(&t, "report content:invoice", &Options::default());
    assert_eq!(r.status.stop, Stop::Unsupported);
    assert!(r.hits.is_empty());
    assert_eq!(r.status.unsupported_filters, vec!["content:invoice"]);
}

#[test]
fn cancellation_and_empty_query() {
    let (t, _) = table();
    let c = CancelToken::new();
    c.cancel();
    let r = search(&t, &parse("report", 0), &Options::default(), &c, |_| {});
    assert_eq!(r.status.stop, Stop::Canceled);
    assert!(r.status.stages.iter().all(|s| !s.complete));
    let r = run(&t, "   ", &Options::default());
    assert_eq!(r.status.stop, Stop::EmptyQuery);
}

#[test]
fn deleted_entries_are_never_returned() {
    let (mut t, ids) = table();
    let report = ids[5];
    t.remove(report);
    let r = run(&t, "report", &Options { exhaustive: true, enough: usize::MAX, ..Default::default() });
    assert!(r.hits.iter().all(|h| h.id != report));
}

#[test]
fn progress_snapshots_are_monotone() {
    let (t, _) = table();
    let mut snaps = Vec::new();
    search(&t, &parse("re", 0), &Options { enough: usize::MAX, exhaustive: true, ..Default::default() }, &CancelToken::new(), |r| {
        snaps.push((r.hits.len(), r.hits.first().map(|h| h.id), r.status.stages.len()))
    });
    assert!(snaps.len() >= 4);
    for w in snaps.windows(2) {
        assert!(w[1].0 >= w[0].0);
        if w[0].1.is_some() {
            assert_eq!(w[1].1, w[0].1, "strongest result must stay stable");
        }
        assert!(w[1].2 > w[0].2);
    }
}

// ---- Property tests against brute-force references ----

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn word(&mut self, alpha: &[char], lo: usize, hi: usize) -> String {
        let n = lo + (self.next() as usize) % (hi - lo + 1);
        (0..n).map(|_| alpha[(self.next() as usize) % alpha.len()]).collect()
    }
}

fn random_table(rng: &mut Rng, n: usize) -> NameTable {
    let alpha: Vec<char> = "abcdeéRS_. 1".chars().collect();
    let mut t = NameTable::new();
    let root = t.push(NO_PARENT, "R", Meta::dir(0));
    let mut dirs = vec![root];
    for i in 0..n {
        let parent = dirs[(rng.next() as usize) % dirs.len()];
        let name = rng.word(&alpha, 1, 14);
        let is_dir = i % 7 == 0;
        let id = t.push(parent, &name, if is_dir { Meta::dir(0) } else { Meta::file(0, i as u64, 0) });
        if is_dir {
            dirs.push(id);
        }
    }
    t
}

#[test]
fn substring_stage_is_complete() {
    let mut rng = Rng(77);
    let t = random_table(&mut rng, 3000);
    let alpha: Vec<char> = "abcde".chars().collect();
    for _ in 0..60 {
        let q = rng.word(&alpha, 1, 3);
        let r = run(&t, &q, &Options { enough: usize::MAX, limit: usize::MAX, intensity: Intensity::Off, best_effort: false, ..Default::default() });
        let got: std::collections::BTreeSet<EntryId> = r.hits.iter().filter(|h| h.tier >= tier::SUBSTRING).map(|h| h.id).collect();
        let want: std::collections::BTreeSet<EntryId> = (0..t.len() as EntryId).filter(|&id| t.folded(id).contains(q.as_str())).collect();
        assert_eq!(got, want, "query {q:?}");
    }
}

#[test]
fn fuzzy_stage_is_complete_within_edit_budget() {
    let mut rng = Rng(4242);
    let t = random_table(&mut rng, 2000);
    let alpha: Vec<char> = "abcdeé".chars().collect();
    for _ in 0..60 {
        let q = rng.word(&alpha, 3, 8);
        let k = Intensity::Normal.max_edits(q.chars().count());
        let qc: Vec<char> = fold_str(&q).chars().collect();
        let r = run(&t, &q, &Options { enough: usize::MAX, limit: usize::MAX, best_effort: false, ..Default::default() });
        let got: std::collections::BTreeSet<EntryId> = r.hits.iter().filter(|h| h.tier >= tier::FUZZY).map(|h| h.id).collect();
        for id in 0..t.len() as EntryId {
            let fc: Vec<char> = t.folded(id).chars().collect();
            let d = crate::kernels::osa_substring_positions(&qc, &fc).0;
            assert_eq!(d <= k, got.contains(&id), "q={q:?} name={:?} d={d} k={k}", t.folded(id));
        }
    }
}

#[test]
fn subsequence_stage_is_complete_and_highlights_are_valid() {
    let mut rng = Rng(99);
    let t = random_table(&mut rng, 2000);
    let alpha: Vec<char> = "abcde".chars().collect();
    for _ in 0..40 {
        let q = rng.word(&alpha, 2, 5);
        let qc: Vec<char> = q.chars().collect();
        let query = parse(&q, 0);
        let r = search(&t, &query, &Options { enough: usize::MAX, limit: usize::MAX, best_effort: false, ..Default::default() }, &CancelToken::new(), |_| {});
        let got: std::collections::BTreeSet<EntryId> = r.hits.iter().map(|h| h.id).collect();
        for id in 0..t.len() as EntryId {
            let fc: Vec<char> = t.folded(id).chars().collect();
            if crate::kernels::subsequence_positions(&qc, &fc).is_some() {
                assert!(got.contains(&id), "q={q:?} name={:?}", t.folded(id));
            }
        }
        for h in &r.hits {
            let hl = highlight(&t, &query, Intensity::Normal, h);
            let n = t.name(h.id).chars().count();
            assert!(hl.name.iter().all(|&p| p < n));
            assert!(hl.name.windows(2).all(|w| w[0] < w[1]));
        }
        assert!(r.hits.windows(2).all(|w| w[0] >= w[1]), "ranking order");
    }
}
