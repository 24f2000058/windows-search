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
//! Known spike limitations (tracked in docs/requirements.md): no file icons,
//! no theming or settings UI, comctl32 v6 manifest not embedded, Enter only
//! reveals in Explorer.

use crate::model::{status_line, Coverage, LatencyStats, ResultsModel};
use flint_commands::{websearch, Action, Registry};
use flint_fuzzy::engine::{highlight, Intensity, Options};
use flint_fuzzy::query::parse;
use flint_fuzzy::session::{Event, Session};
use flint_fuzzy::table::{Meta, NameTable, NO_PARENT};
use flint_index::model::FileRecord;
use flint_index::traverse::{RealFs, TraversalSource};
use flint_index::volume::{Changes, VolumeIndex};
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
const ID_EDIT: isize = 100;
const ID_LIST: isize = 101;
const HOTKEY_ID: i32 = 1;
const SELFTEST_TIMER: usize = 7;

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
struct State {
    hwnd: HWND,
    edit: HWND,
    list: HWND,
    table: Arc<RwLock<NameTable>>,
    session: Session,
    latest: Arc<AtomicU64>,
    coverage: Arc<Mutex<Coverage>>,
    commands: Registry,
    freq: f64,
    model: RefCell<ResultsModel>,
    query_text: RefCell<String>,
    command_line: RefCell<String>,
    fonts: Cell<(HFONT, HFONT)>,
    dpi: Cell<u32>,
    t_hotkey: Cell<Option<i64>>,
    t_key: Cell<Option<(u64, i64)>>,
    hotkey_stats: RefCell<LatencyStats>,
    key_stats: RefCell<LatencyStats>,
    disp_buf: RefCell<Vec<u16>>,
    /// `--selftest`: queries typed programmatically, one per timer tick.
    selftest: Vec<String>,
    selftest_pos: Cell<usize>,
}

thread_local! {
    static STATE: RefCell<Option<Rc<State>>> = const { RefCell::new(None) };
}

fn state() -> Option<Rc<State>> {
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

fn layout(s: &State) {
    let d = s.dpi.get();
    let (w, pad, eh, lh) = (scale(720, d), scale(8, d), scale(32, d), scale(420, d));
    unsafe {
        SetWindowPos(s.edit, null_mut(), pad, pad, w - 2 * pad, eh, SWP_NOZORDER);
        SetWindowPos(s.list, null_mut(), pad, pad * 2 + eh, w - 2 * pad, lh, SWP_NOZORDER);
        SendMessageW(s.list, LVM_SETCOLUMNWIDTH, 0, scale(260, d) as LPARAM);
        SendMessageW(s.list, LVM_SETCOLUMNWIDTH, 1, (w - 2 * pad - scale(280, d)) as LPARAM);
        for h in [s.edit, s.list] {
            SendMessageW(h, WM_SETFONT, s.fonts.get().0 as WPARAM, 1);
        }
    }
}

fn window_size(dpi: u32) -> (i32, i32) {
    (scale(720, dpi), scale(8 * 3 + 32 + 420 + 26, dpi))
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
        let (w, h) = window_size(s.dpi.get());
        let wa = mi.rcWork;
        let x = wa.left + ((wa.right - wa.left) - w) / 2;
        let y = wa.top + ((wa.bottom - wa.top) - h) / 4;
        SetWindowPos(s.hwnd, HWND_TOPMOST, x, y, w, h, SWP_SHOWWINDOW);
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
    // Release derived search caches while idle; history is never stored.
    s.table.read().unwrap().trim_caches();
    s.session.clear_cache();
}

fn submit(s: &State) {
    let mut buf = vec![0u16; 1024];
    let n = unsafe { GetWindowTextW(s.edit, buf.as_mut_ptr(), buf.len() as i32) } as usize;
    let text = String::from_utf16_lossy(&buf[..n]);
    let cmds = s.commands.evaluate(&text);
    *s.command_line.borrow_mut() = cmds.first().map(|c| format!("{}  —  {}", c.title, c.subtitle)).unwrap_or_default();
    *s.query_text.borrow_mut() = text.clone();
    if cmds.first().is_some_and(|c| c.exclusive) {
        // `/s` claims the input; no file search runs for it.
        *s.model.borrow_mut() = ResultsModel::default();
        // Advance the generation so an in-flight file search is canceled and
        // its late results are rejected as stale.
        let g = s.session.submit("", Options::default());
        s.latest.store(g, Ordering::SeqCst);
        set_count(s);
        unsafe { InvalidateRect(s.hwnd, null(), 1) };
        return;
    }
    let generation = s.session.submit(&text, Options::default());
    s.latest.store(generation, Ordering::SeqCst);
    s.t_key.set(Some((generation, qpc())));
}

fn set_count(s: &State) {
    let (len, selected) = {
        let m = s.model.borrow();
        (m.hits.len(), m.selected)
    };
    unsafe {
        SendMessageW(s.list, LVM_SETITEMCOUNT, len, (LVSICF_NOINVALIDATEALL | LVSICF_NOSCROLL) as LPARAM);
        InvalidateRect(s.list, null(), 0);
        if let Some(i) = selected {
            let mut item: LVITEMW = std::mem::zeroed();
            item.stateMask = LVIS_SELECTED | LVIS_FOCUSED;
            item.state = LVIS_SELECTED | LVIS_FOCUSED;
            SendMessageW(s.list, LVM_SETITEMSTATE, i, &item as *const _ as LPARAM);
            SendMessageW(s.list, LVM_ENSUREVISIBLE, i, 0);
        }
    }
}

fn on_event(s: &State, ev: Event) {
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
    let text = s.query_text.borrow().clone();
    let cmds = s.commands.evaluate(&text);
    if let Some(c) = cmds.first().filter(|c| c.exclusive) {
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
    let Some(id) = s.model.borrow().selected_id() else { return };
    let path = s.table.read().unwrap().path(id);
    // Revalidate: the index may be stale. Never execute; only reveal.
    if !std::path::Path::new(&path).exists() {
        *s.command_line.borrow_mut() = format!("no longer exists: {path}");
        unsafe { InvalidateRect(s.hwnd, null(), 1) };
        return;
    }
    let (op, exe, args) = (wide("open"), wide("explorer.exe"), wide(&format!("/select,\"{path}\"")));
    unsafe { ShellExecuteW(null_mut(), op.as_ptr(), exe.as_ptr(), args.as_ptr(), null(), SW_SHOWNORMAL) };
    hide(s);
}

/// Draws `text` with the chars at `hl` (char indices) in bold accent color.
unsafe fn draw_highlighted(hdc: HDC, s: &State, rc: &RECT, text: &str, hl: &[usize], selected: bool) {
    let (font, bold) = s.fonts.get();
    let normal = if selected { GetSysColor(COLOR_HIGHLIGHTTEXT) } else { GetSysColor(COLOR_WINDOWTEXT) };
    let accent = if selected { normal } else { 0x00C0_5A00 }; // BGR: blue-ish accent
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
            let bg = CreateSolidBrush(if selected { GetSysColor(COLOR_HIGHLIGHT) } else { GetSysColor(COLOR_WINDOW) });
            FillRect(cd.nmcd.hdc, &rc, bg);
            DeleteObject(bg);
            let (text, hl) = {
                let t = s.table.read().unwrap();
                if !t.is_live(hit.id) {
                    return CDRF_SKIPDEFAULT as LRESULT;
                }
                let q = parse(&s.query_text.borrow(), 0);
                let h = highlight(&t, &q, Intensity::Normal, &hit);
                if cd.iSubItem == 0 { (t.name(hit.id).to_owned(), h.name) } else { (t.parent_path(hit.id), h.dir) }
            };
            draw_highlighted(cd.nmcd.hdc, s, &rc, &text, &hl, selected);
            CDRF_SKIPDEFAULT as LRESULT
        }
        _ => CDRF_DODEFAULT as LRESULT,
    }
}

unsafe extern "system" fn edit_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM, _id: usize, _data: usize) -> LRESULT {
    if msg == WM_KEYDOWN {
        let vk = wp as u16;
        let delta = match vk {
            VK_DOWN => Some(1),
            VK_UP => Some(-1),
            VK_NEXT => Some(10),
            VK_PRIOR => Some(-10),
            _ => None,
        };
        if let Some(s) = state() {
            if let Some(d) = delta {
                s.model.borrow_mut().move_selection(d);
                set_count(&s);
                return 0;
            }
            if vk == VK_RETURN {
                activate_selected(&s);
                return 0;
            }
            if vk == VK_ESCAPE {
                hide(&s);
                return 0;
            }
        }
    }
    if msg == WM_CHAR && (wp == 13 || wp == 27) {
        return 0; // swallow the beep for Enter/Esc
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
            if (wp & 0xFFFF) as u32 == WA_INACTIVE && s.selftest.is_empty() {
                hide(s);
            }
            0
        }
        WM_COMMAND => {
            let (id, code) = ((wp & 0xFFFF) as isize, ((wp >> 16) & 0xFFFF) as u32);
            if id == ID_EDIT && code == EN_CHANGE {
                submit(s);
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
                        let text = hit.map(|h| {
                            let t = s.table.read().unwrap();
                            if di.item.iSubItem == 0 { t.name(h.id).to_owned() } else { t.parent_path(h.id) }
                        });
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
                LVN_ITEMCHANGED => {
                    let nm = &*(lp as *const NMLISTVIEW);
                    // Our own programmatic selection arrives while the model
                    // may be borrowed; that change is already reflected.
                    if nm.uNewState & LVIS_SELECTED != 0 && nm.iItem >= 0 {
                        if let Ok(mut m) = s.model.try_borrow_mut() {
                            m.selected = Some(nm.iItem as usize);
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
            let cov = s.coverage.lock().unwrap().clone();
            let mut line = {
                let m = s.model.borrow();
                m.status.as_ref().map(|st| status_line(st, m.hits.len(), &cov)).unwrap_or_else(|| status_line_idle(&cov))
            };
            let cmd = s.command_line.borrow().clone();
            if !cmd.is_empty() {
                line = format!("{cmd}   |   {line}");
            }
            let w = wide(&line);
            // Erase first: text is drawn transparently and repaints are not
            // always preceded by WM_ERASEBKGND.
            let mut bg_rc = rc;
            bg_rc.left = 0;
            FillRect(hdc, &bg_rc, (COLOR_WINDOW + 1) as usize as HBRUSH);
            SelectObject(hdc, s.fonts.get().0);
            SetBkMode(hdc, TRANSPARENT as i32);
            SetTextColor(hdc, GetSysColor(COLOR_GRAYTEXT));
            DrawTextW(hdc, w.as_ptr(), w.len() as i32 - 1, &mut rc, DT_SINGLELINE | DT_VCENTER | DT_NOPREFIX | DT_END_ELLIPSIS);
            EndPaint(hwnd, &ps);
            0
        }
        WM_APP_EVENT => {
            let ev = *Box::from_raw(lp as *mut Event);
            on_event(s, ev);
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
        WM_APP_INDEX => {
            InvalidateRect(hwnd, null(), 1);
            0
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

fn status_line_idle(cov: &Coverage) -> String {
    if cov.complete { "type to search".into() } else { format!("type to search · indexing: {}", cov.problems.join(", ")) }
}

/// Background indexing for the spike: traversal of `root` (no persistence),
/// or a synthetic corpus. Batches take the table write lock briefly so
/// searches keep running.
fn spawn_indexer(table: Arc<RwLock<NameTable>>, coverage: Arc<Mutex<Coverage>>, hwnd_bits: usize, root: Option<String>, synthetic: Option<usize>) {
    std::thread::spawn(move || {
        let notify = || unsafe {
            PostMessageW(hwnd_bits as HWND, WM_APP_INDEX, 0, 0);
        };
        if let Some(n) = synthetic {
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
            *coverage.lock().unwrap() = Coverage { complete: true, problems: vec![] };
            drop(t);
            notify();
            return;
        }
        let root = root.unwrap_or_else(|| std::env::var("USERPROFILE").unwrap_or_else(|_| "C:\\".into()));
        let label = root.trim_end_matches('\\').to_owned();
        *coverage.lock().unwrap() = Coverage { complete: false, problems: vec![format!("{label} indexing")] };
        let mut src = TraversalSource::new(root.clone().into(), RealFs);
        let mut vol = VolumeIndex::new(&mut table.write().unwrap(), 1, src.root_frn, &label);
        let mut batch: Vec<FileRecord> = Vec::new();
        let mut ch = Changes::default();
        let mut flush = |batch: &mut Vec<FileRecord>, vol: &mut VolumeIndex| {
            let mut t = table.write().unwrap();
            for r in batch.drain(..) {
                vol.apply_record(&mut t, &r, &mut ch);
            }
            ch.ops.clear();
            ch.followups.clear();
        };
        src.walk(&mut |r| {
            batch.push(r);
            if batch.len() >= 10_000 {
                flush(&mut batch, &mut vol);
                notify();
            }
            true
        });
        flush(&mut batch, &mut vol);
        let rep = src.report.clone();
        *coverage.lock().unwrap() = if rep.complete() {
            Coverage { complete: true, problems: vec![] }
        } else {
            Coverage { complete: false, problems: vec![format!("{label}: {} folders unreadable", rep.skipped.len())] }
        };
        println!("indexed {} entries under {root} ({} unreadable folders, {} links not followed)", rep.entries, rep.skipped.len(), rep.links_not_followed);
        notify();
    });
}

pub fn run() {
    let t_start = qpc();
    let args: Vec<String> = std::env::args().collect();
    let synthetic = args.iter().position(|a| a == "--synthetic").and_then(|i| args.get(i + 1)).and_then(|v| v.parse().ok());
    let flag_value = |f: &str| args.iter().position(|a| a == f).and_then(|i| args.get(i + 1)).cloned();
    let selftest: Vec<String> = flag_value("--selftest").map(|v| v.split('|').map(str::to_owned).collect()).unwrap_or_default();
    let show_now = args.iter().any(|a| a == "--show") || !selftest.is_empty();
    let root = args.iter().skip(1).filter(|a| !a.starts_with("--")).find(|a| Some(*a) != flag_value("--synthetic").as_ref() && Some(*a) != flag_value("--selftest").as_ref()).cloned();
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
            hbrBackground: (COLOR_WINDOW + 1) as usize as HBRUSH,
            lpszMenuName: null(),
            lpszClassName: class.as_ptr(),
            hIconSm: null_mut(),
        };
        RegisterClassExW(&wc);
        let title = wide("Flint");
        let hwnd = CreateWindowExW(WS_EX_TOOLWINDOW | WS_EX_TOPMOST, class.as_ptr(), title.as_ptr(), WS_POPUP | WS_BORDER, 0, 0, 720, 520, null_mut(), null_mut(), hinst, null());
        let dpi = GetDpiForWindow(hwnd).max(96);
        let edit_cls = wide("EDIT");
        let edit = CreateWindowExW(0, edit_cls.as_ptr(), null(), WS_CHILD | WS_VISIBLE | WS_BORDER | ES_AUTOHSCROLL as u32, 0, 0, 10, 10, hwnd, ID_EDIT as HMENU, hinst, null());
        let lv_cls = wide("SysListView32");
        let list = CreateWindowExW(0, lv_cls.as_ptr(), null(), WS_CHILD | WS_VISIBLE | (LVS_REPORT | LVS_OWNERDATA | LVS_SINGLESEL | LVS_SHOWSELALWAYS | LVS_NOCOLUMNHEADER), 0, 0, 10, 10, hwnd, ID_LIST as HMENU, hinst, null());
        SendMessageW(list, LVM_SETEXTENDEDLISTVIEWSTYLE, 0, (LVS_EX_FULLROWSELECT | LVS_EX_DOUBLEBUFFER) as LPARAM);
        for (i, name) in ["Name", "Folder"].iter().enumerate() {
            let mut text = wide(name);
            let mut col: LVCOLUMNW = std::mem::zeroed();
            col.mask = LVCF_TEXT | LVCF_WIDTH;
            col.cx = 200;
            col.pszText = text.as_mut_ptr();
            SendMessageW(list, LVM_INSERTCOLUMNW, i, &col as *const _ as LPARAM);
        }
        SetWindowSubclass(edit, Some(edit_proc), 1, 0);

        let table = Arc::new(RwLock::new(NameTable::new()));
        let latest = Arc::new(AtomicU64::new(0));
        let hwnd_bits = hwnd as usize;
        let session = Session::new(table.clone(), Duration::from_millis(8), move |ev: Event| {
            let p = Box::into_raw(Box::new(ev));
            if PostMessageW(hwnd_bits as HWND, WM_APP_EVENT, 0, p as LPARAM) == 0 {
                drop(Box::from_raw(p));
            }
        });
        let coverage = Arc::new(Mutex::new(Coverage::default()));
        spawn_indexer(table.clone(), coverage.clone(), hwnd_bits, root, synthetic);
        let (font, bold) = make_fonts(dpi);
        let mut freq = 0i64;
        QueryPerformanceFrequency(&mut freq);
        let state = Rc::new(State {
            hwnd,
            edit,
            list,
            table,
            session,
            latest,
            coverage,
            commands: Registry::with_builtins(websearch::Provider::duckduckgo()),
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
        });
        layout(&state);
        STATE.with(|s| *s.borrow_mut() = Some(state.clone()));

        // Ctrl+Space can be taken (e.g. IME toggling on CJK systems): report
        // it and fall back instead of silently doing nothing.
        let hotkey = if RegisterHotKey(hwnd, HOTKEY_ID, MOD_CONTROL | MOD_NOREPEAT, VK_SPACE as u32) != 0 {
            "Ctrl+Space"
        } else if RegisterHotKey(hwnd, HOTKEY_ID, MOD_CONTROL | MOD_ALT | MOD_NOREPEAT, VK_SPACE as u32) != 0 {
            "Ctrl+Alt+Space (Ctrl+Space is registered by another application)"
        } else {
            "none (both hotkeys are taken; showing window now)"
        };
        println!("ready in {:.1} ms; hotkey: {hotkey}; Esc hides, Alt+F4 quits", (qpc() - t_start) as f64 * 1000.0 / freq as f64);
        if hotkey.starts_with("none") || show_now {
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
        drop(state);
        STATE.with(|s| s.borrow_mut().take());
    }
}
