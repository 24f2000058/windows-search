//! Which volumes to index and how (platform-neutral, tested here; the
//! Win32 side only gathers [`Candidate`]s and executes the plan).

use flint_config::Indexing;

/// `GetDriveTypeW` values (winbase.h).
pub mod drive {
    pub const REMOVABLE: u32 = 2;
    pub const FIXED: u32 = 3;
    pub const REMOTE: u32 = 4;
    pub const CDROM: u32 = 5;
    pub const RAMDISK: u32 = 6;
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Candidate {
    /// Stable identity, e.g. `\\?\Volume{GUID}\`.
    pub key: String,
    /// Mount points, e.g. `C:\`.
    pub mounts: Vec<String>,
    pub filesystem: String,
    pub drive_type: u32,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Mode {
    /// Try the NTFS change journal; fall back to watched traversal if it
    /// cannot be opened (no elevation, journal disabled).
    JournalOrTraversal,
    /// Traversal plus change notifications (FAT/exFAT/ReFS, shares, folders).
    WatchedTraversal,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Planned {
    pub key: String,
    /// Display root without trailing separator, e.g. `C:`.
    pub root_name: String,
    /// Path to traverse/watch, e.g. `C:\`.
    pub root_path: String,
    pub mode: Mode,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Skipped {
    pub key: String,
    pub reason: &'static str,
}

fn norm(root: &str) -> String {
    root.trim_end_matches(['\\', '/']).to_ascii_uppercase()
}

/// Applies the indexing settings. Every candidate is either planned or
/// listed as skipped with a reason, so nothing disappears silently.
pub fn plan(cands: &[Candidate], cfg: &Indexing) -> (Vec<Planned>, Vec<Skipped>) {
    let (mut planned, mut skipped) = (Vec::new(), Vec::new());
    let listed: Vec<String> = cfg.volumes.iter().map(|v| norm(v)).collect();
    for c in cands {
        let skip = |reason| Skipped { key: c.key.clone(), reason };
        let Some(mount) = c.mounts.first() else {
            skipped.push(skip("not mounted (no drive letter or folder)"));
            continue;
        };
        let explicitly = c.mounts.iter().any(|m| listed.contains(&norm(m)));
        let reason = if !cfg.all_volumes && !explicitly {
            Some("not in the configured volume list")
        } else {
            match c.drive_type {
                drive::FIXED | drive::RAMDISK => None,
                drive::REMOVABLE if cfg.include_removable || explicitly => None,
                drive::REMOVABLE => Some("removable drives are excluded in settings"),
                drive::REMOTE if cfg.include_network || explicitly => None,
                drive::REMOTE => Some("network drives are excluded in settings"),
                drive::CDROM if explicitly => None,
                drive::CDROM => Some("optical drive (add it explicitly to index it)"),
                _ => Some("unknown drive type"),
            }
        };
        if let Some(r) = reason {
            skipped.push(skip(r));
            continue;
        }
        let ntfs = c.filesystem.eq_ignore_ascii_case("NTFS") && c.drive_type != drive::REMOTE;
        planned.push(Planned {
            // Journal and traversal modes use different identities for the
            // root, so they must never share stored state.
            key: if ntfs { c.key.clone() } else { format!("{}|walk", c.key) },
            root_name: mount.trim_end_matches(['\\', '/']).to_owned(),
            root_path: mount.clone(),
            mode: if ntfs { Mode::JournalOrTraversal } else { Mode::WatchedTraversal },
        });
    }
    for root in &cfg.extra_roots {
        planned.push(Planned { key: format!("folder:{}", norm(root)), root_name: root.trim_end_matches(['\\', '/']).to_owned(), root_path: root.clone(), mode: Mode::WatchedTraversal });
    }
    (planned, skipped)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn c(key: &str, mount: &str, fs: &str, ty: u32) -> Candidate {
        Candidate { key: key.into(), mounts: if mount.is_empty() { vec![] } else { vec![mount.into()] }, filesystem: fs.into(), drive_type: ty }
    }

    #[test]
    fn plans_by_drive_type_filesystem_and_settings() {
        let cands = vec![
            c("v1", "C:\\", "NTFS", drive::FIXED),
            c("v2", "E:\\", "exFAT", drive::REMOVABLE),
            c("v3", "Z:\\", "NTFS", drive::REMOTE),
            c("v4", "", "NTFS", drive::FIXED),
            c("v5", "D:\\", "UDF", drive::CDROM),
            c("v6", "R:\\", "ReFS", drive::FIXED),
        ];
        let mut cfg = Indexing { extra_roots: vec!["\\\\nas\\photos\\".into()], ..Default::default() };
        let (p, s) = plan(&cands, &cfg);
        let keys: Vec<(&str, &Mode)> = p.iter().map(|x| (x.key.as_str(), &x.mode)).collect();
        assert_eq!(keys, vec![("v1", &Mode::JournalOrTraversal), ("v2|walk", &Mode::WatchedTraversal), ("v6|walk", &Mode::WatchedTraversal), ("folder:\\\\NAS\\PHOTOS", &Mode::WatchedTraversal)]);
        assert_eq!(p[0].root_name, "C:");
        assert_eq!(s.len(), 3, "network, unmounted and optical are skipped with reasons: {s:?}");
        assert!(s.iter().all(|x| !x.reason.is_empty()));

        cfg.include_removable = false;
        cfg.include_network = true;
        let (p, _) = plan(&cands, &cfg);
        assert!(p.iter().any(|x| x.key == "v3|walk"), "network NTFS is traversed, never journaled");
        assert!(!p.iter().any(|x| x.key.starts_with("v2")));

        let only = Indexing { all_volumes: false, volumes: vec!["d:".into()], ..Default::default() };
        let (p, s) = plan(&cands, &only);
        assert_eq!(p.iter().map(|x| x.key.as_str()).collect::<Vec<_>>(), vec!["v5|walk"], "explicitly listed optical drive is indexed");
        assert!(s.iter().any(|x| x.key == "v1" && x.reason.contains("volume list")));
    }
}
