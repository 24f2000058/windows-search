//! Per-candidate matching kernels.
//!
//! The hot kernels are bit-parallel (one machine word per text character):
//! * [`osa_substring`] — Hyyrö's bit-vector optimal-string-alignment distance
//!   (insertions, deletions, substitutions, adjacent transpositions), in
//!   approximate-*substring* mode so `reprot` matches inside `q3_report.pdf`.
//! * [`lcs_len`] — bit-parallel longest common subsequence, used for weak
//!   similarity of arbitrary (even nonsensical) queries.
//!
//! Highlight positions are recovered afterwards with small explicit DP
//! tracebacks, but only for results that are actually displayed.

use crate::fold::is_separator;

/// Maximum pattern length handled by the bit-parallel kernels.
pub const MAX_PATTERN: usize = 64;

/// A folded query (or query token) precompiled for the kernels.
#[derive(Clone, Debug)]
pub struct Pattern {
    pub chars: Vec<char>,
    ascii_peq: [u64; 128],
    other_peq: Vec<(char, u64)>,
    /// True if the source was longer than [`MAX_PATTERN`] and was cut.
    pub truncated: bool,
}

impl Pattern {
    pub fn new(folded: &str) -> Self {
        let all: Vec<char> = folded.chars().collect();
        let truncated = all.len() > MAX_PATTERN;
        let chars: Vec<char> = all.into_iter().take(MAX_PATTERN).collect();
        let mut ascii_peq = [0u64; 128];
        let mut other_peq: Vec<(char, u64)> = Vec::new();
        for (i, &c) in chars.iter().enumerate() {
            if (c as u32) < 128 {
                ascii_peq[c as usize] |= 1 << i;
            } else if let Some(e) = other_peq.iter_mut().find(|e| e.0 == c) {
                e.1 |= 1 << i;
            } else {
                other_peq.push((c, 1 << i));
            }
        }
        Pattern { chars, ascii_peq, other_peq, truncated }
    }

    #[inline]
    pub fn len(&self) -> usize {
        self.chars.len()
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.chars.is_empty()
    }

    #[inline]
    fn peq(&self, c: char) -> u64 {
        if (c as u32) < 128 {
            self.ascii_peq[c as usize]
        } else {
            self.other_peq.iter().find(|e| e.0 == c).map_or(0, |e| e.1)
        }
    }

    #[inline]
    fn word_mask(&self) -> u64 {
        if self.len() == 64 { !0 } else { (1u64 << self.len()) - 1 }
    }
}

/// Minimum OSA edit distance between `p` and any substring of `text`.
/// Returns `(distance, end)` where `end` is the exclusive end index (in chars)
/// of the leftmost best-matching substring, or `None` if the distance exceeds
/// `max_k`. Runs in O(|text|) word operations.
pub fn osa_substring(p: &Pattern, text: &[char], max_k: u32) -> Option<(u32, usize)> {
    osa_substring_iter(p, text.iter().copied(), max_k)
}

/// [`osa_substring`] over a character stream (no decoding buffer needed).
#[inline]
pub fn osa_substring_iter(p: &Pattern, text: impl IntoIterator<Item = char>, max_k: u32) -> Option<(u32, usize)> {
    let m = p.len();
    if m == 0 {
        return Some((0, 0));
    }
    let mask = p.word_mask();
    let high = 1u64 << (m - 1);
    let (mut vp, mut vn) = (mask, 0u64);
    let mut score = m as u32;
    let mut best = (score, 0usize);
    let (mut d0_prev, mut eq_prev) = (0u64, 0u64);
    for (j, c) in text.into_iter().enumerate() {
        let eq = p.peq(c);
        let tr = (((!d0_prev) & eq) << 1) & eq_prev;
        let d0 = (((eq & vp).wrapping_add(vp)) ^ vp) | eq | vn | tr;
        let hp = vn | !(d0 | vp);
        let hn = vp & d0;
        if hp & high != 0 {
            score += 1;
        } else if hn & high != 0 {
            score -= 1;
        }
        // Approximate-substring mode: the top DP row is all zeros, so no
        // horizontal delta is shifted in.
        let hp = hp << 1;
        let hn = hn << 1;
        vp = (hn | !(d0 | hp)) & mask;
        vn = hp & d0 & mask;
        d0_prev = d0;
        eq_prev = eq;
        if score < best.0 {
            best = (score, j + 1);
            if score == 0 {
                break;
            }
        }
    }
    (best.0 <= max_k).then_some(best)
}

/// Length of the longest common subsequence of `p` and `text`.
pub fn lcs_len(p: &Pattern, text: &[char]) -> u32 {
    lcs_len_iter(p, text.iter().copied())
}

/// [`lcs_len`] over a character stream.
#[inline]
pub fn lcs_len_iter(p: &Pattern, text: impl IntoIterator<Item = char>) -> u32 {
    if p.is_empty() {
        return 0;
    }
    let mask = p.word_mask();
    let mut v = !0u64;
    for c in text {
        let u = v & p.peq(c);
        v = v.wrapping_add(u) | (v & !p.peq(c));
    }
    (p.len() as u32) - (v & mask).count_ones()
}

/// OSA substring alignment with traceback. Returns the distance and the text
/// positions of characters that matched exactly (including both characters of
/// a transposed pair). O(|p|·|text|); call only for displayed results.
pub fn osa_substring_positions(p: &[char], text: &[char]) -> (u32, Vec<usize>) {
    let (m, n) = (p.len(), text.len());
    let w = n + 1;
    let mut d = vec![0u32; (m + 1) * w];
    for i in 0..=m {
        d[i * w] = i as u32;
    }
    for i in 1..=m {
        for j in 1..=n {
            let cost = (p[i - 1] != text[j - 1]) as u32;
            let mut v = (d[(i - 1) * w + j] + 1).min(d[i * w + j - 1] + 1).min(d[(i - 1) * w + j - 1] + cost);
            if i > 1 && j > 1 && p[i - 1] == text[j - 2] && p[i - 2] == text[j - 1] {
                v = v.min(d[(i - 2) * w + j - 2] + 1);
            }
            d[i * w + j] = v;
        }
    }
    let (mut best_j, mut best) = (0, d[m * w]);
    for j in 1..=n {
        if d[m * w + j] < best {
            best = d[m * w + j];
            best_j = j;
        }
    }
    let mut pos = Vec::new();
    let (mut i, mut j) = (m, best_j);
    while i > 0 && j > 0 {
        let cur = d[i * w + j];
        if p[i - 1] == text[j - 1] && cur == d[(i - 1) * w + j - 1] {
            pos.push(j - 1);
            i -= 1;
            j -= 1;
        } else if i > 1 && j > 1 && p[i - 1] == text[j - 2] && p[i - 2] == text[j - 1] && cur == d[(i - 2) * w + j - 2] + 1 {
            pos.push(j - 1);
            pos.push(j - 2);
            i -= 2;
            j -= 2;
        } else if cur == d[(i - 1) * w + j - 1] + 1 {
            i -= 1;
            j -= 1;
        } else if cur == d[(i - 1) * w + j] + 1 {
            i -= 1;
        } else {
            j -= 1;
        }
    }
    pos.reverse();
    (best, pos)
}

/// LCS with traceback: positions in `text` of one longest common subsequence,
/// preferring later-in-text alignments to be compact.
pub fn lcs_positions(p: &[char], text: &[char]) -> Vec<usize> {
    let (m, n) = (p.len(), text.len());
    let w = n + 1;
    let mut d = vec![0u16; (m + 1) * w];
    for i in 1..=m {
        for j in 1..=n {
            d[i * w + j] = if p[i - 1] == text[j - 1] {
                d[(i - 1) * w + j - 1] + 1
            } else {
                d[(i - 1) * w + j].max(d[i * w + j - 1])
            };
        }
    }
    let mut pos = Vec::new();
    let (mut i, mut j) = (m, n);
    while i > 0 && j > 0 {
        if p[i - 1] == text[j - 1] && d[i * w + j] == d[(i - 1) * w + j - 1] + 1 {
            pos.push(j - 1);
            i -= 1;
            j -= 1;
        } else if d[(i - 1) * w + j] >= d[i * w + j - 1] {
            i -= 1;
        } else {
            j -= 1;
        }
    }
    pos.reverse();
    pos
}

/// Exact-substring needles for a pigeonhole prefilter of OSA matching.
///
/// The pattern is split into `k + 1` pieces. In any alignment of OSA cost
/// `≤ k`, every substitution, deletion, insertion *between a piece's chars* or
/// transposition *within* a piece damages at most one piece, so at least one
/// piece is free of internal edits. Such a piece occurs contiguously in the
/// text, except that a transposition straddling its left/right boundary swaps
/// its first/last char with the neighbour's, so those variants are included.
/// Hence: `osa_substring(p, t) ≤ k` ⇒ `t` contains at least one needle.
///
/// Returns `None` when pieces would be shorter than `min_piece` chars (the
/// filter would be too weak to pay for itself).
pub fn piece_needles(p: &[char], k: usize, min_piece: usize) -> Option<Vec<String>> {
    let m = p.len();
    let pieces = k + 1;
    if k == 0 || m / pieces < min_piece {
        return None;
    }
    let bound = |i: usize| i * m / pieces;
    let mut out: Vec<String> = Vec::new();
    for i in 0..pieces {
        let (a, b) = (bound(i), bound(i + 1));
        let mut variants: Vec<Vec<char>> = vec![p[a..b].to_vec()];
        if a > 0 {
            variants.push(std::iter::once(p[a - 1]).chain(p[a + 1..b].iter().copied()).collect());
        }
        if b < m {
            variants.push(p[a..b - 1].iter().copied().chain(std::iter::once(p[b])).collect());
        }
        if a > 0 && b < m && b - a >= 2 {
            variants.push(std::iter::once(p[a - 1]).chain(p[a + 1..b - 1].iter().copied()).chain(std::iter::once(p[b])).collect());
        }
        for v in variants {
            let v: String = v.into_iter().collect();
            if !out.contains(&v) {
                out.push(v);
            }
        }
    }
    Some(out)
}

/// True if `p` is an in-order subsequence of `text`.
#[inline]
pub fn is_subsequence(p: &[char], text: &str) -> bool {
    let mut it = p.iter();
    let mut need = it.next();
    for c in text.chars() {
        match need {
            None => return true,
            Some(&n) if n == c => need = it.next(),
            _ => {}
        }
    }
    need.is_none()
}

/// Finds `p` as an in-order subsequence of `text`, returning compact positions
/// (forward pass to find the earliest end, backward pass to tighten the start).
pub fn subsequence_positions(p: &[char], text: &[char]) -> Option<Vec<usize>> {
    if p.is_empty() {
        return Some(Vec::new());
    }
    let mut pi = 0;
    let mut end = None;
    for (j, &c) in text.iter().enumerate() {
        if c == p[pi] {
            pi += 1;
            if pi == p.len() {
                end = Some(j);
                break;
            }
        }
    }
    let end = end?;
    let mut pos = vec![0; p.len()];
    let mut pi = p.len();
    let mut j = end + 1;
    while pi > 0 {
        j -= 1;
        if text[j] == p[pi - 1] {
            pi -= 1;
            pos[pi] = j;
        }
    }
    Some(pos)
}

/// True if a word starts at `i` in `orig` (original-case name chars).
#[inline]
pub fn is_boundary(orig: &[char], i: usize) -> bool {
    if i == 0 {
        return true;
    }
    let (prev, cur) = (orig[i - 1], orig[i]);
    is_separator(prev)
        || (prev.is_lowercase() && cur.is_uppercase())
        || (prev.is_alphabetic() && cur.is_numeric())
        || (prev.is_numeric() && cur.is_alphabetic())
}

/// Scores a set of matched positions: rewards word starts, contiguous runs and
/// early matches; penalizes gaps and long names.
pub fn score_positions(orig: &[char], pos: &[usize]) -> i32 {
    let mut s = 0i32;
    let mut prev: Option<usize> = None;
    for &i in pos {
        s += 16;
        if i == 0 {
            s += 12;
        } else if is_boundary(orig, i) {
            s += 9;
        }
        if let Some(p) = prev {
            if i == p + 1 {
                s += 6;
            } else {
                let gap = (i - p - 1) as i32;
                s -= 3 + (gap - 1).min(12);
            }
        }
        prev = Some(i);
    }
    if let Some(&first) = pos.first() {
        s -= (first as i32).min(10);
    }
    s - (orig.len() as i32) / 8
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chars(s: &str) -> Vec<char> {
        s.chars().collect()
    }

    /// Reference OSA approximate-substring distance (explicit DP).
    fn brute_osa_substring(p: &[char], t: &[char]) -> u32 {
        osa_substring_positions(p, t).0
    }

    fn brute_lcs(p: &[char], t: &[char]) -> u32 {
        let mut d = vec![vec![0u32; t.len() + 1]; p.len() + 1];
        for i in 1..=p.len() {
            for j in 1..=t.len() {
                d[i][j] = if p[i - 1] == t[j - 1] { d[i - 1][j - 1] + 1 } else { d[i - 1][j].max(d[i][j - 1]) };
            }
        }
        d[p.len()][t.len()]
    }

    struct Rng(u64);
    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }
        fn string(&mut self, alphabet: &[char], max: usize) -> Vec<char> {
            let n = (self.next() as usize) % (max + 1);
            (0..n).map(|_| alphabet[(self.next() as usize) % alphabet.len()]).collect()
        }
    }

    #[test]
    fn osa_examples() {
        let p = Pattern::new("reprot");
        assert_eq!(osa_substring(&p, &chars("q3_report.pdf"), 3).map(|r| r.0), Some(1));
        let p = Pattern::new("raport");
        assert_eq!(osa_substring(&p, &chars("q3_report.pdf"), 3).map(|r| r.0), Some(1));
        let p = Pattern::new("report");
        assert_eq!(osa_substring(&p, &chars("q3_report.pdf"), 0), Some((0, 9)));
        let p = Pattern::new("xyz");
        assert_eq!(osa_substring(&p, &chars("abc"), 1), None);
    }

    #[test]
    fn bit_parallel_osa_matches_reference_dp() {
        let mut rng = Rng(0x9e3779b97f4a7c15);
        let alpha = chars("abcdé日");
        for _ in 0..20_000 {
            let p = rng.string(&alpha, 9);
            let t = rng.string(&alpha, 24);
            if p.is_empty() {
                continue;
            }
            let pat = Pattern::new(&p.iter().collect::<String>());
            let fast = osa_substring(&pat, &t, u32::MAX).unwrap().0;
            assert_eq!(fast, brute_osa_substring(&p, &t), "p={p:?} t={t:?}");
        }
    }

    #[test]
    fn osa_handles_64_char_pattern() {
        let s: String = "abcdefgh".repeat(8);
        let p = Pattern::new(&s);
        assert_eq!(p.len(), 64);
        let t: Vec<char> = format!("xx{s}yy").chars().collect();
        assert_eq!(osa_substring(&p, &t, 0).map(|r| r.0), Some(0));
        let long = Pattern::new(&"a".repeat(80));
        assert!(long.truncated);
    }

    #[test]
    fn bit_parallel_lcs_matches_reference_dp() {
        let mut rng = Rng(12345);
        let alpha = chars("abcxyzü");
        for _ in 0..20_000 {
            let p = rng.string(&alpha, 12);
            let t = rng.string(&alpha, 30);
            let pat = Pattern::new(&p.iter().collect::<String>());
            assert_eq!(lcs_len(&pat, &t), brute_lcs(&p, &t), "p={p:?} t={t:?}");
            assert_eq!(lcs_positions(&p, &t).len() as u32, brute_lcs(&p, &t));
        }
    }

    #[test]
    fn piece_filter_never_rejects_a_match() {
        let mut rng = Rng(0xfeed);
        let alpha = chars("abcdé");
        let mut checked = 0;
        for _ in 0..40_000 {
            let p = rng.string(&alpha, 12);
            let t = rng.string(&alpha, 20);
            for k in 1..=3usize {
                let Some(needles) = piece_needles(&p, k, 1) else { continue };
                let ts: String = t.iter().collect();
                if brute_osa_substring(&p, &t) <= k as u32 {
                    checked += 1;
                    assert!(needles.iter().any(|n| ts.contains(n.as_str())), "p={p:?} t={t:?} k={k} needles={needles:?}");
                }
            }
        }
        assert!(checked > 1000, "test should exercise many true matches ({checked})");
        // Transposition straddling a piece boundary.
        let n = piece_needles(&chars("reprot"), 1, 2).unwrap();
        assert!(n.iter().any(|x| "rerpot".contains(x.as_str())));
    }

    #[test]
    fn traceback_positions_are_matching_chars() {
        let p = chars("reprot");
        let t = chars("q3_report.pdf");
        let (d, pos) = osa_substring_positions(&p, &t);
        assert_eq!(d, 1);
        assert_eq!(pos, vec![3, 4, 5, 6, 7, 8]);
    }

    #[test]
    fn subsequence_finds_compact_positions() {
        let t = chars("my_great_project_file.rs");
        assert_eq!(subsequence_positions(&chars("mgp"), &t), Some(vec![0, 3, 9]));
        assert_eq!(subsequence_positions(&chars("zz"), &t), None);
        // Backward pass tightens: picks the last 'a' before 'b'.
        assert_eq!(subsequence_positions(&chars("ab"), &chars("a_a_ab")), Some(vec![4, 5]));
    }

    #[test]
    fn scoring_prefers_boundaries_and_runs() {
        let a = chars("FooBarBaz.txt");
        let b = chars("xfoxoxbxaxrz.txt");
        let sa = score_positions(&a, &subsequence_positions(&chars("fbb"), &chars("foobarbaz.txt")).unwrap());
        let sb = score_positions(&b, &subsequence_positions(&chars("fbb"), &chars("xfoxoxbxaxrz.txt").iter().map(|c| if *c == 'r' { 'b' } else { *c }).collect::<Vec<_>>()).unwrap());
        assert!(sa > sb);
    }
}
