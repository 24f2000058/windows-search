//! Case/diacritic folding with a strict 1:1 character mapping.
//!
//! Every original character maps to exactly one folded character, so match
//! positions computed on folded text are valid highlight positions on the
//! original text. Characters whose full lowercase/decomposition expands to
//! several characters keep only the first (e.g. `ﬁ` -> `f`), which is a
//! deliberate trade-off for exact highlighting.

use unicode_normalization::char::decompose_compatible;

/// Folds a single character: compatibility-decomposes it, keeps the base
/// character, and lowercases it.
#[inline]
pub fn fold_char(c: char) -> char {
    if c.is_ascii() {
        return c.to_ascii_lowercase();
    }
    let mut base = None;
    decompose_compatible(c, |d| {
        if base.is_none() {
            base = Some(d);
        }
    });
    let base = base.unwrap_or(c);
    base.to_lowercase().next().unwrap_or(base)
}

/// Folds a string. The result has exactly as many `char`s as the input.
pub fn fold_str(s: &str) -> String {
    if s.bytes().all(|b| b.is_ascii() && !b.is_ascii_uppercase()) {
        return s.to_owned();
    }
    s.chars().map(fold_char).collect()
}

/// Bit assigned to a folded character in a 64-bit presence mask.
///
/// `a`-`z` -> 0..26, `0`-`9` -> 26..36, a few separators get dedicated bits,
/// everything else is hashed into the remaining bits. The mask is used only as
/// a conservative prefilter: a hash collision can let a non-match through,
/// never reject a true match.
#[inline]
pub fn char_bit(c: char) -> u64 {
    let i = match c {
        'a'..='z' => c as u32 - 'a' as u32,
        '0'..='9' => 26 + (c as u32 - '0' as u32),
        '.' => 36,
        '_' => 37,
        '-' => 38,
        ' ' => 39,
        _ => 40 + ((c as u32).wrapping_mul(2_654_435_761) >> 27) % 24,
    };
    1u64 << i
}

/// Presence mask of all characters in an already-folded string.
pub fn mask_of(folded: &str) -> u64 {
    folded.chars().fold(0, |m, c| m | char_bit(c))
}

/// True if `c` separates words for boundary bonuses and tokenization.
#[inline]
pub fn is_separator(c: char) -> bool {
    matches!(c, ' ' | '_' | '-' | '.' | '(' | ')' | '[' | ']' | '{' | '}' | ',' | '+' | '&' | '\'' | '~' | '@' | '#' | '!' | '=')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn folding_is_one_to_one() {
        for s in ["Résumé.PDF", "ÅNGSTRÖM", "Straße", "İstanbul", "ﬁle", "Ｆｕｌｌ", "日本語.txt", "e\u{301}cole"] {
            assert_eq!(s.chars().count(), fold_str(s).chars().count(), "{s}");
        }
    }

    #[test]
    fn folds_case_and_diacritics() {
        assert_eq!(fold_str("Résumé.PDF"), "resume.pdf");
        assert_eq!(fold_str("ÅNGSTRÖM"), "angstrom");
        assert_eq!(fold_str("Ｆｕｌｌ"), "full");
        assert_eq!(fold_str("İstanbul"), "istanbul");
        assert_eq!(fold_str("日本語"), "日本語");
    }

    #[test]
    fn mask_is_superset_filter() {
        let m = mask_of("report_2024.pdf");
        assert_eq!(m & mask_of("rpt"), mask_of("rpt"));
        assert_ne!(m & mask_of("z"), mask_of("z"));
    }
}
