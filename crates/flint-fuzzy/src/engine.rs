//! Staged, progressive, cancellable search over a [`NameTable`].
//!
//! Stages run strongest-first. Each stage scans only slots not already
//! matched by a stronger stage, in parallel chunks, each with a bounded top-K
//! heap. After every stage the caller receives a ranked snapshot, so strong
//! results render immediately while weaker stages continue.
//!
//! | Stage        | Tiers | Matches                                              |
//! |--------------|-------|------------------------------------------------------|
//! | Exact        | 9, 8  | whole name / name without extension                  |
//! | Substring    | 7..5  | prefix, word-start substring, substring, all tokens  |
//! | Fuzzy        | 4     | OSA edit distance (subst/ins/del/transposition)      |
//! | Subsequence  | 3     | in-order scattered characters within the name        |
//! | Weak         | 2, 1  | tokens/scattered chars across the path; LCS ≥ 50%    |
//! | Exhaustive   | 0     | every remaining indexed candidate (opt-in)           |
//!
//! "Exhausted" always refers to the *indexed* candidate set; whether the index
//! covers the whole filesystem is reported separately by the indexer.

use crate::fold::char_bit;
use crate::kernels::*;
use crate::query::{Filter, Kind, Query};
use crate::table::{flags, DirIndex, EntryId, NameTable, NOT_DIR, NO_PARENT, NO_TIME};
use memchr::memmem;
use rayon::prelude::*;
use std::cmp::Reverse;
use std::collections::BinaryHeap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Stage {
    Exact,
    Substring,
    Fuzzy,
    Subsequence,
    Weak,
    Exhaustive,
}

impl Stage {
    pub const ALL: [Stage; 6] = [Stage::Exact, Stage::Substring, Stage::Fuzzy, Stage::Subsequence, Stage::Weak, Stage::Exhaustive];
    pub fn label(self) -> &'static str {
        match self {
            Stage::Exact => "exact",
            Stage::Substring => "prefix/substring/token",
            Stage::Fuzzy => "fuzzy (edit distance)",
            Stage::Subsequence => "scattered characters",
            Stage::Weak => "weak path/similarity",
            Stage::Exhaustive => "remaining indexed candidates",
        }
    }
}

pub mod tier {
    pub const EXACT_NAME: u8 = 9;
    pub const EXACT_STEM: u8 = 8;
    pub const PREFIX: u8 = 7;
    pub const WORD: u8 = 6;
    pub const SUBSTRING: u8 = 5;
    pub const FUZZY: u8 = 4;
    pub const SUBSEQUENCE: u8 = 3;
    pub const PATH: u8 = 2;
    pub const SIMILAR: u8 = 1;
    pub const REMAINING: u8 = 0;
}

/// How many edits the fuzzy stage tolerates.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Intensity {
    Off,
    Low,
    Normal,
    Aggressive,
}

impl Intensity {
    pub fn max_edits(self, len: usize) -> u32 {
        let k = match self {
            Intensity::Off => 0,
            // Short queries get no edits: one edit in three characters matches
            // noise and would outrank genuine scattered-character matches.
            Intensity::Low => (len >= 6) as usize + (len >= 12) as usize,
            Intensity::Normal => (len / 4).min(3),
            Intensity::Aggressive => (len / 3).min(4),
        };
        k as u32
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Options {
    /// Number of ranked hits kept (bounded top-K).
    pub limit: usize,
    pub intensity: Intensity,
    /// Weaker stages run only while fewer than this many slots have matched.
    pub enough: usize,
    /// Run the weak path/similarity stage when still short of `enough`.
    pub best_effort: bool,
    /// Run the exhaustive stage (all remaining indexed candidates).
    pub exhaustive: bool,
    /// Force all stages up to and including this one regardless of `enough`
    /// (the "show weaker matches" action).
    pub through: Option<Stage>,
    pub deadline: Option<Duration>,
}

impl Default for Options {
    fn default() -> Self {
        Options {
            limit: 500,
            intensity: Intensity::Normal,
            enough: 50,
            best_effort: true,
            exhaustive: false,
            through: None,
            deadline: None,
        }
    }
}

#[derive(Clone, Default, Debug)]
pub struct CancelToken(Arc<AtomicBool>);

impl CancelToken {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn cancel(&self) {
        self.0.store(true, Ordering::Relaxed);
    }
    pub fn is_canceled(&self) -> bool {
        self.0.load(Ordering::Relaxed)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Hit {
    pub id: EntryId,
    pub tier: u8,
    pub score: i32,
    pub stage: Stage,
    name_len: u16,
}

impl Hit {
    fn key(&self) -> (u8, i32, Reverse<u16>, Reverse<EntryId>) {
        (self.tier, self.score, Reverse(self.name_len), Reverse(self.id))
    }
}
impl Ord for Hit {
    fn cmp(&self, o: &Self) -> std::cmp::Ordering {
        self.key().cmp(&o.key())
    }
}
impl PartialOrd for Hit {
    fn partial_cmp(&self, o: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(o))
    }
}

struct TopK {
    k: usize,
    heap: BinaryHeap<Reverse<Hit>>,
}

impl TopK {
    fn new(k: usize) -> Self {
        TopK { k, heap: BinaryHeap::with_capacity(k.min(4096) + 1) }
    }
    #[inline]
    fn push(&mut self, h: Hit) {
        if self.heap.len() < self.k {
            self.heap.push(Reverse(h));
        } else if self.k > 0 && h > self.heap.peek().unwrap().0 {
            self.heap.pop();
            self.heap.push(Reverse(h));
        }
    }
    /// Lowest tier that can still enter the heap (0 until it is full).
    #[inline]
    fn floor_tier(&self) -> u8 {
        if self.heap.len() < self.k { 0 } else { self.heap.peek().map_or(0, |r| r.0.tier) }
    }
    fn sorted(&self) -> Vec<Hit> {
        let mut v: Vec<Hit> = self.heap.iter().map(|r| r.0).collect();
        v.sort_unstable_by(|a, b| b.cmp(a));
        v
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct StageReport {
    /// Weakest stage covered by the scan pass this report describes.
    pub stage: Stage,
    pub matched: usize,
    pub micros: u64,
    /// False if the stage was cut short by cancellation or the deadline.
    pub complete: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Stop {
    /// Enough matches; weaker stages were not run (`next` is available).
    Enough { next: Stage },
    /// All enabled stages ran to completion.
    StagesDone,
    Canceled,
    Deadline,
    /// No free text and no filters.
    EmptyQuery,
    /// A filter requires a capability the caller does not have.
    Unsupported,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Status {
    pub stages: Vec<StageReport>,
    pub stop: Stop,
    /// Slots matched by any stage that ran (may exceed `hits.len()`).
    pub total_matched: usize,
    /// Live slots that passed the filters.
    pub considered: usize,
    /// Every considered indexed candidate was examined by the exhaustive
    /// stage. Says nothing about unindexed parts of the filesystem.
    pub indexed_exhausted: bool,
    pub unsupported_filters: Vec<String>,
    pub query_errors: Vec<String>,
    pub truncated_query: bool,
    pub elapsed_micros: u64,
    pub generation: u64,
}

#[derive(Clone, Debug)]
pub struct SearchResult {
    pub hits: Vec<Hit>,
    pub status: Status,
}

struct Tok {
    s: String,
    chars: Vec<char>,
    pat: Pattern,
    mask: u64,
    k: u32,
    /// Precompiled SIMD substring searcher (building one per candidate
    /// dominated the profile).
    finder: memmem::Finder<'static>,
    /// Pigeonhole needles: a fuzzy match must contain one (see
    /// [`piece_needles`]). Empty when the filter would be too weak.
    pieces: Vec<(memmem::Finder<'static>, u64)>,
}

impl Tok {
    fn new(s: &str, intensity: Intensity) -> Self {
        let chars: Vec<char> = s.chars().collect();
        let k = intensity.max_edits(chars.len());
        Tok {
            s: s.to_owned(),
            mask: chars.iter().fold(0, |m, &c| m | char_bit(c)),
            pat: Pattern::new(s),
            chars,
            pieces: piece_needles(&s.chars().collect::<Vec<_>>(), k as usize, 2)
                .unwrap_or_default()
                .into_iter()
                .map(|n| (memmem::Finder::new(n.as_bytes()).into_owned(), crate::fold::mask_of(&n)))
                .collect(),
            k,
            finder: memmem::Finder::new(s.as_bytes()).into_owned(),
        }
    }
    /// Pigeonhole prefilter for the fuzzy stage; `true` means "may match".
    #[inline]
    fn may_fuzzy_match(&self, hay: &str, mask: u64) -> bool {
        self.pieces.is_empty() || self.pieces.iter().any(|(f, m)| m & !mask == 0 && f.find(hay.as_bytes()).is_some())
    }
    /// Byte offset of the first occurrence in `hay`.
    #[inline]
    fn find(&self, hay: &str) -> Option<usize> {
        self.finder.find(hay.as_bytes())
    }
    #[inline]
    fn found_in(&self, hay: &str) -> bool {
        self.finder.find(hay.as_bytes()).is_some()
    }
}

struct Plan<'a> {
    full: String,
    full_finder: memmem::Finder<'static>,
    toks: Vec<Tok>,
    toks_mask: u64,
    /// Free text without spaces, for scattered-character stages.
    sub: Tok,
    /// Distinct mask bits of `sub` with how many query chars map to each.
    sub_bit_counts: Vec<(u64, u8)>,
    dirs: Vec<Tok>,
    dirs_mask: u64,
    filters: &'a [Filter],
    path_filter_mask: u64,
    intensity: Intensity,
}

impl<'a> Plan<'a> {
    /// Mask-only rejection, inlined into the scan loop so most slots never
    /// pay for an `eval` call. Must be conservative: it may only reject slots
    /// that `eval` would reject for the same pass.
    #[inline(always)]
    fn rejects(&self, pass: Pass, mask: u64, chain: u64) -> bool {
        if self.toks.is_empty() {
            return false;
        }
        let m = self.sub.chars.len();
        match pass {
            Pass::Strong => mask & self.toks_mask != self.toks_mask,
            Pass::Approx => {
                let fuzzy = self.intensity != Intensity::Off && self.toks.iter().all(|tk| (tk.mask & !mask).count_ones() <= tk.k);
                let subseq = m >= 2 && mask & self.sub.mask == self.sub.mask;
                !fuzzy && !subseq
            }
            Pass::Weak => {
                let path_tokens = chain & self.toks_mask == self.toks_mask;
                let path_sub = m >= 2 && chain & self.sub.mask == self.sub.mask;
                let present: usize = self.sub_bit_counts.iter().filter(|(b, _)| mask & b != 0).map(|(_, c)| *c as usize).sum();
                !path_tokens && !path_sub && (m < 2 || present < m.div_ceil(2).max(1))
            }
            Pass::Exhaustive | Pass::FilterOnly => false,
        }
    }

    fn new(q: &'a Query, intensity: Intensity) -> Self {
        let toks: Vec<Tok> = q.tokens.iter().map(|t| Tok::new(t, intensity)).collect();
        let dirs: Vec<Tok> = q.dir_segments.iter().map(|t| Tok::new(t, intensity)).collect();
        let full = q.text();
        let sub: String = full.chars().filter(|c| !c.is_whitespace()).collect();
        let path_filter_mask = q
            .filters
            .iter()
            .filter_map(|f| if let Filter::PathContains(s) = f { Some(crate::fold::mask_of(s)) } else { None })
            .fold(0, |a, b| a | b);
        let mut sub_bit_counts: Vec<(u64, u8)> = Vec::new();
        for c in sub.chars() {
            let b = char_bit(c);
            match sub_bit_counts.iter_mut().find(|e| e.0 == b) {
                Some(e) => e.1 = e.1.saturating_add(1),
                None => sub_bit_counts.push((b, 1)),
            }
        }
        Plan {
            sub_bit_counts,
            toks_mask: toks.iter().fold(0, |m, t| m | t.mask),
            dirs_mask: dirs.iter().fold(0, |m, t| m | t.mask),
            sub: Tok::new(&sub, intensity),
            full_finder: memmem::Finder::new(full.as_bytes()).into_owned(),
            full,
            toks,
            dirs,
            filters: &q.filters,
            path_filter_mask,
            intensity,
        }
    }
}

#[derive(Default)]
struct Scratch {
    folded: Vec<char>,
    orig: Vec<char>,
}

fn ext_of(folded: &str) -> &str {
    match folded.rfind('.') {
        Some(0) | None => "",
        Some(i) => &folded[i + 1..],
    }
}

fn stem_of(folded: &str) -> &str {
    match folded.rfind('.') {
        Some(0) | None => folded,
        Some(i) => &folded[..i],
    }
}

fn char_index(s: &str, byte: usize) -> usize {
    if s.is_ascii() { byte } else { s[..byte].chars().count() }
}

fn passes_filters(t: &NameTable, chain: Option<&[u64]>, id: EntryId, plan: &Plan) -> bool {
    let fl = t.flags(id);
    let is_dir = fl & flags::DIR != 0;
    for f in plan.filters {
        let ok = match f {
            Filter::Ext(list) => !is_dir && list.iter().any(|e| e == ext_of(t.folded(id))),
            Filter::Kind(Kind::File) => !is_dir,
            Filter::Kind(Kind::Folder) => is_dir,
            Filter::Kind(Kind::Extensions(list)) => !is_dir && list.contains(&ext_of(t.folded(id))),
            Filter::Size(lo, hi) => !is_dir && (*lo..=*hi).contains(&t.meta(id).size),
            Filter::Modified(lo, hi) => {
                let m = t.meta(id).mtime;
                m != NO_TIME && (*lo..=*hi).contains(&m)
            }
            Filter::PathContains(s) => {
                let p = t.parent(id);
                p != NO_PARENT
                    && chain.is_none_or(|c| c[p as usize] & plan.path_filter_mask == plan.path_filter_mask)
                    && t.parent_path_folded(id).contains(s.as_str())
            }
            Filter::Content(_) => false,
        };
        if !ok {
            return false;
        }
    }
    true
}

/// Checks partial-path directory segments against ancestors, innermost
/// segment against the nearest matching ancestor. Returns the tier penalty
/// (0 = every segment is a substring, 1 = some only fuzzy/scattered), or
/// `None` if the ancestors cannot satisfy the segments.
fn dir_check(t: &NameTable, chain: &[u64], id: EntryId, plan: &Plan) -> Option<u8> {
    if plan.dirs.is_empty() {
        return Some(0);
    }
    let p = t.parent(id);
    if p == NO_PARENT {
        return None;
    }
    let c = chain[p as usize];
    let strong_possible = c & plan.dirs_mask == plan.dirs_mask;
    if strong_possible {
        let mut segs = plan.dirs.iter().rev().peekable();
        for a in t.ancestors(id) {
            match segs.peek() {
                Some(s) if s.found_in(t.folded(a)) => {
                    segs.next();
                }
                Some(_) => {}
                None => break,
            }
        }
        if segs.peek().is_none() {
            return Some(0);
        }
    }
    let budget: u32 = plan.dirs.iter().map(|d| d.k).sum();
    if (plan.dirs_mask & !c).count_ones() > budget && !strong_possible {
        return None;
    }
    let mut segs = plan.dirs.iter().rev().peekable();
    let mut buf = Vec::new();
    for a in t.ancestors(id) {
        let Some(s) = segs.peek() else { break };
        buf.clear();
        buf.extend(t.folded(a).chars());
        if subsequence_positions(&s.chars, &buf).is_some() || osa_substring(&s.pat, &buf, s.k).is_some() {
            segs.next();
        }
    }
    segs.peek().is_none().then_some(1)
}

impl Scratch {
    fn load_folded(&mut self, t: &NameTable, id: EntryId) {
        self.folded.clear();
        self.folded.extend(t.folded(id).chars());
    }
    fn load_orig(&mut self, t: &NameTable, id: EntryId) {
        self.orig.clear();
        self.orig.extend(t.name(id).chars());
    }
}

/// Character length of a name and whether a word starts at char `start`,
/// without decoding ASCII names.
fn boundary_at(name: &str, folded: &str, start: usize) -> (usize, bool) {
    let ascii = name.len() == folded.len() && name.is_ascii();
    let len = if ascii { name.len() } else { name.chars().count() };
    if start == 0 {
        return (len, true);
    }
    let (prev, cur) = if ascii {
        (name.as_bytes()[start - 1] as char, name.as_bytes()[start] as char)
    } else {
        let mut it = name.chars().skip(start - 1);
        (it.next().unwrap_or(' '), it.next().unwrap_or(' '))
    };
    (len, is_boundary(&[prev, cur], 1))
}

/// Score of a contiguous match of `n` chars starting at char `start`;
/// mirrors [`score_positions`] on that run without decoding the name.
fn contiguous_score(name: &str, folded: &str, start: usize, n: usize) -> i32 {
    let (len, boundary) = boundary_at(name, folded, start);
    let bonus = match (start, boundary) {
        (0, _) => 12,
        (_, true) => 9,
        _ => 0,
    };
    let n = n as i32;
    16 * n + 6 * (n - 1) + bonus - (start as i32).min(10) - len as i32 / 8
}

/// Exact, prefix, word-start, substring and all-tokens matches (one scan).
/// `floor` is the lowest tier that can still enter the caller's full top-K
/// heap; weaker matches are counted but not scored.
fn eval_strong(t: &NameTable, id: EntryId, plan: &Plan, floor: u8) -> Option<(u8, i32, Stage)> {
    // The mask column is checked before touching name bytes; every strong
    // match (including exact equality) implies the mask superset.
    if t.mask(id) & plan.toks_mask != plan.toks_mask {
        return None;
    }
    let f = t.folded(id);
    if f == plan.full {
        return Some((tier::EXACT_NAME, 1000, Stage::Exact));
    }
    if stem_of(f) == plan.full {
        return Some((tier::EXACT_STEM, 1000, Stage::Exact));
    }
    if let Some(b) = plan.full_finder.find(f.as_bytes()) {
        let start = char_index(f, b);
        let name = t.name(id);
        if start > 0 && floor > tier::WORD {
            return Some((tier::SUBSTRING, i32::MIN, Stage::Substring));
        }
        let tr = match (start, boundary_at(name, f, start).1) {
            (0, _) => tier::PREFIX,
            (_, true) => tier::WORD,
            _ => tier::SUBSTRING,
        };
        if tr < floor {
            return Some((tr, i32::MIN, Stage::Substring));
        }
        return Some((tr, contiguous_score(name, f, start, plan.full.chars().count()), Stage::Substring));
    }
    if plan.toks.len() > 1 && plan.toks.iter().all(|tk| tk.found_in(f)) {
        if floor > tier::SUBSTRING {
            return Some((tier::SUBSTRING, i32::MIN, Stage::Substring));
        }
        let name = t.name(id);
        let s = plan.toks.iter().map(|tk| contiguous_score(name, f, char_index(f, tk.find(f).unwrap()), tk.chars.len())).sum::<i32>();
        return Some((tier::SUBSTRING, s - 20, Stage::Substring));
    }
    None
}

/// Typo-tolerant (OSA) token matches, falling back to scattered characters.
fn eval_approx(t: &NameTable, id: EntryId, plan: &Plan, sc: &mut Scratch) -> Option<(u8, i32, Stage)> {
    let mask = t.mask(id);
    // Cheap mask prefilters before any substring search: each distinct
    // missing character costs at least one edit.
    let fuzzy_possible = plan.intensity != Intensity::Off && plan.toks.iter().all(|tk| (tk.mask & !mask).count_ones() <= tk.k);
    let subseq_possible = plan.sub.chars.len() >= 2 && mask & plan.sub.mask == plan.sub.mask;
    if !fuzzy_possible && !subseq_possible {
        return None;
    }
    let f = t.folded(id);
    if fuzzy_possible {
        let mut total = 0u32;
        let mut first_start = 0usize;
        let mut ok = true;
        for (i, tk) in plan.toks.iter().enumerate() {
            if let Some(b) = tk.find(f) {
                if i == 0 {
                    first_start = char_index(f, b);
                }
                continue;
            }
            // A match needs at least |token| - k characters.
            if tk.k == 0 || f.len() + (tk.k as usize) < tk.chars.len() || !tk.may_fuzzy_match(f, mask) {
                ok = false;
                break;
            }
            match osa_substring_iter(&tk.pat, f.chars(), tk.k) {
                Some((d, end)) => {
                    if i == 0 {
                        first_start = end.saturating_sub(tk.chars.len());
                    }
                    total += d;
                }
                None => {
                    ok = false;
                    break;
                }
            }
        }
        if ok && total > 0 {
            return Some((tier::FUZZY, 1000 - 200 * total as i32 - 3 * first_start as i32 - f.len() as i32 / 2, Stage::Fuzzy));
        }
    }
    if !subseq_possible || !is_subsequence(&plan.sub.chars, f) {
        return None;
    }
    sc.load_folded(t, id);
    let pos = subsequence_positions(&plan.sub.chars, &sc.folded)?;
    sc.load_orig(t, id);
    Some((tier::SUBSEQUENCE, score_positions(&sc.orig, &pos), Stage::Subsequence))
}

/// Per-query facts about every directory, computed once before the weak pass
/// so per-entry path checks are O(1) instead of an ancestor walk.
struct DirInfo {
    index: Arc<DirIndex>,
    /// Per directory ordinal: bit i set if token i occurs in some component
    /// of the directory's path.
    tok_bits: Vec<u64>,
    /// Query characters matched by a greedy leftmost walk from the root
    /// through the directory's path. Greedy leftmost matching is optimal for
    /// subsequence tests, so "query ⊑ path + name" ⇔ "query[pre..] ⊑ name".
    pre: Vec<u8>,
    depth: Vec<u8>,
}

const UNSET: u8 = u8::MAX;

fn greedy(sub: &[char], start: usize, s: &str) -> usize {
    let mut i = start;
    for c in s.chars() {
        if i < sub.len() && sub[i] == c {
            i += 1;
        }
    }
    i
}

fn tok_bits_of(plan: &Plan, s: &str, mask: u64) -> u64 {
    plan.toks.iter().take(64).enumerate().fold(0, |b, (i, tk)| if tk.mask & !mask == 0 && tk.found_in(s) { b | 1 << i } else { b })
}

impl DirInfo {
    fn build(index: Arc<DirIndex>, plan: &Plan) -> Self {
        let nd = index.dirs.len();
        let (mut tok_bits, mut pre, mut depth) = (vec![0u64; nd], vec![0u8; nd], vec![UNSET; nd]);
        let mut stack: Vec<u32> = Vec::new();
        for o in 0..nd as u32 {
            if depth[o as usize] != UNSET {
                continue;
            }
            let mut cur = o;
            while cur != NOT_DIR && depth[cur as usize] == UNSET && stack.len() < crate::table::MAX_DEPTH {
                stack.push(cur);
                cur = index.parent_ord[cur as usize];
            }
            let (mut bits, mut p, mut d) = if cur != NOT_DIR && depth[cur as usize] != UNSET {
                let c = cur as usize;
                (tok_bits[c], pre[c] as usize, depth[c] as usize + 1)
            } else {
                (0, 0, 0)
            };
            while let Some(e) = stack.pop() {
                let f = index.folded(e);
                bits |= tok_bits_of(plan, f, index.mask[e as usize]);
                p = greedy(&plan.sub.chars, p, f);
                let i = e as usize;
                tok_bits[i] = bits;
                pre[i] = p as u8;
                depth[i] = d.min(UNSET as usize - 1) as u8;
                d += 1;
            }
        }
        DirInfo { index, tok_bits, pre, depth }
    }

    /// (token bits, greedy prefix, depth) for a slot's parent directory.
    #[inline]
    fn of_parent(&self, t: &NameTable, id: EntryId) -> (u64, usize, i32) {
        let p = t.parent(id);
        if p == NO_PARENT || p as usize >= self.index.ord.len() {
            return (0, 0, 0);
        }
        match self.index.ord[p as usize] {
            // Parents not flagged as directories (inconsistent data) count as roots.
            NOT_DIR => (0, 0, 0),
            o => {
                let o = o as usize;
                (self.tok_bits[o], self.pre[o] as usize, self.depth[o] as i32 + 1)
            }
        }
    }
}

/// Tokens or scattered characters across the whole path; LCS similarity.
fn eval_weak(t: &NameTable, chain: &[u64], dirs: &DirInfo, id: EntryId, plan: &Plan) -> Option<(u8, i32, Stage)> {
    let mask = t.mask(id);
    let c = chain[id as usize];
    let m = plan.sub.chars.len();
    let need = m.div_ceil(2).max(1);
    let path_tokens_possible = c & plan.toks_mask == plan.toks_mask;
    let path_sub_possible = m >= 2 && c & plan.sub.mask == plan.sub.mask;
    let present = || plan.sub.chars.iter().filter(|&&ch| mask & char_bit(ch) != 0).count();
    if !path_tokens_possible && !path_sub_possible && (m < 2 || present() < need) {
        return None;
    }
    let f = t.folded(id);
    let (pbits, ppre, pdepth) = dirs.of_parent(t, id);
    let ntok = plan.toks.len().min(64);
    let all_bits = if ntok == 64 { !0 } else { (1u64 << ntok) - 1 };
    // Tokens not found along the directory path must be in the name; check
    // their masks before running any substring search.
    let missing = all_bits & !pbits;
    if path_tokens_possible
        && plan.toks.iter().take(64).enumerate().all(|(i, tk)| missing >> i & 1 == 0 || (tk.mask & !mask == 0 && tk.found_in(f)))
    {
        return Some((tier::PATH, 500 - 4 * pdepth - f.len() as i32 / 4, Stage::Weak));
    }
    if path_sub_possible && is_subsequence(&plan.sub.chars[ppre..], f) {
        let in_name = (m - ppre) as i32;
        return Some((tier::PATH, 8 * in_name - 2 * pdepth - f.len() as i32 / 4, Stage::Weak));
    }
    if m < 2 || present() < need {
        return None;
    }
    let l = lcs_len_iter(&plan.sub.pat, f.chars()) as usize;
    (l >= need).then(|| (tier::SIMILAR, (l * 1000 / m) as i32 - f.len() as i32, Stage::Weak))
}

/// Scan passes. Each pass covers one or more fallback stages in a single
/// sweep over memory; hits keep the stage and tier they actually matched at.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Pass {
    Strong,
    Approx,
    Weak,
    Exhaustive,
    FilterOnly,
}

impl Pass {
    /// Weakest stage this pass covers.
    fn stage(self) -> Stage {
        match self {
            Pass::Strong => Stage::Substring,
            Pass::Approx => Stage::Subsequence,
            Pass::Weak => Stage::Weak,
            Pass::Exhaustive | Pass::FilterOnly => Stage::Exhaustive,
        }
    }
    fn first_stage(self) -> Stage {
        match self {
            Pass::Strong => Stage::Exact,
            Pass::Approx => Stage::Fuzzy,
            p => p.stage(),
        }
    }
}

/// Evaluates one slot for one pass, returning (tier, score, stage).
#[allow(clippy::too_many_arguments)]
fn eval(pass: Pass, t: &NameTable, chain: &[u64], dirs: Option<&DirInfo>, id: EntryId, plan: &Plan, sc: &mut Scratch, floor: u8) -> Option<(u8, i32, Stage)> {
    let r = if plan.toks.is_empty() {
        // Directory-segments-only query ("proj/src/"): everything inside.
        match pass {
            Pass::Strong => Some((tier::SUBSTRING, -(t.folded(id).len() as i32), Stage::Substring)),
            Pass::FilterOnly => Some(filter_only_rank(t, id)),
            _ => None,
        }
    } else {
        match pass {
            Pass::Strong => eval_strong(t, id, plan, floor),
            Pass::Approx => eval_approx(t, id, plan, sc),
            Pass::Weak => eval_weak(t, chain, dirs.expect("dir info built before weak pass"), id, plan),
            Pass::Exhaustive => {
                let m = plan.sub.chars.len();
                let f = t.folded(id);
                let l = (lcs_len_iter(&plan.sub.pat, f.chars()) as usize * 100).checked_div(m).unwrap_or(0);
                Some((tier::REMAINING, l as i32 - f.len() as i32, Stage::Exhaustive))
            }
            Pass::FilterOnly => Some(filter_only_rank(t, id)),
        }
    };
    finish(t, chain, id, plan, r)
}

fn filter_only_rank(t: &NameTable, id: EntryId) -> (u8, i32, Stage) {
    let m = t.meta(id).mtime;
    let s = if m == NO_TIME { i32::MIN } else { (m / 60).clamp(i32::MIN as i64 + 1, i32::MAX as i64) as i32 };
    (tier::REMAINING, s, Stage::Exhaustive)
}

fn finish(t: &NameTable, chain: &[u64], id: EntryId, plan: &Plan, r: Option<(u8, i32, Stage)>) -> Option<(u8, i32, Stage)> {
    let (tr, s, st) = r?;
    let pen = dir_check(t, chain, id, plan)?;
    Some((tr.saturating_sub(pen), s, st))
}

/// Runs a search. `progress` receives a ranked snapshot after each stage.
pub fn search(t: &NameTable, q: &Query, opts: &Options, cancel: &CancelToken, mut progress: impl FnMut(&SearchResult)) -> SearchResult {
    let t0 = Instant::now();
    let deadline = opts.deadline.map(|d| t0 + d);
    let plan = Plan::new(q, opts.intensity);
    let mut status = Status {
        stages: Vec::new(),
        stop: Stop::StagesDone,
        total_matched: 0,
        considered: 0,
        indexed_exhausted: false,
        unsupported_filters: q.filters.iter().filter_map(|f| if let Filter::Content(s) = f { Some(format!("content:{s}")) } else { None }).collect(),
        query_errors: q.errors.clone(),
        truncated_query: plan.toks.iter().chain(plan.dirs.iter()).any(|t| t.pat.truncated) || plan.sub.pat.truncated,
        elapsed_micros: 0,
        generation: t.generation(),
    };
    let done_result = |status: Status, hits: Vec<Hit>| SearchResult { hits, status };
    if !status.unsupported_filters.is_empty() {
        status.stop = Stop::Unsupported;
        return done_result(status, Vec::new());
    }
    if !q.has_text() && q.filters.is_empty() {
        status.stop = Stop::EmptyQuery;
        return done_result(status, Vec::new());
    }

    let n = t.len();
    let chain = t.chain_masks();
    const CHUNK_WORDS: usize = 64; // 4096 slots per parallel task
    let mut done = vec![0u64; n.div_ceil(64)];
    let mut top = TopK::new(opts.limit);

    // Filter-only queries list everything that passes, most recent first.
    let passes: &[Pass] = if q.has_text() { &[Pass::Strong, Pass::Approx, Pass::Weak, Pass::Exhaustive] } else { &[Pass::FilterOnly] };
    for (si, &pass) in passes.iter().enumerate() {
        let forced = opts.through.is_some_and(|th| pass.first_stage() <= th);
        let enabled = match pass {
            Pass::Weak => opts.best_effort || forced,
            Pass::Exhaustive => opts.exhaustive || forced,
            _ => true,
        };
        if !enabled {
            continue;
        }
        if si > 0 && status.total_matched >= opts.enough && !forced {
            status.stop = Stop::Enough { next: pass.first_stage() };
            break;
        }
        let first = si == 0;
        let ts = Instant::now();
        let dirs = (pass == Pass::Weak && q.has_text()).then(|| DirInfo::build(t.dir_index(), &plan));


        let parts: Vec<(Vec<Hit>, usize, usize, bool)> = done
            .par_chunks_mut(CHUNK_WORDS)
            .enumerate()
            .map(|(ci, words)| {
                if cancel.is_canceled() || deadline.is_some_and(|d| Instant::now() >= d) {
                    return (Vec::new(), 0, 0, true);
                }
                let base = ci * CHUNK_WORDS * 64;
                let end = (base + CHUNK_WORDS * 64).min(n);
                let mut local = TopK::new(opts.limit);
                let mut sc = Scratch::default();
                let (mut matched, mut considered) = (0, 0);
                #[allow(clippy::needless_range_loop)] // `words[w]` is also written below
                for w in 0..words.len() {
                    let wbase = base + w * 64;
                    if wbase >= end {
                        break;
                    }
                    let valid = if end - wbase >= 64 { !0u64 } else { (1u64 << (end - wbase)) - 1 };
                    // Iterate only slots not yet matched/excluded.
                    let mut free = !words[w] & valid;
                    while free != 0 {
                        let b = free.trailing_zeros() as usize;
                        free &= free - 1;
                        let id32 = (wbase + b) as EntryId;
                        if first {
                            if !t.is_live(id32) || !passes_filters(t, Some(&chain), id32, &plan) {
                                words[w] |= 1 << b;
                                continue;
                            }
                            considered += 1;
                        }
                        if plan.rejects(pass, t.mask(id32), chain[id32 as usize]) {
                            continue;
                        }
                        if let Some((tr, score, stage)) = eval(pass, t, &chain, dirs.as_ref(), id32, &plan, &mut sc, local.floor_tier()) {
                            words[w] |= 1 << b;
                            matched += 1;
                            local.push(Hit { id: id32, tier: tr, score, stage, name_len: t.name(id32).len() as u16 });
                        }
                    }
                }
                (local.heap.into_iter().map(|r| r.0).collect(), matched, considered, false)
            })
            .collect();
        let mut matched = 0;
        let mut aborted = false;
        for (hits, m, c, a) in parts {
            for h in hits {
                top.push(h);
            }
            matched += m;
            status.considered += c;
            aborted |= a;
        }
        status.total_matched += matched;
        status.stages.push(StageReport { stage: pass.stage(), matched, micros: ts.elapsed().as_micros() as u64, complete: !aborted });
        if aborted {
            status.stop = if cancel.is_canceled() { Stop::Canceled } else { Stop::Deadline };
            break;
        }
        if pass.stage() == Stage::Exhaustive {
            status.indexed_exhausted = true;
        }
        status.elapsed_micros = t0.elapsed().as_micros() as u64;
        progress(&SearchResult { hits: top.sorted(), status: status.clone() });
    }
    status.elapsed_micros = t0.elapsed().as_micros() as u64;
    done_result(status, top.sorted())
}

/// Character positions to highlight, in the name and in the parent path as
/// rendered by [`NameTable::parent_path`].
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Highlight {
    pub name: Vec<usize>,
    pub dir: Vec<usize>,
}

fn find_all_tokens(hay: &str, toks: &[Tok]) -> Vec<usize> {
    let mut pos = Vec::new();
    for tk in toks {
        if let Some(b) = hay.find(tk.s.as_str()) {
            let s = char_index(hay, b);
            pos.extend(s..s + tk.chars.len());
        }
    }
    pos.sort_unstable();
    pos.dedup();
    pos
}

/// Computes highlight positions for a displayed hit. O(|query|·|name|).
pub fn highlight(t: &NameTable, q: &Query, intensity: Intensity, hit: &Hit) -> Highlight {
    let plan = Plan::new(q, intensity);
    let f = t.folded(hit.id);
    let fc: Vec<char> = f.chars().collect();
    let dir_folded = t.parent_path_folded(hit.id);
    let mut h = Highlight::default();
    match hit.stage {
        Stage::Exact => {
            let n = if f == plan.full { fc.len() } else { stem_of(f).chars().count() };
            h.name = (0..n).collect();
        }
        Stage::Substring => {
            h.name = match f.find(plan.full.as_str()) {
                Some(b) if !plan.full.is_empty() => {
                    let s = char_index(f, b);
                    (s..s + plan.full.chars().count()).collect()
                }
                _ => find_all_tokens(f, &plan.toks),
            };
        }
        Stage::Fuzzy => {
            for tk in &plan.toks {
                match f.find(tk.s.as_str()) {
                    Some(b) => {
                        let s = char_index(f, b);
                        h.name.extend(s..s + tk.chars.len());
                    }
                    None => h.name.extend(osa_substring_positions(&tk.chars, &fc).1),
                }
            }
            h.name.sort_unstable();
            h.name.dedup();
        }
        Stage::Subsequence => h.name = subsequence_positions(&plan.sub.chars, &fc).unwrap_or_default(),
        Stage::Weak | Stage::Exhaustive => {
            let all_tokens = plan.toks.iter().all(|tk| f.contains(tk.s.as_str()) || dir_folded.contains(tk.s.as_str()));
            if hit.tier == tier::PATH && all_tokens {
                h.name = find_all_tokens(f, &plan.toks);
                let rest: Vec<Tok> = plan.toks.iter().filter(|tk| !f.contains(tk.s.as_str())).map(|tk| Tok::new(&tk.s, intensity)).collect();
                h.dir = find_all_tokens(&dir_folded, &rest);
            } else if hit.tier == tier::PATH {
                let mut full: Vec<char> = dir_folded.chars().collect();
                let dlen = full.len();
                full.push(t.sep);
                full.extend(fc.iter());
                // Subsequence over the whole path, preferring matches near the leaf.
                let rev_p: Vec<char> = plan.sub.chars.iter().rev().copied().collect();
                let rev_t: Vec<char> = full.iter().rev().copied().collect();
                let mut pos = Vec::new();
                let mut pi = 0;
                for (j, &c) in rev_t.iter().enumerate() {
                    if pi < rev_p.len() && c == rev_p[pi] {
                        pos.push(full.len() - 1 - j);
                        pi += 1;
                    }
                }
                for p in pos {
                    if p > dlen {
                        h.name.push(p - dlen - 1);
                    } else if p < dlen {
                        h.dir.push(p);
                    }
                }
                h.name.sort_unstable();
                h.dir.sort_unstable();
            } else {
                h.name = lcs_positions(&plan.sub.chars, &fc);
            }
        }
    }
    if !plan.dirs.is_empty() {
        h.dir.extend(find_all_tokens(&dir_folded, &plan.dirs));
        h.dir.sort_unstable();
        h.dir.dedup();
    }
    h
}

#[cfg(test)]
mod tests;
