//! Command registry and built-in commands (`/s` web search, calculator).
//!
//! Commands only *describe* what would happen ([`Action`]); the UI performs
//! the action when the user explicitly activates the result, re-validating
//! URLs before handing them to the shell. Nothing here executes input.

pub mod calc;
pub mod websearch;

/// What activating a command result does.
#[derive(Clone, Debug, PartialEq)]
pub enum Action {
    /// Open an http(s) URL in the default browser.
    OpenUrl(String),
    CopyText(String),
    None,
}

#[derive(Clone, Debug, PartialEq)]
pub struct CommandResult {
    pub command: &'static str,
    pub title: String,
    pub subtitle: String,
    pub action: Action,
    /// The command claims the whole input (file search is not run).
    pub exclusive: bool,
}

pub trait Command: Send + Sync {
    fn id(&self) -> &'static str;
    /// Returns a result if this command handles `input`. Must be cheap and
    /// side-effect free: it runs on every keystroke.
    fn evaluate(&self, input: &str) -> Option<CommandResult>;
}

#[derive(Default)]
pub struct Registry {
    commands: Vec<Box<dyn Command>>,
}

impl Registry {
    /// Registry with the built-in commands.
    pub fn with_builtins(search: websearch::Provider) -> Self {
        let mut r = Registry::default();
        r.register(Box::new(websearch::WebSearch { provider: search }));
        r.register(Box::new(calc::Calculator));
        r
    }

    /// Registers a command. Duplicate ids are rejected so a plugin cannot
    /// shadow a built-in.
    pub fn register(&mut self, c: Box<dyn Command>) -> bool {
        if self.commands.iter().any(|x| x.id() == c.id()) {
            return false;
        }
        self.commands.push(c);
        true
    }

    pub fn ids(&self) -> Vec<&'static str> {
        self.commands.iter().map(|c| c.id()).collect()
    }

    /// Evaluates all commands; exclusive results come first.
    pub fn evaluate(&self, input: &str) -> Vec<CommandResult> {
        let mut v: Vec<CommandResult> = self.commands.iter().filter_map(|c| c.evaluate(input)).collect();
        v.sort_by_key(|r| !r.exclusive);
        v
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ordinary_file_queries_trigger_no_exclusive_command() {
        let r = Registry::with_builtins(websearch::Provider::duckduckgo());
        for q in ["report.pdf", "s report", "/srep", "my /s file", "2024-01-01", "v1.2.3", "a-b", "resume"] {
            assert!(r.evaluate(q).iter().all(|c| !c.exclusive), "{q}");
        }
        assert!(r.evaluate("/s cats").first().unwrap().exclusive);
    }

    #[test]
    fn duplicate_ids_are_rejected() {
        let mut r = Registry::with_builtins(websearch::Provider::duckduckgo());
        assert!(!r.register(Box::new(calc::Calculator)));
        assert_eq!(r.ids(), vec!["websearch", "calc"]);
    }
}
