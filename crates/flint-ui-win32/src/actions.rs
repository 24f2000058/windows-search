//! File actions on the selected results (Windows).
//!
//! Every action resolves the selection to [`Target`]s (path plus the
//! identity the index holds), and destructive ones go through
//! `flint_ops::plan` → reviewed preview → `flint_ops::execute` (final
//! revalidation, `IFileOperation`, verification by observation). Shell verbs
//! that would bypass that pipeline ("delete", "rename" from the native
//! context menu) are intercepted and routed through it.

use crate::win::{Modal, State};
use flint_config::OnCollision;
use flint_fuzzy::table::{flags, EntryId};
use flint_ops::win::{self as shell, MenuChoice, ShellExecutor, WinProbe};
use flint_ops::{check, execute, paths, plan, Collision, Identity, OpKind, Probe, Target};
use std::collections::HashMap;
use std::sync::atomic::AtomicBool;
use windows_sys::Win32::Foundation::*;
use windows_sys::Win32::UI::Controls::*;
use windows_sys::Win32::UI::WindowsAndMessaging::*;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Act {
    Open,
    OpenWith,
    Reveal,
    CopyPath,
    CopyFiles,
    CutFiles,
    Paste,
    Rename,
    MoveTo,
    CopyTo,
    NewFolder,
    Recycle,
    DeletePermanently,
    Compare,
    Duplicates,
    Properties,
}

impl Act {
    /// Menu order and labels (with shortcuts); the menu id is position + 1.
    const MENU: &'static [(Act, &'static str)] = &[
        (Act::Open, "&Open\tEnter"),
        (Act::OpenWith, "Open &with…\tShift+Enter"),
        (Act::Reveal, "Show in &Explorer\tCtrl+Enter"),
        (Act::CopyPath, "Copy &path\tCtrl+Shift+C"),
        (Act::CopyFiles, "&Copy\tAlt+C"),
        (Act::CutFiles, "Cu&t\tAlt+X"),
        (Act::Paste, "&Paste into folder\tAlt+V"),
        (Act::Rename, "&Rename…\tF2"),
        (Act::MoveTo, "&Move to…"),
        (Act::CopyTo, "Copy t&o…"),
        (Act::NewFolder, "&New folder…\tCtrl+Shift+N"),
        (Act::Recycle, "&Delete (Recycle Bin)\tCtrl+Del"),
        (Act::DeletePermanently, "Delete perma&nently…\tCtrl+Shift+Del"),
        (Act::Compare, "Co&mpare the two files"),
        (Act::Duplicates, "Find d&uplicates"),
        (Act::Properties, "Propert&ies\tAlt+Enter"),
    ];
    fn id(self) -> u32 {
        Self::MENU.iter().position(|(a, _)| *a == self).unwrap() as u32 + 1
    }
    fn from_id(id: u32) -> Option<Act> {
        Self::MENU.get(id.checked_sub(1)? as usize).map(|(a, _)| *a)
    }
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

fn raw(h: HWND) -> shell::RawHwnd {
    h as shell::RawHwnd
}


/// Selected result rows (multi-select), falling back to the focused row.
pub fn selected_ids(s: &State) -> Vec<EntryId> {
    let m = s.model.borrow();
    let mut out = Vec::new();
    let mut i: isize = -1;
    loop {
        i = unsafe { SendMessageW(s.list, LVM_GETNEXTITEM, i as WPARAM, LVNI_SELECTED as LPARAM) };
        if i < 0 {
            break;
        }
        if let Some(h) = m.hits.get(i as usize) {
            out.push(h.id);
        }
    }
    if out.is_empty() {
        out.extend(m.selected_id());
    }
    out
}

/// What the index knows about each entry. Hard-link counts come from one
/// pass over the table for the whole selection.
fn targets(s: &State, ids: &[EntryId]) -> Vec<Target> {
    let t = s.table.read().unwrap();
    let wanted: HashMap<(u16, u64), usize> = ids.iter().filter(|&&i| t.is_live(i)).map(|&i| ((t.volume(i), t.key(i)), 0)).collect();
    let mut links = wanted;
    for id in 0..t.len() as EntryId {
        if t.is_live(id) {
            if let Some(n) = links.get_mut(&(t.volume(id), t.key(id))) {
                *n += 1;
            }
        }
    }
    ids.iter()
        .filter(|&&i| t.is_live(i))
        .map(|&i| {
            let m = t.meta(i);
            let is_dir = m.flags & flags::DIR != 0;
            let file_ids = s.catalog.as_ref().is_some_and(|c| c.has_file_ids(m.volume));
            let identity = if file_ids {
                Identity::FileId(t.key(i))
            } else if m.flags & flags::META_STALE != 0 {
                Identity::Observed { size: None, mtime: None }
            } else {
                Identity::Observed { size: Some(m.size), mtime: (m.mtime != flint_fuzzy::table::NO_TIME).then_some(m.mtime) }
            };
            let other_links = if file_ids && !is_dir { links.get(&(m.volume, t.key(i))).copied().unwrap_or(1).saturating_sub(1) } else { 0 };
            Target { path: t.path(i), is_dir, identity, other_links }
        })
        .collect()
}

/// A folder or clipboard item that is not (necessarily) an index entry:
/// identified only by what is there now.
fn live_target(path: &str) -> Option<Target> {
    let st = WinProbe.stat(path).ok()??;
    Some(Target { path: path.to_owned(), is_dir: st.is_dir, identity: Identity::Observed { size: None, mtime: None }, other_links: 0 })
}

fn message_box(s: &State, text: &str, title: &str, flags: u32) -> i32 {
    let _m = Modal::new(s);
    let (t, c) = (wide(text), wide(title));
    unsafe { MessageBoxW(s.hwnd, t.as_ptr(), c.as_ptr(), flags | MB_TOPMOST) }
}

fn collision(s: &State) -> Collision {
    match s.files.on_collision {
        OnCollision::Ask => Collision::Ask,
        OnCollision::KeepBoth => Collision::KeepBoth,
        OnCollision::Skip => Collision::Skip,
    }
}

/// Plan, review, revalidate, execute and verify; reports in the status line.
fn run_plan(s: &State, kind: OpKind, targets: Vec<Target>) {
    if targets.is_empty() {
        s.say("nothing selected");
        return;
    }
    let p = plan(kind, targets, collision(s), &WinProbe);
    let permanent = p.kind == OpKind::DeletePermanently;
    if let Some(f) = &p.fatal {
        message_box(s, &p.preview(15).join("\n"), "Flint", MB_OK | MB_ICONWARNING);
        s.say(&format!("{}: {f}", p.kind.verb()));
        return;
    }
    let confirm = p.needs_confirmation() || (p.kind == OpKind::Recycle && s.files.confirm_recycle);
    if confirm {
        let mut text = p.preview(15).join("\n");
        text.push_str("\n\nContinue?");
        let style = if permanent { MB_YESNO | MB_ICONWARNING | MB_DEFBUTTON2 } else { MB_YESNO | MB_ICONQUESTION };
        if message_box(s, &text, p.kind.verb(), style) != IDYES {
            s.say("cancelled; nothing was changed");
            return;
        }
    }
    let report = {
        let _m = Modal::new(s);
        execute(&p, permanent && confirm, &WinProbe, &mut ShellExecutor { hwnd: raw(s.hwnd) })
    };
    let skipped: Vec<String> = report.outcomes.iter().filter_map(|(path, o)| match o {
        flint_ops::Outcome::Skipped(why) => Some(format!("{}: {why}", paths::file_name(path))),
        _ => None,
    }).collect();
    let mut line = format!("{}: {}", p.kind.verb(), report.summary());
    if let Some(first) = skipped.first() {
        line.push_str(&format!(" — {first}"));
    }
    s.say(&line);
    s.refresh_after_change();
}

/// Revalidated paths of the selection, or None (with a message) if any
/// selected entry no longer matches the filesystem.
fn valid_paths(s: &State, ts: &[Target]) -> Option<Vec<String>> {
    for t in ts {
        if let Err(p) = check(t, WinProbe.stat(&t.path)) {
            s.say(&format!("{}: {p} — refresh the search", t.path));
            return None;
        }
    }
    Some(ts.iter().map(|t| t.path.clone()).collect())
}

pub fn run(s: &State, act: Act) {
    let ids = selected_ids(s);
    let ts = targets(s, &ids);
    let one = || ts.first().cloned();
    let hwnd = raw(s.hwnd);
    match act {
        Act::Open | Act::OpenWith | Act::Properties => {
            let Some(t) = one() else { return s.say("nothing selected") };
            let Some(p) = valid_paths(s, std::slice::from_ref(&t)) else { return };
            let r = {
                let _m = Modal::new(s);
                match act {
                    Act::Open => shell::open(hwnd, &p[0]),
                    Act::OpenWith if t.is_dir => Err("folders open in Explorer; use Open".into()),
                    Act::OpenWith => shell::open_with(hwnd, &p[0]),
                    _ => shell::properties(hwnd, &p[0]),
                }
            };
            match r {
                Ok(()) => s.leave(),
                Err(e) => s.say(&format!("could not open {}: {e}", p[0])),
            }
        }
        Act::Reveal => {
            let Some(p) = valid_paths(s, &ts) else { return };
            match shell::reveal(&p) {
                Ok(()) => s.leave(),
                Err(e) => s.say(&format!("could not open Explorer: {e}")),
            }
        }
        Act::CopyPath => {
            let text = ts.iter().map(|t| t.path.as_str()).collect::<Vec<_>>().join("\r\n");
            match shell::copy_text(hwnd, &text) {
                Ok(()) => s.say(&format!("copied {} path(s)", ts.len())),
                Err(e) => s.say(&e),
            }
        }
        Act::CopyFiles | Act::CutFiles => {
            let Some(p) = valid_paths(s, &ts) else { return };
            let cut = act == Act::CutFiles;
            match shell::copy_files(hwnd, &p, cut) {
                Ok(()) => s.say(&format!("{} {} item(s); paste with Alt+V or in Explorer", if cut { "cut" } else { "copied" }, p.len())),
                Err(e) => s.say(&e),
            }
        }
        Act::Paste => {
            let Some(dest) = one().map(|t| if t.is_dir { t.path } else { paths::parent(&t.path).unwrap_or(&t.path).to_owned() }) else { return s.say("select a folder (or a file in it) to paste into") };
            match shell::clipboard_files(hwnd) {
                Ok(Some((files, cut))) => {
                    let items: Vec<Target> = files.iter().filter_map(|f| live_target(f)).collect();
                    let kind = if cut { OpKind::Move { dest_dir: dest } } else { OpKind::Copy { dest_dir: dest } };
                    run_plan(s, kind, items);
                }
                Ok(None) => s.say("the clipboard holds no files"),
                Err(e) => s.say(&e),
            }
        }
        Act::Rename => {
            let Some(t) = one().filter(|_| ts.len() == 1) else { return s.say("select one item to rename") };
            let old = paths::file_name(&t.path).to_owned();
            let name = {
                let _m = Modal::new(s);
                crate::prompt::prompt(s.hwnd, "Rename", &format!("New name for {old}:"), &old, !t.is_dir, s.fonts.get().0, s.dpi.get())
            };
            match name {
                Some(n) if n != old => run_plan(s, OpKind::Rename { new_name: n }, vec![t]),
                _ => s.say("rename cancelled"),
            }
        }
        Act::MoveTo | Act::CopyTo => {
            if ts.is_empty() {
                return s.say("nothing selected");
            }
            let dest = {
                let _m = Modal::new(s);
                shell::pick_folder(hwnd, if act == Act::MoveTo { "Move to" } else { "Copy to" })
            };
            if let Some(d) = dest {
                run_plan(s, if act == Act::MoveTo { OpKind::Move { dest_dir: d } } else { OpKind::Copy { dest_dir: d } }, ts);
            }
        }
        Act::NewFolder => {
            let Some(folder) = one().map(|t| if t.is_dir { t.path } else { paths::parent(&t.path).unwrap_or(&t.path).to_owned() }) else { return s.say("select a folder (or a file in it)") };
            let name = {
                let _m = Modal::new(s);
                crate::prompt::prompt(s.hwnd, "New folder", &format!("Create a folder in {folder}:"), "New folder", false, s.fonts.get().0, s.dpi.get())
            };
            if let (Some(n), Some(t)) = (name, live_target(&folder)) {
                run_plan(s, OpKind::NewFolder { name: n }, vec![t]);
            }
        }
        Act::Recycle => run_plan(s, OpKind::Recycle, ts),
        Act::DeletePermanently => run_plan(s, OpKind::DeletePermanently, ts),
        Act::Compare => compare(s, &ts),
        Act::Duplicates => duplicates(s, &ids),
    }
}

fn compare(s: &State, ts: &[Target]) {
    let [a, b] = ts else { return s.say("select exactly two files to compare") };
    if a.is_dir || b.is_dir {
        return s.say("compare works on files, not folders");
    }
    let Some(p) = valid_paths(s, ts) else { return };
    let budget = s.files.compare_limit_mb.saturating_mul(1 << 20);
    s.say("comparing…");
    s.background(move || {
        use flint_ops::compare::{compare_files, Comparison};
        let r = compare_files(p[0].as_ref(), p[1].as_ref(), budget, &AtomicBool::new(false));
        let (a, b) = (paths::file_name(&p[0]).to_owned(), paths::file_name(&p[1]).to_owned());
        match r {
            Ok(Comparison::Identical) => format!("{a} and {b} are identical"),
            Ok(Comparison::SizeDiffers { a: x, b: y }) => format!("{a} and {b} differ in size ({x} vs {y} bytes)"),
            Ok(Comparison::DiffersAt(off)) => format!("{a} and {b} differ, first at byte {off}"),
            Ok(Comparison::Incomplete { compared }) => format!("{a} and {b}: first {compared} bytes identical; stopped at the compare limit (files.compare_limit_mb)"),
            Err(e) => format!("compare failed: {e}"),
        }
    });
}

fn duplicates(s: &State, ids: &[EntryId]) {
    let [id] = ids else { return s.say("select one file to find its duplicates") };
    let limits = flint_ops::dupes::Limits { max_bytes: s.files.duplicate_limit_mb.saturating_mul(1 << 20), ..Default::default() };
    let (orig, cands, truncated) = {
        let t = s.table.read().unwrap();
        let (c, more) = flint_ops::dupes::candidates(&t, *id, limits.max_candidates);
        let size = t.meta(*id).size;
        (t.path(*id), c.into_iter().map(|c| (c, t.path(c), size)).collect::<Vec<_>>(), more)
    };
    if cands.is_empty() {
        return s.say("no other indexed file has the same size (folders, empty files and files with unrefreshed sizes are not checked)");
    }
    s.say(&format!("checking {} same-size file(s)…", cands.len()));
    s.background(move || {
        let r = flint_ops::dupes::confirm(&orig, &cands, &limits, &AtomicBool::new(false));
        let path_of = |id: EntryId| cands.iter().find(|c| c.0 == id).map(|c| c.1.clone()).unwrap_or_default();
        let mut lines = vec![format!("{}: {} identical, {} different, {} unchecked{}", paths::file_name(&orig), r.identical.len(), r.different, r.unchecked.len(), if truncated { " (more same-size files exist than were checked)" } else { "" })];
        lines.extend(r.identical.iter().take(12).map(|&i| path_of(i)));
        if r.identical.len() > 12 {
            lines.push(format!("… and {} more", r.identical.len() - 12));
        }
        lines.join("\n")
    });
}

/// Context menu: our items on top, then the native shell menu (which keeps
/// third-party extensions). `at` is in screen coordinates.
pub fn context_menu(s: &State, at: POINT, extended: bool) {
    let ids = selected_ids(s);
    let ts = targets(s, &ids);
    if ts.is_empty() {
        return;
    }
    let files = ts.iter().filter(|t| !t.is_dir).count();
    let single = ts.len() == 1;
    let enabled = |a: Act| match a {
        Act::Open | Act::Properties | Act::Rename => single,
        Act::OpenWith => single && files == 1,
        Act::Compare => ts.len() == 2 && files == 2,
        Act::Duplicates => single && files == 1,
        Act::Paste | Act::NewFolder => single,
        _ => true,
    };
    let mut items: Vec<(u32, String, bool)> = Vec::new();
    for (i, (a, label)) in Act::MENU.iter().enumerate() {
        if [3, 7, 11, 13, 15].contains(&i) {
            items.push((0, String::new(), true));
        }
        items.push((a.id(), (*label).to_owned(), enabled(*a)));
    }
    // Approved plugins' result actions (they receive the selected paths
    // only with the selection_paths permission).
    let plugin_items = s.plugins.menu_items();
    if !plugin_items.is_empty() {
        items.push((0, String::new(), true));
        items.extend(plugin_items);
    }
    // The native menu acts on whatever is at the path now, so it is only
    // offered for entries that still match the index.
    let native: Vec<String> = ts.iter().filter(|t| check(t, WinProbe.stat(&t.path)).is_ok()).map(|t| t.path.clone()).collect();
    let native = if native.len() == ts.len() { native } else { Vec::new() };
    let choice = {
        let _m = Modal::new(s);
        shell::context_menu(raw(s.hwnd), &native, &items, at.x, at.y, extended)
    };
    match choice {
        Ok(MenuChoice::Custom(id)) if id >= crate::plugins::FIRST_MENU_ID => {
            // Revalidated like any other action: only entries still on disk.
            let paths: Vec<String> = ts.iter().filter(|t| check(t, WinProbe.stat(&t.path)).is_ok()).map(|t| t.path.clone()).collect();
            crate::plugins::run_menu_action(s, id, paths);
        }
        Ok(MenuChoice::Custom(id)) => {
            if let Some(a) = Act::from_id(id) {
                run(s, a);
            }
        }
        Ok(MenuChoice::Intercepted(v)) if v == "delete" => run(s, if extended { Act::DeletePermanently } else { Act::Recycle }),
        Ok(MenuChoice::Intercepted(_)) => run(s, Act::Rename),
        Ok(MenuChoice::Shell(verb)) => {
            s.say(&format!("ran \"{}\"", if verb.is_empty() { "shell command" } else { &verb }));
            s.refresh_after_change();
        }
        Ok(MenuChoice::None) => {}
        Err(e) => s.say(&format!("menu command failed: {e}")),
    }
}

/// Position for a keyboard-invoked context menu: below the focused row.
pub fn menu_point_for_keyboard(s: &State) -> POINT {
    let row = s.model.borrow().selected.unwrap_or(0);
    let mut rc: RECT = unsafe { std::mem::zeroed() };
    rc.left = LVIR_BOUNDS as i32;
    unsafe {
        SendMessageW(s.list, LVM_GETITEMRECT, row, &mut rc as *mut _ as LPARAM);
    }
    let mut pt = POINT { x: rc.left + 24, y: rc.bottom };
    unsafe { windows_sys::Win32::Graphics::Gdi::ClientToScreen(s.list, &mut pt) };
    pt
}
