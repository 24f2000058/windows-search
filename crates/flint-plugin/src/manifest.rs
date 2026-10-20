//! `plugin.toml`: what a plugin is, what it may ask for, and its limits.
//!
//! ```toml
//! api = 1
//! id = "epoch"
//! name = "Epoch converter"
//! version = "0.1.0"
//! exe = "flint-plugin-epoch.exe"
//! prefix = "epoch"                 # typed as "epoch 1700000000"
//! permissions = ["open_urls"]      # see Permission
//!
//! [[actions]]                      # entries in the result context menu
//! id = "sha256"
//! title = "Copy SHA-256"           # needs the "selection_paths" permission
//!
//! [limits]
//! timeout_ms = 1500
//! memory_mb = 64
//! ```
//!
//! Validation is strict: an invalid manifest is reported and the plugin is
//! not loaded. The executable must be a plain relative path inside the
//! plugin's own folder.

use serde::{Deserialize, Serialize};
use std::path::{Component, Path, PathBuf};

/// Protocol and manifest version this host implements.
pub const API_VERSION: u32 = 1;

/// Prefixes the launcher already uses; a plugin cannot take them.
pub const RESERVED_PREFIXES: &[&str] = &["/s", "content", "ext", "size", "modified", "dm", "type", "path", "folder", "file", "diag", "update", "plugins"];

/// What the host gives a plugin or does on its behalf. Granted only by the
/// user's approval. These limit what *Flint* passes to and performs for the
/// plugin; they do not restrict what the plugin program itself can do with
/// the rights it runs with (see ADR-019).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Permission {
    /// Result actions receive the full paths of the selected results.
    SelectionPaths,
    /// Results may open http(s) URLs in the default browser (on Enter).
    OpenUrls,
    /// The plugin may start child processes (otherwise its Job Object
    /// allows exactly one process on Windows).
    LaunchProcesses,
    /// The plugin says it uses the network. Declared and shown at approval;
    /// **not enforced** by Flint.
    Network,
}

impl Permission {
    pub fn describe(self) -> &'static str {
        match self {
            Permission::SelectionPaths => "receive the paths of results you select for its actions",
            Permission::OpenUrls => "open web links in your browser when you press Enter on its results",
            Permission::LaunchProcesses => "start other programs",
            Permission::Network => "use the network (declared by the plugin; Flint cannot enforce this)",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ActionDecl {
    pub id: String,
    pub title: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Limits {
    /// Per-request time limit; the plugin process is killed when exceeded.
    pub timeout_ms: u64,
    /// Process memory limit (Windows Job Object).
    pub memory_mb: u64,
}

impl Default for Limits {
    fn default() -> Self {
        Limits { timeout_ms: 1500, memory_mb: 64 }
    }
}

/// Upper bounds a manifest cannot exceed.
pub const MAX_TIMEOUT_MS: u64 = 10_000;
pub const MAX_MEMORY_MB: u64 = 512;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    pub api: u32,
    pub id: String,
    pub name: String,
    pub version: String,
    #[serde(default)]
    pub description: String,
    pub exe: String,
    #[serde(default)]
    pub prefix: Option<String>,
    #[serde(default)]
    pub permissions: Vec<Permission>,
    #[serde(default)]
    pub actions: Vec<ActionDecl>,
    #[serde(default)]
    pub limits: Limits,
}

fn valid_id(s: &str) -> bool {
    (1..=32).contains(&s.len()) && s.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-') && !s.starts_with('-')
}

impl Manifest {
    pub fn parse(text: &str) -> Result<Manifest, String> {
        let m: Manifest = toml::from_str(text).map_err(|e| e.message().to_owned())?;
        m.validate()?;
        Ok(m)
    }

    pub fn validate(&self) -> Result<(), String> {
        if self.api != API_VERSION {
            return Err(format!("plugin API version {} is not supported (this Flint supports {API_VERSION})", self.api));
        }
        if !valid_id(&self.id) {
            return Err("id must be 1-32 characters of a-z, 0-9 and '-'".into());
        }
        if self.name.trim().is_empty() || self.name.chars().count() > 64 || self.name.chars().any(char::is_control) {
            return Err("name must be 1-64 printable characters".into());
        }
        if self.version.is_empty() || self.version.len() > 32 || self.version.chars().any(char::is_control) {
            return Err("version must be 1-32 printable characters".into());
        }
        if self.description.chars().count() > 300 || self.description.chars().any(|c| c.is_control() && c != '\n') {
            return Err("description must be at most 300 printable characters".into());
        }
        relative_exe(&self.exe)?;
        if let Some(p) = &self.prefix {
            if !valid_id(p) {
                return Err("prefix must be 1-32 characters of a-z, 0-9 and '-'".into());
            }
            if RESERVED_PREFIXES.contains(&p.as_str()) {
                return Err(format!("prefix \"{p}\" is used by Flint itself"));
            }
        }
        if self.prefix.is_none() && self.actions.is_empty() {
            return Err("a plugin needs a prefix (commands) or actions (result actions)".into());
        }
        let mut seen = std::collections::HashSet::new();
        for a in &self.actions {
            if !valid_id(&a.id) || !seen.insert(&a.id) {
                return Err(format!("action id \"{}\" is invalid or repeated", a.id));
            }
            if a.title.trim().is_empty() || a.title.chars().count() > 48 || a.title.chars().any(char::is_control) {
                return Err(format!("action \"{}\" needs a 1-48 character title", a.id));
            }
        }
        if !self.actions.is_empty() && !self.permissions.contains(&Permission::SelectionPaths) {
            return Err("result actions need the \"selection_paths\" permission".into());
        }
        if self.actions.len() > 8 {
            return Err("at most 8 actions".into());
        }
        if !(50..=MAX_TIMEOUT_MS).contains(&self.limits.timeout_ms) {
            return Err(format!("limits.timeout_ms must be 50..={MAX_TIMEOUT_MS}"));
        }
        if !(8..=MAX_MEMORY_MB).contains(&self.limits.memory_mb) {
            return Err(format!("limits.memory_mb must be 8..={MAX_MEMORY_MB}"));
        }
        Ok(())
    }

    /// Permissions, sorted and de-duplicated.
    pub fn permission_set(&self) -> Vec<Permission> {
        let mut p = self.permissions.clone();
        p.sort();
        p.dedup();
        p
    }

    pub fn exe_path(&self, dir: &Path) -> PathBuf {
        dir.join(&self.exe)
    }
}

/// The executable path must stay inside the plugin folder: relative, no
/// `..`, no drive or root, no stream (`:`).
fn relative_exe(exe: &str) -> Result<(), String> {
    let p = Path::new(exe);
    let ok = !exe.is_empty()
        && exe.len() <= 128
        && !exe.contains(':')
        && !exe.starts_with('\\')
        && !exe.starts_with('/')
        && p.components().all(|c| matches!(c, Component::Normal(_)))
        && !exe.split(['/', '\\']).any(|part| part == ".." || part.is_empty());
    if ok { Ok(()) } else { Err("exe must be a relative path inside the plugin folder".into()) }
}

#[cfg(test)]
mod tests {
    use super::*;

    const GOOD: &str = "api = 1\nid = \"epoch\"\nname = \"Epoch\"\nversion = \"0.1.0\"\nexe = \"epoch.exe\"\nprefix = \"epoch\"\n";

    #[test]
    fn accepts_a_minimal_manifest_with_default_limits() {
        let m = Manifest::parse(GOOD).unwrap();
        assert_eq!((m.limits.timeout_ms, m.limits.memory_mb), (1500, 64));
        assert!(m.permission_set().is_empty());
    }

    #[test]
    fn rejects_what_could_escape_or_confuse() {
        let bad = [
            ("exe = \"epoch.exe\"", "exe = \"../evil.exe\""),
            ("exe = \"epoch.exe\"", "exe = \"C:\\\\Windows\\\\cmd.exe\""),
            ("exe = \"epoch.exe\"", "exe = \"/bin/sh\""),
            ("exe = \"epoch.exe\"", "exe = \"bin\\\\..\\\\..\\\\x.exe\""),
            ("exe = \"epoch.exe\"", "exe = \"epoch.exe:stream\""),
            ("prefix = \"epoch\"", "prefix = \"content\""),
            ("prefix = \"epoch\"", "prefix = \"Epoch Now\""),
            ("id = \"epoch\"", "id = \"../x\""),
            ("api = 1", "api = 2"),
            ("name = \"Epoch\"", "name = \"\""),
        ];
        for (from, to) in bad {
            let text = GOOD.replace(from, to);
            assert!(Manifest::parse(&text).is_err(), "{to}");
        }
        assert!(Manifest::parse(&format!("{GOOD}surprise = true\n")).is_err(), "unknown keys are rejected");
        assert!(Manifest::parse(&format!("{GOOD}[limits]\ntimeout_ms = 999999\n")).is_err());
        assert!(Manifest::parse(&format!("{GOOD}permissions = [\"root_access\"]\n")).is_err());
    }

    #[test]
    fn actions_require_the_selection_permission() {
        let with_action = format!("{GOOD}[[actions]]\nid = \"hash\"\ntitle = \"Hash\"\n");
        assert!(Manifest::parse(&with_action).unwrap_err().contains("selection_paths"));
        let ok = with_action.replace("prefix = \"epoch\"\n", "prefix = \"epoch\"\npermissions = [\"selection_paths\"]\n");
        assert_eq!(Manifest::parse(&ok).unwrap().actions.len(), 1);
    }
}
