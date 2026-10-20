//! `/diag`, `/update`, `/plugins`: the launcher's own screens. Like `/s`,
//! they need the leading slash, so a file search for "diag" is unaffected.

use crate::{Action, AppAction, Command, CommandResult};

pub struct AppCommands;

const TABLE: &[(&str, AppAction, &str, &str)] = &[
    ("/diag", AppAction::Diagnostics, "Diagnostics", "Enter shows indexing, search and plugin health (redacted by default)"),
    ("/update", AppAction::CheckUpdates, "Check for updates", "Enter checks the signed release information; nothing installs without your approval"),
    ("/plugins", AppAction::Plugins, "Plugins", "Enter lists installed plugins, their permissions and health"),
];

impl Command for AppCommands {
    fn id(&self) -> &'static str {
        "app"
    }

    fn evaluate(&self, input: &str) -> Option<CommandResult> {
        let t = input.trim();
        TABLE.iter().find(|(word, ..)| t.eq_ignore_ascii_case(word)).map(|(_, a, title, sub)| CommandResult { command: "app", title: (*title).into(), subtitle: (*sub).into(), action: Action::App(*a), exclusive: true })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_slash_forms_match() {
        assert_eq!(AppCommands.evaluate(" /DIAG ").unwrap().action, Action::App(AppAction::Diagnostics));
        assert_eq!(AppCommands.evaluate("/update").unwrap().action, Action::App(AppAction::CheckUpdates));
        for q in ["diag", "update", "/diagnostics", "plugins", "/plugins x"] {
            assert!(AppCommands.evaluate(q).is_none(), "{q}");
        }
    }
}
