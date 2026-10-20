//! Finding plugins and remembering what the user approved.
//!
//! Plugins live in `<data dir>\plugins\<folder>\plugin.toml`. Nothing in
//! that folder runs until the user approves the plugin. An approval
//! records a SHA-256 over every file in the plugin's folder and the
//! permissions granted. Any change to the files or to the requested
//! permissions requires approval again. The approvals file is kept outside
//! the plugin folders.

use crate::manifest::{Manifest, Permission};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

pub const MANIFEST_FILE: &str = "plugin.toml";
const MAX_PLUGINS: usize = 64;
const MAX_MANIFEST_BYTES: u64 = 64 << 10;
/// Bounds for hashing a plugin folder.
const MAX_FILES: usize = 2000;
const MAX_TOTAL_BYTES: u64 = 512 << 20;
const MAX_DEPTH: usize = 6;

#[derive(Clone, Debug)]
pub struct Discovered {
    pub dir: PathBuf,
    pub manifest: Result<Manifest, String>,
}

impl Discovered {
    /// Folder name for messages when the manifest is invalid.
    pub fn label(&self) -> String {
        match &self.manifest {
            Ok(m) => format!("{} ({})", m.name, m.id),
            Err(_) => self.dir.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default(),
        }
    }
}

/// Every plugin folder, sorted by folder name. Invalid manifests are
/// returned with the reason so the user can see why a plugin is ignored;
/// later folders that repeat an id or prefix are marked invalid.
pub fn discover(plugins_dir: &Path) -> Vec<Discovered> {
    let Ok(rd) = std::fs::read_dir(plugins_dir) else { return Vec::new() };
    let mut dirs: Vec<PathBuf> = rd.filter_map(|e| e.ok()).filter(|e| e.file_type().is_ok_and(|t| t.is_dir())).map(|e| e.path()).collect();
    dirs.sort();
    dirs.truncate(MAX_PLUGINS);
    let mut out: Vec<Discovered> = Vec::new();
    for dir in dirs {
        let mf = dir.join(MANIFEST_FILE);
        let manifest = match std::fs::metadata(&mf) {
            Err(_) => continue, // not a plugin folder
            Ok(m) if m.len() > MAX_MANIFEST_BYTES => Err("plugin.toml is too large".into()),
            Ok(_) => std::fs::read_to_string(&mf).map_err(|e| e.to_string()).and_then(|t| Manifest::parse(&t)),
        };
        let manifest = manifest.and_then(|m| {
            for o in out.iter().filter_map(|d| d.manifest.as_ref().ok()) {
                if o.id == m.id {
                    return Err(format!("id \"{}\" is already used by another plugin", m.id));
                }
                if m.prefix.is_some() && o.prefix == m.prefix {
                    return Err(format!("prefix \"{}\" is already used by {}", m.prefix.as_deref().unwrap_or(""), o.name));
                }
            }
            Ok(m)
        });
        out.push(Discovered { dir, manifest });
    }
    out
}

/// SHA-256 over the sorted relative paths and contents of every file in
/// the folder (bounded). Symbolic links and reparse points are refused:
/// they could point the approved contents elsewhere.
pub fn hash_folder(dir: &Path) -> Result<String, String> {
    let mut files = Vec::new();
    collect(dir, dir, 0, &mut files)?;
    files.sort();
    let mut total = 0u64;
    let mut h = Sha256::new();
    for rel in &files {
        let p = dir.join(rel);
        let data = std::fs::read(&p).map_err(|e| format!("{}: {e}", rel.display()))?;
        total += data.len() as u64;
        if total > MAX_TOTAL_BYTES {
            return Err("plugin folder is larger than 512 MB".into());
        }
        // Normalised separators so the hash does not depend on the OS.
        let name = rel.to_string_lossy().replace('\\', "/");
        h.update((name.len() as u64).to_le_bytes());
        h.update(name.as_bytes());
        h.update((data.len() as u64).to_le_bytes());
        h.update(Sha256::digest(&data));
    }
    Ok(hex(&h.finalize()))
}

fn collect(root: &Path, dir: &Path, depth: usize, out: &mut Vec<PathBuf>) -> Result<(), String> {
    if depth > MAX_DEPTH {
        return Err("plugin folder is nested too deeply".into());
    }
    for e in std::fs::read_dir(dir).map_err(|e| e.to_string())? {
        let e = e.map_err(|e| e.to_string())?;
        let t = e.file_type().map_err(|e| e.to_string())?;
        if t.is_symlink() {
            return Err(format!("{} is a link; plugin folders may not contain links", e.path().display()));
        }
        if t.is_dir() {
            collect(root, &e.path(), depth + 1, out)?;
        } else {
            out.push(e.path().strip_prefix(root).map_err(|e| e.to_string())?.to_path_buf());
            if out.len() > MAX_FILES {
                return Err("plugin folder has too many files".into());
            }
        }
    }
    Ok(())
}

pub fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Approval {
    pub sha256: String,
    pub permissions: Vec<Permission>,
    /// Seconds since the Unix epoch.
    pub approved_at: i64,
    pub version: String,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Approvals {
    #[serde(default)]
    pub plugins: BTreeMap<String, Approval>,
}

impl Approvals {
    /// A missing file is no approvals; an unreadable or malformed one is
    /// also treated as none (fail closed) and reported.
    pub fn load(path: &Path) -> (Approvals, Option<String>) {
        match std::fs::read_to_string(path) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => (Approvals::default(), None),
            Err(e) => (Approvals::default(), Some(format!("plugin approvals unreadable ({e}); every plugin needs approval again"))),
            Ok(t) => match toml::from_str(&t) {
                Ok(a) => (a, None),
                Err(e) => (Approvals::default(), Some(format!("plugin approvals malformed ({}); every plugin needs approval again", e.message()))),
            },
        }
    }

    /// Atomic replace (write to a temporary file, then rename).
    pub fn save(&self, path: &Path) -> Result<(), String> {
        let text = toml::to_string(self).map_err(|e| e.to_string())?;
        let tmp = path.with_extension("toml.tmp");
        std::fs::write(&tmp, text).map_err(|e| e.to_string())?;
        std::fs::rename(&tmp, path).map_err(|e| e.to_string())
    }

    pub fn approve(&mut self, m: &Manifest, sha256: String, now: i64) {
        self.plugins.insert(m.id.clone(), Approval { sha256, permissions: m.permission_set(), approved_at: now, version: m.version.clone() });
    }

    pub fn revoke(&mut self, id: &str) -> bool {
        self.plugins.remove(id).is_some()
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Standing {
    Approved { sha256: String },
    /// Not runnable until the user reviews it.
    NeedsApproval { reason: String, sha256: String },
    Invalid(String),
}

/// Whether this plugin may run as it is on disk now.
pub fn standing(d: &Discovered, approvals: &Approvals) -> Standing {
    let m = match &d.manifest {
        Ok(m) => m,
        Err(e) => return Standing::Invalid(e.clone()),
    };
    if !m.exe_path(&d.dir).is_file() {
        return Standing::Invalid(format!("{} is missing", m.exe));
    }
    let sha256 = match hash_folder(&d.dir) {
        Ok(h) => h,
        Err(e) => return Standing::Invalid(e),
    };
    match approvals.plugins.get(&m.id) {
        None => Standing::NeedsApproval { reason: "new plugin".into(), sha256 },
        Some(a) if a.sha256 != sha256 => Standing::NeedsApproval { reason: "its files changed since you approved it".into(), sha256 },
        Some(a) if a.permissions != m.permission_set() => Standing::NeedsApproval { reason: "it asks for different permissions".into(), sha256 },
        Some(_) => Standing::Approved { sha256 },
    }
}

/// The text of the approval prompt: everything the user is agreeing to,
/// without overstating the protection.
pub fn approval_text(d: &Discovered, m: &Manifest, sha256: &str, reason: &str) -> String {
    let mut s = format!("{} {} wants to run as a Flint plugin ({reason}).\n\n", m.name, m.version);
    if !m.description.is_empty() {
        s.push_str(&format!("{}\n\n", m.description));
    }
    s.push_str(&format!("Program: {}\nFolder fingerprint (SHA-256): {}…\n", m.exe_path(&d.dir).display(), &sha256[..16.min(sha256.len())]));
    if let Some(p) = &m.prefix {
        s.push_str(&format!("Runs when you type: {p} …\n"));
    }
    for a in &m.actions {
        s.push_str(&format!("Adds the result action: {}\n", a.title));
    }
    s.push_str("\nIt asks to:\n");
    let perms = m.permission_set();
    if perms.is_empty() {
        s.push_str("  • nothing beyond answering what you type after its prefix\n");
    }
    for p in perms {
        s.push_str(&format!("  • {}\n", p.describe()));
    }
    s.push_str(
        "\nPlugins are separate programs. Flint runs them with a time limit, memory and CPU limits, and at low integrity where Windows allows it, \
         and only passes them what you approve here. This reduces what a faulty plugin can do; it is not a security sandbox. \
         Only approve plugins from sources you trust.\n\nApprove this plugin?",
    );
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plugin(root: &Path, folder: &str, id: &str, prefix: &str) -> PathBuf {
        let d = root.join(folder);
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join(MANIFEST_FILE), format!("api = 1\nid = \"{id}\"\nname = \"{id}\"\nversion = \"1\"\nexe = \"p.exe\"\nprefix = \"{prefix}\"\n")).unwrap();
        std::fs::write(d.join("p.exe"), b"binary").unwrap();
        d
    }

    #[test]
    fn approval_is_bound_to_files_and_permissions() {
        let t = tempfile::tempdir().unwrap();
        let d = plugin(t.path(), "a", "alpha", "al");
        let found = discover(t.path());
        assert_eq!(found.len(), 1);
        let mut ap = Approvals::default();
        let Standing::NeedsApproval { sha256, .. } = standing(&found[0], &ap) else { panic!() };
        ap.approve(found[0].manifest.as_ref().unwrap(), sha256, 0);
        assert!(matches!(standing(&found[0], &ap), Standing::Approved { .. }));
        // A changed binary, an added DLL, or new permissions: approve again.
        std::fs::write(d.join("p.exe"), b"other").unwrap();
        assert!(matches!(standing(&discover(t.path())[0], &ap), Standing::NeedsApproval { reason, .. } if reason.contains("changed")));
        std::fs::write(d.join("p.exe"), b"binary").unwrap();
        assert!(matches!(standing(&discover(t.path())[0], &ap), Standing::Approved { .. }));
        std::fs::write(d.join("helper.dll"), b"x").unwrap();
        assert!(matches!(standing(&discover(t.path())[0], &ap), Standing::NeedsApproval { .. }));
        std::fs::remove_file(d.join("helper.dll")).unwrap();
        let mf = std::fs::read_to_string(d.join(MANIFEST_FILE)).unwrap();
        std::fs::write(d.join(MANIFEST_FILE), format!("{mf}permissions = [\"open_urls\"]\n")).unwrap();
        let ap2 = { let mut a = ap.clone(); a.plugins.get_mut("alpha").unwrap().sha256 = hash_folder(&d).unwrap(); a };
        assert!(matches!(standing(&discover(t.path())[0], &ap2), Standing::NeedsApproval { reason, .. } if reason.contains("permissions")));
    }

    #[test]
    fn duplicates_and_bad_manifests_are_reported_not_loaded() {
        let t = tempfile::tempdir().unwrap();
        plugin(t.path(), "a", "alpha", "al");
        plugin(t.path(), "b", "alpha", "be");
        plugin(t.path(), "c", "gamma", "al");
        std::fs::create_dir_all(t.path().join("d")).unwrap();
        std::fs::write(t.path().join("d").join(MANIFEST_FILE), "not toml [").unwrap();
        std::fs::create_dir_all(t.path().join("e-not-a-plugin")).unwrap();
        let found = discover(t.path());
        assert_eq!(found.len(), 4);
        assert!(found[0].manifest.is_ok());
        assert!(found[1].manifest.as_ref().unwrap_err().contains("id"));
        assert!(found[2].manifest.as_ref().unwrap_err().contains("prefix"));
        assert!(matches!(standing(&found[3], &Approvals::default()), Standing::Invalid(_)));
    }

    #[test]
    fn approvals_round_trip_and_fail_closed() {
        let t = tempfile::tempdir().unwrap();
        let p = t.path().join("approvals.toml");
        assert_eq!(Approvals::load(&p), (Approvals::default(), None));
        let m = Manifest::parse("api = 1\nid = \"x\"\nname = \"X\"\nversion = \"1\"\nexe = \"x.exe\"\nprefix = \"xx\"\n").unwrap();
        let mut a = Approvals::default();
        a.approve(&m, "ab".repeat(32), 5);
        a.save(&p).unwrap();
        assert_eq!(Approvals::load(&p).0, a);
        std::fs::write(&p, "plugins = 3").unwrap();
        let (none, note) = Approvals::load(&p);
        assert!(none.plugins.is_empty() && note.unwrap().contains("approval again"));
    }

    #[test]
    fn approval_text_never_claims_a_sandbox() {
        let t = tempfile::tempdir().unwrap();
        plugin(t.path(), "a", "alpha", "al");
        let d = &discover(t.path())[0];
        let text = approval_text(d, d.manifest.as_ref().unwrap(), &"0".repeat(64), "new plugin");
        assert!(text.contains("not a security sandbox") && text.contains("al …"));
    }
}
