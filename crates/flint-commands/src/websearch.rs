//! `/s query` — explicit web search with a configurable provider.

use crate::{Action, Command, CommandResult};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Provider {
    pub name: String,
    /// URL template containing `{query}`, e.g. `https://duckduckgo.com/?q={query}`.
    pub template: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ProviderError {
    NotHttp,
    NoHost,
    MissingPlaceholder,
    /// The placeholder must be in the query or fragment, not the host/path
    /// authority, so user input can never change which site is opened.
    PlaceholderInAuthority,
    ControlCharacters,
}

impl Provider {
    pub fn duckduckgo() -> Self {
        Provider { name: "DuckDuckGo".into(), template: "https://duckduckgo.com/?q={query}".into() }
    }
    pub fn presets() -> Vec<Provider> {
        vec![
            Self::duckduckgo(),
            Provider { name: "Google".into(), template: "https://www.google.com/search?q={query}".into() },
            Provider { name: "Bing".into(), template: "https://www.bing.com/search?q={query}".into() },
            Provider { name: "Wikipedia".into(), template: "https://en.wikipedia.org/w/index.php?search={query}".into() },
        ]
    }

    /// Validates a user-configured template.
    pub fn validate(&self) -> Result<(), ProviderError> {
        let t = &self.template;
        if t.chars().any(|c| c.is_control() || c == ' ') {
            return Err(ProviderError::ControlCharacters);
        }
        let rest = t.strip_prefix("https://").or_else(|| t.strip_prefix("http://")).ok_or(ProviderError::NotHttp)?;
        let auth_end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
        let host = &rest[..auth_end];
        if host.is_empty() || host.starts_with('.') || host.contains('@') {
            return Err(ProviderError::NoHost);
        }
        if !t.contains("{query}") {
            return Err(ProviderError::MissingPlaceholder);
        }
        let first_q = rest.find(['?', '#']).unwrap_or(rest.len());
        if rest.find("{query}").unwrap() < first_q {
            return Err(ProviderError::PlaceholderInAuthority);
        }
        Ok(())
    }

    pub fn url_for(&self, query: &str) -> Result<String, ProviderError> {
        self.validate()?;
        Ok(self.template.replace("{query}", &percent_encode(query)))
    }
}

/// RFC 3986 percent-encoding of everything but unreserved characters, over
/// UTF-8 bytes. Quotes, `&`, `#`, `+` and spaces are all encoded, so the
/// query cannot inject parameters and quoted phrases arrive intact.
pub fn percent_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len() * 3);
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => out.push(b as char),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// Extracts the query of an explicit `/s` invocation: `/s` followed by
/// whitespace (or nothing). `/sfoo` and text merely containing `/s` do not
/// count.
pub fn parse_invocation(input: &str) -> Option<&str> {
    let rest = input.trim_start().strip_prefix("/s")?;
    if rest.is_empty() {
        return Some("");
    }
    rest.starts_with(char::is_whitespace).then(|| rest.trim())
}

/// Safety check the UI applies again right before opening a URL.
pub fn is_safe_url(url: &str) -> bool {
    (url.starts_with("https://") || url.starts_with("http://")) && !url.chars().any(|c| c.is_control() || c == ' ' || c == '"')
}

pub struct WebSearch {
    pub provider: Provider,
}

impl Command for WebSearch {
    fn id(&self) -> &'static str {
        "websearch"
    }

    fn evaluate(&self, input: &str) -> Option<CommandResult> {
        let q = parse_invocation(input)?;
        let (title, action) = if q.is_empty() {
            (format!("Search {} — type a query", self.provider.name), Action::None)
        } else {
            match self.provider.url_for(q) {
                Ok(url) => (format!("Search {} for “{q}”", self.provider.name), Action::OpenUrl(url)),
                Err(e) => (format!("Web search provider is misconfigured: {e:?}"), Action::None),
            }
        };
        let subtitle = match &action {
            Action::OpenUrl(u) => u.clone(),
            _ => String::new(),
        };
        Some(CommandResult { command: self.id(), title, subtitle, action, exclusive: true })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn explicit_prefix_only() {
        assert_eq!(parse_invocation("/s cats"), Some("cats"));
        assert_eq!(parse_invocation("  /s   two words  "), Some("two words"));
        assert_eq!(parse_invocation("/s"), Some(""));
        assert_eq!(parse_invocation("/sfoo"), None);
        assert_eq!(parse_invocation("foo /s bar"), None);
        assert_eq!(parse_invocation("s cats"), None);
    }

    #[test]
    fn encodes_safely_and_preserves_quotes() {
        let p = Provider::duckduckgo();
        assert_eq!(p.url_for(r#""exact phrase" a&b=c #x +y ü"#).unwrap(), "https://duckduckgo.com/?q=%22exact%20phrase%22%20a%26b%3Dc%20%23x%20%2By%20%C3%BC");
        assert!(is_safe_url(&p.url_for("\"x\"\n\r").unwrap()));
    }

    #[test]
    fn rejects_dangerous_templates() {
        let t = |s: &str| Provider { name: "x".into(), template: s.into() }.validate();
        assert_eq!(t("javascript:alert({query})"), Err(ProviderError::NotHttp));
        assert_eq!(t("file:///C:/{query}"), Err(ProviderError::NotHttp));
        assert_eq!(t("https://{query}.evil.com/"), Err(ProviderError::PlaceholderInAuthority));
        assert_eq!(t("https://example.com/{query}"), Err(ProviderError::PlaceholderInAuthority));
        assert_eq!(t("https://user@evil.com/?q={query}"), Err(ProviderError::NoHost));
        assert_eq!(t("https://example.com/search"), Err(ProviderError::MissingPlaceholder));
        assert_eq!(t("https://example.com/?q={query}"), Ok(()));
        for p in Provider::presets() {
            assert_eq!(p.validate(), Ok(()), "{}", p.name);
        }
    }

    #[test]
    fn command_result() {
        let c = WebSearch { provider: Provider::duckduckgo() };
        let r = c.evaluate("/s rust lang").unwrap();
        assert!(r.exclusive);
        assert_eq!(r.action, Action::OpenUrl("https://duckduckgo.com/?q=rust%20lang".into()));
        assert_eq!(c.evaluate("/s").unwrap().action, Action::None);
        assert!(c.evaluate("rust lang").is_none());
    }
}
