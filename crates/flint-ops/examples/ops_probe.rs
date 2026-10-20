//! Exercises the Windows file-operation pipeline end to end on a scratch
//! folder: plan → final check → `IFileOperation` → verification.
//!
//! Usage: `ops_probe.exe [SCRATCH_DIR]` (default: %TEMP%\flint-ops-probe).
//! Prints one line per step and exits non-zero if any expectation fails.

#[cfg(windows)]
fn main() {
    use flint_ops::win::{init_com, ShellExecutor, WinProbe};
    use flint_ops::{execute, plan, Collision, Identity, OpKind, Outcome, Probe, Target};
    init_com().expect("OLE init");
    let root = std::env::args().nth(1).map(std::path::PathBuf::from).unwrap_or_else(|| std::env::temp_dir().join("flint-ops-probe"));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(root.join("dest")).unwrap();
    let p = |n: &str| root.join(n).to_string_lossy().into_owned();
    for n in ["a.txt", "b.txt", "c.txt", "d.txt"] {
        std::fs::write(p(n), n).unwrap();
    }
    let target = |path: &str| {
        let st = WinProbe.stat(path).unwrap().expect("exists");
        Target { path: path.to_owned(), is_dir: st.is_dir, identity: Identity::FileId(st.file_id.unwrap()), other_links: 0 }
    };
    let mut failures = 0;
    let mut step = |name: &str, kind: OpKind, ts: Vec<Target>, permanent: bool, want: &[Outcome]| {
        let pl = plan(kind, ts, Collision::Ask, &WinProbe);
        let r = execute(&pl, permanent, &WinProbe, &mut ShellExecutor { hwnd: 0 });
        let got: Vec<Outcome> = r.outcomes.iter().map(|o| o.1.clone()).collect();
        let ok = got == want;
        failures += !ok as u32;
        println!("{} {name}: {} {:?}", if ok { "ok  " } else { "FAIL" }, r.summary(), if ok { Vec::new() } else { r.outcomes.clone() });
    };
    let st = WinProbe.stat(&p("a.txt")).unwrap().unwrap();
    println!("stat a.txt: file id {:?}, size {}, mtime {}", st.file_id, st.size, st.mtime);
    step("copy", OpKind::Copy { dest_dir: p("dest") }, vec![target(&p("a.txt"))], false, &[Outcome::Done]);
    step("rename", OpKind::Rename { new_name: "renamed.txt".into() }, vec![target(&p("b.txt"))], false, &[Outcome::Done]);
    step("new folder", OpKind::NewFolder { name: "made".into() }, vec![target(&root.to_string_lossy())], false, &[Outcome::Done]);
    step("move", OpKind::Move { dest_dir: p("made") }, vec![target(&p("c.txt"))], false, &[Outcome::Done]);
    // Stale index entry: d.txt replaced by a different file with the same
    // name (written aside and renamed over it, as editors and installers
    // do, so the replacement is a distinct file even where IDs are reused).
    let stale = target(&p("d.txt"));
    std::fs::write(p("d.new"), "new").unwrap();
    std::fs::rename(p("d.new"), p("d.txt")).unwrap();
    step("replaced file is skipped", OpKind::Recycle, vec![stale], false, &[]);
    step("recycle", OpKind::Recycle, vec![target(&p("d.txt"))], false, &[Outcome::Done]);
    step("permanent delete", OpKind::DeletePermanently, vec![target(&p("renamed.txt"))], true, &[Outcome::Done]);
    let _ = std::fs::remove_dir_all(&root);
    std::process::exit(failures as i32);
}

#[cfg(not(windows))]
fn main() {
    eprintln!("ops_probe runs on Windows");
}
