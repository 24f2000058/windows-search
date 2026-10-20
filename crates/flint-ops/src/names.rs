//! Validation of user-typed file names (rename, new folder). A name must be
//! a single path component: anything that could escape the target folder or
//! that Windows would silently alter is rejected with a reason.

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum NameError {
    Empty,
    /// `.` or `..`, or contains a separator: would leave the folder.
    Traversal,
    InvalidChar(char),
    /// Windows strips trailing dots and spaces, so the result would differ.
    TrailingDotOrSpace,
    /// CON, PRN, AUX, NUL, COM1–9, LPT1–9 (with any extension).
    Reserved(String),
    TooLong,
}

impl std::fmt::Display for NameError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            NameError::Empty => write!(f, "the name is empty"),
            NameError::Traversal => write!(f, "the name must not contain \\ or / or be . or .."),
            NameError::InvalidChar(c) if c.is_control() => write!(f, "the name contains a control character"),
            NameError::InvalidChar(c) => write!(f, "the name must not contain {c}"),
            NameError::TrailingDotOrSpace => write!(f, "the name must not end with a dot or space"),
            NameError::Reserved(n) => write!(f, "{n} is a reserved device name on Windows"),
            NameError::TooLong => write!(f, "the name is longer than 255 characters"),
        }
    }
}

pub fn validate_name(name: &str) -> Result<(), NameError> {
    if name.is_empty() {
        return Err(NameError::Empty);
    }
    if name == "." || name == ".." || name.contains(['\\', '/']) {
        return Err(NameError::Traversal);
    }
    if let Some(c) = name.chars().find(|&c| matches!(c, '<' | '>' | ':' | '"' | '|' | '?' | '*') || (c as u32) < 32) {
        return Err(NameError::InvalidChar(c));
    }
    if name.ends_with(['.', ' ']) {
        return Err(NameError::TrailingDotOrSpace);
    }
    if name.encode_utf16().count() > 255 {
        return Err(NameError::TooLong);
    }
    let stem = name.split('.').next().unwrap_or("").trim_end().to_ascii_uppercase();
    let reserved = matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL" | "CONIN$" | "CONOUT$")
        || (stem.len() == 4 && (stem.starts_with("COM") || stem.starts_with("LPT")) && stem[3..].chars().all(|c| matches!(c, '1'..='9' | '¹' | '²' | '³')));
    if reserved {
        return Err(NameError::Reserved(stem));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_escapes_and_names_windows_would_alter() {
        for ok in ["report.pdf", "Ünïcode 名前", ".gitignore", "a..b", "COM10", "console.txt", "LPT"] {
            assert_eq!(validate_name(ok), Ok(()), "{ok}");
        }
        assert_eq!(validate_name(""), Err(NameError::Empty));
        for t in [".", "..", "..\\evil", "a/b", "C:\\x"] {
            assert!(matches!(validate_name(t), Err(NameError::Traversal | NameError::InvalidChar(':'))), "{t}");
        }
        assert_eq!(validate_name("a:b"), Err(NameError::InvalidChar(':')), "alternate data streams");
        assert_eq!(validate_name("x\u{7}"), Err(NameError::InvalidChar('\u{7}')));
        assert_eq!(validate_name("name."), Err(NameError::TrailingDotOrSpace));
        assert_eq!(validate_name("name "), Err(NameError::TrailingDotOrSpace));
        assert_eq!(validate_name("nul.txt"), Err(NameError::Reserved("NUL".into())));
        assert_eq!(validate_name("Com1"), Err(NameError::Reserved("COM1".into())));
        assert_eq!(validate_name(&"x".repeat(256)), Err(NameError::TooLong));
        assert_eq!(validate_name(&"x".repeat(255)), Ok(()));
    }
}
