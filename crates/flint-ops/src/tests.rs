//! Fault-injection tests: a fake filesystem that changes between planning,
//! the final check and execution, and executors that fail or are cancelled
//! part-way.

use super::*;
use std::cell::RefCell;
use std::collections::BTreeMap;

type StatHook = Box<dyn FnMut(usize, &str, &FakeFs)>;

#[derive(Default)]
struct FakeFs {
    /// Normalized path -> (display path, stat).
    entries: RefCell<BTreeMap<String, (String, Stat)>>,
    denied: RefCell<Vec<String>>,
    next_id: RefCell<u64>,
    /// Runs on every stat call (count, path): injects races.
    hook: RefCell<Option<StatHook>>,
    calls: RefCell<usize>,
}

impl FakeFs {
    fn new() -> Self {
        let fs = FakeFs::default();
        *fs.next_id.borrow_mut() = 100;
        fs.dir("C:\\");
        fs
    }
    fn add(&self, path: &str, is_dir: bool, size: u64) -> u64 {
        let id = {
            let mut n = self.next_id.borrow_mut();
            *n += 1;
            *n
        };
        self.entries.borrow_mut().insert(paths::norm(path), (path.to_owned(), Stat { is_dir, is_link: false, file_id: Some(id), size, mtime: 1000 }));
        id
    }
    fn dir(&self, p: &str) -> u64 {
        self.add(p, true, 0)
    }
    fn file(&self, p: &str, size: u64) -> u64 {
        self.add(p, false, size)
    }
    fn remove(&self, p: &str) {
        let k = paths::norm(p);
        self.entries.borrow_mut().retain(|e, _| !(e == &k || e.starts_with(&format!("{k}\\"))));
    }
    fn rename(&self, from: &str, to: &str) {
        let (fk, tk) = (paths::norm(from), paths::norm(to));
        let moved: Vec<(String, (String, Stat))> = self.entries.borrow().iter().filter(|(e, _)| *e == &fk || e.starts_with(&format!("{fk}\\"))).map(|(e, v)| (e.clone(), v.clone())).collect();
        let mut m = self.entries.borrow_mut();
        for (k, (disp, st)) in moved {
            m.remove(&k);
            let nk = format!("{tk}{}", &k[fk.len()..]);
            let nd = format!("{to}{}", &disp[from.len().min(disp.len())..]);
            m.insert(nk, (nd, st));
        }
    }
    fn exists(&self, p: &str) -> bool {
        self.entries.borrow().contains_key(&paths::norm(p))
    }
    fn target(&self, p: &str) -> Target {
        let (_, st) = self.entries.borrow().get(&paths::norm(p)).cloned().expect("exists");
        Target { path: p.to_owned(), is_dir: st.is_dir, identity: Identity::FileId(st.file_id.unwrap()), other_links: 0 }
    }
    fn on_stat(&self, f: impl FnMut(usize, &str, &FakeFs) + 'static) {
        *self.hook.borrow_mut() = Some(Box::new(f));
    }
}

impl Probe for FakeFs {
    fn stat(&self, path: &str) -> Result<Option<Stat>, String> {
        let n = {
            let mut c = self.calls.borrow_mut();
            *c += 1;
            *c
        };
        let hook = self.hook.borrow_mut().take();
        if let Some(mut h) = hook {
            h(n, path, self);
            *self.hook.borrow_mut() = Some(h);
        }
        if self.denied.borrow().iter().any(|d| paths::same(d, path)) {
            return Err("access denied".into());
        }
        Ok(self.entries.borrow().get(&paths::norm(path)).map(|(_, s)| s.clone()))
    }
}

/// Executes against the fake filesystem; can fail or stop part-way.
struct FakeExec<'a> {
    fs: &'a FakeFs,
    /// Act on at most this many items (simulates the user cancelling).
    stop_after: usize,
    fail: Option<String>,
    seen: Vec<String>,
}

impl<'a> FakeExec<'a> {
    fn new(fs: &'a FakeFs) -> Self {
        FakeExec { fs, stop_after: usize::MAX, fail: None, seen: Vec::new() }
    }
}

impl Executor for FakeExec<'_> {
    fn run(&mut self, kind: &OpKind, collision: Collision, items: &[&PlanItem]) -> Result<(), String> {
        for (n, it) in items.iter().enumerate() {
            if n >= self.stop_after {
                break;
            }
            let p = it.target.path.clone();
            self.seen.push(p.clone());
            match kind {
                OpKind::Recycle | OpKind::DeletePermanently => self.fs.remove(&p),
                OpKind::Move { dest_dir } => {
                    let to = paths::join(dest_dir, paths::file_name(&p));
                    if self.fs.exists(&to) && collision == Collision::Skip {
                        continue;
                    }
                    self.fs.remove(&to);
                    self.fs.rename(&p, &to);
                }
                OpKind::Copy { dest_dir } => {
                    let to = paths::join(dest_dir, paths::file_name(&p));
                    if !self.fs.exists(&to) {
                        self.fs.add(&to, it.target.is_dir, 1);
                    }
                }
                OpKind::Rename { new_name } => {
                    let to = paths::join(paths::parent(&p).unwrap(), new_name);
                    self.fs.rename(&p, &to);
                }
                OpKind::NewFolder { name } => {
                    self.fs.dir(&paths::join(&p, name));
                }
            }
        }
        match &self.fail {
            Some(e) => Err(e.clone()),
            None => Ok(()),
        }
    }
}

fn setup() -> FakeFs {
    let fs = FakeFs::new();
    fs.dir("C:\\docs");
    fs.file("C:\\docs\\a.txt", 10);
    fs.file("C:\\docs\\b.txt", 20);
    fs.dir("C:\\docs\\sub");
    fs.file("C:\\docs\\sub\\c.txt", 30);
    fs.dir("C:\\dest");
    fs
}

fn outcome(r: &Report, path: &str) -> Outcome {
    r.outcomes.iter().find(|(p, _)| p == path).unwrap_or_else(|| panic!("no outcome for {path}: {r:?}")).1.clone()
}

#[test]
fn recycle_is_default_path_and_permanent_delete_needs_explicit_confirmation() {
    let fs = setup();
    let t = vec![fs.target("C:\\docs\\a.txt")];
    let p = plan(OpKind::DeletePermanently, t.clone(), Collision::Ask, &fs);
    assert!(p.needs_confirmation(), "permanent delete is always confirmed, even for one item");
    assert!(p.preview(10).iter().any(|l| l.contains("NOT go to the Recycle Bin")));
    let mut ex = FakeExec::new(&fs);
    let r = execute(&p, false, &fs, &mut ex);
    assert!(r.error.as_deref().unwrap().contains("not confirmed"));
    assert!(ex.seen.is_empty() && fs.exists("C:\\docs\\a.txt"), "nothing happens without confirmation");
    let r = execute(&p, true, &fs, &mut ex);
    assert_eq!(outcome(&r, "C:\\docs\\a.txt"), Outcome::Done);
    let p = plan(OpKind::Recycle, vec![fs.target("C:\\docs\\b.txt")], Collision::Ask, &fs);
    assert!(!p.needs_confirmation(), "a single clean recycle needs no extra dialog (it is undoable)");
}

#[test]
fn stale_index_entries_are_never_acted_on() {
    let fs = setup();
    let a = fs.target("C:\\docs\\a.txt");
    // Replaced by a different file with the same name before planning.
    fs.remove("C:\\docs\\a.txt");
    fs.file("C:\\docs\\a.txt", 10);
    let b = fs.target("C:\\docs\\b.txt");
    fs.remove("C:\\docs\\b.txt");
    let sub = fs.target("C:\\docs\\sub");
    fs.remove("C:\\docs\\sub");
    fs.file("C:\\docs\\sub", 5); // folder became a file
    let p = plan(OpKind::Recycle, vec![a, b, sub], Collision::Ask, &fs);
    let probs: Vec<Option<Problem>> = p.items.iter().map(|i| i.problem.clone()).collect();
    assert_eq!(probs, vec![Some(Problem::Replaced), Some(Problem::Missing), Some(Problem::TypeChanged)]);
    assert_eq!(p.fatal.as_deref(), Some("nothing left to act on"));
    let mut ex = FakeExec::new(&fs);
    let r = execute(&p, false, &fs, &mut ex);
    assert!(ex.seen.is_empty());
    assert!(fs.exists("C:\\docs\\a.txt"), "the replacement file survives: {r:?}");
}

#[test]
fn changes_after_the_preview_are_caught_by_the_final_check() {
    let fs = setup();
    let t = vec![fs.target("C:\\docs\\a.txt"), fs.target("C:\\docs\\b.txt"), fs.target("C:\\docs\\sub\\c.txt")];
    let p = plan(OpKind::Recycle, t, Collision::Ask, &fs);
    assert_eq!(p.actionable_count(), 3);
    // While the user reads the preview: a.txt is replaced, b.txt modified,
    // c.txt becomes unreadable.
    fs.remove("C:\\docs\\a.txt");
    fs.file("C:\\docs\\a.txt", 10);
    fs.entries.borrow_mut().get_mut(&paths::norm("C:\\docs\\b.txt")).unwrap().1.size = 21;
    fs.denied.borrow_mut().push("C:\\docs\\sub\\c.txt".into());
    let mut ex = FakeExec::new(&fs);
    let r = execute(&p, false, &fs, &mut ex);
    assert_eq!(outcome(&r, "C:\\docs\\a.txt"), Outcome::Skipped(Problem::Replaced));
    assert_eq!(outcome(&r, "C:\\docs\\b.txt"), Outcome::Skipped(Problem::Modified));
    assert!(matches!(outcome(&r, "C:\\docs\\sub\\c.txt"), Outcome::Skipped(Problem::Inaccessible(_))));
    assert!(ex.seen.is_empty(), "executor never sees changed items");
    assert!(fs.exists("C:\\docs\\a.txt") && fs.exists("C:\\docs\\b.txt"));
}

#[test]
fn race_during_final_check_of_a_batch_only_skips_the_affected_item() {
    let fs = setup();
    let t = vec![fs.target("C:\\docs\\a.txt"), fs.target("C:\\docs\\b.txt")];
    let p = plan(OpKind::Recycle, t, Collision::Ask, &fs);
    // The moment a.txt passes its final check, b.txt is deleted by someone else.
    fs.on_stat(|_, path, fs| {
        if paths::same(path, "C:\\docs\\a.txt") {
            fs.remove("C:\\docs\\b.txt");
        }
    });
    let mut ex = FakeExec::new(&fs);
    let r = execute(&p, false, &fs, &mut ex);
    assert_eq!(outcome(&r, "C:\\docs\\a.txt"), Outcome::Done);
    assert_eq!(outcome(&r, "C:\\docs\\b.txt"), Outcome::Skipped(Problem::Missing));
    assert_eq!(ex.seen, vec!["C:\\docs\\a.txt"]);
}

#[test]
fn cancelled_or_failed_execution_is_reported_per_item_by_observation() {
    let fs = setup();
    let t = vec![fs.target("C:\\docs\\a.txt"), fs.target("C:\\docs\\b.txt")];
    let p = plan(OpKind::Move { dest_dir: "C:\\dest".into() }, t, Collision::Ask, &fs);
    let mut ex = FakeExec::new(&fs);
    ex.stop_after = 1;
    ex.fail = Some("operation aborted".into());
    let r = execute(&p, false, &fs, &mut ex);
    assert_eq!(outcome(&r, "C:\\docs\\a.txt"), Outcome::Done);
    assert_eq!(outcome(&r, "C:\\docs\\b.txt"), Outcome::NotDone, "not claimed done after a partial run");
    assert!(r.summary().contains("1 done") && r.summary().contains("1 not completed") && r.summary().contains("aborted"));
    assert!(fs.exists("C:\\dest\\a.txt") && fs.exists("C:\\docs\\b.txt"));
}

#[test]
fn roots_nested_selections_and_duplicates_are_excluded() {
    let fs = setup();
    let t = vec![fs.target("C:\\"), fs.target("C:\\docs"), fs.target("C:\\docs\\sub\\c.txt"), fs.target("c:/DOCS"), fs.target("C:\\dest")];
    let p = plan(OpKind::Recycle, t, Collision::Ask, &fs);
    let probs: Vec<Option<Problem>> = p.items.iter().map(|i| i.problem.clone()).collect();
    assert_eq!(probs, vec![Some(Problem::ProtectedRoot), None, Some(Problem::CoveredBy("C:\\docs".into())), Some(Problem::Duplicate), None]);
    assert!(p.needs_confirmation());
    let pre = p.preview(10).join("\n");
    assert!(pre.contains("Move to Recycle Bin 2 item(s)") && pre.contains("3 item(s) will be skipped"), "{pre}");
    let r = execute(&p, false, &fs, &mut FakeExec::new(&fs));
    assert!(fs.exists("C:\\") && !fs.exists("C:\\docs") && !fs.exists("C:\\dest"));
    assert_eq!(r.count(|o| *o == Outcome::Done), 2);
}

#[test]
fn moves_into_itself_same_folder_and_collisions() {
    let fs = setup();
    fs.file("C:\\dest\\a.txt", 99);
    let t = vec![fs.target("C:\\docs"), fs.target("C:\\docs\\a.txt")];
    let p = plan(OpKind::Move { dest_dir: "C:\\docs\\sub".into() }, vec![fs.target("C:\\docs")], Collision::Ask, &fs);
    assert_eq!(p.items[0].problem, Some(Problem::IntoItself));
    let p = plan(OpKind::Move { dest_dir: "C:\\docs".into() }, vec![fs.target("C:\\docs\\a.txt")], Collision::Ask, &fs);
    assert_eq!(p.items[0].problem, Some(Problem::SameFolder));
    let p = plan(OpKind::Copy { dest_dir: "C:\\dest".into() }, t[1..].to_vec(), Collision::Skip, &fs);
    assert_eq!(p.items[0].notes, vec![Note::Collides("C:\\dest\\a.txt".into())]);
    assert_eq!(p.items[0].problem, Some(Problem::NameTaken), "skip policy skips in the plan, visibly");
    let keep = plan(OpKind::Copy { dest_dir: "C:\\dest".into() }, t[1..].to_vec(), Collision::KeepBoth, &fs);
    assert_eq!(keep.items[0].problem, None);
    assert!(p.preview(5).iter().any(|l| l.contains("skipped")));
    let p = plan(OpKind::Move { dest_dir: "C:\\missing".into() }, t, Collision::Ask, &fs);
    assert!(p.fatal.as_deref().unwrap().contains("does not exist"));
    let r = execute(&p, false, &fs, &mut FakeExec::new(&fs));
    assert!(r.outcomes.is_empty() && r.error.is_some());
    let p = plan(OpKind::Copy { dest_dir: "C:\\docs\\a.txt".into() }, vec![fs.target("C:\\docs\\b.txt")], Collision::Ask, &fs);
    assert!(p.fatal.as_deref().unwrap().contains("not a folder"));
}

#[test]
fn rename_and_new_folder_validate_names_and_collisions() {
    let fs = setup();
    let a = fs.target("C:\\docs\\a.txt");
    for (name, want) in [("..\\..\\evil.txt", Some(Problem::InvalidName(NameError::Traversal))), ("b.txt", Some(Problem::NameTaken)), ("x:y", Some(Problem::InvalidName(NameError::InvalidChar(':')))), ("A.TXT", None), ("new.txt", None)] {
        let p = plan(OpKind::Rename { new_name: name.into() }, vec![a.clone()], Collision::Ask, &fs);
        assert_eq!(p.items[0].problem, want, "{name}");
    }
    let p = plan(OpKind::Rename { new_name: "new.txt".into() }, vec![a.clone()], Collision::Ask, &fs);
    let r = execute(&p, false, &fs, &mut FakeExec::new(&fs));
    assert_eq!(outcome(&r, "C:\\docs\\a.txt"), Outcome::Done);
    assert!(fs.exists("C:\\docs\\new.txt") && !fs.exists("C:\\docs\\a.txt"));
    let p = plan(OpKind::Rename { new_name: "z".into() }, vec![fs.target("C:\\docs\\b.txt"), fs.target("C:\\docs\\new.txt")], Collision::Ask, &fs);
    assert!(p.fatal.is_some(), "rename takes exactly one item");

    let p = plan(OpKind::NewFolder { name: "sub".into() }, vec![fs.target("C:\\docs")], Collision::Ask, &fs);
    assert_eq!(p.items[0].problem, Some(Problem::NameTaken));
    let p = plan(OpKind::NewFolder { name: "fresh".into() }, vec![fs.target("C:\\docs\\b.txt")], Collision::Ask, &fs);
    assert_eq!(p.items[0].problem, Some(Problem::NotAFolder));
    let p = plan(OpKind::NewFolder { name: "fresh".into() }, vec![fs.target("C:\\")], Collision::Ask, &fs);
    assert_eq!(p.items[0].problem, None, "a new folder may be created in a drive root");
    let r = execute(&p, false, &fs, &mut FakeExec::new(&fs));
    assert_eq!(outcome(&r, "C:\\"), Outcome::Done);
    assert!(fs.exists("C:\\fresh"));
}

#[test]
fn links_and_hard_links_are_called_out_in_the_preview() {
    let fs = setup();
    fs.entries.borrow_mut().get_mut(&paths::norm("C:\\docs\\sub")).unwrap().1.is_link = true;
    let mut a = fs.target("C:\\docs\\a.txt");
    a.other_links = 2;
    let p = plan(OpKind::Recycle, vec![a, fs.target("C:\\docs\\sub")], Collision::Ask, &fs);
    assert_eq!(p.items[0].notes, vec![Note::OtherLinks(2)]);
    assert_eq!(p.items[1].notes, vec![Note::Link]);
    let text = p.preview(10).join("\n");
    assert!(text.contains("hard link: 2 other name(s)") && text.contains("the link itself, not its target"), "{text}");
}

#[test]
fn traversal_identity_detects_modification_but_ignores_folder_times() {
    let fs = setup();
    let mut a = fs.target("C:\\docs\\a.txt");
    a.identity = Identity::Observed { size: Some(10), mtime: Some(1000) };
    let mut d = fs.target("C:\\docs\\sub");
    d.identity = Identity::Observed { size: Some(0), mtime: Some(1) };
    let p = plan(OpKind::Recycle, vec![a.clone(), d], Collision::Ask, &fs);
    assert!(p.items.iter().all(|i| i.problem.is_none()), "{p:?}");
    a.identity = Identity::Observed { size: Some(10), mtime: Some(999) };
    let p = plan(OpKind::Recycle, vec![a], Collision::Ask, &fs);
    assert_eq!(p.items[0].problem, Some(Problem::Modified));
}

#[test]
fn preview_is_capped_for_large_batches() {
    let fs = setup();
    let t: Vec<Target> = (0..500).map(|i| {
        let p = format!("C:\\docs\\f{i}.txt");
        fs.file(&p, 1);
        fs.target(&p)
    }).collect();
    let p = plan(OpKind::Recycle, t, Collision::Ask, &fs);
    let pre = p.preview(20);
    assert_eq!(pre.len(), 1 + 20 + 1);
    assert_eq!(pre.last().unwrap(), "… and 480 more");
}
