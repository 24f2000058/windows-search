//! Platform-neutral UI state: result list with stable selection, stale-event
//! rejection, user-facing status text and latency statistics.

use flint_fuzzy::engine::{Hit, SearchResult, Stage, Status, Stop};
use flint_fuzzy::table::EntryId;

#[derive(Default)]
pub struct ResultsModel {
    pub hits: Vec<Hit>,
    pub status: Option<Status>,
    /// Generation of the displayed results.
    pub generation: u64,
    pub selected: Option<usize>,
}

impl ResultsModel {
    pub fn selected_id(&self) -> Option<EntryId> {
        self.selected.and_then(|i| self.hits.get(i)).map(|h| h.id)
    }

    /// Applies a (partial or final) result if it is not older than what is
    /// shown. Keeps the same entry selected when it is still present;
    /// otherwise selects the first row. Returns false for stale results.
    pub fn apply(&mut self, generation: u64, latest: u64, r: &SearchResult) -> bool {
        if generation != latest || generation < self.generation {
            return false;
        }
        let keep = if generation == self.generation { self.selected_id() } else { None };
        self.generation = generation;
        self.hits = r.hits.clone();
        self.status = Some(r.status.clone());
        self.selected = match keep.and_then(|id| self.hits.iter().position(|h| h.id == id)) {
            Some(i) => Some(i),
            None if self.hits.is_empty() => None,
            None => Some(0),
        };
        true
    }

    pub fn move_selection(&mut self, delta: isize) {
        if self.hits.is_empty() {
            self.selected = None;
            return;
        }
        let cur = self.selected.unwrap_or(0) as isize;
        self.selected = Some((cur + delta).clamp(0, self.hits.len() as isize - 1) as usize);
    }
}

/// Index coverage summary supplied by the indexer.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Coverage {
    pub complete: bool,
    /// e.g. "D: indexing 43%", "\\\\nas\\share unavailable".
    pub problems: Vec<String>,
}

/// One-line status that never conflates "nothing matched" with "we did not
/// look everywhere".
pub fn status_line(s: &Status, shown: usize, cov: &Coverage) -> String {
    let mut parts = Vec::new();
    match s.stop {
        Stop::Canceled => parts.push("search canceled".to_owned()),
        Stop::Deadline => parts.push(format!("time limit reached after {} — results partial", s.stages.last().map_or("start", |r| r.stage.label()))),
        Stop::EmptyQuery => return if cov.complete { "type to search".into() } else { format!("type to search · {}", cov.problems.join(", ")) },
        Stop::Unsupported => parts.push(format!("not available yet: {}", s.unsupported_filters.join(", "))),
        _ => {}
    }
    if !matches!(s.stop, Stop::Canceled | Stop::Unsupported) {
        if s.total_matched == 0 {
            parts.push(if s.indexed_exhausted { "no indexed candidates".into() } else { "no matches".into() });
        } else if s.total_matched > shown {
            parts.push(format!("{shown} of {} matches", s.total_matched));
        } else {
            parts.push(format!("{} matches", s.total_matched));
        }
        if let Some(top) = s.stages.last() {
            parts.push(format!("through: {}", top.stage.label()));
        }
        match s.stop {
            Stop::Enough { next } if next <= Stage::Weak => parts.push("weaker matches available".into()),
            Stop::Enough { .. } => parts.push("remaining candidates available".into()),
            _ if s.indexed_exhausted => parts.push("all indexed entries examined".into()),
            _ => {}
        }
    }
    if !s.query_errors.is_empty() {
        parts.push(format!("ignored invalid filters: {}", s.query_errors.join(" ")));
    }
    if !cov.complete {
        // Even "all indexed entries examined" says nothing about unindexed
        // parts of the filesystem.
        parts.push(format!("index incomplete: {}", if cov.problems.is_empty() { "in progress".into() } else { cov.problems.join(", ") }));
    }
    parts.push(format!("{:.1} ms", s.elapsed_micros as f64 / 1000.0));
    parts.join(" · ")
}

#[derive(Default)]
pub struct LatencyStats {
    samples: Vec<f64>,
}

impl LatencyStats {
    pub fn record(&mut self, ms: f64) {
        self.samples.push(ms);
    }
    pub fn summary(&self) -> Option<(usize, f64, f64, f64)> {
        if self.samples.is_empty() {
            return None;
        }
        let mut v = self.samples.clone();
        v.sort_by(|a, b| a.partial_cmp(b).unwrap());
        let p = |q: f64| v[((v.len() - 1) as f64 * q).round() as usize];
        Some((v.len(), p(0.5), p(0.95), v[v.len() - 1]))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use flint_fuzzy::engine::{search, CancelToken, Options};
    use flint_fuzzy::query::parse;
    use flint_fuzzy::table::{Meta, NameTable, NO_PARENT};

    fn table() -> NameTable {
        let mut t = NameTable::new();
        let r = t.push(NO_PARENT, "C:", Meta::dir(0));
        for n in ["report.pdf", "report_old.pdf", "repo", "notes.txt", "raport.doc"] {
            t.push(r, n, Meta::file(0, 1, 1));
        }
        t
    }

    fn run(t: &NameTable, q: &str, o: &Options) -> SearchResult {
        search(t, &parse(q, 0), o, &CancelToken::new(), |_| {})
    }

    #[test]
    fn selection_survives_result_updates_and_stale_results_are_dropped() {
        let t = table();
        let mut m = ResultsModel::default();
        let partial = run(&t, "rep", &Options { enough: 0, ..Default::default() });
        assert!(m.apply(5, 5, &partial));
        m.move_selection(1);
        let chosen = m.selected_id();
        // Final result for the same generation contains more rows.
        let full = run(&t, "rep", &Options::default());
        assert!(m.apply(5, 5, &full));
        assert_eq!(m.selected_id(), chosen, "selection preserved within a generation");
        // A late result from an older query is ignored.
        let old = run(&t, "notes", &Options::default());
        assert!(!m.apply(4, 5, &old));
        assert_eq!(m.selected_id(), chosen);
        // A new query resets to the top.
        let next = run(&t, "notes", &Options::default());
        assert!(m.apply(6, 6, &next));
        assert_eq!(m.selected, Some(0));
        m.move_selection(-10);
        assert_eq!(m.selected, Some(0));
    }

    #[test]
    fn status_distinguishes_no_match_from_incomplete_index() {
        let t = table();
        let r = run(&t, "zzzzqqq", &Options { best_effort: false, ..Default::default() });
        let complete = Coverage { complete: true, problems: vec![] };
        let partial = Coverage { complete: false, problems: vec!["D: indexing 40%".into()] };
        assert!(status_line(&r.status, 0, &complete).starts_with("no matches"));
        let s = status_line(&r.status, 0, &partial);
        assert!(s.contains("no matches") && s.contains("index incomplete: D: indexing 40%"), "{s}");
        let ex = run(&t, "zzzzqqq", &Options { exhaustive: true, ..Default::default() });
        let s = status_line(&ex.status, ex.hits.len(), &partial);
        assert!(s.contains("all indexed entries examined") && s.contains("index incomplete"), "{s}");
        let c = CancelToken::new();
        c.cancel();
        let canceled = search(&t, &parse("rep", 0), &Options::default(), &c, |_| {});
        assert!(status_line(&canceled.status, 0, &complete).starts_with("search canceled"));
        let unsupported = run(&t, "content:x", &Options::default());
        assert!(status_line(&unsupported.status, 0, &complete).contains("not available yet: content:x"));
    }
}
