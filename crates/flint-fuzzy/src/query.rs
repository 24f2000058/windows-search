//! Query parsing: free text, quoted phrases, partial paths and filters.
//!
//! Syntax (case-insensitive keys):
//! * `ext:pdf,docx` — extension list
//! * `type:file|folder|image|video|audio|doc|archive|code|exe`
//! * `size:>10mb`, `size:<1kb`, `size:1mb..1gb`, `size:empty`
//! * `modified:>2024-01-01`, `modified:2024-01-01..2024-06-30`,
//!   `modified:today|yesterday|week|month|year` (alias `dm:`). Dates are
//!   calendar days in the user's local time zone (see [`Clock`]), including
//!   23- and 25-hour days at daylight-saving changes.
//! * `path:text` — folded substring of the containing directory path
//! * `content:text` — requires the content index (reported as unavailable if
//!   the caller has none; never silently ignored)
//! * `"quoted phrase"` — literal token, also used to escape `key:` syntax
//! * text containing `\` or `/` is a partial path: the last segment matches
//!   the name, earlier segments must match ancestor directories in order.

use crate::fold::fold_str;

#[derive(Clone, Debug, PartialEq)]
pub enum Kind {
    File,
    Folder,
    Extensions(&'static [&'static str]),
}

#[derive(Clone, Debug, PartialEq)]
pub enum Filter {
    Ext(Vec<String>),
    Kind(Kind),
    /// Inclusive byte range.
    Size(u64, u64),
    /// Inclusive unix-seconds range (UTC).
    Modified(i64, i64),
    PathContains(String),
    Content(String),
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Query {
    pub raw: String,
    /// Folded free-text tokens (quoted phrases kept intact, without quotes).
    pub tokens: Vec<String>,
    /// Folded directory segments of a partial-path query, outermost first.
    pub dir_segments: Vec<String>,
    pub filters: Vec<Filter>,
    /// Filter terms that could not be parsed, reported back to the user.
    pub errors: Vec<String>,
}

impl Query {
    /// Free text as one folded string (tokens joined by single spaces).
    pub fn text(&self) -> String {
        self.tokens.join(" ")
    }
    pub fn has_text(&self) -> bool {
        !self.tokens.is_empty() || !self.dir_segments.is_empty()
    }
}

pub const IMAGE: &[&str] = &["png", "jpg", "jpeg", "gif", "bmp", "webp", "tif", "tiff", "svg", "ico", "heic", "avif", "raw", "cr2", "nef"];
pub const VIDEO: &[&str] = &["mp4", "mkv", "avi", "mov", "wmv", "webm", "m4v", "mpg", "mpeg", "flv"];
pub const AUDIO: &[&str] = &["mp3", "wav", "flac", "ogg", "m4a", "aac", "wma", "opus", "aiff"];
pub const DOC: &[&str] = &["pdf", "doc", "docx", "odt", "rtf", "txt", "md", "xls", "xlsx", "ods", "csv", "ppt", "pptx", "odp", "epub", "mobi"];
pub const ARCHIVE: &[&str] = &["zip", "7z", "rar", "tar", "gz", "bz2", "xz", "zst", "cab", "iso", "tgz"];
pub const CODE: &[&str] = &["rs", "c", "h", "cpp", "hpp", "cc", "cs", "java", "kt", "py", "js", "ts", "tsx", "jsx", "go", "rb", "php", "swift", "sh", "ps1", "bat", "cmd", "lua", "sql", "json", "toml", "yaml", "yml", "xml", "html", "css"];
pub const EXE: &[&str] = &["exe", "msi", "dll", "sys", "com", "scr", "appx", "msix"];

/// Splits on whitespace while keeping `"quoted phrases"` (and `key:"quoted"`)
/// together. Returns (term, contains_quotes, starts_with_quote).
fn split_terms(s: &str) -> Vec<(String, bool, bool)> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let (mut quoted, mut lead_quote, mut in_q) = (false, false, false);
    for c in s.chars() {
        match c {
            '"' => {
                if !quoted && cur.is_empty() {
                    lead_quote = true;
                }
                in_q = !in_q;
                quoted = true;
            }
            c if c.is_whitespace() && !in_q => {
                if !cur.is_empty() {
                    out.push((std::mem::take(&mut cur), quoted, lead_quote));
                }
                quoted = false;
                lead_quote = false;
            }
            c => cur.push(c),
        }
    }
    if !cur.is_empty() {
        out.push((cur, quoted, lead_quote));
    }
    out
}

pub fn parse_size(s: &str) -> Option<u64> {
    let s = s.trim().to_ascii_lowercase();
    let split = s.find(|c: char| !(c.is_ascii_digit() || c == '.')).unwrap_or(s.len());
    let (num, unit) = s.split_at(split);
    let n: f64 = num.parse().ok()?;
    let mul: f64 = match unit {
        "" | "b" => 1.0,
        "k" | "kb" => 1024.0,
        "m" | "mb" => 1024.0 * 1024.0,
        "g" | "gb" => 1024.0 * 1024.0 * 1024.0,
        "t" | "tb" => 1024.0f64.powi(4),
        _ => return None,
    };
    let v = n * mul;
    (v.is_finite() && v >= 0.0 && v < u64::MAX as f64).then_some(v as u64)
}

/// Days since 1970-01-01 for a proleptic Gregorian date (H. Hinnant).
pub fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (m as i64 + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d as i64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

fn parse_date(s: &str) -> Option<i64> {
    let mut it = s.split('-');
    let y: i64 = it.next()?.parse().ok()?;
    let m: u32 = it.next().map_or(Some(1), |v| v.parse().ok())?;
    let d: u32 = it.next().map_or(Some(1), |v| v.parse().ok())?;
    if it.next().is_some() || !(1..=12).contains(&m) || !(1..=31).contains(&d) || !(1601..=9999).contains(&y) {
        return None;
    }
    Some(days_from_civil(y, m, d) * 86_400)
}

/// Integer neighbours, so strict comparisons become inclusive bounds.
trait Step: Copy {
    fn succ(self) -> Self;
    fn pred(self) -> Self;
}
impl Step for u64 {
    fn succ(self) -> Self {
        self.saturating_add(1)
    }
    fn pred(self) -> Self {
        self.saturating_sub(1)
    }
}
impl Step for i64 {
    fn succ(self) -> Self {
        self.saturating_add(1)
    }
    fn pred(self) -> Self {
        self.saturating_sub(1)
    }
}

/// Parses `>a`, `<a`, `>=a`, `<=a`, `a..b`, or `a` into an inclusive range,
/// with `exact` deciding what a bare value covers (a size is one value, a
/// date a whole day). `>` and `<` are strict.
fn parse_range<T: Step + Ord>(s: &str, min: T, max: T, parse: impl Fn(&str) -> Option<T>, exact: impl Fn(T) -> (T, T)) -> Option<(T, T)> {
    if let Some((a, b)) = s.split_once("..") {
        let lo = if a.is_empty() { min } else { parse(a)? };
        let hi = if b.is_empty() { max } else { exact(parse(b)?).1 };
        return (lo <= hi).then_some((lo, hi));
    }
    for (op, f) in [(">=", 0), ("<=", 1), (">", 2), ("<", 3)] {
        if let Some(v) = s.strip_prefix(op) {
            let v = parse(v)?;
            return Some(match f {
                0 => (exact(v).0, max),
                1 => (min, exact(v).1),
                2 => (exact(v).1.succ(), max),
                _ => (min, exact(v).0.pred()),
            });
        }
    }
    Some(exact(parse(s)?))
}

/// Local time zone as a function from a UTC instant (unix seconds) to the
/// offset in seconds that local wall-clock time had at that instant.
pub type UtcOffset = dyn Fn(i64) -> i64 + Send + Sync;

/// What date filters are interpreted against.
pub struct Clock<'a> {
    /// Unix seconds (UTC).
    pub now: i64,
    pub offset: &'a UtcOffset,
}

impl Clock<'_> {
    /// UTC instant of a local wall-clock time. Wall-clock times skipped by a
    /// DST change map to just after it; repeated ones to the first instance.
    pub fn to_utc(&self, local: i64) -> i64 {
        let guess = local - (self.offset)(local);
        let fixed = local - (self.offset)(guess);
        // The second probe settles every transition; prefer the earlier
        // instant when both are valid (repeated hour).
        if guess != fixed && local - (self.offset)(fixed) == fixed { fixed.min(guess) } else { fixed }
    }
    pub fn to_local(&self, utc: i64) -> i64 {
        utc + (self.offset)(utc)
    }
}

fn utc_offset(_: i64) -> i64 {
    0
}

/// Parses a raw query with dates in UTC. `now` (unix seconds) anchors
/// relative dates. The launcher uses [`parse_with`] and the local zone.
pub fn parse(raw: &str, now: i64) -> Query {
    parse_with(raw, &Clock { now, offset: &utc_offset })
}

/// Converts an inclusive range of local seconds to UTC, keeping open ends.
fn local_range(c: &Clock, (lo, hi): (i64, i64)) -> (i64, i64) {
    let lo = if lo <= i64::MIN + 1 { lo } else { c.to_utc(lo) };
    let hi = if hi == i64::MAX { hi } else { c.to_utc(hi + 1) - 1 };
    (lo, hi)
}

/// Parses a raw query; calendar dates are local days of `clock`.
pub fn parse_with(raw: &str, clock: &Clock) -> Query {
    let now = clock.now;
    let mut q = Query { raw: raw.to_owned(), ..Default::default() };
    for (term, quoted, lead_quote) in split_terms(raw) {
        let lower = term.to_ascii_lowercase();
        let kv = if lead_quote { None } else { lower.split_once(':') };
        let handled = match kv {
            Some((k, v)) if !v.is_empty() => {
                let value_orig = &term[k.len() + 1..];
                match k {
                    "ext" => {
                        q.filters.push(Filter::Ext(v.split([',', ';']).map(|e| e.trim_start_matches('.').to_owned()).filter(|e| !e.is_empty()).collect()));
                        true
                    }
                    "type" | "kind" => {
                        let kind = match v {
                            "file" | "files" => Some(Kind::File),
                            "folder" | "dir" | "directory" | "folders" => Some(Kind::Folder),
                            "image" | "picture" | "photo" => Some(Kind::Extensions(IMAGE)),
                            "video" | "movie" => Some(Kind::Extensions(VIDEO)),
                            "audio" | "music" => Some(Kind::Extensions(AUDIO)),
                            "doc" | "document" => Some(Kind::Extensions(DOC)),
                            "archive" | "zip" => Some(Kind::Extensions(ARCHIVE)),
                            "code" | "source" => Some(Kind::Extensions(CODE)),
                            "exe" | "program" | "executable" => Some(Kind::Extensions(EXE)),
                            _ => None,
                        };
                        match kind {
                            Some(k) => q.filters.push(Filter::Kind(k)),
                            None => q.errors.push(term.clone()),
                        }
                        true
                    }
                    "size" => {
                        let r = if v == "empty" {
                            Some((0, 0))
                        } else {
                            parse_range(v, 0, u64::MAX, parse_size, |x| (x, x))
                        };
                        match r {
                            Some((a, b)) => q.filters.push(Filter::Size(a, b)),
                            None => q.errors.push(term.clone()),
                        }
                        true
                    }
                    "modified" | "dm" | "date" => {
                        let day = 86_400;
                        // Local midnight today, in local seconds.
                        let today = clock.to_local(now).div_euclid(day) * day;
                        let r = match v {
                            "today" => Some(local_range(clock, (today, i64::MAX))),
                            "yesterday" => Some(local_range(clock, (today - day, today - 1))),
                            "week" => Some((now - 7 * day, i64::MAX)),
                            "month" => Some((now - 30 * day, i64::MAX)),
                            "year" => Some((now - 365 * day, i64::MAX)),
                            _ => parse_range(v, i64::MIN + 1, i64::MAX, parse_date, |d| (d, d + day - 1)).map(|r| local_range(clock, r)),
                        };
                        match r {
                            Some((a, b)) => q.filters.push(Filter::Modified(a, b)),
                            None => q.errors.push(term.clone()),
                        }
                        true
                    }
                    "path" | "in" => {
                        q.filters.push(Filter::PathContains(fold_str(value_orig.trim_matches('"'))));
                        true
                    }
                    "content" => {
                        q.filters.push(Filter::Content(value_orig.trim_matches('"').to_owned()));
                        true
                    }
                    _ => false,
                }
            }
            _ => false,
        };
        if handled {
            continue;
        }
        let folded = fold_str(&term);
        if !quoted && (folded.contains('\\') || folded.contains('/')) {
            let mut segs: Vec<String> = folded.split(['\\', '/']).map(str::to_owned).collect();
            let last = segs.pop().unwrap_or_default();
            q.dir_segments.extend(segs.into_iter().filter(|s| !s.is_empty()));
            if !last.is_empty() {
                q.tokens.push(last);
            }
        } else {
            q.tokens.push(folded);
        }
    }
    q
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_filters_and_text() {
        let q = parse(r#"Annual "Q3 Report" ext:pdf,DOCX size:>1mb type:doc"#, 0);
        assert_eq!(q.tokens, vec!["annual", "q3 report"]);
        assert_eq!(q.filters[0], Filter::Ext(vec!["pdf".into(), "docx".into()]));
        assert_eq!(q.filters[1], Filter::Size(1_048_577, u64::MAX), "> is strict");
        assert_eq!(parse("size:>=1kb size:<1kb size:<=1kb", 0).filters, vec![Filter::Size(1024, u64::MAX), Filter::Size(0, 1023), Filter::Size(0, 1024)]);
        assert!(matches!(q.filters[2], Filter::Kind(Kind::Extensions(_))));
        assert!(q.errors.is_empty());
    }

    #[test]
    fn quoted_key_value_is_literal_text() {
        let q = parse(r#""ext:pdf""#, 0);
        assert_eq!(q.tokens, vec!["ext:pdf"]);
        assert!(q.filters.is_empty());
    }

    #[test]
    fn partial_paths_split_into_segments() {
        let q = parse(r"proj\src/main", 0);
        assert_eq!(q.dir_segments, vec!["proj", "src"]);
        assert_eq!(q.tokens, vec!["main"]);
        let q = parse("C:/Users/", 0);
        assert_eq!(q.dir_segments, vec!["c:", "users"]);
        assert!(q.tokens.is_empty());
    }

    #[test]
    fn date_ranges() {
        let q = parse("modified:2024-01-01..2024-01-31", 0);
        let jan1 = days_from_civil(2024, 1, 1) * 86_400;
        assert_eq!(q.filters[0], Filter::Modified(jan1, jan1 + 31 * 86_400 - 1));
        assert_eq!(days_from_civil(1970, 1, 1), 0);
        let q = parse("dm:2024-13-01 size:huge type:nope", 0);
        assert_eq!(q.errors.len(), 3);
    }

    #[test]
    fn dates_are_local_days_including_dst_changes() {
        let day = 86_400;
        // A zone at UTC+1 that moves to UTC+2 at 2024-03-31 01:00 UTC (EU rule).
        let switch = days_from_civil(2024, 3, 31) * day + 3600;
        let eu = move |t: i64| if t >= switch { 7200 } else { 3600 };
        let clock = Clock { now: days_from_civil(2024, 3, 31) * day + 12 * 3600, offset: &eu };
        let q = parse_with("modified:2024-03-31", &clock);
        let midnight_utc = days_from_civil(2024, 3, 31) * day - 3600; // 00:00 at +1
        let next_midnight_utc = days_from_civil(2024, 4, 1) * day - 7200; // 00:00 at +2
        assert_eq!(q.filters[0], Filter::Modified(midnight_utc, next_midnight_utc - 1), "a 23-hour local day");
        let q = parse_with("modified:today", &clock);
        assert_eq!(q.filters[0], Filter::Modified(midnight_utc, i64::MAX));
        let q = parse_with("modified:yesterday", &clock);
        assert_eq!(q.filters[0], Filter::Modified(days_from_civil(2024, 3, 30) * day - 3600, midnight_utc - 1));
        let q = parse_with("modified:<2024-01-01", &clock);
        assert_eq!(q.filters[0], Filter::Modified(i64::MIN + 1, days_from_civil(2024, 1, 1) * day - 3600 - 1));
        // Round trips away from the transition.
        for t in [0, switch - 7200, switch + 7200, 1_800_000_000] {
            assert_eq!(clock.to_utc(clock.to_local(t)), t);
        }
        // UTC parse is unchanged.
        assert_eq!(parse("modified:2024-03-31", 0).filters[0], Filter::Modified(days_from_civil(2024, 3, 31) * day, days_from_civil(2024, 4, 1) * day - 1));
    }

    #[test]
    fn unknown_keys_are_text() {
        let q = parse("C:notes http://x", 0);
        assert_eq!(q.tokens, vec!["c:notes", "x"]);
        assert_eq!(q.dir_segments, vec!["http:"]);
        let q = parse(r#"path:"My Docs" "a/b""#, 0);
        assert_eq!(q.filters, vec![Filter::PathContains("my docs".into())]);
        assert_eq!(q.tokens, vec!["a/b"]);
    }
}
