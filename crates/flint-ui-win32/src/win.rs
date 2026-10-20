//! Direct-Win32 launcher spike (UI-stack benchmark candidate).
//!
//! Single UI thread; search runs on the session worker and posts results back
//! with `PostMessageW`, so the UI never blocks on search or indexing. The
//! list is an owner-data (virtual) ListView: only visible rows are queried
//! and drawn. Match highlighting is custom-drawn; `LVN_GETDISPINFOW` still
//! supplies plain text so screen readers see every row.
//!
//! Measurements printed to the console (QueryPerformanceCounter):
//! * hotkey -> window painted and focused
//! * keystroke -> final results painted
//!
//! Indexing runs in a [`Catalog`] (one worker per volume); settings come from
//! the versioned config in the data directory (installed or portable).
//!
//! File actions (open, reveal, clipboard, rename, move/copy, delete, compare,
//! duplicates, native context menu) are in [`crate::actions`]; every
//! destructive one is planned, reviewed and revalidated by `flint-ops`.
//!
//! Known limitations (tracked in docs/requirements.md): no settings window
//! (edit config.toml), comctl32 v6 manifest not embedded, file-type icons
//! only.

use crate::icons::Icons;
use crate::actions::Act;
use crate::model::{search_options, status_line, Coverage, LatencyStats, Palette, ResultsModel};
use crate::ux::{deeper_action, deeper_options, format_size, format_time, match_label, FilterChoice};
use flint_config::Theme;
use flint_index::catalog::{Catalog, CatalogOptions};
use flint_commands::{websearch, Action, AppAction, Registry};
use flint_fuzzy::engine::{highlight, Options};
use flint_fuzzy::table::flags;
use flint_fuzzy::query::{parse_with, Clock, UtcOffset};
use flint_fuzzy::session::{Event, Session};
use flint_fuzzy::table::{Meta, NameTable, NO_PARENT};
use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::ptr::{null, null_mut};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;
use windows_sys::Win32::Foundation::*;
use windows_sys::Win32::Graphics::Gdi::*;
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::System::Performance::{QueryPerformanceCounter, QueryPerformanceFrequency};
use windows_sys::Win32::UI::Controls::*;
use windows_sys::Win32::UI::HiDpi::{GetDpiForWindow, SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2};
use windows_sys::Win32::UI::Input::KeyboardAndMouse::*;
use windows_sys::Win32::UI::Shell::{DefSubclassProc, SetWindowSubclass, ShellExecuteW};
use windows_sys::Win32::UI::WindowsAndMessaging::*;

const WM_APP_EVENT: u32 = WM_APP + 1;
const WM_APP_INDEX: u32 = WM_APP + 2;
/// A background task finished; lParam is a `Box<String>` to show.
const WM_APP_NOTICE: u32 = WM_APP + 3;
/// A preview arrived; lParam is a `Box<(u64, Delivered)>`.
pub(crate) const WM_APP_PREVIEW: u32 = WM_APP + 4;
/// A deep search finished; lParam is a `Box<DeepDone>`.
const WM_APP_DEEP: u32 = WM_APP + 5;
/// A plugin answered; lParam is a `Box<flint_plugin::service::Delivery>`.
pub(crate) const WM_APP_PLUGIN: u32 = WM_APP + 6;
/// An update check or download finished; lParam is a `Box<UpdateEvent>`.
pub(crate) const WM_APP_UPDATE: u32 = WM_APP + 7;
/// Notify-mode update checks (the interval itself is in the check state).
const UPDATE_TIMER: usize = 8;
const ID_PANE: isize = 120;
const ID_EDIT: isize = 100;
const ID_LIST: isize = 101;
/// Filter-bar dropdowns: ID_FILTER + index.
const ID_FILTER: isize = 110;
const COLUMNS: [(&str, i32); 5] = [("Name", 240), ("Folder", 0), ("Size", 76), ("Modified", 124), ("Match", 100)];
const WIDTH: i32 = 860;
const FILTER_W: i32 = 118;
const HOTKEY_ID: i32 = 1;
const SELFTEST_TIMER: usize = 7;
/// Coalesce index-change repaints.
static INDEX_REPAINT_PENDING: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

fn qpc() -> i64 {
    let mut v = 0i64;
    unsafe { QueryPerformanceCounter(&mut v) };
    v
}

/// UI-thread state. Win32 re-enters the window procedure synchronously
/// (SetWindowText -> EN_CHANGE, UpdateWindow -> WM_PAINT, LVM_* ->
/// LVN_GETDISPINFO/custom draw), so mutable fields are individually wrapped
/// and no borrow is ever held across a call that can dispatch messages.
pub(crate) struct State {
    pub(crate) hwnd: HWND,
    edit: HWND,
    pub(crate) list: HWND,
    combos: Vec<HWND>,
    filters: RefCell<Vec<FilterChoice>>,
    pub(crate) table: Arc<RwLock<NameTable>>,
    session: Session,
    latest: Arc<AtomicU64>,
    coverage: Arc<Mutex<Coverage>>,
    commands: Registry,
    freq: f64,
    pub(crate) model: RefCell<ResultsModel>,
    query_text: RefCell<String>,
    pub(crate) command_line: RefCell<String>,
    pub(crate) fonts: Cell<(HFONT, HFONT)>,
    pub(crate) dpi: Cell<u32>,
    t_hotkey: Cell<Option<i64>>,
    t_key: Cell<Option<(u64, i64)>>,
    hotkey_stats: RefCell<LatencyStats>,
    key_stats: RefCell<LatencyStats>,
    disp_buf: RefCell<Vec<u16>>,
    /// `--selftest`: queries typed programmatically, one per timer tick.
    pub(crate) selftest: Vec<String>,
    selftest_pos: Cell<usize>,
    /// None in `--synthetic` mode.
    pub(crate) catalog: Option<Arc<Catalog>>,
    indexing: flint_config::Indexing,
    only_root: Option<String>,
    /// Search options from the settings.
    base_opts: Options,
    /// Current options: `base_opts`, or deeper after F3 (reset on edit).
    opts: RefCell<Options>,
    tz: Arc<UtcOffset>,
    pub(crate) files: flint_config::Files,
    /// Open dialogs/menus; while > 0, losing activation does not hide.
    modal: Cell<u32>,
    /// Re-run the query when the index next changes (after a file action).
    refresh_pending: Cell<bool>,
    /// Range-selection anchor (Shift+Up/Down).
    anchor: Cell<Option<usize>>,
    /// Viewer pane (crate::pane).
    pub(crate) pane_edit: HWND,
    pub(crate) viewer: RefCell<Option<flint_preview::scheduler::Viewer>>,
    pub(crate) viewer_on: Cell<bool>,
    pub(crate) viewer_cfg: flint_config::Viewer,
    pub(crate) preview_gen: Cell<u64>,
    pub(crate) last_preview: RefCell<Option<flint_preview::scheduler::FileKey>>,
    pub(crate) pane_header: RefCell<String>,
    pub(crate) pane_bitmap: Cell<(HBITMAP, i32, i32)>,
    pub(crate) host_exe: std::path::PathBuf,
    mono_font: HFONT,
    /// Background content indexing (privacy.content_indexing).
    content_service: RefCell<Option<flint_index::content::ContentService>>,
    content_index: Option<Arc<flint_index::content::ContentIndex>>,
    /// Mounted volumes skipped by settings (deep-search scopes).
    deep_scopes: Arc<Mutex<Vec<String>>>,
    deep_cancel: RefCell<Arc<std::sync::atomic::AtomicBool>>,
    deep_cfg: flint_config::DeepSearch,
    /// Deep-search hits are in the table (removed on the next edit).
    deep_active: Cell<bool>,
    index_was_complete: Cell<bool>,
    theme: Theme,
    pub(crate) palette: Cell<Palette>,
    /// (window background, edit background)
    brushes: Cell<(HBRUSH, HBRUSH)>,
    icons: RefCell<Icons>,
    /// Config/indexing messages shown until the user types.
    pub(crate) notice: RefCell<String>,
    pub(crate) plugins: crate::plugins::Plugins,
    pub(crate) updates_cfg: flint_config::Updates,
    /// Last update-check outcome (diagnostics).
    pub(crate) update_status: RefCell<String>,
    pub(crate) data_dir: std::path::PathBuf,
    pub(crate) portable: bool,
    pub(crate) redact_diagnostics: bool,
    started: std::time::Instant,
    /// A command or plugin owns the input: file-search events are ignored.
    claimed: Cell<bool>,
    /// Startup problems (diagnostics).
    startup_notes: Vec<String>,
}

/// Marks a dialog/menu as open for its lifetime (see `State::modal`).
pub(crate) struct Modal<'a>(&'a State);

impl<'a> Modal<'a> {
    pub(crate) fn new(s: &'a State) -> Self {
        s.modal.set(s.modal.get() + 1);
        Modal(s)
    }
}

impl Drop for Modal<'_> {
    fn drop(&mut self) {
        self.0.modal.set(self.0.modal.get() - 1);
    }
}

impl State {
    /// Shows a message in the status strip until the next keystroke.
    pub(crate) fn say(&self, text: &str) {
        println!("action: {text}");
        *self.command_line.borrow_mut() = text.to_owned();
        unsafe { InvalidateRect(self.hwnd, null(), 0) };
    }

    /// The action handed the user to another window (opened a file,
    /// Explorer, properties).
    pub(crate) fn leave(&self) {
        hide(self);
    }

    /// After a file action: nudge indexing and re-run the query once the
    /// index reflects the change (journal volumes see it within a poll).
    pub(crate) fn refresh_after_change(&self) {
        self.refresh_pending.set(true);
        if let Some(c) = &self.catalog {
            for k in c.keys() {
                c.wake(&k);
            }
        }
    }

    /// Runs `f` off the UI thread and shows its result as a notice.
    pub(crate) fn background(&self, f: impl FnOnce() -> String + Send + 'static) {
        let hwnd = self.hwnd as usize;
        std::thread::spawn(move || {
            let p = Box::into_raw(Box::new(f()));
            unsafe {
                if PostMessageW(hwnd as HWND, WM_APP_NOTICE, 0, p as LPARAM) == 0 {
                    drop(Box::from_raw(p));
                }
            }
        });
    }

    fn clock(&self) -> Clock<'_> {
        let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs() as i64);
        Clock { now, offset: &*self.tz }
    }
}

fn coverage(s: &State) -> Coverage {
    match &s.catalog {
        Some(c) => c.coverage(),
        None => s.coverage.lock().unwrap().clone(),
    }
}

fn apply_palette(s: &State) {
    let p = crate::theme::current(s.theme);
    s.palette.set(p);
    let (old_bg, old_edit) = s.brushes.get();
    unsafe {
        s.brushes.set((CreateSolidBrush(p.bg), CreateSolidBrush(p.edit_bg)));
        if !old_bg.is_null() {
            DeleteObject(old_bg);
            DeleteObject(old_edit);
        }
        SendMessageW(s.list, LVM_SETBKCOLOR, 0, p.bg as LPARAM);
        InvalidateRect(s.hwnd, null(), 1);
        InvalidateRect(s.edit, null(), 1);
        InvalidateRect(s.list, null(), 1);
    }
}

/// Re-plans volumes off the UI thread (startup, device arrival/removal).
fn resync_volumes(s: &State) {
    if let Some(cat) = s.catalog.clone() {
        let (cfg, root, scopes) = (s.indexing.clone(), s.only_root.clone(), s.deep_scopes.clone());
        std::thread::spawn(move || {
            let (notes, unindexed) = crate::volumes::sync(&cat, &cfg, root.as_deref());
            for n in notes {
                println!("volumes: {n}");
            }
            *scopes.lock().unwrap() = unindexed;
        });
    }
}

thread_local! {
    static STATE: RefCell<Option<Rc<State>>> = const { RefCell::new(None) };
}

pub(crate) fn state() -> Option<Rc<State>> {
    STATE.with(|s| s.borrow().clone())
}

fn scale(v: i32, dpi: u32) -> i32 {
    v * dpi as i32 / 96
}

fn make_fonts(dpi: u32) -> (HFONT, HFONT) {
    let face = wide("Segoe UI");
    let h = -scale(15, dpi);
    unsafe {
        (
            CreateFontW(h, 0, 0, 0, FW_NORMAL as i32, 0, 0, 0, DEFAULT_CHARSET as u32, OUT_DEFAULT_PRECIS as u32, CLIP_DEFAULT_PRECIS as u32, CLEARTYPE_QUALITY as u32, (DEFAULT_PITCH | FF_DONTCARE) as u32, face.as_ptr()),
            CreateFontW(h, 0, 0, 0, FW_BOLD as i32, 0, 0, 0, DEFAULT_CHARSET as u32, OUT_DEFAULT_PRECIS as u32, CLIP_DEFAULT_PRECIS as u32, CLEARTYPE_QUALITY as u32, (DEFAULT_PITCH | FF_DONTCARE) as u32, face.as_ptr()),
        )
    }
}

pub(crate) fn layout(s: &State) {
    let d = s.dpi.get();
    let (w, pad, eh, lh) = (scale(WIDTH, d), scale(8, d), scale(32, d), scale(420, d));
    unsafe {
        if s.viewer_on.get() {
            let (r, top) = pane_rect(s);
            let y = top + crate::pane::top_height(s);
            SetWindowPos(s.pane_edit, null_mut(), r.left, y, r.right - r.left, (r.bottom - y).max(0), SWP_NOZORDER | SWP_SHOWWINDOW);
            SendMessageW(s.pane_edit, WM_SETFONT, s.mono_font as WPARAM, 1);
        } else {
            ShowWindow(s.pane_edit, SW_HIDE);
        }
    }
    let fw = scale(FILTER_W, d);
    let n = s.combos.len() as i32;
    let edit_w = w - 2 * pad - n * (fw + pad);
    unsafe {
        SetWindowPos(s.edit, null_mut(), pad, pad, edit_w, eh, SWP_NOZORDER);
        for (i, c) in s.combos.iter().enumerate() {
            SetWindowPos(*c, null_mut(), pad + edit_w + pad + i as i32 * (fw + pad), pad + scale(2, d), fw, scale(300, d), SWP_NOZORDER);
        }
        SetWindowPos(s.list, null_mut(), pad, pad * 2 + eh, w - 2 * pad, lh, SWP_NOZORDER);
        let fixed: i32 = COLUMNS.iter().map(|c| c.1).sum();
        for (i, (_, cw)) in COLUMNS.iter().enumerate() {
            // The folder column takes the remaining width.
            let width = if *cw == 0 { w - 2 * pad - scale(fixed + 24, d) } else { scale(*cw, d) };
            SendMessageW(s.list, LVM_SETCOLUMNWIDTH, i, width as LPARAM);
        }
        for h in [s.edit, s.list].into_iter().chain(s.combos.iter().copied()) {
            SendMessageW(h, WM_SETFONT, s.fonts.get().0 as WPARAM, 1);
        }
    }
}

fn window_size(dpi: u32, viewer: bool) -> (i32, i32) {
    (scale(WIDTH + if viewer { crate::pane::PANE_W } else { 0 }, dpi), scale(8 * 3 + 32 + 420 + 26, dpi))
}

/// Viewer pane rectangle (client coordinates) and its top.
fn pane_rect(s: &State) -> (RECT, i32) {
    let d = s.dpi.get();
    let (pad, eh, lh) = (scale(8, d), scale(32, d), scale(420, d));
    let top = pad * 2 + eh;
    (RECT { left: scale(WIDTH, d) - pad / 2, top, right: scale(WIDTH + crate::pane::PANE_W, d) - pad, bottom: top + lh }, top)
}

/// F9: viewer on/off for this session.
fn toggle_viewer(s: &State) {
    let on = !s.viewer_on.get();
    s.viewer_on.set(on);
    if on {
        crate::pane::start(s);
    } else {
        crate::pane::stop(s);
    }
    unsafe {
        let mut r: RECT = std::mem::zeroed();
        GetWindowRect(s.hwnd, &mut r);
        let (w, h) = window_size(s.dpi.get(), on);
        SetWindowPos(s.hwnd, null_mut(), r.left, r.top, w, h, SWP_NOZORDER);
    }
    layout(s);
    crate::pane::selection_changed(s);
    s.say(if on { "viewer on (F9 to turn off)" } else { "viewer off: no file is read for previews" });
}

/// Result of a deep search, posted to the UI thread.
pub(crate) struct DeepDone {
    generation: u64,
    hits: Vec<flint_index::deep::DeepHit>,
    report: flint_index::deep::DeepReport,
}

/// F4: walks locations the index does not cover (bounded, cancellable).
fn deep_search(s: &State) {
    let mut scopes: Vec<std::path::PathBuf> = s.deep_scopes.lock().unwrap().iter().map(std::path::PathBuf::from).collect();
    if let Some(c) = &s.catalog {
        for v in c.statuses() {
            scopes.extend(v.unreadable.iter().cloned());
        }
    }
    let text = s.query_text.borrow().clone();
    if scopes.is_empty() {
        return s.say("nothing unindexed to search: every planned location is indexed (excluded folders are never searched)");
    }
    if text.trim().is_empty() {
        return s.say("type what to look for, then press F4");
    }
    s.deep_cancel.borrow().store(true, Ordering::SeqCst);
    let cancel = Arc::new(std::sync::atomic::AtomicBool::new(false));
    *s.deep_cancel.borrow_mut() = cancel.clone();
    let generation = s.latest.load(Ordering::SeqCst);
    let (exclude, opts, tz, hwnd) = (s.indexing.excluded_paths.clone(), s.opts.borrow().clone(), s.tz.clone(), s.hwnd as usize);
    let limits = flint_index::deep::DeepLimits { max_entries: s.deep_cfg.max_entries, max_time: Duration::from_secs(s.deep_cfg.max_seconds.max(1)), max_depth: s.deep_cfg.max_depth };
    s.say(&format!("deep search in {} unindexed location(s)… (editing the query cancels)", scopes.len()));
    std::thread::Builder::new()
        .name("flint-deep".into())
        .spawn(move || {
            let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs() as i64);
            let q = parse_with(&text, &Clock { now, offset: &*tz });
            let fs = flint_index::traverse::RealFs;
            let (hits, report) = flint_index::deep::deep_search(&fs, &scopes, &exclude, &q, &opts, &limits, &cancel);
            if cancel.load(Ordering::SeqCst) {
                return;
            }
            let p = Box::into_raw(Box::new(DeepDone { generation, hits, report }));
            unsafe {
                if PostMessageW(hwnd as HWND, WM_APP_DEEP, 0, p as LPARAM) == 0 {
                    drop(Box::from_raw(p));
                }
            }
        })
        .ok();
}

fn clear_deep(s: &State) {
    s.deep_cancel.borrow().store(true, Ordering::SeqCst);
    if s.deep_active.replace(false) {
        flint_index::deep::remove_deep(&mut s.table.write().unwrap());
    }
}

/// (Re)fills the filter dropdowns; "older than a year" depends on today.
fn fill_filters(s: &State) {
    let bar = crate::ux::filter_bar(&s.clock());
    for (c, choice) in s.combos.iter().zip(&bar) {
        unsafe {
            SendMessageW(*c, CB_RESETCONTENT, 0, 0);
            for (label, _) in &choice.options {
                let w = wide(label);
                SendMessageW(*c, CB_ADDSTRING, 0, w.as_ptr() as LPARAM);
            }
        }
    }
    *s.filters.borrow_mut() = bar;
    sync_filters(s);
}

/// Shows the query's current filter terms in the dropdowns (blank when a
/// typed value has no dropdown entry).
fn sync_filters(s: &State) {
    let text = s.query_text.borrow().clone();
    let bar = s.filters.borrow();
    for (c, choice) in s.combos.iter().zip(bar.iter()) {
        let sel = crate::ux::current_choice(&text, choice).map_or(-1, |i| i as isize);
        unsafe { SendMessageW(*c, CB_SETCURSEL, sel as WPARAM, 0) };
    }
}

/// A dropdown changed: rewrite that filter term in the query text.
fn on_filter_picked(s: &State, index: usize) {
    let sel = unsafe { SendMessageW(s.combos[index], CB_GETCURSEL, 0, 0) };
    let text = {
        let bar = s.filters.borrow();
        let (Some(choice), true) = (bar.get(index), sel >= 0) else { return };
        let value = choice.options.get(sel as usize).map(|o| o.1.clone()).unwrap_or_default();
        crate::ux::set_filter(&s.query_text.borrow(), choice, &value)
    };
    let w = wide(&text);
    unsafe {
        // Re-enters through EN_CHANGE, which submits the query.
        SetWindowTextW(s.edit, w.as_ptr());
        SetFocus(s.edit);
        SendMessageW(s.edit, EM_SETSEL, w.len() - 1, (w.len() - 1) as LPARAM);
    }
}

/// Shows the window centered on the monitor under the cursor.
fn show(s: &State) {
    unsafe {
        let mut pt = POINT { x: 0, y: 0 };
        GetCursorPos(&mut pt);
        let mon = MonitorFromPoint(pt, MONITOR_DEFAULTTONEAREST);
        let mut mi: MONITORINFO = std::mem::zeroed();
        mi.cbSize = std::mem::size_of::<MONITORINFO>() as u32;
        GetMonitorInfoW(mon, &mut mi);
        let (w, h) = window_size(s.dpi.get(), s.viewer_on.get());
        let wa = mi.rcWork;
        let x = wa.left + ((wa.right - wa.left) - w) / 2;
        let y = wa.top + ((wa.bottom - wa.top) - h) / 4;
        SetWindowPos(s.hwnd, HWND_TOPMOST, x, y, w, h, SWP_SHOWWINDOW);
        fill_filters(s);
        if s.viewer_on.get() {
            crate::pane::start(s);
            layout(s);
            crate::pane::selection_changed(s);
        }
        SetForegroundWindow(s.hwnd);
        SetFocus(s.edit);
        SendMessageW(s.edit, EM_SETSEL, 0, -1);
        UpdateWindow(s.hwnd);
        UpdateWindow(s.list);
    }
    if let Some(t0) = s.t_hotkey.take() {
        let ms = (qpc() - t0) as f64 * 1000.0 / s.freq;
        s.hotkey_stats.borrow_mut().record(ms);
        println!("hotkey -> painted+focused: {ms:.2} ms");
    }
}

fn hide(s: &State) {
    unsafe { ShowWindow(s.hwnd, SW_HIDE) };
    // No helper process while hidden (it restarts on the next preview).
    crate::pane::stop(s);
    // Plugin processes too (they restart when next used).
    s.plugins.stop();
    clear_deep(s);
    // Release derived search caches while idle; history is never stored.
    s.table.read().unwrap().trim_caches();
    s.session.clear_cache();
    if let Some((private, ws)) = crate::resources::memory_usage() {
        println!("hidden: private bytes {:.1} MB, working set {:.1} MB", private as f64 / 1e6, ws as f64 / 1e6);
    }
}

fn submit(s: &State) {
    let mut buf = vec![0u16; 1024];
    let n = unsafe { GetWindowTextW(s.edit, buf.as_mut_ptr(), buf.len() as i32) } as usize;
    let text = String::from_utf16_lossy(&buf[..n]);
    // A new query starts from the configured options again, without the
    // previous query's deep-search hits.
    *s.opts.borrow_mut() = s.base_opts.clone();
    clear_deep(s);
    run_query(s, text);
    sync_filters(s);
}

/// F3: re-runs the current query one step deeper (weaker matches, then all
/// remaining indexed candidates).
fn search_deeper(s: &State) {
    let d = {
        let m = s.model.borrow();
        m.status.as_ref().and_then(|st| deeper_action(st, &s.opts.borrow()))
    };
    match d {
        Some(d) => {
            *s.opts.borrow_mut() = deeper_options(&s.base_opts, d);
            let text = s.query_text.borrow().clone();
            run_query(s, text);
        }
        None => s.say("nothing deeper to search: every indexed candidate was already examined"),
    }
}

fn run_query(s: &State, text: String) {
    let cmds = s.commands.evaluate(&text);
    *s.command_line.borrow_mut() = cmds.first().map(|c| format!("{}  —  {}", c.title, c.subtitle)).unwrap_or_default();
    s.notice.borrow_mut().clear();
    *s.query_text.borrow_mut() = text.clone();
    if let (Some(c), true) = (&s.catalog, s.indexing.pause_while_typing) {
        c.governor().note_user_activity();
    }
    let plugin = !cmds.first().is_some_and(|c| c.exclusive) && crate::plugins::route(s, &text);
    s.claimed.set(plugin || cmds.first().is_some_and(|c| c.exclusive));
    if s.claimed.get() {
        // `/s`, `/diag`… and plugin prefixes claim the input; no file
        // search runs for them.
        if !plugin {
            s.plugins.showing.borrow_mut().take();
            *s.model.borrow_mut() = ResultsModel::default();
        }
        // Advance the generation so an in-flight file search is canceled and
        // its late results are rejected as stale.
        let g = s.session.submit("", s.opts.borrow().clone());
        s.latest.store(g, Ordering::SeqCst);
        set_count(s);
        unsafe { InvalidateRect(s.hwnd, null(), 1) };
        return;
    }
    let generation = s.session.submit(&text, s.opts.borrow().clone());
    s.latest.store(generation, Ordering::SeqCst);
    s.t_key.set(Some((generation, qpc())));
}

/// Re-reads the model into the list (row count, selection, repaint).
pub(crate) fn refresh_list(s: &State) {
    set_count(s);
    unsafe { InvalidateRect(s.hwnd, null(), 0) };
}

/// Replaces the search text (re-runs the query through EN_CHANGE).
pub(crate) fn set_query(s: &State, text: &str) {
    let w = wide(text);
    unsafe {
        SetWindowTextW(s.edit, w.as_ptr());
        SetFocus(s.edit);
        SendMessageW(s.edit, EM_SETSEL, w.len() - 1, (w.len() - 1) as LPARAM);
    }
}

fn set_count(s: &State) {
    let (len, selected) = {
        let m = s.model.borrow();
        (m.len(), m.selected)
    };
    unsafe {
        SendMessageW(s.list, LVM_SETITEMCOUNT, len, (LVSICF_NOINVALIDATEALL | LVSICF_NOSCROLL) as LPARAM);
        InvalidateRect(s.list, null(), 0);
        if let Some(i) = selected {
            s.anchor.set(Some(i));
            select_rows(s, i, i);
        }
    }
}

/// Selects rows `a..=b` (either order) only, focusing `focus`.
fn select_rows(s: &State, a: usize, focus: usize) {
    let (lo, hi) = (a.min(focus), a.max(focus));
    unsafe {
        let mut item: LVITEMW = std::mem::zeroed();
        item.stateMask = LVIS_SELECTED | LVIS_FOCUSED;
        // Clear only rows that are selected now (cheap for long lists).
        let mut i: isize = -1;
        loop {
            i = SendMessageW(s.list, LVM_GETNEXTITEM, i as WPARAM, LVNI_SELECTED as LPARAM);
            if i < 0 {
                break;
            }
            if !(lo..=hi).contains(&(i as usize)) {
                SendMessageW(s.list, LVM_SETITEMSTATE, i as usize, &item as *const _ as LPARAM);
            }
        }
        for i in lo..=hi {
            item.state = LVIS_SELECTED | if i == focus { LVIS_FOCUSED } else { 0 };
            SendMessageW(s.list, LVM_SETITEMSTATE, i, &item as *const _ as LPARAM);
        }
        SendMessageW(s.list, LVM_ENSUREVISIBLE, focus, 0);
    }
    crate::pane::selection_changed(s);
}

/// Text of one result cell (also what screen readers get).
fn cell_text(s: &State, t: &NameTable, hit: &flint_fuzzy::engine::Hit, col: i32) -> String {
    let m = t.meta(hit.id);
    let is_dir = m.flags & flags::DIR != 0;
    match col {
        0 => t.name(hit.id).to_owned(),
        1 => t.parent_path(hit.id),
        2 => format_size(m.size, is_dir, m.flags & flags::META_STALE != 0),
        3 => format_time(m.mtime, &s.clock()),
        _ if m.volume == flint_index::deep::DEEP_VOLUME => "deep (unindexed)".to_owned(),
        // Filter-only results (one filter pass, no text): every row is
        // simply a filter match, so no tier label.
        _ if s.model.borrow().status.as_ref().is_some_and(|st| st.stages.len() == 1 && st.stages[0].stage == flint_fuzzy::engine::Stage::Exhaustive) => String::new(),
        _ => match_label(hit.tier).to_owned(),
    }
}

fn on_event(s: &State, ev: Event) {
    if s.claimed.get() {
        return; // the (empty) search that canceled the previous one
    }
    let latest = s.latest.load(Ordering::SeqCst);
    let (generation, result, is_final) = match ev {
        Event::Partial { generation, result } => (generation, result, false),
        Event::Final { generation, result, .. } => (generation, result, true),
    };
    if !s.model.borrow_mut().apply(generation, latest, &result) {
        return;
    }
    set_count(s);
    unsafe {
        InvalidateRect(s.hwnd, null(), 0);
        UpdateWindow(s.list);
        UpdateWindow(s.hwnd);
    }
    if is_final {
        if let Some((g, t0)) = s.t_key.get().filter(|(g, _)| *g == generation) {
            s.t_key.set(None);
            let ms = (qpc() - t0) as f64 * 1000.0 / s.freq;
            s.key_stats.borrow_mut().record(ms);
            println!("gen {g}: keystroke -> final results painted {ms:.2} ms (engine {:.2} ms, {} hits)", result.status.elapsed_micros as f64 / 1000.0, result.hits.len());
        }
    }
}

fn activate_selected(s: &State) {
    let row = s.model.borrow().selected_row().cloned();
    if let Some(row) = row {
        return crate::plugins::activate(s, &row);
    }
    let text = s.query_text.borrow().clone();
    let cmds = s.commands.evaluate(&text);
    if let Some(c) = cmds.first().filter(|c| c.exclusive) {
        match c.action {
            Action::App(AppAction::Diagnostics) => return crate::diagview::open(s),
            Action::App(AppAction::CheckUpdates) => return crate::updates::check(s, true),
            Action::App(AppAction::Plugins) => return crate::plugins::list(s),
            _ => {}
        }
        if let Action::OpenUrl(url) = &c.action {
            // Re-validate immediately before handing anything to the shell.
            if websearch::is_safe_url(url) {
                let (op, u) = (wide("open"), wide(url));
                unsafe { ShellExecuteW(null_mut(), op.as_ptr(), u.as_ptr(), null(), null(), SW_SHOWNORMAL) };
                hide(s);
            }
        }
        return;
    }
    // Opening runs only on this explicit request, after revalidation
    // against the index identity (see actions::run).
    crate::actions::run(s, Act::Open);
}

/// Draws `text` with the chars at `hl` (char indices) in bold accent color.
unsafe fn draw_highlighted(hdc: HDC, s: &State, rc: &RECT, text: &str, hl: &[usize], selected: bool) {
    let (font, bold) = s.fonts.get();
    let p = s.palette.get();
    let normal = if selected { p.sel_text } else { p.text };
    let accent = if selected { normal } else { p.accent };
    SetBkMode(hdc, TRANSPARENT as i32);
    let mut x = rc.left + scale(4, s.dpi.get());
    let chars: Vec<char> = text.chars().collect();
    let mut i = 0;
    while i < chars.len() && x < rc.right {
        let on = hl.binary_search(&i).is_ok();
        let mut j = i + 1;
        while j < chars.len() && hl.binary_search(&j).is_ok() == on {
            j += 1;
        }
        let run: Vec<u16> = chars[i..j].iter().collect::<String>().encode_utf16().collect();
        SelectObject(hdc, if on { bold } else { font });
        SetTextColor(hdc, if on { accent } else { normal });
        let mut sz = SIZE { cx: 0, cy: 0 };
        GetTextExtentPoint32W(hdc, run.as_ptr(), run.len() as i32, &mut sz);
        let mut r = RECT { left: x, top: rc.top, right: rc.right, bottom: rc.bottom };
        DrawTextW(hdc, run.as_ptr(), run.len() as i32, &mut r, DT_SINGLELINE | DT_VCENTER | DT_NOPREFIX);
        x += sz.cx;
        i = j;
    }
}

unsafe fn on_custom_draw(s: &State, cd: &mut NMLVCUSTOMDRAW) -> LRESULT {
    match cd.nmcd.dwDrawStage {
        CDDS_PREPAINT => CDRF_NOTIFYITEMDRAW as LRESULT,
        CDDS_ITEMPREPAINT => CDRF_NOTIFYSUBITEMDRAW as LRESULT,
        st if st == (CDDS_ITEMPREPAINT | CDDS_SUBITEM) => {
            let row = cd.nmcd.dwItemSpec;
            if !s.model.borrow().rows.is_empty() {
                return draw_row(s, cd, row);
            }
            // Gather everything (and release borrows) before drawing.
            let (hit, selected) = {
                let m = s.model.borrow();
                match m.hits.get(row) {
                    Some(h) => (*h, m.selected == Some(row)),
                    None => return CDRF_DODEFAULT as LRESULT,
                }
            };
            let mut rc: RECT = std::mem::zeroed();
            rc.top = cd.iSubItem;
            rc.left = LVIR_BOUNDS as i32;
            SendMessageW(s.list, LVM_GETSUBITEMRECT, row, &mut rc as *mut _ as LPARAM);
            if cd.iSubItem == 0 {
                rc.right = rc.left + SendMessageW(s.list, LVM_GETCOLUMNWIDTH, 0, 0) as i32;
            }
            let p = s.palette.get();
            let bg = CreateSolidBrush(if selected { p.sel_bg } else { p.bg });
            FillRect(cd.nmcd.hdc, &rc, bg);
            DeleteObject(bg);
            if cd.iSubItem >= 2 {
                let text = {
                    let t = s.table.read().unwrap();
                    if !t.is_live(hit.id) {
                        return CDRF_SKIPDEFAULT as LRESULT;
                    }
                    cell_text(s, &t, &hit, cd.iSubItem)
                };
                let w: Vec<u16> = text.encode_utf16().collect();
                if w.is_empty() {
                    // Never hand DrawTextW an empty buffer's dangling pointer.
                    return CDRF_SKIPDEFAULT as LRESULT;
                }
                SelectObject(cd.nmcd.hdc, s.fonts.get().0);
                SetBkMode(cd.nmcd.hdc, TRANSPARENT as i32);
                // Match labels are dim: they describe, they do not rank.
                SetTextColor(cd.nmcd.hdc, if selected { p.sel_text } else if cd.iSubItem == 4 { p.dim } else { p.text });
                rc.left += scale(4, s.dpi.get());
                rc.right -= scale(6, s.dpi.get());
                let align = if cd.iSubItem == 2 { DT_RIGHT } else { DT_LEFT };
                DrawTextW(cd.nmcd.hdc, w.as_ptr(), w.len() as i32, &mut rc, align | DT_SINGLELINE | DT_VCENTER | DT_NOPREFIX | DT_END_ELLIPSIS);
                return CDRF_SKIPDEFAULT as LRESULT;
            }
            let (text, hl, is_dir) = {
                let t = s.table.read().unwrap();
                if !t.is_live(hit.id) {
                    return CDRF_SKIPDEFAULT as LRESULT;
                }
                let q = parse_with(&s.query_text.borrow(), &s.clock());
                let h = highlight(&t, &q, s.opts.borrow().intensity, &hit);
                let is_dir = t.flags(hit.id) & flags::DIR != 0;
                if cd.iSubItem == 0 { (t.name(hit.id).to_owned(), h.name, is_dir) } else { (t.parent_path(hit.id), h.dir, is_dir) }
            };
            if cd.iSubItem == 0 {
                let icon = s.icons.borrow_mut().index(&text, is_dir);
                let size = GetSystemMetrics(SM_CXSMICON);
                if let Some(i) = icon {
                    let list = s.icons.borrow().list;
                    ImageList_Draw(list, i, cd.nmcd.hdc, rc.left + scale(4, s.dpi.get()), rc.top + (rc.bottom - rc.top - size) / 2, ILD_TRANSPARENT);
                }
                rc.left += size + scale(6, s.dpi.get());
            }
            draw_highlighted(cd.nmcd.hdc, s, &rc, &text, &hl, selected);
            CDRF_SKIPDEFAULT as LRESULT
        }
        _ => CDRF_DODEFAULT as LRESULT,
    }
}

/// Text of a non-file row's cell.
fn row_cell(r: &crate::model::Row, col: i32) -> &str {
    match col {
        0 => &r.title,
        1 => &r.subtitle,
        4 => &r.source,
        _ => "",
    }
}

/// Plugin and management rows: title, subtitle, source.
unsafe fn draw_row(s: &State, cd: &mut NMLVCUSTOMDRAW, row: usize) -> LRESULT {
    let (text, selected) = {
        let m = s.model.borrow();
        match m.rows.get(row) {
            Some(r) => (row_cell(r, cd.iSubItem).to_owned(), m.selected == Some(row)),
            None => return CDRF_DODEFAULT as LRESULT,
        }
    };
    let mut rc: RECT = std::mem::zeroed();
    rc.top = cd.iSubItem;
    rc.left = LVIR_BOUNDS as i32;
    SendMessageW(s.list, LVM_GETSUBITEMRECT, row, &mut rc as *mut _ as LPARAM);
    if cd.iSubItem == 0 {
        rc.right = rc.left + SendMessageW(s.list, LVM_GETCOLUMNWIDTH, 0, 0) as i32;
    }
    let p = s.palette.get();
    let bg = CreateSolidBrush(if selected { p.sel_bg } else { p.bg });
    FillRect(cd.nmcd.hdc, &rc, bg);
    DeleteObject(bg);
    let w: Vec<u16> = text.encode_utf16().collect();
    if !w.is_empty() {
        SelectObject(cd.nmcd.hdc, if cd.iSubItem == 0 { s.fonts.get().1 } else { s.fonts.get().0 });
        SetBkMode(cd.nmcd.hdc, TRANSPARENT as i32);
        SetTextColor(cd.nmcd.hdc, if selected { p.sel_text } else if cd.iSubItem == 0 { p.text } else { p.dim });
        rc.left += scale(6, s.dpi.get());
        rc.right -= scale(6, s.dpi.get());
        DrawTextW(cd.nmcd.hdc, w.as_ptr(), w.len() as i32, &mut rc, DT_LEFT | DT_SINGLELINE | DT_VCENTER | DT_NOPREFIX | DT_END_ELLIPSIS);
    }
    CDRF_SKIPDEFAULT as LRESULT
}

fn key_down(vk: u16) -> bool {
    unsafe { GetKeyState(vk as i32) < 0 }
}

/// Keyboard shortcuts in the search box (focus normally stays there).
/// Returns true if the key was handled.
fn on_key(s: &State, vk: u16, alt: bool) -> bool {
    let (ctrl, shift) = (key_down(VK_CONTROL), key_down(VK_SHIFT));
    if alt {
        let act = match vk {
            VK_RETURN => Act::Properties,
            0x43 => Act::CopyFiles, // C
            0x58 => Act::CutFiles,  // X
            0x56 => Act::Paste,     // V
            _ => return false,
        };
        crate::actions::run(s, act);
        return true;
    }
    let delta = match vk {
        VK_DOWN => Some(1),
        VK_UP => Some(-1),
        VK_NEXT => Some(10),
        VK_PRIOR => Some(-10),
        _ => None,
    };
    if let Some(d) = delta {
        s.model.borrow_mut().move_selection(d);
        let focus = s.model.borrow().selected;
        if let Some(f) = focus {
            let anchor = if shift { s.anchor.get().unwrap_or(f) } else { f };
            s.anchor.set(Some(anchor));
            select_rows(s, anchor, f);
        }
        return true;
    }
    match vk {
        VK_RETURN if ctrl => crate::actions::run(s, Act::Reveal),
        VK_RETURN if shift => crate::actions::run(s, Act::OpenWith),
        VK_RETURN => activate_selected(s),
        VK_ESCAPE => hide(s),
        VK_F3 => search_deeper(s),
        VK_F4 => deep_search(s),
        VK_F9 => toggle_viewer(s),
        VK_F2 => crate::actions::run(s, Act::Rename),
        VK_DELETE if ctrl && shift => crate::actions::run(s, Act::DeletePermanently),
        VK_DELETE if ctrl => crate::actions::run(s, Act::Recycle),
        0x43 if ctrl && shift => crate::actions::run(s, Act::CopyPath), // C
        0x4E if ctrl && shift => crate::actions::run(s, Act::NewFolder), // N
        VK_APPS => crate::actions::context_menu(s, crate::actions::menu_point_for_keyboard(s), shift),
        VK_F10 if shift => crate::actions::context_menu(s, crate::actions::menu_point_for_keyboard(s), false),
        VK_TAB => unsafe {
            // Tab moves to the filter bar (Shift+Tab to its last dropdown).
            SetFocus(if shift { *s.combos.last().unwrap() } else { s.combos[0] });
        },
        _ => return false,
    }
    true
}

unsafe extern "system" fn edit_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM, _id: usize, _data: usize) -> LRESULT {
    if msg == WM_KEYDOWN || msg == WM_SYSKEYDOWN {
        if let Some(s) = state() {
            if on_key(&s, wp as u16, msg == WM_SYSKEYDOWN) {
                return 0;
            }
        }
    }
    // Swallow the beep for handled Enter/Esc/Tab and Alt shortcuts.
    if msg == WM_CHAR && (wp == 13 || wp == 27 || wp == 9) {
        return 0;
    }
    if msg == WM_SYSCHAR && matches!(wp as u8, b'c' | b'x' | b'v' | b'C' | b'X' | b'V' | 13) {
        return 0;
    }
    DefSubclassProc(hwnd, msg, wp, lp)
}

/// Filter dropdowns: Tab/Shift+Tab move along the bar and back to the
/// search box; Esc (when closed) returns to the search box.
unsafe extern "system" fn combo_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM, id: usize, _data: usize) -> LRESULT {
    if msg == WM_KEYDOWN {
        if let Some(s) = state() {
            let i = id - ID_FILTER as usize;
            let vk = wp as u16;
            let dropped = SendMessageW(hwnd, CB_GETDROPPEDSTATE, 0, 0) != 0;
            if vk == VK_TAB {
                let next = if key_down(VK_SHIFT) { i.checked_sub(1) } else { Some(i + 1).filter(|&n| n < s.combos.len()) };
                SetFocus(next.map_or(s.edit, |n| s.combos[n]));
                return 0;
            }
            if vk == VK_ESCAPE && !dropped {
                SetFocus(s.edit);
                return 0;
            }
        }
    }
    if msg == WM_GETDLGCODE {
        return DLGC_WANTALLKEYS as LRESULT;
    }
    if msg == WM_CHAR && (wp == 9 || wp == 27) {
        return 0;
    }
    DefSubclassProc(hwnd, msg, wp, lp)
}

unsafe extern "system" fn wnd_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    // A panic must not unwind into user32 (that aborts the process); log it
    // and fall back to default handling.
    match std::panic::catch_unwind(|| wnd_proc_inner(hwnd, msg, wp, lp)) {
        Ok(r) => r,
        Err(_) => {
            eprintln!("internal error handling message {msg:#x}; continuing");
            DefWindowProcW(hwnd, msg, wp, lp)
        }
    }
}

unsafe fn wnd_proc_inner(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    let Some(st) = state() else { return DefWindowProcW(hwnd, msg, wp, lp) };
    let s = &*st;
    match msg {
        WM_HOTKEY => {
            if IsWindowVisible(s.hwnd) != 0 && GetForegroundWindow() == s.hwnd {
                hide(s);
            } else {
                s.t_hotkey.set(Some(qpc()));
                show(s);
            }
            0
        }
        WM_ACTIVATE => {
            // Dialogs and menus we opened take activation; that is not the
            // user leaving.
            if (wp & 0xFFFF) as u32 == WA_INACTIVE && s.selftest.is_empty() && s.modal.get() == 0 {
                hide(s);
            }
            0
        }
        WM_INITMENUPOPUP | WM_DRAWITEM | WM_MEASUREITEM | WM_MENUCHAR => {
            // Owner-drawn and lazily filled items of shell extensions.
            match flint_ops::win::forward_menu_message(msg, wp, lp) {
                Some(r) => r,
                None => DefWindowProcW(hwnd, msg, wp, lp),
            }
        }
        WM_COMMAND => {
            let (id, code) = ((wp & 0xFFFF) as isize, ((wp >> 16) & 0xFFFF) as u32);
            if id == ID_EDIT && code == EN_CHANGE {
                submit(s);
            }
            if (ID_FILTER..ID_FILTER + s.combos.len() as isize).contains(&id) && code == CBN_SELCHANGE {
                on_filter_picked(s, (id - ID_FILTER) as usize);
            }
            0
        }
        WM_NOTIFY => {
            let hdr = &*(lp as *const NMHDR);
            if hdr.idFrom as isize != ID_LIST {
                return 0;
            }
            match hdr.code {
                LVN_GETDISPINFOW => {
                    let di = &mut *(lp as *mut NMLVDISPINFOW);
                    if di.item.mask & LVIF_TEXT != 0 && !di.item.pszText.is_null() {
                        let row = di.item.iItem as usize;
                        let hit = s.model.borrow().hits.get(row).copied();
                        let text = match s.model.borrow().rows.get(row) {
                            Some(r) => Some(row_cell(r, di.item.iSubItem).to_owned()),
                            None => hit.map(|h| {
                                let t = s.table.read().unwrap();
                                if t.is_live(h.id) { cell_text(s, &t, &h, di.item.iSubItem) } else { String::new() }
                            }),
                        };
                        let mut buf = s.disp_buf.borrow_mut();
                        *buf = wide(&text.unwrap_or_default());
                        let max = (di.item.cchTextMax as usize).min(buf.len());
                        if max > 0 {
                            std::ptr::copy_nonoverlapping(buf.as_ptr(), di.item.pszText, max);
                            *di.item.pszText.add(max - 1) = 0;
                        }
                    }
                    0
                }
                NM_CUSTOMDRAW => on_custom_draw(s, &mut *(lp as *mut NMLVCUSTOMDRAW)),
                NM_DBLCLK => {
                    activate_selected(s);
                    0
                }
                NM_RCLICK => {
                    let mut pt = POINT { x: 0, y: 0 };
                    GetCursorPos(&mut pt);
                    crate::actions::context_menu(s, pt, key_down(VK_SHIFT));
                    0
                }
                LVN_ITEMCHANGED => {
                    let nm = &*(lp as *const NMLISTVIEW);
                    // Our own programmatic selection arrives while the model
                    // may be borrowed; that change is already reflected.
                    if nm.uNewState & LVIS_FOCUSED != 0 && nm.iItem >= 0 {
                        let moved = match s.model.try_borrow_mut() {
                            Ok(mut m) => {
                                m.selected = Some(nm.iItem as usize);
                                true
                            }
                            Err(_) => false,
                        };
                        if moved {
                            crate::pane::selection_changed(s);
                        }
                    }
                    0
                }
                _ => 0,
            }
        }
        WM_PAINT => {
            let mut ps: PAINTSTRUCT = std::mem::zeroed();
            let hdc = BeginPaint(hwnd, &mut ps);
            let mut rc: RECT = std::mem::zeroed();
            GetClientRect(hwnd, &mut rc);
            rc.top = rc.bottom - scale(26, s.dpi.get());
            rc.left += scale(10, s.dpi.get());
            let cov = coverage(s);
            let mut line = {
                let m = s.model.borrow();
                m.status.as_ref().map(|st| status_line(st, m.hits.len(), &cov, deeper_action(st, &s.opts.borrow()))).unwrap_or_else(|| status_line_idle(&cov))
            };
            let cmd = s.command_line.borrow().clone();
            if !cmd.is_empty() {
                line = format!("{cmd}   |   {line}");
            }
            let notice = s.notice.borrow().clone();
            if !notice.is_empty() {
                line = format!("{notice}   |   {line}");
            }
            let unindexed = !s.deep_scopes.lock().unwrap().is_empty() || s.catalog.as_ref().is_some_and(|c| c.statuses().iter().any(|v| !v.unreadable.is_empty()));
            if unindexed && s.model.borrow().status.is_some() && !s.deep_active.get() {
                line.push_str(" · F4: search unindexed locations");
            }
            if s.viewer_on.get() {
                let (r, _) = pane_rect(s);
                crate::pane::paint(s, hdc, r);
            }
            let w = wide(&line);
            // Erase first: text is drawn transparently and repaints are not
            // always preceded by WM_ERASEBKGND.
            let mut bg_rc = rc;
            bg_rc.left = 0;
            FillRect(hdc, &bg_rc, s.brushes.get().0);
            SelectObject(hdc, s.fonts.get().0);
            SetBkMode(hdc, TRANSPARENT as i32);
            SetTextColor(hdc, s.palette.get().dim);
            DrawTextW(hdc, w.as_ptr(), w.len() as i32 - 1, &mut rc, DT_SINGLELINE | DT_VCENTER | DT_NOPREFIX | DT_END_ELLIPSIS);
            EndPaint(hwnd, &ps);
            0
        }
        WM_APP_EVENT => {
            let ev = *Box::from_raw(lp as *mut Event);
            on_event(s, ev);
            0
        }
        WM_TIMER if wp == SELFTEST_TIMER && s.modal.get() > 0 => {
            EndMenu();
            let cls = wide("FlintPrompt");
            let prompt = FindWindowW(cls.as_ptr(), null());
            if !prompt.is_null() {
                PostMessageW(prompt, WM_CLOSE, 0, 0);
            }
            0
        }
        WM_TIMER if wp == SELFTEST_TIMER => {
            // Wait for indexing before typing, then one query per tick.
            let indexing = {
                let c = s.coverage.lock().unwrap();
                !c.complete && c.problems.iter().any(|p| p.ends_with("indexing"))
            };
            if indexing {
                return 0;
            }
            let pos = s.selftest_pos.get();
            if pos < s.selftest.len() {
                s.selftest_pos.set(pos + 1);
                // Type it character by character, as a user would; each
                // SetWindowText synchronously re-enters via EN_CHANGE.
                // {KEY} steps stand for shortcuts; a menu or dialog they open
                // is dismissed on the next tick (timers still fire in its
                // modal loop).
                match s.selftest[pos].as_str() {
                    "{F3}" => return { search_deeper(s); 0 },
                    "{MENU}" => return { crate::actions::context_menu(s, crate::actions::menu_point_for_keyboard(s), false); 0 },
                    "{RENAME}" => return { crate::actions::run(s, Act::Rename); 0 },
                    "{COPYPATH}" => return { crate::actions::run(s, Act::CopyPath); 0 },
                    "{ENTER}" => return { activate_selected(s); 0 },
                    "{DOWN}" => return { s.model.borrow_mut().move_selection(1); set_count(s); 0 },
                    _ => {}
                }
                let mut acc = String::new();
                for c in s.selftest[pos].chars() {
                    acc.push(c);
                    let w = wide(&acc);
                    SetWindowTextW(s.edit, w.as_ptr());
                }
                if pos + 1 == s.selftest.len() {
                    s.model.borrow_mut().move_selection(1);
                    set_count(s);
                }
            } else if pos == s.selftest.len() {
                s.selftest_pos.set(pos + 1); // one extra tick so the last results paint
            } else {
                KillTimer(hwnd, SELFTEST_TIMER);
                PostMessageW(hwnd, WM_CLOSE, 0, 0);
            }
            0
        }
        WM_APP_PLUGIN => {
            let d = *Box::from_raw(lp as *mut flint_plugin::service::Delivery);
            crate::plugins::on_delivery(s, d);
            0
        }
        WM_APP_UPDATE => {
            let ev = *Box::from_raw(lp as *mut crate::updates::UpdateEvent);
            crate::updates::on_event(s, ev);
            0
        }
        WM_TIMER if wp == UPDATE_TIMER => {
            crate::updates::tick(s);
            0
        }
        WM_APP_PREVIEW => {
            let (g, r) = *Box::from_raw(lp as *mut (u64, flint_preview::scheduler::Delivered));
            crate::pane::on_result(s, g, r);
            0
        }
        WM_APP_DEEP => {
            let done = *Box::from_raw(lp as *mut DeepDone);
            if done.generation != s.latest.load(Ordering::SeqCst) {
                return 0; // the query changed meanwhile
            }
            let n = done.hits.len();
            flint_index::deep::insert_hits(&mut s.table.write().unwrap(), &done.hits);
            s.deep_active.set(n > 0);
            let msg = format!("{} · {n} match(es) outside the index", done.report.summary());
            let text = s.query_text.borrow().clone();
            run_query(s, text);
            *s.command_line.borrow_mut() = msg;
            InvalidateRect(s.hwnd, null(), 0);
            0
        }
        WM_APP_NOTICE => {
            let text = *Box::from_raw(lp as *mut String);
            s.say(text.lines().next().unwrap_or(""));
            if text.contains('\n') && IsWindowVisible(s.hwnd) != 0 {
                let _m = Modal::new(s);
                let (t, c) = (wide(&text), wide("Flint"));
                MessageBoxW(s.hwnd, t.as_ptr(), c.as_ptr(), MB_OK | MB_ICONINFORMATION | MB_TOPMOST);
            }
            0
        }
        WM_APP_INDEX => {
            INDEX_REPAINT_PENDING.store(false, Ordering::SeqCst);
            // The filename index just finished: content indexing can start.
            let complete = coverage(s).complete;
            if complete && !s.index_was_complete.replace(true) {
                if let Some(c) = s.content_service.borrow().as_ref() {
                    c.wake();
                }
            }
            if s.refresh_pending.replace(false) {
                // Show the effect of a file action; the status keeps its message.
                let (text, msg) = (s.query_text.borrow().clone(), s.command_line.borrow().clone());
                run_query(s, text);
                *s.command_line.borrow_mut() = msg;
            }
            // Repaint only the status strip; results refresh on next query.
            let mut rc: RECT = std::mem::zeroed();
            GetClientRect(hwnd, &mut rc);
            rc.top = rc.bottom - scale(26, s.dpi.get());
            InvalidateRect(hwnd, &rc, 0);
            0
        }
        WM_ERASEBKGND => {
            let mut rc: RECT = std::mem::zeroed();
            GetClientRect(hwnd, &mut rc);
            FillRect(wp as HDC, &rc, s.brushes.get().0);
            1
        }
        WM_CTLCOLOREDIT | WM_CTLCOLORLISTBOX | WM_CTLCOLORSTATIC => {
            let p = s.palette.get();
            SetTextColor(wp as HDC, p.text);
            SetBkColor(wp as HDC, p.edit_bg);
            s.brushes.get().1 as LRESULT
        }
        WM_SETTINGCHANGE => {
            // Theme switches arrive as "ImmersiveColorSet"; high contrast via
            // SPI_SETHIGHCONTRAST. Re-evaluating is cheap, so do it on any.
            apply_palette(s);
            0
        }
        WM_DEVICECHANGE => {
            if matches!(wp as u32, DBT_DEVICEARRIVAL | DBT_DEVICEREMOVECOMPLETE) {
                resync_volumes(s);
            }
            1
        }
        WM_DPICHANGED => {
            s.dpi.set((wp & 0xFFFF) as u32);
            let (old_f, old_b) = s.fonts.get();
            s.fonts.set(make_fonts(s.dpi.get()));
            DeleteObject(old_f);
            DeleteObject(old_b);
            let r = &*(lp as *const RECT);
            SetWindowPos(hwnd, null_mut(), r.left, r.top, r.right - r.left, r.bottom - r.top, SWP_NOZORDER);
            layout(s);
            0
        }
        WM_CLOSE => {
            DestroyWindow(hwnd);
            0
        }
        WM_DESTROY => {
            if let Some(c) = &s.catalog {
                for v in c.statuses() {
                    println!("volume {}: {:?}, loaded via {:?}, stale meta {}, rescans {}", v.root_name, v.phase, v.load_source, v.stale_meta, v.stats.rescans);
                }
            }
            if let Some(c) = &s.catalog {
                let g = c.governor().stats();
                println!("indexing paused for typing: {:.1} s total", g.paused.as_secs_f64());
            }
            if let Some((private, ws)) = crate::resources::memory_usage() {
                println!("private bytes {:.1} MB, working set {:.1} MB, index entries {}", private as f64 / 1e6, ws as f64 / 1e6, s.table.read().unwrap().live_count());
            }
            for (label, st) in [("hotkey -> painted", &s.hotkey_stats), ("keystroke -> results painted", &s.key_stats)] {
                if let Some((n, p50, p95, max)) = st.borrow().summary() {
                    println!("{label}: n={n} p50={p50:.2} ms p95={p95:.2} ms max={max:.2} ms");
                }
            }
            PostQuitMessage(0);
            0
        }
        _ => DefWindowProcW(hwnd, msg, wp, lp),
    }
}

/// Everything `/diag` shows (no search text or result names).
pub(crate) fn diag_snapshot(s: &State) -> crate::diag::Snapshot {
    let (bytes, slack, entries) = {
        let t = s.table.read().unwrap();
        (t.heap_bytes(), t.slack_bytes(), t.live_count())
    };
    let mem = crate::resources::memory_usage();
    let plugin_health = s.plugins.service.borrow().as_ref().map(|svc| svc.health()).unwrap_or_default();
    let plugins = s
        .plugins
        .found
        .borrow()
        .iter()
        .map(|(d, st)| {
            let id = d.manifest.as_ref().map(|m| m.id.clone()).unwrap_or_default();
            let h = plugin_health.iter().find(|(m, _)| m.id == id).map(|(_, h)| h.clone());
            crate::diag::PluginLine {
                label: d.label(),
                folder: d.dir.display().to_string(),
                standing: match st {
                    flint_plugin::registry::Standing::Approved { .. } => "approved".into(),
                    flint_plugin::registry::Standing::NeedsApproval { reason, .. } => format!("needs approval ({reason})"),
                    flint_plugin::registry::Standing::Invalid(e) => format!("not loaded: {e}"),
                },
                isolation: h.as_ref().map_or("not started".into(), flint_plugin::service::isolation_of),
                disabled: h.as_ref().and_then(|h| h.disabled.clone()),
                stats: h.map(|h| h.stats),
            }
        })
        .collect();
    let viewer = Some(if s.viewer_on.get() { "on (the preview helper runs only while the launcher is shown)".to_owned() } else { "off: no file is read for previews".to_owned() });
    crate::diag::Snapshot {
        version: format!("{} ({})", env!("CARGO_PKG_VERSION"), option_env!("FLINT_BUILD_ID").unwrap_or("local build")),
        os: crate::resources::os_version(),
        portable: s.portable,
        data_dir: s.data_dir.display().to_string(),
        uptime_secs: s.started.elapsed().as_secs(),
        private_bytes: mem.map(|m| m.0 as u64),
        working_set: mem.map(|m| m.1 as u64),
        entries,
        table_bytes: bytes,
        table_slack: slack,
        governor: s.catalog.as_ref().map(|c| c.governor().stats()),
        volumes: s.catalog.as_ref().map(|c| c.statuses()).unwrap_or_default(),
        coverage: Some(coverage(s)),
        latency: vec![("hotkey → window shown".into(), s.hotkey_stats.borrow().summary()), ("keystroke → results painted".into(), s.key_stats.borrow().summary())],
        last_search: s.model.borrow().status.clone(),
        content: s.content_index.as_ref().map(|c| c.stats()),
        viewer,
        plugins,
        plugins_enabled: s.plugins.enabled,
        update: format!("mode {:?}; {}", s.updates_cfg.mode, if s.update_status.borrow().is_empty() { "not checked this session".to_owned() } else { s.update_status.borrow().clone() }),
        notices: s.startup_notes.iter().cloned().chain(s.plugins.notes.borrow().iter().cloned()).collect(),
        errors: Vec::new(),
    }
}

pub(crate) fn redactor(s: &State, enabled: bool) -> crate::diag::Redactor {
    let env = |k: &str| std::env::var(k).ok();
    let salt = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_nanos() as u64) ^ (std::process::id() as u64) << 32 ^ s.hwnd as u64;
    crate::diag::Redactor::new(enabled, salt, env("USERPROFILE").as_deref(), env("USERNAME").as_deref(), env("COMPUTERNAME").as_deref())
}

fn status_line_idle(cov: &Coverage) -> String {
    if cov.complete { "type to search".into() } else { format!("type to search · indexing: {}", cov.problems.join(", ")) }
}

/// `--synthetic N`: an in-memory corpus for UI benchmarking (no catalog).
fn spawn_synthetic(table: Arc<RwLock<NameTable>>, coverage: Arc<Mutex<Coverage>>, hwnd_bits: usize, n: usize) {
    std::thread::spawn(move || {
        let mut t = table.write().unwrap();
        let r = t.push(NO_PARENT, "S:", Meta::dir(9));
        let words = ["report", "invoice", "notes", "project", "photo", "budget", "draft", "final", "main", "index"];
        let mut dirs = vec![r];
        let mut x = 0x9E37_79B9u64;
        for i in 0..n {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            let parent = dirs[(x as usize) % dirs.len()];
            if i % 10 == 0 {
                dirs.push(t.push(parent, &format!("{}_{}", words[(x >> 8) as usize % 10], i), Meta::dir(9)));
            } else {
                t.push(parent, &format!("{}-{}_{}.txt", words[(x >> 8) as usize % 10], words[(x >> 16) as usize % 10], i), Meta::file(9, i as u64, 1_700_000_000));
            }
        }
        *coverage.lock().unwrap() = Coverage { complete: true, loading: false, problems: vec![] };
        drop(t);
        unsafe { PostMessageW(hwnd_bits as HWND, WM_APP_INDEX, 0, 0) };
    });
}

/// Asks a running launcher (this user's) to exit and waits up to 5 s.
fn quit_running() -> bool {
    let class = wide("FlintLauncher");
    unsafe {
        let h = FindWindowW(class.as_ptr(), null());
        if h.is_null() {
            return false;
        }
        PostMessageW(h, WM_CLOSE, 0, 0);
        for _ in 0..50 {
            std::thread::sleep(Duration::from_millis(100));
            if FindWindowW(class.as_ptr(), null()).is_null() {
                return true;
            }
        }
    }
    false
}

pub fn run() {
    let t_start = qpc();
    let args: Vec<String> = std::env::args().collect();
    let synthetic = args.iter().position(|a| a == "--synthetic").and_then(|i| args.get(i + 1)).and_then(|v| v.parse().ok());
    let flag_value = |f: &str| args.iter().position(|a| a == f).and_then(|i| args.get(i + 1)).cloned();
    let selftest: Vec<String> = flag_value("--selftest").map(|v| v.split('|').map(str::to_owned).collect()).unwrap_or_default();
    let show_now = args.iter().any(|a| a == "--show") || !selftest.is_empty();
    // `--root PATH` (or a bare path) indexes only that folder.
    let root = flag_value("--root").or_else(|| args.iter().skip(1).filter(|a| !a.starts_with("--")).find(|a| Some(*a) != flag_value("--synthetic").as_ref() && Some(*a) != flag_value("--selftest").as_ref()).cloned());

    // Settings and data locations (installed: %LOCALAPPDATA%\Flint; portable:
    // <exe dir>\data when portable.marker exists).
    let exe_dir = std::env::current_exe().ok().and_then(|p| p.parent().map(|d| d.to_path_buf())).unwrap_or_else(|| ".".into());
    let lad = std::env::var_os("LOCALAPPDATA").map(std::path::PathBuf::from);
    let paths = match flint_config::resolve_paths(&exe_dir, lad.as_deref()) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("cannot determine data directory: {e}");
            return;
        }
    };
    // Installer hooks: stop a running launcher; remove data on uninstall.
    if args.iter().any(|a| a == "--quit" || a == "--cleanup-data") {
        let closed = quit_running();
        println!("running launcher: {}", if closed { "closed" } else { "none" });
        if args.iter().any(|a| a == "--cleanup-data") {
            match flint_config::cleanup_data(&paths, args.iter().any(|a| a == "--all")) {
                Ok(v) => println!("removed {} item(s) from {}", v.len(), paths.data_dir.display()),
                Err(e) => {
                    eprintln!("{e}");
                    std::process::exit(1);
                }
            }
        }
        return;
    }
    let _ = std::fs::create_dir_all(&paths.data_dir);
    let loaded = flint_config::load(&paths.config);
    let cfg = loaded.config;
    let mut notices: Vec<String> = loaded.notes.iter().filter(|n| !matches!(n, flint_config::Note::Created)).map(|n| format!("settings: {n:?}")).collect();
    println!("data dir: {} (portable: {}), config notes: {:?}", paths.data_dir.display(), paths.portable, loaded.notes);
    // The shell (file operations, context menus, clipboard) needs OLE.
    if let Err(e) = flint_ops::win::init_com() {
        notices.push(format!("shell integration unavailable: {e}"));
    }
    unsafe {
        SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
        let icc = INITCOMMONCONTROLSEX { dwSize: std::mem::size_of::<INITCOMMONCONTROLSEX>() as u32, dwICC: ICC_LISTVIEW_CLASSES };
        InitCommonControlsEx(&icc);
        let hinst = GetModuleHandleW(null());
        let class = wide("FlintLauncher");
        let wc = WNDCLASSEXW {
            cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
            style: CS_HREDRAW | CS_VREDRAW,
            lpfnWndProc: Some(wnd_proc),
            cbClsExtra: 0,
            cbWndExtra: 0,
            hInstance: hinst,
            hIcon: null_mut(),
            hCursor: LoadCursorW(null_mut(), IDC_ARROW),
            // Painted in WM_ERASEBKGND with the theme palette.
            hbrBackground: null_mut(),
            lpszMenuName: null(),
            lpszClassName: class.as_ptr(),
            hIconSm: null_mut(),
        };
        RegisterClassExW(&wc);
        let title = wide("Flint");
        let hwnd = CreateWindowExW(WS_EX_TOOLWINDOW | WS_EX_TOPMOST, class.as_ptr(), title.as_ptr(), WS_POPUP | WS_BORDER, 0, 0, WIDTH, 520, null_mut(), null_mut(), hinst, null());
        let dpi = GetDpiForWindow(hwnd).max(96);
        let edit_cls = wide("EDIT");
        let edit = CreateWindowExW(0, edit_cls.as_ptr(), null(), WS_CHILD | WS_VISIBLE | WS_BORDER | ES_AUTOHSCROLL as u32, 0, 0, 10, 10, hwnd, ID_EDIT as HMENU, hinst, null());
        let lv_cls = wide("SysListView32");
        let combo_cls = wide("COMBOBOX");
        let mut combos = Vec::new();
        for (i, label) in ["File type filter", "Size filter", "Date modified filter"].iter().enumerate() {
            let name = wide(label);
            let c = CreateWindowExW(0, combo_cls.as_ptr(), name.as_ptr(), WS_CHILD | WS_VISIBLE | WS_TABSTOP | WS_VSCROLL | CBS_DROPDOWNLIST as u32, 0, 0, 10, 10, hwnd, (ID_FILTER + i as isize) as HMENU, hinst, null());
            SetWindowSubclass(c, Some(combo_proc), ID_FILTER as usize + i, 0);
            combos.push(c);
        }
        // Multi-select: batch operations act on every selected row.
        let list = CreateWindowExW(0, lv_cls.as_ptr(), null(), WS_CHILD | WS_VISIBLE | (LVS_REPORT | LVS_OWNERDATA | LVS_SHOWSELALWAYS | LVS_NOCOLUMNHEADER), 0, 0, 10, 10, hwnd, ID_LIST as HMENU, hinst, null());
        SendMessageW(list, LVM_SETEXTENDEDLISTVIEWSTYLE, 0, (LVS_EX_FULLROWSELECT | LVS_EX_DOUBLEBUFFER) as LPARAM);
        for (i, (name, _)) in COLUMNS.iter().enumerate() {
            let mut text = wide(name);
            let mut col: LVCOLUMNW = std::mem::zeroed();
            col.mask = LVCF_TEXT | LVCF_WIDTH;
            col.cx = 200;
            col.pszText = text.as_mut_ptr();
            SendMessageW(list, LVM_INSERTCOLUMNW, i, &col as *const _ as LPARAM);
        }
        SetWindowSubclass(edit, Some(edit_proc), 1, 0);
        // Viewer body: read-only, never takes keyboard focus from the box.
        let pane_edit = CreateWindowExW(0, edit_cls.as_ptr(), null(), WS_CHILD | WS_VSCROLL | (ES_MULTILINE | ES_READONLY | ES_AUTOVSCROLL) as u32, 0, 0, 10, 10, hwnd, ID_PANE as HMENU, hinst, null());
        let mono_face = wide("Consolas");
        let mono_font = CreateFontW(-scale(13, dpi), 0, 0, 0, FW_NORMAL as i32, 0, 0, 0, DEFAULT_CHARSET as u32, OUT_DEFAULT_PRECIS as u32, CLIP_DEFAULT_PRECIS as u32, CLEARTYPE_QUALITY as u32, (FIXED_PITCH | FF_MODERN) as u32, mono_face.as_ptr());

        let table = Arc::new(RwLock::new(NameTable::new()));
        let latest = Arc::new(AtomicU64::new(0));
        let hwnd_bits = hwnd as usize;
        let tz: Arc<UtcOffset> = Arc::new(crate::resources::local_offset);
        // Content search: the real index when enabled, otherwise a provider
        // that says why `content:` is unavailable.
        let content_index = if cfg.privacy.content_indexing {
            match flint_index::content::ContentIndex::open(&paths.data_dir.join("content.db")) {
                Ok(c) => Some(c),
                Err(e) => {
                    notices.push(format!("content index unavailable: {e}"));
                    None
                }
            }
        } else {
            None
        };
        let provider: Arc<dyn flint_fuzzy::session::ContentProvider> = match &content_index {
            Some(c) => c.clone(),
            None => flint_index::content::ContentIndex::disabled("content search is off; enable privacy.content_indexing in settings"),
        };
        let session = Session::start(table.clone(), flint_fuzzy::session::SessionConfig { debounce: Duration::from_millis(cfg.search.debounce_ms), offset: tz.clone(), content: Some(provider) }, move |ev: Event| {
            let p = Box::into_raw(Box::new(ev));
            if PostMessageW(hwnd_bits as HWND, WM_APP_EVENT, 0, p as LPARAM) == 0 {
                drop(Box::from_raw(p));
            }
        });
        let coverage = Arc::new(Mutex::new(Coverage::default()));
        let catalog = match synthetic {
            Some(n) => {
                spawn_synthetic(table.clone(), coverage.clone(), hwnd_bits, n);
                None
            }
            None => {
                let on_change: flint_index::catalog::OnChange = Arc::new(move || {
                    if !INDEX_REPAINT_PENDING.swap(true, Ordering::SeqCst) {
                        PostMessageW(hwnd_bits as HWND, WM_APP_INDEX, 0, 0);
                    }
                });
                let meta = CatalogOptions { meta_batch: cfg.indexing.metadata_refresh_per_second as usize, ..CatalogOptions::default() };
                let setup: Option<flint_index::governor::ThreadSetup> = cfg.indexing.background_priority.then(|| Arc::new(crate::resources::enter_background_mode) as _);
                let gov = flint_index::governor::Governor::new(flint_index::governor::GovernorOptions { max_scans: cfg.indexing.concurrent_scans as usize, ..Default::default() }, setup);
                match Catalog::new(table.clone(), paths.index_db.clone(), Some(paths.snapshots.clone()), meta, on_change) {
                    Ok(c) => Some(Arc::new(c.with_governor(gov))),
                    Err(e) => {
                        notices.push(format!("index database unavailable: {e}"));
                        None
                    }
                }
            }
        };
        let host_exe = exe_dir.join("flint-preview-host.exe");
        let content_service = match (&content_index, &catalog) {
            (Some(ci), Some(cat)) => {
                let profile = std::env::var("USERPROFILE").unwrap_or_default();
                let roots = if cfg.content.roots.is_empty() { vec![format!("{profile}\\Documents"), format!("{profile}\\Desktop")] } else { cfg.content.roots.clone() };
                let ccfg = flint_index::content::ContentConfig {
                    roots,
                    extensions: if cfg.content.extensions.is_empty() { flint_index::content::ContentConfig::default_extensions() } else { cfg.content.extensions.iter().map(|e| e.trim_start_matches('.').to_ascii_lowercase()).collect() },
                    excluded: cfg.indexing.excluded_paths.iter().chain(&cfg.content.excluded_paths).cloned().collect(),
                    max_file_bytes: cfg.content.max_file_mb << 20,
                    limits: flint_preview::Limits { max_read: cfg.content.max_file_mb << 20, max_chars: cfg.content.max_chars, thumb: 0, ..Default::default() },
                };
                let mut hc = flint_preview::client::HostConfig::new(host_exe.clone());
                hc.timeout = Duration::from_secs(20);
                hc.memory_limit = cfg.viewer.helper_memory_mb << 20;
                let on_change: Arc<dyn Fn() + Send + Sync> = Arc::new(|| {});
                Some(flint_index::content::ContentService::start(flint_index::content::ContentJob {
                    index: ci.clone(),
                    db: paths.data_dir.join("content.db"),
                    table: table.clone(),
                    cfg: ccfg,
                    extractor: Box::new(flint_preview::client::HostClient::new(hc)),
                    governor: cat.governor().clone(),
                    every: Duration::from_secs(cfg.content.resync_minutes.max(1) * 60),
                    on_change,
                }))
            }
            _ => None,
        };
        let (font, bold) = make_fonts(dpi);
        let mut freq = 0i64;
        QueryPerformanceFrequency(&mut freq);
        let state = Rc::new(State {
            hwnd,
            edit,
            list,
            combos,
            filters: RefCell::new(Vec::new()),
            table,
            session,
            latest,
            coverage,
            commands: Registry::with_builtins(websearch::Provider { name: cfg.web_search.provider.clone(), template: cfg.web_search.template.clone() }),
            freq: freq as f64,
            model: RefCell::new(ResultsModel::default()),
            query_text: RefCell::new(String::new()),
            command_line: RefCell::new(String::new()),
            fonts: Cell::new((font, bold)),
            dpi: Cell::new(dpi),
            t_hotkey: Cell::new(None),
            t_key: Cell::new(None),
            hotkey_stats: RefCell::new(LatencyStats::default()),
            key_stats: RefCell::new(LatencyStats::default()),
            disp_buf: RefCell::new(Vec::new()),
            selftest: selftest.clone(),
            selftest_pos: Cell::new(0),
            catalog,
            indexing: cfg.indexing.clone(),
            only_root: root,
            base_opts: search_options(&cfg.search),
            opts: RefCell::new(search_options(&cfg.search)),
            tz,
            files: cfg.files.clone(),
            modal: Cell::new(0),
            refresh_pending: Cell::new(false),
            anchor: Cell::new(None),
            pane_edit,
            viewer: RefCell::new(None),
            viewer_on: Cell::new(cfg.viewer.enabled),
            viewer_cfg: cfg.viewer.clone(),
            preview_gen: Cell::new(0),
            last_preview: RefCell::new(None),
            pane_header: RefCell::new(String::new()),
            pane_bitmap: Cell::new((null_mut(), 0, 0)),
            host_exe,
            mono_font,
            content_service: RefCell::new(content_service),
            content_index: content_index.clone(),
            deep_scopes: Arc::new(Mutex::new(Vec::new())),
            deep_cancel: RefCell::new(Arc::new(std::sync::atomic::AtomicBool::new(false))),
            deep_cfg: cfg.deep_search.clone(),
            deep_active: Cell::new(false),
            index_was_complete: Cell::new(false),
            theme: cfg.general.theme,
            palette: Cell::new(crate::model::palette(false)),
            brushes: Cell::new((null_mut(), null_mut())),
            icons: RefCell::new(Icons::new()),
            notice: RefCell::new(String::new()),
            plugins: crate::plugins::Plugins::new(cfg.plugins.enabled, &paths.data_dir),
            updates_cfg: cfg.updates.clone(),
            update_status: RefCell::new(String::new()),
            data_dir: paths.data_dir.clone(),
            portable: paths.portable,
            redact_diagnostics: cfg.privacy.redact_diagnostics,
            started: std::time::Instant::now(),
            claimed: Cell::new(false),
            startup_notes: notices.clone(),
        });
        layout(&state);
        fill_filters(&state);
        apply_palette(&state);
        resync_volumes(&state);
        STATE.with(|s| *s.borrow_mut() = Some(state.clone()));
        // Plugins are discovered (and hashed) now; none starts until used.
        state.plugins.reload(hwnd);
        notices.extend(state.plugins.notes.borrow().iter().cloned());
        if cfg.updates.mode == flint_config::UpdateMode::Notify {
            // No network at startup: the first check is a quarter hour in.
            SetTimer(hwnd, UPDATE_TIMER, 15 * 60 * 1000, None);
        }

        // The configured hotkey can be taken by another application (Ctrl+Space
        // often is, by IMEs on CJK systems): say so instead of doing nothing.
        let hk = cfg.hotkey();
        let registered = RegisterHotKey(hwnd, HOTKEY_ID, hk.win32_modifiers() | MOD_NOREPEAT, hk.vk as u32) != 0;
        if !registered {
            notices.push(format!("hotkey {hk} is used by another application; set general.hotkey in {}", paths.config.display()));
        }
        *state.notice.borrow_mut() = notices.join(" · ");
        println!("ready in {:.1} ms; hotkey: {}; Esc hides, Alt+F4 quits", (qpc() - t_start) as f64 * 1000.0 / freq as f64, if registered { hk.to_string() } else { format!("{hk} (unavailable)") });
        if !registered || show_now {
            show(&state);
        }
        if !selftest.is_empty() {
            SetTimer(hwnd, SELFTEST_TIMER, 400, None);
        }
        let mut msg: MSG = std::mem::zeroed();
        while GetMessageW(&mut msg, null_mut(), 0, 0) > 0 {
            TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
        // Stops the viewer and content helpers, then the volume workers
        // (each writes its startup snapshot).
        crate::pane::stop(&state);
        state.content_service.borrow_mut().take();
        if let Some(c) = &state.catalog {
            c.shutdown();
        }
        drop(state);
        STATE.with(|s| s.borrow_mut().take());
    }
}
