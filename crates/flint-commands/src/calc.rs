//! Safe arithmetic: a bounded recursive-descent parser over a fixed grammar.
//! No code is evaluated; only numbers, operators, parentheses and a fixed set
//! of functions/constants are accepted.
//!
//! ```text
//! expr   := term (('+' | '-') term)*
//! term   := unary (('*' | '/' | '%') unary)*
//! unary  := ('+' | '-') unary | power
//! power  := atom ('^' unary)?            (right-associative)
//! atom   := number | const | func '(' args ')' | '(' expr ')'
//! ```

use crate::{Action, Command, CommandResult};

pub const MAX_INPUT: usize = 256;
pub const MAX_DEPTH: usize = 64;

#[derive(Clone, Debug, PartialEq)]
pub enum CalcError {
    TooLong,
    TooDeep,
    Unexpected(usize),
    UnknownName(String),
    BadArity(String),
    NotFinite,
    Empty,
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Tok {
    Num(f64),
    Op(char),
    LParen,
    RParen,
    Comma,
    Ident(usize, usize),
}

fn lex(s: &str) -> Result<Vec<(usize, Tok)>, CalcError> {
    let b = s.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < b.len() {
        let c = b[i];
        match c {
            b' ' | b'\t' => i += 1,
            b'0'..=b'9' | b'.' => {
                let start = i;
                if c == b'0' && i + 1 < b.len() && (b[i + 1] == b'x' || b[i + 1] == b'X') {
                    i += 2;
                    let hs = i;
                    while i < b.len() && b[i].is_ascii_hexdigit() {
                        i += 1;
                    }
                    let v = u64::from_str_radix(&s[hs..i], 16).map_err(|_| CalcError::Unexpected(start))?;
                    out.push((start, Tok::Num(v as f64)));
                    continue;
                }
                while i < b.len() && (b[i].is_ascii_digit() || b[i] == b'.' || b[i] == b'_') {
                    i += 1;
                }
                if i < b.len() && (b[i] == b'e' || b[i] == b'E') {
                    let save = i;
                    i += 1;
                    if i < b.len() && (b[i] == b'+' || b[i] == b'-') {
                        i += 1;
                    }
                    if i < b.len() && b[i].is_ascii_digit() {
                        while i < b.len() && b[i].is_ascii_digit() {
                            i += 1;
                        }
                    } else {
                        i = save;
                    }
                }
                let text: String = s[start..i].chars().filter(|&c| c != '_').collect();
                let v: f64 = text.parse().map_err(|_| CalcError::Unexpected(start))?;
                out.push((start, Tok::Num(v)));
            }
            b'+' | b'-' | b'*' | b'/' | b'%' | b'^' => {
                out.push((i, Tok::Op(c as char)));
                i += 1;
            }
            b'(' => {
                out.push((i, Tok::LParen));
                i += 1;
            }
            b')' => {
                out.push((i, Tok::RParen));
                i += 1;
            }
            b',' => {
                out.push((i, Tok::Comma));
                i += 1;
            }
            b'a'..=b'z' | b'A'..=b'Z' => {
                let start = i;
                while i < b.len() && (b[i].is_ascii_alphanumeric()) {
                    i += 1;
                }
                out.push((start, Tok::Ident(start, i)));
            }
            _ => return Err(CalcError::Unexpected(i)),
        }
    }
    Ok(out)
}

struct Parser<'a> {
    src: &'a str,
    toks: Vec<(usize, Tok)>,
    pos: usize,
    depth: usize,
}

impl Parser<'_> {
    fn peek(&self) -> Option<Tok> {
        self.toks.get(self.pos).map(|t| t.1)
    }
    fn at(&self) -> usize {
        self.toks.get(self.pos).map_or(self.src.len(), |t| t.0)
    }
    fn enter(&mut self) -> Result<(), CalcError> {
        self.depth += 1;
        if self.depth > MAX_DEPTH { Err(CalcError::TooDeep) } else { Ok(()) }
    }

    fn expr(&mut self) -> Result<f64, CalcError> {
        self.enter()?;
        let mut v = self.term()?;
        while let Some(Tok::Op(c @ ('+' | '-'))) = self.peek() {
            self.pos += 1;
            let r = self.term()?;
            v = if c == '+' { v + r } else { v - r };
        }
        self.depth -= 1;
        Ok(v)
    }

    fn term(&mut self) -> Result<f64, CalcError> {
        let mut v = self.unary()?;
        while let Some(Tok::Op(c @ ('*' | '/' | '%'))) = self.peek() {
            self.pos += 1;
            let r = self.unary()?;
            v = match c {
                '*' => v * r,
                '/' => v / r,
                _ => v % r,
            };
        }
        Ok(v)
    }

    fn unary(&mut self) -> Result<f64, CalcError> {
        self.enter()?;
        let v = match self.peek() {
            Some(Tok::Op('-')) => {
                self.pos += 1;
                -self.unary()?
            }
            Some(Tok::Op('+')) => {
                self.pos += 1;
                self.unary()?
            }
            _ => self.power()?,
        };
        self.depth -= 1;
        Ok(v)
    }

    fn power(&mut self) -> Result<f64, CalcError> {
        let base = self.atom()?;
        if let Some(Tok::Op('^')) = self.peek() {
            self.pos += 1;
            let e = self.unary()?;
            return Ok(base.powf(e));
        }
        Ok(base)
    }

    fn atom(&mut self) -> Result<f64, CalcError> {
        let at = self.at();
        match self.peek() {
            Some(Tok::Num(v)) => {
                self.pos += 1;
                Ok(v)
            }
            Some(Tok::LParen) => {
                self.pos += 1;
                let v = self.expr()?;
                if self.peek() != Some(Tok::RParen) {
                    return Err(CalcError::Unexpected(self.at()));
                }
                self.pos += 1;
                Ok(v)
            }
            Some(Tok::Ident(a, b)) => {
                self.pos += 1;
                let name = self.src[a..b].to_ascii_lowercase();
                match name.as_str() {
                    "pi" => return Ok(std::f64::consts::PI),
                    "e" => return Ok(std::f64::consts::E),
                    "tau" => return Ok(std::f64::consts::TAU),
                    _ => {}
                }
                if self.peek() != Some(Tok::LParen) {
                    return Err(CalcError::UnknownName(name));
                }
                self.pos += 1;
                let mut args = Vec::new();
                if self.peek() != Some(Tok::RParen) {
                    loop {
                        args.push(self.expr()?);
                        match self.peek() {
                            Some(Tok::Comma) => self.pos += 1,
                            Some(Tok::RParen) => break,
                            _ => return Err(CalcError::Unexpected(self.at())),
                        }
                    }
                }
                self.pos += 1;
                apply(&name, &args)
            }
            _ => Err(CalcError::Unexpected(at)),
        }
    }
}

fn apply(name: &str, a: &[f64]) -> Result<f64, CalcError> {
    let one = |f: fn(f64) -> f64| if a.len() == 1 { Ok(f(a[0])) } else { Err(CalcError::BadArity(name.to_owned())) };
    match name {
        "sqrt" => one(f64::sqrt),
        "abs" => one(f64::abs),
        "sin" => one(f64::sin),
        "cos" => one(f64::cos),
        "tan" => one(f64::tan),
        "asin" => one(f64::asin),
        "acos" => one(f64::acos),
        "atan" => one(f64::atan),
        "ln" => one(f64::ln),
        "log" | "log10" => one(f64::log10),
        "log2" => one(f64::log2),
        "exp" => one(f64::exp),
        "floor" => one(f64::floor),
        "ceil" => one(f64::ceil),
        "round" => one(f64::round),
        "min" | "max" if !a.is_empty() => Ok(a.iter().copied().fold(if name == "min" { f64::INFINITY } else { f64::NEG_INFINITY }, |x, y| if name == "min" { x.min(y) } else { x.max(y) })),
        "min" | "max" => Err(CalcError::BadArity(name.to_owned())),
        _ => Err(CalcError::UnknownName(name.to_owned())),
    }
}

/// Evaluates an expression (an optional leading `=` is ignored).
pub fn eval(input: &str) -> Result<f64, CalcError> {
    let s = input.trim().trim_start_matches('=').trim();
    if s.len() > MAX_INPUT {
        return Err(CalcError::TooLong);
    }
    if s.is_empty() {
        return Err(CalcError::Empty);
    }
    let toks = lex(s)?;
    let mut p = Parser { src: s, toks, pos: 0, depth: 0 };
    let v = p.expr()?;
    if p.pos != p.toks.len() {
        return Err(CalcError::Unexpected(p.at()));
    }
    if !v.is_finite() {
        return Err(CalcError::NotFinite);
    }
    Ok(v)
}

/// Formats with up to 12 significant digits, trimming trailing zeros.
pub fn format(v: f64) -> String {
    if v == 0.0 {
        return "0".into();
    }
    let a = v.abs();
    if !(1e-6..1e15).contains(&a) {
        return format!("{v:.6e}");
    }
    let digits = 12 - (a.log10().floor() as i32 + 1);
    let s = format!("{:.*}", digits.clamp(0, 12) as usize, v);
    if s.contains('.') { s.trim_end_matches('0').trim_end_matches('.').to_owned() } else { s }
}

/// Heuristic for auto-detecting arithmetic without an `=` prefix: requires an
/// operator or function call, and excludes date-, version- and range-like
/// strings that are far more likely to be file names.
pub fn looks_like_math(input: &str) -> bool {
    let s = input.trim();
    if s.starts_with('=') {
        return true;
    }
    let has_op = s.chars().any(|c| matches!(c, '+' | '*' | '/' | '^' | '%')) || s.contains(" - ") || s.contains('(');
    let date_like = {
        let parts: Vec<&str> = s.split(['-', '/', '.']).collect();
        parts.len() == 3 && parts.iter().all(|p| !p.is_empty() && p.len() <= 4 && p.chars().all(|c| c.is_ascii_digit()))
    };
    let only_math_chars = s.chars().all(|c| c.is_ascii_digit() || " .+-*/%^(),_".contains(c) || c.is_ascii_alphabetic());
    has_op && !date_like && only_math_chars && s.chars().any(|c| c.is_ascii_digit() || s.contains("pi"))
}

pub struct Calculator;

impl Command for Calculator {
    fn id(&self) -> &'static str {
        "calc"
    }
    fn evaluate(&self, input: &str) -> Option<CommandResult> {
        if !looks_like_math(input) {
            return None;
        }
        let explicit = input.trim().starts_with('=');
        match eval(input) {
            Ok(v) => {
                let r = format(v);
                Some(CommandResult { command: "calc", title: format!("= {r}"), subtitle: "Calculator — Enter copies the result".into(), action: Action::CopyText(r), exclusive: false })
            }
            // Errors are only shown when the user explicitly asked with `=`.
            Err(e) if explicit => Some(CommandResult { command: "calc", title: format!("Cannot evaluate: {e:?}"), subtitle: String::new(), action: Action::None, exclusive: false }),
            Err(_) => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn arithmetic_and_precedence() {
        assert_eq!(eval("1 + 2 * 3").unwrap(), 7.0);
        assert_eq!(eval("(1 + 2) * 3").unwrap(), 9.0);
        assert_eq!(eval("2 ^ 3 ^ 2").unwrap(), 512.0);
        assert_eq!(eval("-2 ^ 2").unwrap(), -4.0);
        assert_eq!(eval("10 % 4").unwrap(), 2.0);
        assert_eq!(eval("=sqrt(16) + max(1, 5, 3)").unwrap(), 9.0);
        assert_eq!(eval("0x1F + 1_000").unwrap(), 1031.0);
        assert_eq!(eval("1.5e3 / 3").unwrap(), 500.0);
        assert!((eval("2*pi").unwrap() - std::f64::consts::TAU).abs() < 1e-12);
    }

    #[test]
    fn rejects_everything_else() {
        assert_eq!(eval("1/0"), Err(CalcError::NotFinite));
        assert_eq!(eval("sqrt(-1)"), Err(CalcError::NotFinite));
        assert!(matches!(eval("system(1)"), Err(CalcError::UnknownName(_))));
        assert!(matches!(eval("1 +"), Err(CalcError::Unexpected(_))));
        assert!(matches!(eval("(1"), Err(CalcError::Unexpected(_))));
        assert!(matches!(eval("1; rm -rf /"), Err(CalcError::Unexpected(_))));
        assert!(matches!(eval("max()"), Err(CalcError::BadArity(_))));
        assert_eq!(eval(&"(".repeat(100)), Err(CalcError::TooDeep));
        assert_eq!(eval(&"-".repeat(100)), Err(CalcError::TooDeep));
        assert_eq!(eval(&"1+".repeat(200)), Err(CalcError::TooLong));
    }

    #[test]
    fn formatting() {
        assert_eq!(format(0.1 + 0.2), "0.3");
        assert_eq!(format(1.0 / 3.0), "0.333333333333");
        assert_eq!(format(1e20), "1.000000e20");
        assert_eq!(format(-42.0), "-42");
    }

    #[test]
    fn detection_avoids_file_like_queries() {
        for q in ["2+2", "3 * 4", "=1", "(1+2)", "sqrt(2)", "10 - 3", "2^10"] {
            assert!(looks_like_math(q), "{q}");
        }
        for q in ["2024-01-01", "01/02/2024", "1.2.3", "report", "a-b", "my file (1).txt", "10-3", "C++ notes"] {
            assert!(!looks_like_math(q) || eval(q).is_err(), "{q}");
        }
        assert!(Calculator.evaluate("my file (1).txt").is_none());
        assert!(Calculator.evaluate("2024-01-01").is_none());
        assert_eq!(Calculator.evaluate("2+2").unwrap().action, Action::CopyText("4".into()));
    }

    #[test]
    fn random_input_never_panics() {
        let alphabet: Vec<char> = "0123456789.+-*/%^(),e xpisqrtmaE_".chars().collect();
        let mut s = 99u64;
        for _ in 0..100_000 {
            s ^= s << 13;
            s ^= s >> 7;
            s ^= s << 17;
            let n = (s % 40) as usize;
            let mut x = s;
            let input: String = (0..n)
                .map(|_| {
                    x = x.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
                    alphabet[(x >> 33) as usize % alphabet.len()]
                })
                .collect();
            let _ = eval(&input);
        }
    }
}
