//! Platform-neutral search-UX logic: the "search deeper" action, match
//! labels for weak tiers, size/date display and the filter bar, which edits
//! filter terms in the query text (the query stays the single source of
//! truth, so typed and picked filters can never disagree).

use flint_fuzzy::engine::{tier, Options, Stage, Status, Stop};
use flint_fuzzy::query::Clock;
use flint_fuzzy::table::NO_TIME;

/// What the "search deeper" key (F3) does next for the current result.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Deeper {
    /// Run stages through the weak path/similarity stage.
    Weaker,
    /// Also list every remaining indexed candidate (unranked).
    Remaining,
}

impl Deeper {
    pub fn hint(self) -> &'static str {
        match self {
            Deeper::Weaker => "F3: show weaker matches",
            Deeper::Remaining => "F3: show remaining indexed candidates",
        }
    }
}

/// The next deeper step available, if any. Nothing is offered for empty,
/// canceled or unsupported queries, or once every candidate was examined.
pub fn deeper_action(s: &Status, current: &Options) -> Option<Deeper> {
    match s.stop {
        Stop::EmptyQuery | Stop::Canceled | Stop::Unsupported => return None,
        _ => {}
    }
    if s.indexed_exhausted {
        return None;
    }
    let through = current.through.unwrap_or(Stage::Exact);
    match s.stop {
        Stop::Enough { next } if next <= Stage::Weak && through < Stage::Weak => Some(Deeper::Weaker),
        _ if through < Stage::Exhaustive => Some(Deeper::Remaining),
        _ => None,
    }
}

/// Options for the deeper search, derived from the configured `base`.
pub fn deeper_options(base: &Options, d: Deeper) -> Options {
    match d {
        Deeper::Weaker => Options { best_effort: true, through: Some(Stage::Weak), ..base.clone() },
        Deeper::Remaining => Options { best_effort: true, exhaustive: true, through: Some(Stage::Exhaustive), ..base.clone() },
    }
}

/// Label for how a row matched. Strong tiers (exact, prefix, word,
/// substring) get none; weaker ones say what they are, so a weak candidate
/// is never presented as if it were relevant.
pub fn match_label(t: u8) -> &'static str {
    match t {
        tier::FUZZY => "fuzzy",
        tier::SUBSEQUENCE => "scattered",
        tier::PATH => "weak: path",
        tier::SIMILAR => "weak: similar",
        tier::REMAINING => "unranked",
        _ => "",
    }
}

/// File size as Explorer shows it (KB rounded up, binary units).
pub fn format_size(size: u64, is_dir: bool, stale: bool) -> String {
    if is_dir {
        return String::new();
    }
    let s = if size < 1024 {
        format!("{size} B")
    } else if size < 1024 * 1024 {
        format!("{} KB", size.div_ceil(1024))
    } else if size < 1 << 30 {
        format!("{:.1} MB", size as f64 / (1 << 20) as f64)
    } else {
        format!("{:.2} GB", size as f64 / (1u64 << 30) as f64)
    };
    // Size not yet refreshed after a change (journal records carry none).
    if stale { format!("~{s}") } else { s }
}

/// Civil date from days since 1970-01-01 (inverse of `days_from_civil`).
pub fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (yoe + era * 400 + (m <= 2) as i64, m, d)
}

/// `YYYY-MM-DD HH:MM` in the clock's local zone ("" when unknown).
pub fn format_time(utc: i64, clock: &Clock) -> String {
    if utc == NO_TIME {
        return String::new();
    }
    let l = clock.to_local(utc);
    let (y, m, d) = civil_from_days(l.div_euclid(86_400));
    let secs = l.rem_euclid(86_400);
    format!("{y:04}-{m:02}-{d:02} {:02}:{:02}", secs / 3600, secs % 3600 / 60)
}

/// Local calendar date `days` days before now, as a query date.
pub fn local_date_ago(clock: &Clock, days: i64) -> String {
    let (y, m, d) = civil_from_days(clock.to_local(clock.now).div_euclid(86_400) - days);
    format!("{y:04}-{m:02}-{d:02}")
}

/// One filter-bar dropdown.
pub struct FilterChoice {
    pub key: &'static str,
    /// Keys that mean the same filter (replaced together).
    pub aliases: &'static [&'static str],
    /// (label, value) pairs; the first is "any" (no filter).
    pub options: Vec<(&'static str, String)>,
}

pub fn filter_bar(clock: &Clock) -> Vec<FilterChoice> {
    let v = |s: &str| s.to_owned();
    vec![
        FilterChoice {
            key: "type",
            aliases: &["kind"],
            options: vec![("Any type", v("")), ("Folders", v("folder")), ("Files", v("file")), ("Documents", v("doc")), ("Images", v("image")), ("Videos", v("video")), ("Audio", v("audio")), ("Archives", v("archive")), ("Code", v("code")), ("Programs", v("exe"))],
        },
        FilterChoice {
            key: "size",
            aliases: &[],
            options: vec![("Any size", v("")), ("Empty", v("empty")), ("Tiny (< 16 KB)", v("<16kb")), ("Small (16 KB–1 MB)", v("16kb..1mb")), ("Medium (1–128 MB)", v("1mb..128mb")), ("Large (128 MB–1 GB)", v("128mb..1gb")), ("Huge (> 1 GB)", v(">1gb"))],
        },
        FilterChoice {
            key: "modified",
            aliases: &["dm", "date"],
            options: vec![("Any date", v("")), ("Today", v("today")), ("Yesterday", v("yesterday")), ("Past week", v("week")), ("Past month", v("month")), ("Past year", v("year")), ("Older than a year", format!("<{}", local_date_ago(clock, 365)))],
        },
    ]
}

/// Whitespace-separated terms, keeping `"quoted phrases"` together
/// (mirrors the query parser).
fn terms(text: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let (mut start, mut in_q) = (None, false);
    for (i, c) in text.char_indices() {
        if c == '"' {
            in_q = !in_q;
        }
        if c.is_whitespace() && !in_q {
            if let Some(s) = start.take() {
                out.push(&text[s..i]);
            }
        } else if start.is_none() {
            start = Some(i);
        }
    }
    if let Some(s) = start {
        out.push(&text[s..]);
    }
    out
}

fn term_key(term: &str) -> Option<String> {
    if term.starts_with('"') {
        return None;
    }
    term.split_once(':').map(|(k, _)| k.to_ascii_lowercase())
}

fn is_choice_term(term: &str, c: &FilterChoice) -> bool {
    term_key(term).is_some_and(|k| k == c.key || c.aliases.contains(&k.as_str()))
}

/// Replaces (or removes, for an empty value) the filter term for `c`,
/// leaving the rest of the query untouched.
pub fn set_filter(text: &str, c: &FilterChoice, value: &str) -> String {
    let mut out: Vec<String> = terms(text).into_iter().filter(|t| !is_choice_term(t, c)).map(str::to_owned).collect();
    if !value.is_empty() {
        out.push(format!("{}:{value}", c.key));
    }
    let mut s = out.join(" ");
    // Keep a trailing space so typing continues as a new term.
    if !s.is_empty() && text.ends_with(' ') {
        s.push(' ');
    }
    s
}

/// Index of the option matching the query's current filter term (0 = any;
/// None = a typed value the dropdown has no entry for).
pub fn current_choice(text: &str, c: &FilterChoice) -> Option<usize> {
    let found: Vec<&str> = terms(text).into_iter().filter(|t| is_choice_term(t, c)).collect();
    match found.as_slice() {
        [] => Some(0),
        [one] => {
            let v = one.split_once(':').map_or("", |(_, v)| v).to_ascii_lowercase();
            c.options.iter().position(|(_, o)| !o.is_empty() && o.eq_ignore_ascii_case(&v))
        }
        _ => None,
    }
}

/// Hex dump: offset, 16 bytes, printable ASCII.
pub fn format_hex(bytes: &[u8], file_size: u64) -> String {
    let mut out = String::new();
    for (row, chunk) in bytes.chunks(16).enumerate() {
        out.push_str(&format!("{:08x}  ", row * 16));
        for i in 0..16 {
            match chunk.get(i) {
                Some(b) => out.push_str(&format!("{b:02x} ")),
                None => out.push_str("   "),
            }
        }
        out.push(' ');
        out.extend(chunk.iter().map(|&b| if (0x20..0x7f).contains(&b) { b as char } else { '.' }));
        out.push('\n');
    }
    if (bytes.len() as u64) < file_size {
        out.push_str(&format!("… first {} of {} bytes\n", bytes.len(), file_size));
    }
    out
}

fn size_text(n: u64) -> String {
    format_size(n, false, false)
}

/// (header line, body text) for the viewer pane. Never claims more than
/// the helper produced: truncation and warnings are always shown.
pub fn preview_text(p: &flint_preview::Preview) -> (String, String) {
    use flint_preview::Preview as P;
    match p {
        P::Text { text, encoding, truncated } => {
            let mut h = encoding.clone();
            if *truncated {
                h.push_str(" · showing the beginning only");
            }
            (h, text.clone())
        }
        P::Hex { offset_bytes, file_size } => (format!("binary file · {}", size_text(*file_size)), format_hex(offset_bytes, *file_size)),
        P::Image { format, width, height, thumbnail } => {
            let dims = if *width > 0 { format!("{width} × {height}") } else { "size unknown".into() };
            (format!("{format} image · {dims}"), if thumbnail.is_some() { String::new() } else { "(no thumbnail available)".into() })
        }
        P::Archive { format, entries, total_entries, total_size, warnings } => {
            let mut body = String::new();
            for w in warnings {
                body.push_str(&format!("Warning: {w}\n"));
            }
            if !warnings.is_empty() {
                body.push('\n');
            }
            for e in entries {
                body.push_str(&if e.is_dir { format!("{}\n", e.name) } else { format!("{}  ({})\n", e.name, size_text(e.size)) });
            }
            if (entries.len() as u64) < *total_entries {
                body.push_str(&format!("… and {} more\n", total_entries - entries.len() as u64));
            }
            (format!("{format} archive · {total_entries} entries · {} uncompressed · listing only, nothing extracted", size_text(*total_size)), body)
        }
        P::Unsupported { kind, reason } => (kind.clone(), reason.clone()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use flint_fuzzy::engine::{search, CancelToken};
    use flint_fuzzy::query::{days_from_civil, parse};
    use flint_fuzzy::table::{Meta, NameTable, NO_PARENT};

    fn utc() -> impl Fn(i64) -> i64 + Send + Sync {
        |_| 0
    }

    #[test]
    fn deeper_escalates_weaker_then_remaining_then_stops() {
        let mut t = NameTable::new();
        let r = t.push(NO_PARENT, "C:", Meta::dir(0));
        for i in 0..200 {
            t.push(r, &format!("report-{i}.pdf"), Meta::file(0, 1, 1));
        }
        t.push(r, "unrelated.txt", Meta::file(0, 1, 1));
        let base = Options { enough: 50, ..Default::default() };
        let run = |o: &Options| search(&t, &parse("report", 0), o, &CancelToken::new(), |_| {});
        let first = run(&base);
        assert!(matches!(first.status.stop, Stop::Enough { .. }));
        let d = deeper_action(&first.status, &base).unwrap();
        assert_eq!(d, Deeper::Weaker);
        let o = deeper_options(&base, d);
        let second = run(&o);
        assert_eq!(deeper_action(&second.status, &o), Some(Deeper::Remaining));
        let o = deeper_options(&base, Deeper::Remaining);
        let third = run(&o);
        assert!(third.status.indexed_exhausted);
        assert_eq!(deeper_action(&third.status, &o), None, "nothing deeper after exhausting the index");
        assert!(third.hits.iter().any(|h| match_label(h.tier) == "unranked"));
        let empty = run_empty(&t);
        assert_eq!(deeper_action(&empty, &base), None);
    }

    fn run_empty(t: &NameTable) -> Status {
        search(t, &parse("", 0), &Options::default(), &CancelToken::new(), |_| {}).status
    }

    #[test]
    fn labels_never_dress_up_weak_matches() {
        for t in [tier::EXACT_NAME, tier::EXACT_STEM, tier::PREFIX, tier::WORD, tier::SUBSTRING] {
            assert_eq!(match_label(t), "");
        }
        for t in [tier::FUZZY, tier::SUBSEQUENCE, tier::PATH, tier::SIMILAR, tier::REMAINING] {
            assert!(!match_label(t).is_empty());
        }
    }

    #[test]
    fn sizes_and_local_times_format_like_explorer() {
        assert_eq!(format_size(0, false, false), "0 B");
        assert_eq!(format_size(1025, false, false), "2 KB");
        assert_eq!(format_size(5 << 20, false, true), "~5.0 MB");
        assert_eq!(format_size(3 << 30, false, false), "3.00 GB");
        assert_eq!(format_size(99, true, false), "");
        let tz = utc();
        let c = Clock { now: 0, offset: &tz };
        assert_eq!(format_time(0, &c), "1970-01-01 00:00");
        assert_eq!(format_time(NO_TIME, &c), "");
        let plus_5_30 = |_| 19_800;
        let c = Clock { now: 0, offset: &plus_5_30 };
        assert_eq!(format_time(1_700_000_000, &c), "2023-11-15 03:43");
        assert_eq!(format_time(-1, &c), "1970-01-01 05:29");
        for days in [-1_000_000, -1, 0, 1, 19_000, 2_932_896] {
            let (y, m, d) = civil_from_days(days);
            assert_eq!(days_from_civil(y, m, d), days);
        }
    }

    #[test]
    fn preview_rendering_shows_limits_and_warnings() {
        use flint_preview::{ArchiveEntry, Preview};
        let (h, b) = preview_text(&Preview::Text { text: "abc".into(), encoding: "UTF-8".into(), truncated: true });
        assert_eq!((h.as_str(), b.as_str()), ("UTF-8 · showing the beginning only", "abc"));
        let (h, b) = preview_text(&Preview::Hex { offset_bytes: b"MZ\x00\x01ABCDEFGHIJKLMNOP".to_vec(), file_size: 4096 });
        assert!(h.starts_with("binary file · 4 KB"));
        assert!(b.starts_with("00000000  4d 5a 00 01 41 42") && !b.contains("|") && b.contains("MZ..ABCD") && b.contains("first 20 of 4096 bytes"), "{b}");
        let (h, b) = preview_text(&Preview::Archive { format: "ZIP".into(), entries: vec![ArchiveEntry { name: "a.txt".into(), size: 10, compressed: 5, is_dir: false }], total_entries: 3, total_size: 10, warnings: vec!["possible zip bomb".into()] });
        assert!(h.contains("3 entries") && h.contains("nothing extracted"));
        assert!(b.starts_with("Warning: possible zip bomb") && b.contains("a.txt  (10 B)") && b.ends_with("… and 2 more\n"), "{b}");
    }

    #[test]
    fn filter_bar_edits_only_its_own_term() {
        let tz = utc();
        let clock = Clock { now: days_from_civil(2026, 10, 20) * 86_400, offset: &tz };
        let bar = filter_bar(&clock);
        let (ty, size, date) = (&bar[0], &bar[1], &bar[2]);
        let q = set_filter("report \"type:x\" ", ty, "doc");
        assert_eq!(q, "report \"type:x\" type:doc ", "quoted text is not a filter");
        let q = set_filter(&q, size, ">1gb");
        assert_eq!(current_choice(&q, ty), Some(3));
        assert_eq!(current_choice(&q, size), Some(6));
        let q = set_filter("kind:image dm:today q", ty, "");
        assert_eq!(q, "dm:today q");
        assert_eq!(current_choice(&q, date), Some(1), "aliases are recognised");
        assert_eq!(current_choice("modified:2020-01-01", date), None, "typed values have no dropdown entry");
        assert_eq!(current_choice("x", date), Some(0));
        assert_eq!(date.options[6].1, "<2025-10-20");
        // Every dropdown value parses without errors.
        for c in &bar {
            for (_, v) in &c.options {
                let q = flint_fuzzy::query::parse_with(&set_filter("", c, v), &clock);
                assert!(q.errors.is_empty(), "{}:{v}", c.key);
                assert_eq!(q.filters.len(), (!v.is_empty()) as usize);
            }
        }
    }
}
