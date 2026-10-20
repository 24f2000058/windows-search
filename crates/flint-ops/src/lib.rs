//! Safe file operations (ADR-015).
//!
//! The index can be stale: an entry may have been deleted, replaced by a
//! different file with the same name, or turned from a file into a folder
//! since it was indexed. So every operation goes through three steps:
//!
//! 1. **Plan** ([`plan`]): validate the selection against the live
//!    filesystem and the operation's rules (protected roots, nested
//!    selections, moving a folder into itself, collisions, invalid names,
//!    links), producing a reviewable [`Plan`] with per-item notes.
//! 2. **Revalidate** ([`execute`]): immediately before acting, every item is
//!    checked again; anything that changed since the plan was reviewed is
//!    skipped with a reason, never acted on.
//! 3. **Verify**: after the executor (Windows: `IFileOperation`) returns,
//!    each item's outcome is determined by observing the filesystem, not by
//!    trusting the executor's summary.
//!
//! Identity: on journal (NTFS) volumes an entry carries its file reference
//! number, which is compared with the live file ID, so a same-named
//! replacement is detected. Traversal volumes have no stable IDs; there the
//! check is type + size + modification time, which is weaker (documented in
//! ADR-015). A window remains between the final check and the OS acting on
//! the path; it is milliseconds wide and the shell's own checks apply in it.

pub mod compare;
pub mod dupes;
pub mod names;
pub mod paths;
#[cfg(windows)]
pub mod win;

use names::{validate_name, NameError};

/// What the index knows about an entry, captured when the user selected it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Target {
    pub path: String,
    pub is_dir: bool,
    pub identity: Identity,
    /// Other hard-link names of the same file known to the index.
    pub other_links: usize,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Identity {
    /// NTFS file reference number (from the journal-backed index).
    FileId(u64),
    /// Size and mtime seen by a traversal (`None` = unknown).
    Observed { size: Option<u64>, mtime: Option<i64> },
}

/// Live filesystem facts about a path.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Stat {
    pub is_dir: bool,
    /// Symbolic link, junction or other reparse point.
    pub is_link: bool,
    pub file_id: Option<u64>,
    pub size: u64,
    pub mtime: i64,
}

/// Read-only view of the live filesystem.
pub trait Probe {
    /// `Ok(None)` if nothing exists at `path`.
    fn stat(&self, path: &str) -> Result<Option<Stat>, String>;
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum OpKind {
    /// Delete to the Recycle Bin (the default).
    Recycle,
    /// Delete bypassing the Recycle Bin; requires explicit confirmation.
    DeletePermanently,
    Copy { dest_dir: String },
    Move { dest_dir: String },
    Rename { new_name: String },
    /// Create `name` inside the (single) target folder.
    NewFolder { name: String },
}

impl OpKind {
    pub fn verb(&self) -> &'static str {
        match self {
            OpKind::Recycle => "Move to Recycle Bin",
            OpKind::DeletePermanently => "Delete permanently",
            OpKind::Copy { .. } => "Copy",
            OpKind::Move { .. } => "Move",
            OpKind::Rename { .. } => "Rename",
            OpKind::NewFolder { .. } => "New folder",
        }
    }
    fn is_destructive(&self) -> bool {
        matches!(self, OpKind::Recycle | OpKind::DeletePermanently | OpKind::Move { .. } | OpKind::Rename { .. })
    }
}

/// How name collisions at the destination are resolved.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Collision {
    /// Let the shell ask per conflict (the default).
    Ask,
    /// Keep both: the new copy gets a "(2)"-style name.
    KeepBoth,
    Skip,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Problem {
    /// Nothing exists at the path any more.
    Missing,
    /// A different file now has this path (file ID differs).
    Replaced,
    /// A file became a folder or vice versa.
    TypeChanged,
    /// Size or modification time differ (traversal volumes).
    Modified,
    Inaccessible(String),
    /// Drive roots and share roots are never operated on.
    ProtectedRoot,
    /// Inside another selected folder, which already covers it.
    CoveredBy(String),
    /// Moving or copying a folder into itself or its own subfolder.
    IntoItself,
    /// Already in the destination folder (move/copy onto itself).
    SameFolder,
    InvalidName(NameError),
    /// The new name exists already (rename / new folder).
    NameTaken,
    NotAFolder,
    Duplicate,
}

impl std::fmt::Display for Problem {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Problem::Missing => write!(f, "no longer exists"),
            Problem::Replaced => write!(f, "was replaced by a different file with the same name"),
            Problem::TypeChanged => write!(f, "changed between file and folder"),
            Problem::Modified => write!(f, "changed since it was indexed"),
            Problem::Inaccessible(e) => write!(f, "cannot be read: {e}"),
            Problem::ProtectedRoot => write!(f, "is a drive or share root"),
            Problem::CoveredBy(p) => write!(f, "is inside {p}, which is also selected"),
            Problem::IntoItself => write!(f, "cannot go into itself"),
            Problem::SameFolder => write!(f, "is already in that folder"),
            Problem::InvalidName(e) => write!(f, "{e}"),
            Problem::NameTaken => write!(f, "an item with that name already exists"),
            Problem::NotAFolder => write!(f, "is not a folder"),
            Problem::Duplicate => write!(f, "is selected twice"),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Note {
    /// Deleting/moving a link affects the link, not what it points to.
    Link,
    /// Deleting one hard-link name leaves the file reachable by the others.
    OtherLinks(usize),
    /// Something with this name exists at the destination.
    Collides(String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PlanItem {
    pub target: Target,
    /// Live state seen while planning.
    pub seen: Option<Stat>,
    /// Set if the item will not be acted on.
    pub problem: Option<Problem>,
    pub notes: Vec<Note>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Plan {
    pub kind: OpKind,
    pub collision: Collision,
    pub items: Vec<PlanItem>,
    /// Problem with the operation as a whole (e.g. destination missing).
    pub fatal: Option<String>,
}

impl Plan {
    pub fn actionable(&self) -> impl Iterator<Item = &PlanItem> {
        self.items.iter().filter(|i| i.problem.is_none())
    }
    pub fn actionable_count(&self) -> usize {
        self.actionable().count()
    }
    /// Everything (every operation here) gets a preview; batches, permanent
    /// deletes and anything with problems or collisions must be confirmed.
    pub fn needs_confirmation(&self) -> bool {
        self.kind == OpKind::DeletePermanently || self.items.len() > 1 || self.items.iter().any(|i| i.problem.is_some() || !i.notes.is_empty())
    }
    /// Human-readable review text: one line per item (capped), then totals.
    pub fn preview(&self, max_lines: usize) -> Vec<String> {
        let mut out = Vec::new();
        let head = match &self.kind {
            OpKind::Copy { dest_dir } | OpKind::Move { dest_dir } => format!("{} {} item(s) to {dest_dir}", self.kind.verb(), self.actionable_count()),
            OpKind::Rename { new_name } => format!("Rename to \"{new_name}\""),
            OpKind::NewFolder { name } => format!("Create folder \"{name}\""),
            k => format!("{} {} item(s)", k.verb(), self.actionable_count()),
        };
        out.push(head);
        if self.kind == OpKind::DeletePermanently {
            out.push("These items will NOT go to the Recycle Bin and cannot be restored.".into());
        }
        if let Some(f) = &self.fatal {
            out.push(format!("Cannot continue: {f}"));
        }
        for (n, i) in self.items.iter().enumerate() {
            if n >= max_lines {
                out.push(format!("… and {} more", self.items.len() - n));
                break;
            }
            let mut line = i.target.path.clone();
            if let Some(p) = &i.problem {
                line = format!("SKIP {line}: {p}");
            }
            for note in &i.notes {
                line.push_str(&match note {
                    Note::Link => " (link: the link itself, not its target)".to_owned(),
                    Note::OtherLinks(n) => format!(" (hard link: {n} other name(s) keep the file)"),
                    Note::Collides(_) => match self.collision {
                        Collision::Ask => " (name exists at destination: you will be asked)".to_owned(),
                        Collision::KeepBoth => " (name exists at destination: both kept)".to_owned(),
                        Collision::Skip => " (name exists at destination: skipped)".to_owned(),
                    },
                });
            }
            out.push(line);
        }
        let skipped = self.items.len() - self.actionable_count();
        if skipped > 0 {
            out.push(format!("{skipped} item(s) will be skipped."));
        }
        out
    }
}

/// Compares the indexed identity with the live state.
pub fn check(t: &Target, live: Result<Option<Stat>, String>) -> Result<Stat, Problem> {
    let st = match live {
        Err(e) => return Err(Problem::Inaccessible(e)),
        Ok(None) => return Err(Problem::Missing),
        Ok(Some(s)) => s,
    };
    if st.is_dir != t.is_dir {
        return Err(Problem::TypeChanged);
    }
    match &t.identity {
        Identity::FileId(id) => {
            // The index stores FRNs; a live ID that differs is another file.
            if st.file_id.is_some_and(|live| live != *id) {
                return Err(Problem::Replaced);
            }
        }
        Identity::Observed { size, mtime } => {
            // Folders' size/mtime change with their contents; only files are
            // compared.
            if !t.is_dir && (size.is_some_and(|s| s != st.size) || mtime.is_some_and(|m| m != st.mtime)) {
                return Err(Problem::Modified);
            }
        }
    }
    Ok(st)
}

/// Builds a reviewable plan. Nothing is changed.
pub fn plan(kind: OpKind, targets: Vec<Target>, collision: Collision, fs: &dyn Probe) -> Plan {
    let mut fatal = None;
    // Destination checks first: they apply to every item.
    let dest = match &kind {
        OpKind::Copy { dest_dir } | OpKind::Move { dest_dir } => {
            match fs.stat(dest_dir) {
                Ok(Some(s)) if s.is_dir => {}
                Ok(Some(_)) => fatal = Some(format!("{dest_dir} is not a folder")),
                Ok(None) => fatal = Some(format!("{dest_dir} does not exist")),
                Err(e) => fatal = Some(format!("{dest_dir} cannot be read: {e}")),
            }
            Some(dest_dir.clone())
        }
        _ => None,
    };
    if matches!(kind, OpKind::Rename { .. } | OpKind::NewFolder { .. }) && targets.len() != 1 {
        fatal = Some(format!("{} works on exactly one item", kind.verb()));
    }
    let mut items: Vec<PlanItem> = Vec::with_capacity(targets.len());
    let mut seen_paths: Vec<String> = Vec::new();
    for t in &targets {
        let mut item = PlanItem { target: t.clone(), seen: None, problem: None, notes: Vec::new() };
        let key = paths::norm(&t.path);
        item.problem = if seen_paths.contains(&key) {
            Some(Problem::Duplicate)
        } else if paths::is_root(&t.path) && !matches!(kind, OpKind::NewFolder { .. } | OpKind::Copy { .. }) {
            Some(Problem::ProtectedRoot)
        } else {
            match check(t, fs.stat(&t.path)) {
                Err(p) => Some(p),
                Ok(st) => {
                    item.seen = Some(st.clone());
                    rule_problem(&kind, collision, t, &st, dest.as_deref(), fs, &mut item.notes)
                }
            }
        };
        seen_paths.push(key);
        items.push(item);
    }
    // Items inside another folder that *will* be acted on are covered by it
    // (only actionable folders cover: a refused root covers nothing).
    let covering: Vec<String> = items.iter().filter(|i| i.problem.is_none() && i.target.is_dir).map(|i| i.target.path.clone()).collect();
    for item in items.iter_mut().filter(|i| i.problem.is_none()) {
        if let Some(outer) = covering.iter().filter(|d| !paths::same(d, &item.target.path) && paths::is_within(&item.target.path, d)).min_by_key(|d| d.len()) {
            item.problem = Some(Problem::CoveredBy(outer.clone()));
            item.notes.clear();
        }
    }
    if fatal.is_none() && !items.is_empty() && items.iter().all(|i| i.problem.is_some()) && kind.is_destructive() {
        fatal = Some("nothing left to act on".into());
    }
    Plan { kind, collision, items, fatal }
}

fn rule_problem(kind: &OpKind, collision: Collision, t: &Target, st: &Stat, dest: Option<&str>, fs: &dyn Probe, notes: &mut Vec<Note>) -> Option<Problem> {
    if st.is_link {
        notes.push(Note::Link);
    }
    match kind {
        OpKind::Recycle | OpKind::DeletePermanently => {
            if t.other_links > 0 && !t.is_dir {
                notes.push(Note::OtherLinks(t.other_links));
            }
            None
        }
        OpKind::Copy { .. } | OpKind::Move { .. } => {
            let dest = dest?;
            if t.is_dir && paths::is_within(dest, &t.path) {
                return Some(Problem::IntoItself);
            }
            if paths::parent(&t.path).is_some_and(|p| paths::same(p, dest)) {
                // Copying into the same folder is a valid "duplicate" request
                // for the shell (it names the copy); moving there is a no-op.
                if matches!(kind, OpKind::Move { .. }) {
                    return Some(Problem::SameFolder);
                }
            }
            let at = paths::join(dest, paths::file_name(&t.path));
            if matches!(fs.stat(&at), Ok(Some(_))) {
                notes.push(Note::Collides(at));
                if collision == Collision::Skip {
                    return Some(Problem::NameTaken);
                }
            }
            None
        }
        OpKind::Rename { new_name } => {
            if let Err(e) = validate_name(new_name) {
                return Some(Problem::InvalidName(e));
            }
            let parent = paths::parent(&t.path)?;
            let to = paths::join(parent, new_name);
            // A case-only rename targets the same entry; anything else that
            // exists is a different item.
            if !paths::same(&to, &t.path) && matches!(fs.stat(&to), Ok(Some(_))) {
                return Some(Problem::NameTaken);
            }
            None
        }
        OpKind::NewFolder { name } => {
            if !t.is_dir {
                return Some(Problem::NotAFolder);
            }
            if let Err(e) = validate_name(name) {
                return Some(Problem::InvalidName(e));
            }
            matches!(fs.stat(&paths::join(&t.path, name)), Ok(Some(_))).then_some(Problem::NameTaken)
        }
    }
}

/// Performs an operation on already revalidated items. The Windows
/// implementation uses one `IFileOperation` for the whole batch.
pub trait Executor {
    /// Returns Err for a failure of the whole call (e.g. COM error); the user
    /// cancelling part-way is not an error. Per-item outcomes are then
    /// determined by [`execute`] observing the filesystem.
    fn run(&mut self, kind: &OpKind, collision: Collision, items: &[&PlanItem]) -> Result<(), String>;
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Outcome {
    Done,
    /// Skipped at the final check (or in the plan).
    Skipped(Problem),
    /// The executor ran but the expected result is not observable (the user
    /// cancelled, chose "skip" on a conflict, or the OS refused).
    NotDone,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Report {
    pub outcomes: Vec<(String, Outcome)>,
    pub error: Option<String>,
}

impl Report {
    pub fn count(&self, f: impl Fn(&Outcome) -> bool) -> usize {
        self.outcomes.iter().filter(|(_, o)| f(o)).count()
    }
    pub fn summary(&self) -> String {
        let done = self.count(|o| *o == Outcome::Done);
        let skipped = self.count(|o| matches!(o, Outcome::Skipped(_)));
        let not = self.count(|o| *o == Outcome::NotDone);
        let mut s = format!("{done} done");
        if skipped > 0 {
            s.push_str(&format!(", {skipped} skipped"));
        }
        if not > 0 {
            s.push_str(&format!(", {not} not completed"));
        }
        if let Some(e) = &self.error {
            s.push_str(&format!(" (error: {e})"));
        }
        s
    }
}

/// Runs a reviewed plan: refuses unconfirmed permanent deletes, revalidates
/// every item immediately before acting, runs the executor on what is still
/// valid, then verifies each item by observation.
pub fn execute(plan: &Plan, permanent_confirmed: bool, fs: &dyn Probe, exec: &mut dyn Executor) -> Report {
    let mut report = Report::default();
    if let Some(f) = &plan.fatal {
        report.error = Some(f.clone());
        return report;
    }
    if plan.kind == OpKind::DeletePermanently && !permanent_confirmed {
        report.error = Some("permanent deletion was not confirmed".into());
        return report;
    }
    let mut ready: Vec<&PlanItem> = Vec::new();
    for item in &plan.items {
        if let Some(p) = &item.problem {
            report.outcomes.push((item.target.path.clone(), Outcome::Skipped(p.clone())));
            continue;
        }
        // Final check: against the index identity *and* what the user
        // reviewed (catches changes after the preview was shown).
        let live = check(&item.target, fs.stat(&item.target.path)).and_then(|st| match &item.seen {
            Some(seen) if seen.file_id != st.file_id => Err(Problem::Replaced),
            Some(seen) if !st.is_dir && (seen.size != st.size || seen.mtime != st.mtime) => Err(Problem::Modified),
            _ => Ok(st),
        });
        match live {
            Ok(_) => ready.push(item),
            Err(p) => report.outcomes.push((item.target.path.clone(), Outcome::Skipped(p))),
        }
    }
    if ready.is_empty() {
        return report;
    }
    if let Err(e) = exec.run(&plan.kind, plan.collision, &ready) {
        report.error = Some(e);
    }
    for item in ready {
        let done = verify(&plan.kind, item, fs);
        report.outcomes.push((item.target.path.clone(), if done { Outcome::Done } else { Outcome::NotDone }));
    }
    report
}

/// Observes whether the operation took effect for one item.
fn verify(kind: &OpKind, item: &PlanItem, fs: &dyn Probe) -> bool {
    let p = &item.target.path;
    let exists = |path: &str| matches!(fs.stat(path), Ok(Some(_)));
    match kind {
        OpKind::Recycle | OpKind::DeletePermanently => !exists(p),
        OpKind::Move { dest_dir } => !exists(p) && exists(&paths::join(dest_dir, paths::file_name(p))),
        // With "keep both" the copy may carry a new name; the source staying
        // intact plus no reported failure is what can be observed.
        OpKind::Copy { dest_dir } => exists(p) && (exists(&paths::join(dest_dir, paths::file_name(p))) || item.notes.iter().any(|n| matches!(n, Note::Collides(_)))),
        OpKind::Rename { new_name } => paths::parent(p).is_some_and(|d| {
            let to = paths::join(d, new_name);
            exists(&to) && (paths::same(&to, p) || !exists(p))
        }),
        OpKind::NewFolder { name } => matches!(fs.stat(&paths::join(p, name)), Ok(Some(s)) if s.is_dir),
    }
}

#[cfg(test)]
mod tests;
