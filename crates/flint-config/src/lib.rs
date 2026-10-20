//! Versioned user configuration, data-directory resolution (installed vs
//! portable) and hotkey parsing.
//!
//! Loading never fails: a missing file yields defaults; a malformed file is
//! moved aside and replaced with defaults; a bad section or value is reset
//! individually; a file written by a newer version is used leniently but
//! never overwritten. Every such event is returned as a [`Note`] for the UI
//! and diagnostics.

pub mod hotkey;

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use std::io;
use std::path::{Path, PathBuf};

pub use hotkey::Hotkey;

pub const CONFIG_VERSION: i64 = 1;
pub const PORTABLE_MARKER: &str = "portable.marker";
pub const APP_DIR: &str = "Flint";

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Theme {
    #[default]
    System,
    Light,
    Dark,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum FuzzyIntensity {
    Off,
    Low,
    #[default]
    Normal,
    Aggressive,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum UpdateMode {
    Off,
    /// Check and notify; downloading/installing always needs approval.
    #[default]
    Notify,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct General {
    pub hotkey: String,
    pub theme: Theme,
}

impl Default for General {
    fn default() -> Self {
        General { hotkey: "Ctrl+Space".into(), theme: Theme::System }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Search {
    pub fuzzy_intensity: FuzzyIntensity,
    /// Continue to weak path/similarity matches when strong ones are few.
    pub best_effort: bool,
    /// Also list all remaining indexed candidates (weakest tier).
    pub exhaustive_fallback: bool,
    pub result_limit: usize,
    /// Weaker stages run only until this many matches were found.
    pub enough: usize,
    pub debounce_ms: u64,
    /// 0 = no deadline.
    pub deadline_ms: u64,
}

impl Default for Search {
    fn default() -> Self {
        Search { fuzzy_intensity: FuzzyIntensity::Normal, best_effort: true, exhaustive_fallback: false, result_limit: 500, enough: 50, debounce_ms: 8, deadline_ms: 0 }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Indexing {
    /// Index every detected accessible volume.
    pub all_volumes: bool,
    /// Volume roots (e.g. `D:\`) when `all_volumes` is false.
    pub volumes: Vec<String>,
    pub include_removable: bool,
    pub include_network: bool,
    /// Path prefixes never indexed (case-insensitive).
    pub excluded_paths: Vec<String>,
    /// Extra folders indexed by traversal (e.g. network shares).
    pub extra_roots: Vec<String>,
    /// Background size/date refresh budget.
    pub metadata_refresh_per_second: u32,
    /// Run indexing threads at background priority (low CPU, I/O and memory
    /// priority; EcoQoS on Windows 11).
    pub background_priority: bool,
    /// Volumes scanned at the same time (full scans are disk-heavy).
    pub concurrent_scans: u32,
    /// Pause indexing briefly while the user is typing in the launcher.
    pub pause_while_typing: bool,
}

impl Default for Indexing {
    fn default() -> Self {
        Indexing {
            all_volumes: true,
            volumes: Vec::new(),
            include_removable: true,
            include_network: false,
            excluded_paths: Vec::new(),
            extra_roots: Vec::new(),
            metadata_refresh_per_second: 2000,
            background_priority: true,
            concurrent_scans: 1,
            pause_while_typing: true,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct WebSearch {
    pub provider: String,
    pub template: String,
}

impl Default for WebSearch {
    fn default() -> Self {
        let p = flint_commands::websearch::Provider::duckduckgo();
        WebSearch { provider: p.name, template: p.template }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Viewer {
    pub enabled: bool,
    pub max_file_mb: u64,
    pub audio_video: bool,
    pub ocr: bool,
}

impl Default for Viewer {
    fn default() -> Self {
        Viewer { enabled: false, max_file_mb: 64, audio_video: false, ocr: false }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Privacy {
    pub search_history: bool,
    pub content_indexing: bool,
    pub redact_diagnostics: bool,
}

impl Default for Privacy {
    fn default() -> Self {
        Privacy { search_history: false, content_indexing: false, redact_diagnostics: true }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct Updates {
    pub mode: UpdateMode,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct Plugins {
    pub enabled: bool,
}

/// What happens when a copied or moved item's name exists at the destination.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum OnCollision {
    /// The Windows conflict dialog asks per item.
    #[default]
    Ask,
    /// Keep both (the new item gets a numbered name).
    KeepBoth,
    Skip,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Files {
    pub on_collision: OnCollision,
    /// Ask before moving even a single item to the Recycle Bin (batches,
    /// permanent deletes and anything with warnings are always confirmed).
    pub confirm_recycle: bool,
    /// Bytes read per file by "Compare".
    pub compare_limit_mb: u64,
    /// Total bytes read by "Find duplicates".
    pub duplicate_limit_mb: u64,
}

impl Default for Files {
    fn default() -> Self {
        Files { on_collision: OnCollision::Ask, confirm_recycle: false, compare_limit_mb: 4096, duplicate_limit_mb: 2048 }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub version: i64,
    pub general: General,
    pub search: Search,
    pub indexing: Indexing,
    pub web_search: WebSearch,
    pub viewer: Viewer,
    pub privacy: Privacy,
    pub updates: Updates,
    pub plugins: Plugins,
    pub files: Files,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            version: CONFIG_VERSION,
            general: General::default(),
            search: Search::default(),
            indexing: Indexing::default(),
            web_search: WebSearch::default(),
            viewer: Viewer::default(),
            privacy: Privacy::default(),
            updates: Updates::default(),
            plugins: Plugins::default(),
            files: Files::default(),
        }
    }
}

const SECTIONS: &[&str] = &["general", "search", "indexing", "web_search", "viewer", "privacy", "updates", "plugins", "files"];

#[derive(Clone, Debug, PartialEq)]
pub enum Note {
    Created,
    /// Could not read the file; defaults are used and the file is left alone.
    Unreadable(String),
    Malformed { error: String, moved_to: Option<PathBuf> },
    Migrated { from: i64, to: i64, backup: Option<PathBuf> },
    /// Written by a newer version: used leniently, never saved over.
    NewerVersion(i64),
    SectionReset { section: String, error: String },
    InvalidValue { field: String, error: String },
    /// Unknown keys are dropped on the next save.
    UnknownKeys(Vec<String>),
    SaveFailed(String),
}

pub struct Loaded {
    pub config: Config,
    pub notes: Vec<Note>,
    /// Saving would destroy information (newer or unreadable file).
    pub read_only: bool,
}

type Migration = fn(&mut toml::Table);

/// Ordered migrations: entry `i` upgrades version `i` to `i + 1`.
/// v0 = file without a `version` key (treated as the v1 layout).
const MIGRATIONS: &[Migration] = &[|_| {}];

fn migrate_with(t: &mut toml::Table, from: i64, migrations: &[Migration]) -> i64 {
    let mut v = from.max(0);
    while (v as usize) < migrations.len() {
        migrations[v as usize](t);
        v += 1;
    }
    t.insert("version".into(), toml::Value::Integer(v));
    v
}

fn section<T: DeserializeOwned + Default>(t: &toml::Table, name: &str, notes: &mut Vec<Note>) -> T {
    match t.get(name) {
        None => T::default(),
        Some(v) => v.clone().try_into().unwrap_or_else(|e: toml::de::Error| {
            notes.push(Note::SectionReset { section: name.to_owned(), error: e.message().to_owned() });
            T::default()
        }),
    }
}

impl Config {
    /// Fixes invalid values in place, reporting each.
    pub fn validate(&mut self) -> Vec<Note> {
        let mut notes = Vec::new();
        let d = Config::default();
        let mut bad = |field: &str, error: String| notes.push(Note::InvalidValue { field: field.to_owned(), error });
        if let Err(e) = Hotkey::parse(&self.general.hotkey) {
            bad("general.hotkey", e.to_string());
            self.general.hotkey = d.general.hotkey.clone();
        }
        if !(1..=10_000).contains(&self.search.result_limit) {
            bad("search.result_limit", format!("{} not in 1..=10000", self.search.result_limit));
            self.search.result_limit = d.search.result_limit;
        }
        if self.search.enough > self.search.result_limit {
            bad("search.enough", "larger than result_limit".into());
            self.search.enough = self.search.result_limit.min(d.search.enough);
        }
        if self.search.debounce_ms > 500 {
            bad("search.debounce_ms", "more than 500 ms".into());
            self.search.debounce_ms = d.search.debounce_ms;
        }
        let provider = flint_commands::websearch::Provider { name: self.web_search.provider.clone(), template: self.web_search.template.clone() };
        if let Err(e) = provider.validate() {
            bad("web_search.template", format!("{e:?}"));
            self.web_search = d.web_search.clone();
        }
        if self.viewer.max_file_mb == 0 || self.viewer.max_file_mb > 4096 {
            bad("viewer.max_file_mb", "must be 1..=4096".into());
            self.viewer.max_file_mb = d.viewer.max_file_mb;
        }
        notes
    }

    pub fn hotkey(&self) -> Hotkey {
        Hotkey::parse(&self.general.hotkey).unwrap_or_else(|_| Hotkey::parse(&General::default().hotkey).unwrap())
    }

    pub fn to_toml(&self) -> String {
        format!("# Flint settings. Edit while Flint is closed, or use the settings window.\n{}", toml::to_string_pretty(self).expect("config serializes"))
    }
}

fn sibling(path: &Path, suffix: &str) -> PathBuf {
    let mut s = path.as_os_str().to_owned();
    s.push(suffix);
    PathBuf::from(s)
}

fn free_name(path: &Path, suffix: &str) -> PathBuf {
    let base = sibling(path, suffix);
    if !base.exists() {
        return base;
    }
    (1..).map(|i| sibling(path, &format!("{suffix}.{i}"))).find(|p| !p.exists()).unwrap()
}

/// Atomically replaces `path`: writes a temp file, flushes it, keeps the
/// previous file as `<path>.bak`, then renames the temp file into place.
pub fn save(path: &Path, cfg: &Config) -> io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = sibling(path, ".tmp");
    {
        use std::io::Write;
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(cfg.to_toml().as_bytes())?;
        f.sync_all()?;
    }
    if path.exists() {
        std::fs::copy(path, sibling(path, ".bak"))?;
    }
    std::fs::rename(&tmp, path)
}

pub fn load(path: &Path) -> Loaded {
    load_with(path, MIGRATIONS)
}

fn load_with(path: &Path, migrations: &[Migration]) -> Loaded {
    let mut notes = Vec::new();
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            let config = Config::default();
            notes.push(Note::Created);
            if let Err(e) = save(path, &config) {
                notes.push(Note::SaveFailed(e.to_string()));
            }
            return Loaded { config, notes, read_only: false };
        }
        Err(e) => {
            // Includes invalid UTF-8: keep the file untouched for the user.
            notes.push(Note::Unreadable(e.to_string()));
            return Loaded { config: Config::default(), notes, read_only: true };
        }
    };
    let mut table: toml::Table = match text.parse() {
        Ok(t) => t,
        Err(e) => {
            let moved = free_name(path, ".broken");
            let moved_to = std::fs::rename(path, &moved).ok().map(|_| moved);
            notes.push(Note::Malformed { error: e.to_string(), moved_to });
            let config = Config::default();
            if let Err(e) = save(path, &config) {
                notes.push(Note::SaveFailed(e.to_string()));
            }
            return Loaded { config, notes, read_only: false };
        }
    };
    let current = migrations.len() as i64;
    let found = table.get("version").and_then(|v| v.as_integer()).unwrap_or(0);
    let mut read_only = false;
    let mut needs_save = false;
    if found > current {
        notes.push(Note::NewerVersion(found));
        read_only = true;
    } else if found < current {
        let backup = free_name(path, &format!(".v{found}.bak"));
        let backup = std::fs::copy(path, &backup).ok().map(|_| backup);
        let to = migrate_with(&mut table, found, migrations);
        notes.push(Note::Migrated { from: found, to, backup });
        needs_save = true;
    }
    let unknown: Vec<String> = table.keys().filter(|k| *k != "version" && !SECTIONS.contains(&k.as_str())).cloned().collect();
    if !unknown.is_empty() && !read_only {
        notes.push(Note::UnknownKeys(unknown));
    }
    let mut config = Config {
        version: current,
        general: section(&table, "general", &mut notes),
        search: section(&table, "search", &mut notes),
        indexing: section(&table, "indexing", &mut notes),
        web_search: section(&table, "web_search", &mut notes),
        viewer: section(&table, "viewer", &mut notes),
        privacy: section(&table, "privacy", &mut notes),
        updates: section(&table, "updates", &mut notes),
        plugins: section(&table, "plugins", &mut notes),
        files: section(&table, "files", &mut notes),
    };
    let fixes = config.validate();
    needs_save |= !fixes.is_empty() || notes.iter().any(|n| matches!(n, Note::SectionReset { .. }));
    notes.extend(fixes);
    if needs_save && !read_only {
        if let Err(e) = save(path, &config) {
            notes.push(Note::SaveFailed(e.to_string()));
        }
    }
    Loaded { config, notes, read_only }
}

/// Where Flint keeps its data.
///
/// * **Installed**: `%LOCALAPPDATA%\Flint\` (per-user, not roamed: the index
///   is machine-specific). Uninstall removes this directory.
/// * **Portable**: if `portable.marker` exists next to the executable,
///   everything lives in `<exe dir>\data\` and nothing is written elsewhere.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Paths {
    pub portable: bool,
    pub data_dir: PathBuf,
    pub config: PathBuf,
    pub index_db: PathBuf,
    pub snapshots: PathBuf,
    pub logs: PathBuf,
}

pub fn resolve_paths(exe_dir: &Path, local_app_data: Option<&Path>) -> Result<Paths, String> {
    let portable = exe_dir.join(PORTABLE_MARKER).is_file();
    let data_dir = if portable {
        exe_dir.join("data")
    } else {
        local_app_data.ok_or("LOCALAPPDATA is not set and portable.marker is absent")?.join(APP_DIR)
    };
    Ok(Paths {
        portable,
        config: data_dir.join("config.toml"),
        index_db: data_dir.join("index.db"),
        snapshots: data_dir.join("snapshots"),
        logs: data_dir.join("logs"),
        data_dir,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dir() -> tempfile::TempDir {
        tempfile::tempdir().unwrap()
    }

    #[test]
    fn missing_file_creates_defaults_and_round_trips() {
        let d = dir();
        let p = d.path().join("config.toml");
        let l = load(&p);
        assert_eq!(l.notes, vec![Note::Created]);
        assert_eq!(l.config, Config::default());
        let again = load(&p);
        assert!(again.notes.is_empty(), "{:?}", again.notes);
        assert_eq!(again.config, Config::default());
    }

    #[test]
    fn malformed_file_is_moved_aside() {
        let d = dir();
        let p = d.path().join("config.toml");
        std::fs::write(&p, "this is = = not toml [[[").unwrap();
        let l = load(&p);
        let Note::Malformed { moved_to: Some(m), .. } = &l.notes[0] else { panic!("{:?}", l.notes) };
        assert_eq!(std::fs::read_to_string(m).unwrap(), "this is = = not toml [[[");
        assert_eq!(l.config, Config::default());
        assert!(load(&p).notes.is_empty());
    }

    #[test]
    fn bad_section_and_values_reset_individually() {
        let d = dir();
        let p = d.path().join("config.toml");
        std::fs::write(
            &p,
            "version = 1\n[general]\nhotkey = \"Alt+F4\"\ntheme = \"dark\"\n[search]\nresult_limit = \"lots\"\n[indexing]\ninclude_network = true\n[web_search]\ntemplate = \"javascript:{query}\"\n[mystery]\nx = 1\n",
        )
        .unwrap();
        let l = load(&p);
        let c = &l.config;
        assert_eq!(c.general.theme, Theme::Dark, "valid values in other fields survive");
        assert_eq!(c.general.hotkey, "Ctrl+Space");
        assert_eq!(c.search, Search::default());
        assert!(c.indexing.include_network);
        assert_eq!(c.web_search, WebSearch::default());
        assert!(l.notes.iter().any(|n| matches!(n, Note::SectionReset { section, .. } if section == "search")));
        assert!(l.notes.iter().any(|n| matches!(n, Note::InvalidValue { field, .. } if field == "general.hotkey")));
        assert!(l.notes.iter().any(|n| matches!(n, Note::InvalidValue { field, .. } if field == "web_search.template")));
        assert!(l.notes.contains(&Note::UnknownKeys(vec!["mystery".into()])));
        // The corrected file was saved; the original kept as .bak.
        assert!(load(&p).notes.is_empty());
        assert!(sibling(&p, ".bak").exists());
    }

    #[test]
    fn newer_version_is_never_overwritten() {
        let d = dir();
        let p = d.path().join("config.toml");
        let text = "version = 99\nfuture_feature = true\n[general]\ntheme = \"light\"\n";
        std::fs::write(&p, text).unwrap();
        let l = load(&p);
        assert!(l.read_only);
        assert_eq!(l.config.general.theme, Theme::Light);
        assert_eq!(std::fs::read_to_string(&p).unwrap(), text);
    }

    #[test]
    fn migrations_run_in_order_with_backup() {
        let d = dir();
        let p = d.path().join("config.toml");
        std::fs::write(&p, "[general]\nkey = \"Ctrl+K\"\n").unwrap();
        // Test-only history: v0 -> v1 renames general.key to general.hotkey,
        // v1 -> v2 sets a theme.
        let ms: &[Migration] = &[
            |t| {
                if let Some(g) = t.get_mut("general").and_then(|g| g.as_table_mut()) {
                    if let Some(k) = g.remove("key") {
                        g.insert("hotkey".into(), k);
                    }
                }
            },
            |t| {
                t.entry("general").or_insert(toml::Value::Table(Default::default())).as_table_mut().unwrap().insert("theme".into(), "dark".into());
            },
        ];
        let l = load_with(&p, ms);
        assert!(matches!(&l.notes[0], Note::Migrated { from: 0, to: 2, backup: Some(_) }), "{:?}", l.notes);
        assert_eq!(l.config.general.hotkey, "Ctrl+K");
        assert_eq!(l.config.general.theme, Theme::Dark);
        assert_eq!(l.config.version, 2);
        assert!(std::fs::read_to_string(&p).unwrap().contains("version = 2"));
    }

    #[cfg(unix)]
    #[test]
    fn unreadable_file_is_left_alone() {
        let d = dir();
        let p = d.path().join("config.toml");
        std::fs::write(&p, [0xffu8, 0xfe, 0x00]).unwrap();
        let l = load(&p);
        assert!(l.read_only);
        assert!(matches!(l.notes[0], Note::Unreadable(_)));
        assert_eq!(std::fs::read(&p).unwrap(), vec![0xff, 0xfe, 0x00]);
    }

    #[test]
    fn portable_vs_installed_paths() {
        let d = dir();
        let lad = d.path().join("lad");
        let p = resolve_paths(d.path(), Some(&lad)).unwrap();
        assert!(!p.portable);
        assert_eq!(p.config, lad.join("Flint").join("config.toml"));
        std::fs::write(d.path().join(PORTABLE_MARKER), "").unwrap();
        let p = resolve_paths(d.path(), None).unwrap();
        assert!(p.portable);
        assert_eq!(p.index_db, d.path().join("data").join("index.db"));
        assert!(resolve_paths(&d.path().join("nowhere"), None).is_err());
    }
}
