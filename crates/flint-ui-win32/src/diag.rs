//! Diagnostics report: what the launcher is doing and why, as plain text
//! the user can read, copy or save.
//!
//! The report never contains search text, result names or file contents.
//! Paths (volume problems, unreadable folders, errors) are **redacted by
//! default** (`privacy.redact_diagnostics`). The profile folder becomes
//! `%USERPROFILE%`, the user and computer names become `<user>` and
//! `<computer>`, and every other path component becomes a short hash. The
//! hash is keyed per report, so equal paths match within a report but
//! cannot be looked up across reports. Redaction of paths inside free-form
//! error text is best effort, which is why the window shows exactly what
//! would be exported.

use std::fmt::Write as _;

/// Components that identify nothing about the user and stay readable.
const PLAIN: &[&str] = &["windows", "system32", "program files", "program files (x86)", "programdata", "users", "appdata", "local", "locallow", "roaming", "documents", "desktop", "downloads", "pictures", "music", "videos", "onedrive", "flint", "temp", "$recycle.bin", "system volume information"];

pub struct Redactor {
    enabled: bool,
    salt: u64,
    profile: Option<String>,
    words: Vec<(String, &'static str)>,
}

impl Redactor {
    pub fn new(enabled: bool, salt: u64, profile: Option<&str>, user: Option<&str>, computer: Option<&str>) -> Self {
        let mut words = Vec::new();
        for (w, tag) in [(user, "<user>"), (computer, "<computer>")] {
            if let Some(w) = w.filter(|w| w.chars().count() >= 2) {
                words.push((w.to_lowercase(), tag));
            }
        }
        // Longer first, so "alice-pc" is not half-replaced by "alice".
        words.sort_by_key(|(w, _): &(String, &str)| std::cmp::Reverse(w.len()));
        Redactor { enabled, salt, profile: profile.filter(|p| p.len() >= 3).map(|p| p.trim_end_matches('\\').to_lowercase()), words }
    }

    pub fn enabled(&self) -> bool {
        self.enabled
    }

    fn hash(&self, s: &str) -> String {
        // FNV-1a, keyed: stable within one report only.
        let mut h = 0xcbf2_9ce4_8422_2325u64 ^ self.salt;
        for b in s.to_lowercase().bytes() {
            h ^= b as u64;
            h = h.wrapping_mul(0x100_0000_01b3);
        }
        format!("#{:06x}", h & 0xFF_FFFF)
    }

    fn component(&self, c: &str) -> String {
        let lower = c.to_lowercase();
        if c.is_empty() || PLAIN.contains(&lower.as_str()) {
            return c.to_owned();
        }
        if let Some((_, tag)) = self.words.iter().find(|(w, _)| *w == lower) {
            return (*tag).to_owned();
        }
        // Keep a short extension: it says what kind of file, not which.
        match c.rsplit_once('.') {
            Some((stem, ext)) if !stem.is_empty() && (1..=5).contains(&ext.len()) && ext.chars().all(|ch| ch.is_ascii_alphanumeric()) => format!("{}.{ext}", self.hash(stem)),
            _ => self.hash(c),
        }
    }

    /// A path known to be a path.
    pub fn path(&self, p: &str) -> String {
        if !self.enabled {
            return p.to_owned();
        }
        let lower = p.to_lowercase();
        let (head, rest) = match &self.profile {
            Some(pr) if lower == *pr || lower.starts_with(&format!("{pr}\\")) => ("%USERPROFILE%".to_owned(), &p[pr.len()..]),
            _ if p.starts_with("\\\\") => {
                // \\server\share stays hashed as a unit with the server.
                let mut it = p[2..].splitn(3, '\\');
                let (server, share) = (it.next().unwrap_or(""), it.next().unwrap_or(""));
                let consumed = 2 + server.len() + if share.is_empty() { 0 } else { 1 + share.len() };
                (format!("\\\\{}\\{}", self.component(server), self.component(share)), &p[consumed.min(p.len())..])
            }
            _ if p.len() >= 2 && p.as_bytes()[1] == b':' => (p[..2].to_owned(), &p[2..]),
            _ => (String::new(), p),
        };
        let mut out = head;
        for (i, c) in rest.split('\\').enumerate() {
            if i > 0 {
                out.push('\\');
            }
            out.push_str(&self.component(c));
        }
        out
    }

    /// Free-form text (error messages): names replaced, embedded absolute
    /// paths redacted up to the next quote, `: `, ` (`, `|` or line end.
    pub fn text(&self, s: &str) -> String {
        if !self.enabled {
            return s.to_owned();
        }
        let mut out = String::new();
        let mut rest = s;
        while let Some(i) = find_path_start(rest) {
            out.push_str(&self.words_in(&rest[..i]));
            let tail = &rest[i..];
            let end = path_end(tail);
            out.push_str(&self.path(&tail[..end]));
            rest = &tail[end..];
        }
        out.push_str(&self.words_in(rest));
        out
    }

    fn words_in(&self, s: &str) -> String {
        let mut out = s.to_owned();
        for (w, tag) in &self.words {
            out = replace_word_ci(&out, w, tag);
        }
        out
    }
}

fn find_path_start(s: &str) -> Option<usize> {
    let b = s.as_bytes();
    (0..b.len()).find(|&i| {
        let boundary = i == 0 || !b[i - 1].is_ascii_alphanumeric();
        boundary && ((i + 2 < b.len() && b[i].is_ascii_alphabetic() && b[i + 1] == b':' && b[i + 2] == b'\\') || (i + 2 < b.len() && b[i] == b'\\' && b[i + 1] == b'\\' && b[i + 2] != b'\\'))
    })
}

fn path_end(s: &str) -> usize {
    let mut end = s.len();
    for pat in ["\"", "'", ": ", " (", "|", "\n", "\r", "; ", ", "] {
        if let Some(i) = s[2..].find(pat) {
            end = end.min(i + 2);
        }
    }
    end
}

fn replace_word_ci(s: &str, word: &str, tag: &str) -> String {
    let lower = s.to_lowercase();
    if lower.len() != s.len() {
        return s.to_owned(); // case folding changed byte offsets; leave as is
    }
    let (b, mut out, mut last) = (lower.as_bytes(), String::new(), 0);
    let mut i = 0;
    while let Some(j) = lower[i..].find(word) {
        let st = i + j;
        let en = st + word.len();
        let left = st == 0 || !b[st - 1].is_ascii_alphanumeric();
        let right = en >= b.len() || !b[en].is_ascii_alphanumeric();
        if left && right {
            out.push_str(&s[last..st]);
            out.push_str(tag);
            last = en;
        }
        i = en;
    }
    out.push_str(&s[last..]);
    out
}

/// (samples, p50, p95, max) in milliseconds.
pub type LatencySummary = (usize, f64, f64, f64);

/// Everything the report shows, gathered by the UI (no I/O here).
#[derive(Clone, Debug, Default)]
pub struct Snapshot {
    pub version: String,
    pub os: String,
    pub portable: bool,
    pub data_dir: String,
    pub uptime_secs: u64,
    pub private_bytes: Option<u64>,
    pub working_set: Option<u64>,
    pub entries: usize,
    pub table_bytes: usize,
    pub table_slack: usize,
    pub governor: Option<flint_index::governor::GovernorStats>,
    pub volumes: Vec<flint_index::catalog::VolumeStatus>,
    pub coverage: Option<flint_index::catalog::Coverage>,
    pub latency: Vec<(String, Option<LatencySummary>)>,
    pub last_search: Option<flint_fuzzy::engine::Status>,
    pub content: Option<Result<flint_index::content::ContentStats, String>>,
    pub viewer: Option<String>,
    pub plugins: Vec<PluginLine>,
    pub plugins_enabled: bool,
    pub update: String,
    /// Setting notes and startup problems.
    pub notices: Vec<String>,
    pub errors: Vec<String>,
}

#[derive(Clone, Debug, Default)]
pub struct PluginLine {
    pub label: String,
    pub folder: String,
    pub standing: String,
    pub isolation: String,
    pub stats: Option<flint_plugin::host::Stats>,
    pub disabled: Option<String>,
}

fn mb(b: u64) -> String {
    format!("{:.1} MB", b as f64 / 1e6)
}

pub fn render(s: &Snapshot, r: &Redactor) -> String {
    let mut o = String::new();
    let _ = writeln!(o, "Flint diagnostics — {}", if r.enabled() { "paths and names redacted" } else { "NOT REDACTED: contains your paths and user name" });
    let _ = writeln!(o, "No search text, result names or file contents are included.\n");
    let _ = writeln!(o, "version: {}", s.version);
    let _ = writeln!(o, "os: {}", s.os);
    let _ = writeln!(o, "mode: {}; data: {}", if s.portable { "portable" } else { "installed" }, r.path(&s.data_dir));
    let _ = writeln!(o, "uptime: {} min", s.uptime_secs / 60);

    let _ = writeln!(o, "\n[resources]");
    if let (Some(p), Some(w)) = (s.private_bytes, s.working_set) {
        let _ = writeln!(o, "private bytes: {}, working set: {}", mb(p), mb(w));
    }
    let per = if s.entries > 0 { s.table_bytes as f64 / s.entries as f64 } else { 0.0 };
    let _ = writeln!(o, "index entries: {}, name table: {} ({per:.0} B/entry, {} reserved)", s.entries, mb(s.table_bytes as u64), mb(s.table_slack as u64));
    if let Some(g) = &s.governor {
        let _ = writeln!(o, "scans running: {}, queued: {}, paused for typing: {:.1} s", g.scans_running, g.scans_queued, g.paused.as_secs_f64());
    }

    let _ = writeln!(o, "\n[volumes]");
    if let Some(c) = &s.coverage {
        let _ = writeln!(o, "coverage: {}{}", if c.complete { "complete" } else { "incomplete" }, if c.problems.is_empty() { String::new() } else { format!(" — {}", r.text(&c.problems.join("; "))) });
    }
    if s.volumes.is_empty() {
        let _ = writeln!(o, "(no volumes)");
    }
    for v in &s.volumes {
        let _ = writeln!(o, "{}: {:?}, state {}, loaded via {}", r.path(&v.root_name), v.phase, r.text(&format!("{:?}", v.state)), v.load_source.as_deref().map_or("-".into(), |l| r.text(l)));
        let st = &v.stats;
        let _ = writeln!(
            o,
            "    progress {}, stale metadata {}, queued {}, snapshots {}, rescans {} (last: {}), records {}, batches {}, inconsistencies {}, follow-up failures {}",
            v.progress,
            v.stale_meta,
            v.queued,
            v.snapshots_written,
            st.rescans,
            st.last_rescan_reason.as_ref().map_or("-".into(), |x| format!("{x:?}")),
            st.records_applied,
            st.batches_committed,
            st.inconsistencies,
            st.followup_failures
        );
        if let Some(e) = &st.last_error {
            let _ = writeln!(o, "    last error: {}", r.text(e));
        }
        if !v.unreadable.is_empty() {
            let shown: Vec<String> = v.unreadable.iter().take(5).map(|p| r.path(&p.to_string_lossy())).collect();
            let _ = writeln!(o, "    unreadable folders: {} (first: {})", v.unreadable.len(), shown.join(", "));
        }
    }

    let _ = writeln!(o, "\n[search]");
    for (label, st) in &s.latency {
        match st {
            Some((n, p50, p95, max)) => {
                let _ = writeln!(o, "{label}: n={n} p50={p50:.2} ms p95={p95:.2} ms max={max:.2} ms");
            }
            None => {
                let _ = writeln!(o, "{label}: no samples yet");
            }
        }
    }
    if let Some(st) = &s.last_search {
        let stages: Vec<String> = st.stages.iter().map(|x| format!("{} {} in {:.2} ms{}", x.stage.label(), x.matched, x.micros as f64 / 1000.0, if x.complete { "" } else { " (cut short)" })).collect();
        let _ = writeln!(o, "last search: {:?}, {} matched of {} considered, {:.2} ms; stages: {}", st.stop, st.total_matched, st.considered, st.elapsed_micros as f64 / 1000.0, stages.join(" → "));
    }

    let _ = writeln!(o, "\n[content index]");
    match &s.content {
        None => {
            let _ = writeln!(o, "off (privacy.content_indexing)");
        }
        Some(Ok(c)) => {
            let states: Vec<String> = c.by_state.iter().map(|(k, n)| format!("{k} {n}")).collect();
            let _ = writeln!(o, "files {}, pending {}, last sync {}; {}", c.files, c.pending, if c.last_sync_complete { "complete" } else { "partial" }, states.join(", "));
        }
        Some(Err(e)) => {
            let _ = writeln!(o, "unavailable: {}", r.text(e));
        }
    }
    let _ = writeln!(o, "\n[viewer]\n{}", s.viewer.as_deref().unwrap_or("off"));

    let _ = writeln!(o, "\n[plugins]");
    let _ = writeln!(o, "plugins {}", if s.plugins_enabled { "enabled" } else { "disabled (plugins.enabled)" });
    for p in &s.plugins {
        let _ = writeln!(o, "{} in {}: {}", p.label, r.path(&p.folder), r.text(&p.standing));
        if let Some(st) = &p.stats {
            let _ = writeln!(
                o,
                "    starts {}, requests {}, timeouts {}, crashes {}, protocol errors {}, denied actions {}, last latency {}",
                st.starts,
                st.requests,
                st.timeouts,
                st.crashes,
                st.protocol_errors,
                st.denied,
                st.last_latency.map_or("-".into(), |d| format!("{:.1} ms", d.as_secs_f64() * 1000.0))
            );
            if let Some(e) = &st.last_error {
                let _ = writeln!(o, "    last error: {}", r.text(e));
            }
        }
        let _ = writeln!(o, "    isolation: {}", p.isolation);
        if let Some(d) = &p.disabled {
            let _ = writeln!(o, "    disabled: {}", r.text(d));
        }
    }

    let _ = writeln!(o, "\n[updates]\n{}", r.text(&s.update));
    if !s.notices.is_empty() || !s.errors.is_empty() {
        let _ = writeln!(o, "\n[notices and errors]");
        for n in s.notices.iter().chain(&s.errors) {
            let _ = writeln!(o, "{}", r.text(n));
        }
    }
    o
}

#[cfg(test)]
mod tests {
    use super::*;

    fn red() -> Redactor {
        Redactor::new(true, 42, Some("C:\\Users\\Alice"), Some("alice"), Some("ALICE-PC"))
    }

    #[test]
    fn paths_keep_their_shape_but_not_their_names() {
        let r = red();
        assert_eq!(r.path("C:\\Users\\Alice\\Documents\\Taxes 2024\\return.pdf").split('\\').collect::<Vec<_>>()[..2], ["%USERPROFILE%", "Documents"]);
        let p = r.path("C:\\Users\\Alice\\Documents\\Taxes 2024\\return.pdf");
        assert!(!p.contains("Taxes") && !p.contains("return") && p.ends_with(".pdf"), "{p}");
        assert_eq!(r.path("D:\\"), "D:\\");
        assert!(r.path("D:\\Projects\\secret").starts_with("D:\\#"));
        let unc = r.path("\\\\fileserver\\share\\hr\\salaries.xlsx");
        assert!(unc.starts_with("\\\\#") && !unc.contains("fileserver") && !unc.contains("salaries"), "{unc}");
        // Same path, same hash, within one report; a different salt differs.
        assert_eq!(r.path("E:\\a\\b"), r.path("E:\\A\\B"));
        assert_ne!(r.path("E:\\a\\b"), Redactor::new(true, 7, None, None, None).path("E:\\a\\b"));
        assert_eq!(Redactor::new(false, 0, None, None, None).path("C:\\x\\y"), "C:\\x\\y");
    }

    #[test]
    fn free_text_is_scrubbed() {
        let r = red();
        let t = r.text("cannot open C:\\Users\\Alice\\Secret Plans\\q3.docx: Access is denied (os error 5); user alice on ALICE-PC; aliceberg stays");
        assert!(!t.contains("Secret") && !t.contains("q3") && t.contains("%USERPROFILE%") && t.contains(": Access is denied"), "{t}");
        assert!(t.contains("user <user> on <computer>") && t.contains("aliceberg"), "{t}");
        let q = r.text("watching \"\\\\nas\\media\\Films\" failed");
        assert!(!q.contains("nas") && !q.contains("Films") && q.ends_with("\" failed"), "{q}");
    }

    #[test]
    fn report_states_redaction_and_holds_no_query() {
        let mut s = Snapshot { version: "0.1.0".into(), data_dir: "C:\\Users\\Alice\\AppData\\Local\\Flint".into(), ..Default::default() };
        s.errors.push("snapshot write failed for C:\\Users\\Alice\\private\\x.bin".into());
        let out = render(&s, &red());
        assert!(out.starts_with("Flint diagnostics — paths and names redacted"));
        assert!(out.contains("%USERPROFILE%\\AppData\\Local\\Flint") && !out.contains("private") && !out.contains("Alice"), "{out}");
        let raw = render(&s, &Redactor::new(false, 0, None, None, None));
        assert!(raw.contains("NOT REDACTED") && raw.contains("Alice"));
    }
}
