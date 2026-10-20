//! Windows implementation: live file probe, `IFileOperation` executor,
//! shell actions (open, open with, reveal, properties, folder picker),
//! clipboard (paths, files with copy/cut effect) and the native shell
//! context menu (`IContextMenu`, with `IContextMenu2/3` message forwarding
//! so third-party extensions that draw their own items keep working).
//!
//! All functions run on the UI thread, which must have called
//! [`init_com`] (OLE single-threaded apartment, required by the shell).
//!
//! Status: compile-checked and exercised under Wine only; not yet validated
//! on Windows hardware (see docs/requirements.md).

use crate::{Collision, Executor, OpKind, PlanItem, Probe, Stat};
use std::cell::RefCell;
use std::ffi::c_void;
use windows::core::{Interface, BOOL, HRESULT, PCSTR, PCWSTR, PSTR, PWSTR};
use windows::Win32::Foundation::*;
use windows::Win32::Storage::FileSystem::*;
use windows::Win32::System::Com::*;
use windows::Win32::System::DataExchange::*;
use windows::Win32::System::Memory::*;
use windows::Win32::System::Ole::*;
use windows::Win32::UI::Shell::Common::ITEMIDLIST;
use windows::Win32::UI::Shell::*;
use windows::Win32::UI::WindowsAndMessaging::*;

/// Window handle bits (handles are opaque values, not dereferenced).
pub type RawHwnd = usize;

fn hw(h: RawHwnd) -> HWND {
    HWND(h as *mut c_void)
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Win32 path form that is not limited to MAX_PATH.
fn long_path(p: &str) -> String {
    if p.len() < 248 || p.starts_with("\\\\?\\") {
        p.to_owned()
    } else if let Some(unc) = p.strip_prefix("\\\\") {
        format!("\\\\?\\UNC\\{unc}")
    } else {
        format!("\\\\?\\{p}")
    }
}

/// Initializes OLE (STA) on the calling thread. Call once on the UI thread.
pub fn init_com() -> Result<(), String> {
    unsafe { OleInitialize(None).map_err(|e| e.to_string()) }
}

const UNIX_EPOCH_AS_FILETIME: i64 = 116_444_736_000_000_000;

fn filetime_to_unix(ft: FILETIME) -> i64 {
    let t = ((ft.dwHighDateTime as i64) << 32) | ft.dwLowDateTime as i64;
    (t - UNIX_EPOCH_AS_FILETIME).div_euclid(10_000_000)
}

/// Reads file ID, type, size and mtime without following reparse points
/// (so a link is identified as the link, not its target).
pub struct WinProbe;

impl Probe for WinProbe {
    fn stat(&self, path: &str) -> Result<Option<Stat>, String> {
        let w = wide(&long_path(path));
        let h = unsafe { CreateFileW(PCWSTR(w.as_ptr()), FILE_READ_ATTRIBUTES.0, FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE, None, OPEN_EXISTING, FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT, None) };
        let h = match h {
            Ok(h) => h,
            Err(e) => {
                let gone = [ERROR_FILE_NOT_FOUND, ERROR_PATH_NOT_FOUND, ERROR_INVALID_NAME, ERROR_BAD_NETPATH].iter().any(|c| e.code() == HRESULT::from_win32(c.0));
                return if gone { Ok(None) } else { Err(e.message()) };
            }
        };
        let mut info = BY_HANDLE_FILE_INFORMATION::default();
        let r = unsafe { GetFileInformationByHandle(h, &mut info) };
        unsafe {
            let _ = CloseHandle(h);
        }
        r.map_err(|e| e.message())?;
        let attrs = info.dwFileAttributes;
        Ok(Some(Stat {
            is_dir: attrs & FILE_ATTRIBUTE_DIRECTORY.0 != 0,
            is_link: attrs & FILE_ATTRIBUTE_REPARSE_POINT.0 != 0,
            file_id: Some(((info.nFileIndexHigh as u64) << 32) | info.nFileIndexLow as u64),
            size: ((info.nFileSizeHigh as u64) << 32) | info.nFileSizeLow as u64,
            mtime: filetime_to_unix(info.ftLastWriteTime),
        }))
    }
}

fn shell_item(path: &str) -> windows::core::Result<IShellItem> {
    let w = wide(path);
    unsafe { SHCreateItemFromParsingName(PCWSTR(w.as_ptr()), None) }
}

/// Runs a batch through one `IFileOperation`, which provides the standard
/// progress UI, undo (Recycle Bin), elevation prompts and conflict dialogs.
pub struct ShellExecutor {
    pub hwnd: RawHwnd,
}

impl Executor for ShellExecutor {
    fn run(&mut self, kind: &OpKind, collision: Collision, items: &[&PlanItem]) -> Result<(), String> {
        match unsafe { self.run_inner(kind, collision, items) } {
            // IFileOperation queues nothing before failing like this, so the
            // older API can safely take over (Wine lacks IFileOperation).
            Err(e) if e.code() == E_NOTIMPL || e.code() == REGDB_E_CLASSNOTREG => unsafe { self.run_legacy(kind, collision, items) },
            r => r.map_err(|e| e.message()),
        }
    }
}

impl ShellExecutor {
    unsafe fn run_inner(&mut self, kind: &OpKind, collision: Collision, items: &[&PlanItem]) -> windows::core::Result<()> {
        let op: IFileOperation = CoCreateInstance(&FileOperation, None, CLSCTX_ALL)?;
        op.SetOwnerWindow(hw(self.hwnd))?;
        // Confirmation is our reviewed preview; the shell still shows
        // progress, elevation and per-conflict prompts.
        let mut flags = FOF_NOCONFIRMMKDIR.0 | FOFX_NOMINIMIZEBOX.0;
        match kind {
            OpKind::Recycle => flags |= FOF_ALLOWUNDO.0 | FOFX_RECYCLEONDELETE.0 | FOF_NOCONFIRMATION.0 | FOF_WANTNUKEWARNING.0,
            OpKind::DeletePermanently => flags |= FOF_NOCONFIRMATION.0,
            _ => flags |= FOF_ALLOWUNDO.0,
        }
        if collision == Collision::KeepBoth {
            flags |= FOF_RENAMEONCOLLISION.0;
        }
        op.SetOperationFlags(FILEOPERATION_FLAGS(flags))?;
        let dest = match kind {
            OpKind::Copy { dest_dir } | OpKind::Move { dest_dir } => Some(shell_item(dest_dir)?),
            _ => None,
        };
        for it in items {
            let item = shell_item(&it.target.path)?;
            match kind {
                OpKind::Recycle | OpKind::DeletePermanently => op.DeleteItem(&item, None)?,
                OpKind::Copy { .. } => op.CopyItem(&item, dest.as_ref(), PCWSTR::null(), None)?,
                OpKind::Move { .. } => op.MoveItem(&item, dest.as_ref(), PCWSTR::null(), None)?,
                OpKind::Rename { new_name } => {
                    let n = wide(new_name);
                    op.RenameItem(&item, PCWSTR(n.as_ptr()), None)?
                }
                OpKind::NewFolder { name } => {
                    let n = wide(name);
                    op.NewItem(&item, FILE_ATTRIBUTE_DIRECTORY.0, PCWSTR(n.as_ptr()), PCWSTR::null(), None)?
                }
            }
        }
        match op.PerformOperations() {
            // The user cancelling is reported per item by observation.
            Err(e) if e.code() == COPYENGINE_E_USER_CANCELLED || e.code() == HRESULT::from_win32(ERROR_CANCELLED.0) => Ok(()),
            r => r,
        }
    }
}

impl ShellExecutor {
    /// `SHFileOperationW` fallback with the same semantics (Recycle Bin via
    /// undo, conflict handling, progress UI).
    unsafe fn run_legacy(&mut self, kind: &OpKind, collision: Collision, items: &[&PlanItem]) -> Result<(), String> {
        let list = |paths: &mut dyn Iterator<Item = &str>| -> Vec<u16> {
            let mut v: Vec<u16> = Vec::new();
            for p in paths {
                v.extend(p.encode_utf16());
                v.push(0);
            }
            v.push(0);
            v
        };
        let mut flags = FOF_NOCONFIRMMKDIR.0 as u16;
        let (func, from, to) = match kind {
            OpKind::NewFolder { name } => {
                for it in items {
                    let w = wide(&long_path(&crate::paths::join(&it.target.path, name)));
                    CreateDirectoryW(PCWSTR(w.as_ptr()), None).map_err(|e| e.message())?;
                }
                return Ok(());
            }
            OpKind::Recycle => {
                flags |= (FOF_ALLOWUNDO.0 | FOF_NOCONFIRMATION.0 | FOF_WANTNUKEWARNING.0) as u16;
                (FO_DELETE, list(&mut items.iter().map(|i| i.target.path.as_str())), None)
            }
            OpKind::DeletePermanently => {
                flags |= FOF_NOCONFIRMATION.0 as u16;
                (FO_DELETE, list(&mut items.iter().map(|i| i.target.path.as_str())), None)
            }
            OpKind::Copy { dest_dir } | OpKind::Move { dest_dir } => {
                flags |= FOF_ALLOWUNDO.0 as u16;
                let f = if matches!(kind, OpKind::Copy { .. }) { FO_COPY } else { FO_MOVE };
                (f, list(&mut items.iter().map(|i| i.target.path.as_str())), Some(list(&mut std::iter::once(dest_dir.as_str()))))
            }
            OpKind::Rename { new_name } => {
                flags |= FOF_ALLOWUNDO.0 as u16;
                let it = items.first().ok_or("nothing to rename")?;
                let dest = crate::paths::join(crate::paths::parent(&it.target.path).ok_or("cannot rename a root")?, new_name);
                (FO_RENAME, list(&mut std::iter::once(it.target.path.as_str())), Some(list(&mut std::iter::once(dest.as_str()))))
            }
        };
        if collision == Collision::KeepBoth {
            flags |= FOF_RENAMEONCOLLISION.0 as u16;
        }
        let mut op = SHFILEOPSTRUCTW { hwnd: hw(self.hwnd), wFunc: func, pFrom: PCWSTR(from.as_ptr()), pTo: to.as_ref().map_or(PCWSTR::null(), |t| PCWSTR(t.as_ptr())), fFlags: flags, ..Default::default() };
        match SHFileOperationW(&mut op) {
            0 => Ok(()),
            // 0x4C7 = ERROR_CANCELLED: reported per item by observation.
            0x4C7 => Ok(()),
            code => Err(format!("shell file operation failed (code {code:#x})")),
        }
    }
}

/// Opens with the default handler (the user's explicit request).
pub fn open(hwnd: RawHwnd, path: &str) -> Result<(), String> {
    let p = wide(path);
    let mut info = SHELLEXECUTEINFOW { cbSize: std::mem::size_of::<SHELLEXECUTEINFOW>() as u32, fMask: SEE_MASK_INVOKEIDLIST | SEE_MASK_FLAG_NO_UI, hwnd: hw(hwnd), lpFile: PCWSTR(p.as_ptr()), nShow: SW_SHOWNORMAL.0, ..Default::default() };
    unsafe { ShellExecuteExW(&mut info) }.map_err(|e| e.message())
}

pub fn open_with(hwnd: RawHwnd, path: &str) -> Result<(), String> {
    let p = wide(path);
    let info = OPENASINFO { pcszFile: PCWSTR(p.as_ptr()), pcszClass: PCWSTR::null(), oaifInFlags: OAIF_ALLOW_REGISTRATION | OAIF_EXEC };
    unsafe { SHOpenWithDialog(Some(hw(hwnd)), &info) }.map_err(|e| e.message())
}

pub fn properties(hwnd: RawHwnd, path: &str) -> Result<(), String> {
    let p = wide(path);
    let ok = unsafe { SHObjectProperties(Some(hw(hwnd)), SHOP_FILEPATH, PCWSTR(p.as_ptr()), PCWSTR::null()) };
    if ok.as_bool() { Ok(()) } else { Err("the properties dialog could not be opened".into()) }
}

/// Opens Explorer windows with the items selected (one window per folder).
pub fn reveal(paths: &[String]) -> Result<(), String> {
    let mut by_folder: Vec<(String, Vec<&String>)> = Vec::new();
    for p in paths {
        let parent = crate::paths::parent(p).unwrap_or(p).to_owned();
        match by_folder.iter_mut().find(|(f, _)| crate::paths::same(f, &parent)) {
            Some((_, v)) => v.push(p),
            None => by_folder.push((parent, vec![p])),
        }
    }
    for (folder, items) in by_folder {
        unsafe {
            let fw = wide(&folder);
            let fp = ILCreateFromPathW(PCWSTR(fw.as_ptr()));
            if fp.is_null() {
                return Err(format!("cannot open {folder}"));
            }
            let kids: Vec<*mut ITEMIDLIST> = items.iter().map(|p| ILCreateFromPathW(PCWSTR(wide(p).as_ptr()))).filter(|p| !p.is_null()).collect();
            let consts: Vec<*const ITEMIDLIST> = kids.iter().map(|p| *p as *const _).collect();
            let r = SHOpenFolderAndSelectItems(fp, Some(&consts), 0);
            for k in kids {
                ILFree(Some(k));
            }
            ILFree(Some(fp));
            r.map_err(|e| e.message())?;
        }
    }
    Ok(())
}

/// Folder picker (`IFileOpenDialog` with `FOS_PICKFOLDERS`). None if
/// cancelled or the choice is not a filesystem folder.
pub fn pick_folder(hwnd: RawHwnd, title: &str) -> Option<String> {
    unsafe {
        let dlg: IFileOpenDialog = CoCreateInstance(&FileOpenDialog, None, CLSCTX_ALL).ok()?;
        let opts = dlg.GetOptions().ok()?;
        dlg.SetOptions(opts | FOS_PICKFOLDERS | FOS_FORCEFILESYSTEM).ok()?;
        let t = wide(title);
        let _ = dlg.SetTitle(PCWSTR(t.as_ptr()));
        dlg.Show(Some(hw(hwnd))).ok()?;
        let item = dlg.GetResult().ok()?;
        let name: PWSTR = item.GetDisplayName(SIGDN_FILESYSPATH).ok()?;
        let s = name.to_string().ok();
        CoTaskMemFree(Some(name.0 as *const c_void));
        s
    }
}

struct Clipboard;

impl Clipboard {
    fn open(hwnd: RawHwnd) -> Result<Self, String> {
        // Another process may hold the clipboard briefly.
        for _ in 0..10 {
            if unsafe { OpenClipboard(Some(hw(hwnd))) }.is_ok() {
                return Ok(Clipboard);
            }
            std::thread::sleep(std::time::Duration::from_millis(15));
        }
        Err("the clipboard is in use by another application".into())
    }
}

impl Drop for Clipboard {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseClipboard();
        }
    }
}

unsafe fn global_from(bytes: &[u8]) -> Result<HGLOBAL, String> {
    let h = GlobalAlloc(GMEM_MOVEABLE, bytes.len()).map_err(|e| e.message())?;
    let p = GlobalLock(h) as *mut u8;
    if p.is_null() {
        let _ = GlobalFree(Some(h));
        return Err("out of memory".into());
    }
    std::ptr::copy_nonoverlapping(bytes.as_ptr(), p, bytes.len());
    let _ = GlobalUnlock(h);
    Ok(h)
}

unsafe fn set_data(format: u32, bytes: &[u8]) -> Result<(), String> {
    let h = global_from(bytes)?;
    // On success the clipboard owns the memory.
    if SetClipboardData(format, Some(HANDLE(h.0))).is_err() {
        let _ = GlobalFree(Some(h));
        return Err("could not place data on the clipboard".into());
    }
    Ok(())
}

/// Copies text (e.g. full paths, one per line).
pub fn copy_text(hwnd: RawHwnd, text: &str) -> Result<(), String> {
    let _c = Clipboard::open(hwnd)?;
    unsafe {
        EmptyClipboard().map_err(|e| e.message())?;
        let w: Vec<u8> = wide(text).iter().flat_map(|u| u.to_le_bytes()).collect();
        set_data(CF_UNICODETEXT.0 as u32, &w)
    }
}

/// Puts files on the clipboard as Explorer does (`CF_HDROP` plus
/// "Preferred DropEffect"), so paste works in Explorer and here. `cut`
/// marks them to be moved on paste.
pub fn copy_files(hwnd: RawHwnd, paths: &[String], cut: bool) -> Result<(), String> {
    let _c = Clipboard::open(hwnd)?;
    unsafe {
        EmptyClipboard().map_err(|e| e.message())?;
        let header = std::mem::size_of::<DROPFILES>();
        let df = DROPFILES { pFiles: header as u32, pt: POINT::default(), fNC: BOOL(0), fWide: BOOL(1) };
        let mut bytes = std::slice::from_raw_parts(&df as *const DROPFILES as *const u8, header).to_vec();
        for p in paths {
            bytes.extend(wide(p).iter().flat_map(|u| u.to_le_bytes()));
        }
        bytes.extend([0, 0]);
        set_data(CF_HDROP.0 as u32, &bytes)?;
        let effect = if cut { DROPEFFECT_MOVE.0 } else { DROPEFFECT_COPY.0 };
        set_data(RegisterClipboardFormatW(CFSTR_PREFERREDDROPEFFECT), &effect.to_le_bytes())
    }
}

/// Files on the clipboard and whether they were cut (move on paste).
pub fn clipboard_files(hwnd: RawHwnd) -> Result<Option<(Vec<String>, bool)>, String> {
    let _c = Clipboard::open(hwnd)?;
    unsafe {
        if IsClipboardFormatAvailable(CF_HDROP.0 as u32).is_err() {
            return Ok(None);
        }
        let h = GetClipboardData(CF_HDROP.0 as u32).map_err(|e| e.message())?;
        let drop = HDROP(h.0);
        let n = DragQueryFileW(drop, u32::MAX, None);
        let mut files = Vec::with_capacity(n as usize);
        for i in 0..n {
            let len = DragQueryFileW(drop, i, None) as usize;
            let mut buf = vec![0u16; len + 1];
            DragQueryFileW(drop, i, Some(&mut buf));
            files.push(String::from_utf16_lossy(&buf[..len]));
        }
        let mut cut = false;
        let fmt = RegisterClipboardFormatW(CFSTR_PREFERREDDROPEFFECT);
        if let Ok(e) = GetClipboardData(fmt) {
            let g = HGLOBAL(e.0);
            let p = GlobalLock(g) as *const u32;
            if !p.is_null() {
                cut = *p & DROPEFFECT_MOVE.0 != 0;
                let _ = GlobalUnlock(g);
            }
        }
        Ok(Some((files, cut)))
    }
}

/// First command id given to the shell's menu items; ids below it are the
/// caller's own items.
pub const SHELL_FIRST: u32 = 0x1000;
const SHELL_LAST: u32 = 0x7FFF;
const CMIC_MASK_UNICODE: u32 = 0x4000;

thread_local! {
    /// The shell menu while it is shown, for message forwarding.
    static ACTIVE_MENU: RefCell<Option<IContextMenu>> = const { RefCell::new(None) };
}

/// Forwards owner-draw and submenu messages to the active shell menu
/// (`IContextMenu3`/`2`). Call from the window procedure for
/// `WM_INITMENUPOPUP`, `WM_DRAWITEM`, `WM_MEASUREITEM` and `WM_MENUCHAR`;
/// returns Some(result) if the shell handled it.
pub fn forward_menu_message(msg: u32, wp: usize, lp: isize) -> Option<isize> {
    ACTIVE_MENU.with(|m| {
        let m = m.borrow();
        let menu = m.as_ref()?;
        unsafe {
            if let Ok(m3) = menu.cast::<IContextMenu3>() {
                let mut r = LRESULT(0);
                return m3.HandleMenuMsg2(msg, WPARAM(wp), LPARAM(lp), Some(&mut r)).ok().map(|_| r.0);
            }
            if let Ok(m2) = menu.cast::<IContextMenu2>() {
                return m2.HandleMenuMsg(msg, WPARAM(wp), LPARAM(lp)).ok().map(|_| 0);
            }
        }
        None
    })
}

#[derive(Debug, PartialEq, Eq)]
pub enum MenuChoice {
    None,
    /// One of the caller's items.
    Custom(u32),
    /// A shell verb the caller routes through its own safe pipeline
    /// ("delete", "rename") instead of letting the shell act unchecked.
    Intercepted(String),
    /// The shell (or a third-party extension) carried out the command.
    Shell(String),
}

/// Verbs whose shell implementation would bypass revalidation.
const INTERCEPT: &[&str] = &["delete", "rename"];

/// Shows the custom items on top of the native context menu of `paths`
/// (all in the same folder; otherwise only the custom items are shown).
/// `custom` entries with id 0 are separators. `extended` adds the
/// Shift-only verbs, as in Explorer.
pub fn context_menu(hwnd: RawHwnd, paths: &[String], custom: &[(u32, String, bool)], x: i32, y: i32, extended: bool) -> Result<MenuChoice, String> {
    unsafe {
        let menu = CreatePopupMenu().map_err(|e| e.message())?;
        for (id, label, enabled) in custom {
            if *id == 0 {
                let _ = AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null());
            } else {
                let l = wide(label);
                let _ = AppendMenuW(menu, if *enabled { MF_STRING } else { MF_STRING | MF_GRAYED }, *id as usize, PCWSTR(l.as_ptr()));
            }
        }
        let shell = shell_menu(hwnd, paths);
        if let Some(cm) = &shell {
            let _ = AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null());
            let pos = GetMenuItemCount(Some(menu)).max(0) as u32;
            let flags = CMF_NORMAL | CMF_EXPLORE | if extended { CMF_EXTENDEDVERBS } else { 0 };
            let _ = cm.QueryContextMenu(menu, pos, SHELL_FIRST, SHELL_LAST, flags);
        }
        ACTIVE_MENU.with(|m| *m.borrow_mut() = shell.clone());
        let cmd = TrackPopupMenuEx(menu, (TPM_RETURNCMD | TPM_RIGHTBUTTON).0, x, y, hw(hwnd), None).0 as u32;
        ACTIVE_MENU.with(|m| m.borrow_mut().take());
        let _ = DestroyMenu(menu);
        if cmd == 0 {
            return Ok(MenuChoice::None);
        }
        if cmd < SHELL_FIRST {
            return Ok(MenuChoice::Custom(cmd));
        }
        let Some(cm) = shell else { return Ok(MenuChoice::None) };
        let offset = (cmd - SHELL_FIRST) as usize;
        let mut verb = [0u16; 128];
        let verb = match cm.GetCommandString(offset, GCS_VERBW, None, PSTR(verb.as_mut_ptr() as *mut u8), verb.len() as u32) {
            Ok(()) => String::from_utf16_lossy(&verb[..verb.iter().position(|&c| c == 0).unwrap_or(0)]),
            Err(_) => String::new(),
        };
        if INTERCEPT.iter().any(|v| v.eq_ignore_ascii_case(&verb)) {
            return Ok(MenuChoice::Intercepted(verb.to_ascii_lowercase()));
        }
        let info = CMINVOKECOMMANDINFOEX {
            cbSize: std::mem::size_of::<CMINVOKECOMMANDINFOEX>() as u32,
            fMask: CMIC_MASK_UNICODE | CMIC_MASK_PTINVOKE,
            hwnd: hw(hwnd),
            // MAKEINTRESOURCE(offset): verbs are invoked by menu offset.
            lpVerb: PCSTR(offset as *const u8),
            lpVerbW: PCWSTR(offset as *const u16),
            nShow: SW_SHOWNORMAL.0,
            ptInvoke: POINT { x, y },
            ..Default::default()
        };
        cm.InvokeCommand(&info as *const _ as *const CMINVOKECOMMANDINFO).map_err(|e| e.message())?;
        Ok(MenuChoice::Shell(verb))
    }
}

/// `IContextMenu` for items sharing one parent folder.
unsafe fn shell_menu(hwnd: RawHwnd, paths: &[String]) -> Option<IContextMenu> {
    let first = paths.first()?;
    let parent = crate::paths::parent(first)?;
    if !paths.iter().all(|p| crate::paths::parent(p).is_some_and(|q| crate::paths::same(q, parent))) {
        return None;
    }
    let mut pidls: Vec<*mut ITEMIDLIST> = Vec::new();
    for p in paths {
        let w = wide(p);
        let mut pidl = std::ptr::null_mut();
        if SHParseDisplayName(PCWSTR(w.as_ptr()), None, &mut pidl, 0, None).is_err() || pidl.is_null() {
            for q in pidls {
                ILFree(Some(q));
            }
            return None;
        }
        pidls.push(pidl);
    }
    let folder: Option<IShellFolder> = SHBindToParent(pidls[0], None).ok();
    let kids: Vec<*const ITEMIDLIST> = pidls.iter().map(|p| ILFindLastID(*p) as *const _).collect();
    let menu = folder.and_then(|f| f.GetUIObjectOf::<IContextMenu>(hw(hwnd), &kids, None).ok());
    for p in pidls {
        ILFree(Some(p));
    }
    menu
}
