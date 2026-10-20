//! Platform-neutral UI state: result list with stable selection, stale-event
//! rejection, user-facing status text and latency statistics.

use flint_fuzzy::engine::{Hit, SearchResult, Status, Stop};
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

/// Index coverage summary supplied by the catalog.
pub use flint_index::catalog::Coverage;

/// One-line status that never conflates "nothing matched" with "we did not
/// look everywhere". `deeper` is the search-deeper step on offer, if any.
pub fn status_line(s: &Status, shown: usize, cov: &Coverage, deeper: Option<crate::ux::Deeper>) -> String {
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
            parts.push(if cov.loading {
                // Never say "no matches" while part of the index is not loaded.
                "no matches yet — index still loading".into()
            } else if s.indexed_exhausted {
                "no indexed candidates".into()
            } else {
                "no matches".into()
            });
        } else if s.total_matched > shown {
            parts.push(format!("{shown} of {} matches", s.total_matched));
        } else {
            parts.push(format!("{} matches", s.total_matched));
        }
        if let Some(top) = s.stages.last() {
            parts.push(format!("through: {}", top.stage.label()));
        }
        if s.indexed_exhausted {
            parts.push("all indexed entries examined".into());
        } else if let Some(d) = deeper {
            parts.push(d.hint().into());
        }
    }
    if !s.query_errors.is_empty() {
        parts.push(format!("ignored invalid filters: {}", s.query_errors.join(" ")));
    }
    // Content-index coverage and similar caveats from the search.
    parts.extend(s.notes.iter().cloned());
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
        let complete = Coverage { complete: true, loading: false, problems: vec![] };
        let partial = Coverage { complete: false, loading: false, problems: vec!["D: indexing 40%".into()] };
        let loading = Coverage { complete: false, loading: true, problems: vec!["C: loading index".into()] };
        assert!(status_line(&r.status, 0, &loading, None).starts_with("no matches yet — index still loading"));
        assert!(status_line(&r.status, 0, &complete, None).starts_with("no matches"));
        let s = status_line(&r.status, 0, &partial, None);
        assert!(s.contains("no matches") && s.contains("index incomplete: D: indexing 40%"), "{s}");
        let ex = run(&t, "zzzzqqq", &Options { exhaustive: true, ..Default::default() });
        let s = status_line(&ex.status, ex.hits.len(), &partial, None);
        assert!(s.contains("all indexed entries examined") && s.contains("index incomplete"), "{s}");
        let c = CancelToken::new();
        c.cancel();
        let canceled = search(&t, &parse("rep", 0), &Options::default(), &c, |_| {});
        assert!(status_line(&canceled.status, 0, &complete, None).starts_with("search canceled"));
        let unsupported = run(&t, "content:x", &Options::default());
        assert!(status_line(&unsupported.status, 0, &complete, None).contains("not available yet: content:x"));
    }
}

/// UI colors as Win32 COLORREF (0x00BBGGRR).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Palette {
    pub bg: u32,
    pub edit_bg: u32,
    pub text: u32,
    pub dim: u32,
    pub sel_bg: u32,
    pub sel_text: u32,
    pub accent: u32,
}

pub const fn rgb(r: u8, g: u8, b: u8) -> u32 {
    (r as u32) | (g as u32) << 8 | (b as u32) << 16
}

pub fn palette(dark: bool) -> Palette {
    if dark {
        Palette { bg: rgb(32, 32, 32), edit_bg: rgb(43, 43, 43), text: rgb(232, 232, 232), dim: rgb(160, 160, 160), sel_bg: rgb(31, 58, 90), sel_text: rgb(255, 255, 255), accent: rgb(110, 185, 255) }
    } else {
        Palette { bg: rgb(255, 255, 255), edit_bg: rgb(255, 255, 255), text: rgb(32, 32, 32), dim: rgb(105, 105, 105), sel_bg: rgb(204, 227, 245), sel_text: rgb(0, 0, 0), accent: rgb(0, 90, 192) }
    }
}

/// WCAG relative-luminance contrast ratio between two COLORREFs.
#[cfg_attr(not(test), allow(dead_code))] // used by the palette tests
pub fn contrast(a: u32, b: u32) -> f64 {
    let lum = |c: u32| {
        let ch = |v: u32| {
            let s = v as f64 / 255.0;
            if s <= 0.039_28 { s / 12.92 } else { ((s + 0.055) / 1.055).powf(2.4) }
        };
        0.2126 * ch(c & 0xFF) + 0.7152 * ch((c >> 8) & 0xFF) + 0.0722 * ch((c >> 16) & 0xFF)
    };
    let (x, y) = (lum(a), lum(b));
    (x.max(y) + 0.05) / (x.min(y) + 0.05)
}

#[cfg(test)]
mod palette_tests {
    use super::*;

    #[test]
    fn palettes_meet_wcag_aa_text_contrast() {
        for dark in [false, true] {
            let p = palette(dark);
            for (fg, bg, what) in [(p.text, p.bg, "text"), (p.text, p.edit_bg, "edit"), (p.sel_text, p.sel_bg, "selection"), (p.accent, p.bg, "highlight"), (p.dim, p.bg, "status")] {
                let c = contrast(fg, bg);
                assert!(c >= 4.5, "dark={dark} {what}: {c:.2}:1 is below 4.5:1");
            }
        }
    }
}

/// Search options from the user's settings.
pub fn search_options(c: &flint_config::Search) -> flint_fuzzy::engine::Options {
    use flint_config::FuzzyIntensity as F;
    use flint_fuzzy::engine::Intensity as I;
    flint_fuzzy::engine::Options {
        limit: c.result_limit,
        intensity: match c.fuzzy_intensity {
            F::Off => I::Off,
            F::Low => I::Low,
            F::Normal => I::Normal,
            F::Aggressive => I::Aggressive,
        },
        enough: c.enough,
        best_effort: c.best_effort,
        exhaustive: c.exhaustive_fallback,
        through: None,
        deadline: (c.deadline_ms > 0).then(|| std::time::Duration::from_millis(c.deadline_ms)),
    }
}

#[cfg(test)]
mod options_tests {
    #[test]
    fn defaults_match_engine_defaults() {
        assert_eq!(super::search_options(&flint_config::Search::default()), flint_fuzzy::engine::Options::default());
    }
}
