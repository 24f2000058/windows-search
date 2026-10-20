//! Windows path semantics on plain strings, so planning behaves the same in
//! tests on any OS: `\` and `/` are separators, comparison is
//! case-insensitive, and roots (`C:\`, `\\server\share`, `\\?\Volume{..}\`)
//! have no parent.

/// Canonical comparison form: `\` separators, no trailing separator except
/// for a bare drive root, lowercase.
pub fn norm(p: &str) -> String {
    let mut s: String = p.chars().map(|c| if c == '/' { '\\' } else { c }).collect::<String>().to_lowercase();
    while s.len() > 1 && s.ends_with('\\') && !s.ends_with(":\\") {
        s.pop();
    }
    if s.len() == 2 && s.ends_with(':') {
        s.push('\\');
    }
    s
}

pub fn same(a: &str, b: &str) -> bool {
    norm(a) == norm(b)
}

/// Length of the root prefix (`C:\`, `\\server\share\`, `\\?\X:\`,
/// `\\?\Volume{..}\`), or None for relative paths.
fn root_len(p: &str) -> Option<usize> {
    let b = p.as_bytes();
    let sep = |c: u8| c == b'\\' || c == b'/';
    if b.len() >= 2 && b[1] == b':' && b[0].is_ascii_alphabetic() {
        return Some(if b.len() >= 3 && sep(b[2]) { 3 } else { 2 });
    }
    if b.len() >= 2 && sep(b[0]) && sep(b[1]) {
        // \\?\ and \\.\ device prefixes: the next component is the volume.
        let rest = &p[2..];
        let mut parts = rest.splitn(3, ['\\', '/']);
        let a = parts.next()?;
        let bpart = parts.next()?;
        if bpart.is_empty() {
            return None;
        }
        let mut len = 2 + a.len() + 1 + bpart.len();
        if (a == "?" || a == ".") && bpart.eq_ignore_ascii_case("unc") {
            // \\?\UNC\server\share
            let mut more = p[len + 1..].splitn(3, ['\\', '/']);
            let server = more.next()?;
            let share = more.next()?;
            len += 1 + server.len() + 1 + share.len();
        }
        if p.len() > len && sep(b[len]) {
            len += 1;
        }
        return Some(len.min(p.len()));
    }
    None
}

pub fn is_root(p: &str) -> bool {
    let t = p.trim_end_matches(['\\', '/']);
    match root_len(p) {
        Some(n) => t.len() <= n,
        None => false,
    }
}

/// Containing directory, or None for roots and bare names.
pub fn parent(p: &str) -> Option<&str> {
    let t = p.trim_end_matches(['\\', '/']);
    let root = root_len(p)?;
    if t.len() <= root {
        return None;
    }
    let i = t.rfind(['\\', '/'])?;
    Some(if i < root { &p[..root] } else { &t[..i] })
}

pub fn file_name(p: &str) -> &str {
    let t = p.trim_end_matches(['\\', '/']);
    t.rsplit(['\\', '/']).next().unwrap_or(t)
}

pub fn join(dir: &str, name: &str) -> String {
    if dir.ends_with(['\\', '/']) { format!("{dir}{name}") } else { format!("{dir}\\{name}") }
}

/// True if `p` is `dir` itself or lies anywhere beneath it.
pub fn is_within(p: &str, dir: &str) -> bool {
    let (p, d) = (norm(p), norm(dir));
    p == d || p.starts_with(&if d.ends_with('\\') { d.clone() } else { format!("{d}\\") })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roots_parents_and_containment() {
        for r in ["C:\\", "c:", "C:/", "\\\\nas\\photos", "\\\\nas\\photos\\", "\\\\?\\Volume{1234}\\", "\\\\?\\UNC\\nas\\share\\"] {
            assert!(is_root(r), "{r}");
            assert_eq!(parent(r), None, "{r}");
        }
        assert!(!is_root("C:\\Users"));
        assert_eq!(parent("C:\\Users"), Some("C:\\"));
        assert_eq!(parent("C:\\Users\\me\\"), Some("C:\\Users"));
        assert_eq!(parent("\\\\nas\\photos\\2024\\a.jpg"), Some("\\\\nas\\photos\\2024"));
        assert_eq!(parent("\\\\nas\\photos\\a.jpg"), Some("\\\\nas\\photos\\"));
        assert_eq!(parent("relative"), None);
        assert_eq!(file_name("C:\\a\\B.txt"), "B.txt");
        assert_eq!(join("C:\\", "x"), "C:\\x");
        assert_eq!(join("C:\\a", "x"), "C:\\a\\x");
        assert!(is_within("c:/A/b", "C:\\a"));
        assert!(is_within("C:\\a", "c:\\A\\"));
        assert!(!is_within("C:\\ab", "C:\\a"), "prefix of a name is not containment");
        assert!(is_within("C:\\x", "C:\\"));
        assert!(same("C:\\Ünï\\", "c:/ünï"));
    }
}
