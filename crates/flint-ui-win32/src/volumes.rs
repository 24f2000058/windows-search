//! Executes the discovery plan against the catalog (Windows).

use crate::discovery::{plan, Candidate, Mode, Planned};
use flint_config::Indexing;
use flint_index::catalog::{Backend, Catalog, Opener, VolumeSpec};
use flint_index::model::SourceError;
use flint_index::traverse::{Fs, RealFs, TraversalSource};
use flint_ntfs::watch::DirWatcher;
use flint_ntfs::win::{list_volumes, NtfsVolume, VolumeInfo};
use std::path::PathBuf;

fn watched_opener(root: String, exclude: Vec<PathBuf>) -> Opener {
    Box::new(move |_| {
        let mut src = TraversalSource::new(PathBuf::from(&root), Box::new(RealFs) as Box<dyn Fs + Send>);
        src.exclude = exclude.clone();
        // Start watching before traversing so nothing changes unseen.
        Ok(match DirWatcher::start(&src.root) {
            Ok(w) => Backend::Watched(src, Box::new(w)),
            Err(_) => Backend::Traversal(src),
        })
    })
}

fn journal_opener(info: VolumeInfo, first: NtfsVolume) -> Opener {
    let mut first = Some(first);
    Box::new(move |journal_failed| {
        if journal_failed {
            // Journal and traversal use different root identities; switching
            // modes means re-adding the volume under its traversal key.
            return Err(SourceError::Unavailable("change journal became unusable; restart Flint to index this volume by traversal".into()));
        }
        match first.take() {
            Some(v) => Ok(Backend::Journal(Box::new(v))),
            None => NtfsVolume::open(info.clone()).map(|v| Backend::Journal(Box::new(v) as Box<_>)),
        }
    })
}

fn add_watched(cat: &Catalog, p: &Planned, key: String, exclude: &[PathBuf]) {
    let root_frn = TraversalSource::new(PathBuf::from(&p.root_path), RealFs).root_frn;
    cat.add(VolumeSpec { key, root_name: p.root_name.clone(), root_frn }, watched_opener(p.root_path.clone(), exclude.to_vec()));
}

/// Brings the catalog in line with the current volumes and settings.
/// `only_root` restricts indexing to one folder (the `--root` option).
/// Returns human-readable notes (skips and mode decisions).
pub fn sync(cat: &Catalog, cfg: &Indexing, only_root: Option<&str>) -> Vec<String> {
    let exclude: Vec<PathBuf> = cfg.excluded_paths.iter().map(PathBuf::from).collect();
    let mut notes = Vec::new();
    let (infos, planned) = if let Some(root) = only_root {
        let p = Planned { key: format!("folder:{}", root.to_ascii_uppercase()), root_name: root.trim_end_matches('\\').to_owned(), root_path: root.to_owned(), mode: Mode::WatchedTraversal };
        (Vec::new(), vec![p])
    } else {
        let infos = list_volumes();
        let cands: Vec<Candidate> = infos.iter().map(|v| Candidate { key: v.guid_path.clone(), mounts: v.mount_points.clone(), filesystem: v.filesystem.clone(), drive_type: v.drive_type }).collect();
        let (planned, skipped) = plan(&cands, cfg);
        for s in skipped {
            notes.push(format!("skipped {}: {}", s.key, s.reason));
        }
        (infos, planned)
    };
    let existing = cat.keys();
    let mut wanted: Vec<String> = Vec::new();
    for p in &planned {
        let walk_key = format!("{}|walk", p.key);
        if existing.contains(&p.key) || existing.contains(&walk_key) {
            wanted.push(if existing.contains(&p.key) { p.key.clone() } else { walk_key });
            continue;
        }
        match p.mode {
            Mode::WatchedTraversal => {
                add_watched(cat, p, p.key.clone(), &exclude);
                wanted.push(p.key.clone());
            }
            Mode::JournalOrTraversal => {
                let info = infos.iter().find(|i| i.guid_path == p.key).cloned();
                match info.map(|i| (NtfsVolume::open(i.clone()), i)) {
                    Some((Ok(v), info)) => {
                        let spec = VolumeSpec { key: p.key.clone(), root_name: p.root_name.clone(), root_frn: v.root_frn };
                        cat.add(spec, journal_opener(info, v));
                        notes.push(format!("{}: NTFS change journal", p.root_name));
                        wanted.push(p.key.clone());
                    }
                    Some((Err(e), _)) => {
                        notes.push(format!("{}: journal not available ({e:?}); indexing by traversal with change notifications", p.root_name));
                        add_watched(cat, p, walk_key.clone(), &exclude);
                        wanted.push(walk_key);
                    }
                    None => {}
                }
            }
        }
    }
    for k in existing {
        if !wanted.contains(&k) {
            cat.remove(&k);
            notes.push(format!("removed {k} (no longer present or excluded)"));
        }
    }
    notes
}
