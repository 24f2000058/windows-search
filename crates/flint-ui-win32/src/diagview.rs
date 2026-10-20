//! Diagnostics window (`/diag`): shows exactly the text that Copy and
//! Save export. The text is redacted unless the user switches redaction
//! off for this window; the heading always says which.

use crate::win::{state, State};
use std::cell::RefCell;
use std::ptr::{null, null_mut};
use windows_sys::Win32::Foundation::*;
use windows_sys::Win32::Graphics::Gdi::*;
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::UI::Controls::Dialogs::*;
use windows_sys::Win32::UI::Controls::EM_SETLIMITTEXT;
use windows_sys::Win32::UI::WindowsAndMessaging::*;

const ID_TEXT: isize = 1;
const ID_COPY: isize = 2;
const ID_SAVE: isize = 3;
const ID_REDACT: isize = 4;
const ID_REFRESH: isize = 5;
const BUTTONS: [(isize, &str); 4] = [(ID_COPY, "&Copy"), (ID_SAVE, "&Save…"), (ID_REDACT, "Show &unredacted"), (ID_REFRESH, "&Refresh")];

struct Win {
    hwnd: HWND,
    text_box: HWND,
    buttons: Vec<HWND>,
    redact: bool,
    text: String,
    font: HFONT,
}

thread_local! {
    static WIN: RefCell<Option<Win>> = const { RefCell::new(None) };
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

fn refresh(s: &State) {
    let redact = WIN.with(|w| w.borrow().as_ref().map(|w| w.redact)).unwrap_or(true);
    let text = crate::diag::render(&crate::win::diag_snapshot(s), &crate::win::redactor(s, redact)).replace('\n', "\r\n");
    WIN.with(|w| {
        if let Some(w) = w.borrow_mut().as_mut() {
            let t = wide(&text);
            unsafe {
                SetWindowTextW(w.text_box, t.as_ptr());
                let label = wide(if w.redact { "Show &unredacted" } else { "&Redact" });
                SetWindowTextW(w.buttons[2], label.as_ptr());
                let title = wide(if w.redact { "Flint diagnostics (redacted)" } else { "Flint diagnostics (NOT redacted)" });
                SetWindowTextW(w.hwnd, title.as_ptr());
            }
            w.text = text;
        }
    });
}

fn layout(w: &Win) {
    unsafe {
        let mut rc: RECT = std::mem::zeroed();
        GetClientRect(w.hwnd, &mut rc);
        let d = windows_sys::Win32::UI::HiDpi::GetDpiForWindow(w.hwnd).max(96) as i32;
        let (pad, bh, bw) = (8 * d / 96, 28 * d / 96, 130 * d / 96);
        for (i, b) in w.buttons.iter().enumerate() {
            MoveWindow(*b, pad + i as i32 * (bw + pad), pad, bw, bh, 1);
        }
        MoveWindow(w.text_box, pad, pad * 2 + bh, rc.right - 2 * pad, rc.bottom - 3 * pad - bh, 1);
    }
}

fn save(owner: HWND, text: &str) -> Result<Option<String>, String> {
    let mut buf = [0u16; 1024];
    for (i, c) in "flint-diagnostics.txt".encode_utf16().enumerate() {
        buf[i] = c;
    }
    let filter: Vec<u16> = "Text files (*.txt)\0*.txt\0\0".encode_utf16().collect();
    let ext = wide("txt");
    let mut ofn: OPENFILENAMEW = unsafe { std::mem::zeroed() };
    ofn.lStructSize = std::mem::size_of::<OPENFILENAMEW>() as u32;
    ofn.hwndOwner = owner;
    ofn.lpstrFilter = filter.as_ptr();
    ofn.lpstrFile = buf.as_mut_ptr();
    ofn.nMaxFile = buf.len() as u32;
    ofn.lpstrDefExt = ext.as_ptr();
    ofn.Flags = OFN_OVERWRITEPROMPT | OFN_PATHMUSTEXIST | OFN_NOCHANGEDIR;
    if unsafe { GetSaveFileNameW(&mut ofn) } == 0 {
        return Ok(None);
    }
    let n = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    let path = String::from_utf16_lossy(&buf[..n]);
    std::fs::write(&path, text.as_bytes()).map_err(|e| e.to_string())?;
    Ok(Some(path))
}

unsafe extern "system" fn proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match msg {
        WM_SIZE => {
            WIN.with(|w| {
                if let Some(w) = w.borrow().as_ref() {
                    layout(w);
                }
            });
            0
        }
        WM_COMMAND => {
            let id = (wp & 0xFFFF) as isize;
            let Some(s) = state() else { return 0 };
            match id {
                ID_COPY => {
                    let text = WIN.with(|w| w.borrow().as_ref().map(|w| w.text.clone())).unwrap_or_default();
                    let ok = flint_ops::win::copy_text(hwnd as usize, &text).is_ok();
                    let t = wide(if ok { "Copied" } else { "Copy failed" });
                    SetWindowTextW(hwnd, t.as_ptr());
                }
                ID_SAVE => {
                    let text = WIN.with(|w| w.borrow().as_ref().map(|w| w.text.clone())).unwrap_or_default();
                    let r = save(hwnd, &text);
                    let msg = match r {
                        Ok(Some(p)) => format!("Saved to {p}"),
                        Ok(None) => return 0,
                        Err(e) => format!("Could not save: {e}"),
                    };
                    let (m, c) = (wide(&msg), wide("Flint diagnostics"));
                    MessageBoxW(hwnd, m.as_ptr(), c.as_ptr(), MB_OK);
                }
                ID_REDACT => {
                    let turning_off = WIN.with(|w| w.borrow().as_ref().is_some_and(|w| w.redact));
                    if turning_off {
                        let (m, c) = (wide("The unredacted report contains your folder names, file paths from errors, and your user and computer names.\n\nShow it anyway? (Only for this window; the setting is unchanged.)"), wide("Flint diagnostics"));
                        if MessageBoxW(hwnd, m.as_ptr(), c.as_ptr(), MB_YESNO | MB_ICONWARNING | MB_DEFBUTTON2) != IDYES {
                            return 0;
                        }
                    }
                    WIN.with(|w| {
                        if let Some(w) = w.borrow_mut().as_mut() {
                            w.redact = !w.redact;
                        }
                    });
                    refresh(&s);
                }
                ID_REFRESH => refresh(&s),
                _ => {}
            }
            0
        }
        WM_CLOSE => {
            DestroyWindow(hwnd);
            0
        }
        WM_DESTROY => {
            WIN.with(|w| {
                if let Some(w) = w.borrow_mut().take() {
                    DeleteObject(w.font);
                }
            });
            0
        }
        _ => DefWindowProcW(hwnd, msg, wp, lp),
    }
}

/// Opens (or brings forward) the diagnostics window.
pub fn open(s: &State) {
    if let Some(h) = WIN.with(|w| w.borrow().as_ref().map(|w| w.hwnd)) {
        refresh(s);
        unsafe {
            ShowWindow(h, SW_SHOWNORMAL);
            SetForegroundWindow(h);
        }
        return;
    }
    unsafe {
        let hinst = GetModuleHandleW(null());
        let class = wide("FlintDiag");
        let wc = WNDCLASSEXW {
            cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
            lpfnWndProc: Some(proc),
            hInstance: hinst,
            hCursor: LoadCursorW(null_mut(), IDC_ARROW),
            hbrBackground: (COLOR_BTNFACE + 1) as usize as HBRUSH,
            lpszClassName: class.as_ptr(),
            ..std::mem::zeroed()
        };
        RegisterClassExW(&wc);
        let d = s.dpi.get() as i32;
        let title = wide("Flint diagnostics (redacted)");
        let hwnd = CreateWindowExW(0, class.as_ptr(), title.as_ptr(), WS_OVERLAPPEDWINDOW, CW_USEDEFAULT, CW_USEDEFAULT, 820 * d / 96, 640 * d / 96, null_mut(), null_mut(), hinst, null());
        let edit_cls = wide("EDIT");
        let text_box = CreateWindowExW(WS_EX_CLIENTEDGE, edit_cls.as_ptr(), null(), WS_CHILD | WS_VISIBLE | WS_VSCROLL | WS_HSCROLL | (ES_MULTILINE | ES_READONLY | ES_AUTOVSCROLL | ES_AUTOHSCROLL) as u32, 0, 0, 10, 10, hwnd, ID_TEXT as HMENU, hinst, null());
        // EDIT controls default to 30,000 characters.
        SendMessageW(text_box, EM_SETLIMITTEXT, 4 << 20, 0);
        let face = wide("Consolas");
        let font = CreateFontW(-(13 * d / 96), 0, 0, 0, FW_NORMAL as i32, 0, 0, 0, DEFAULT_CHARSET as u32, OUT_DEFAULT_PRECIS as u32, CLIP_DEFAULT_PRECIS as u32, CLEARTYPE_QUALITY as u32, (FIXED_PITCH | FF_MODERN) as u32, face.as_ptr());
        SendMessageW(text_box, WM_SETFONT, font as WPARAM, 1);
        let btn_cls = wide("BUTTON");
        let mut buttons = Vec::new();
        for (id, label) in BUTTONS {
            let l = wide(label);
            let b = CreateWindowExW(0, btn_cls.as_ptr(), l.as_ptr(), WS_CHILD | WS_VISIBLE | WS_TABSTOP | BS_PUSHBUTTON as u32, 0, 0, 10, 10, hwnd, id as HMENU, hinst, null());
            SendMessageW(b, WM_SETFONT, s.fonts.get().0 as WPARAM, 1);
            buttons.push(b);
        }
        let w = Win { hwnd, text_box, buttons, redact: s.redact_diagnostics, text: String::new(), font };
        layout(&w);
        WIN.with(|x| *x.borrow_mut() = Some(w));
        refresh(s);
        ShowWindow(hwnd, SW_SHOWNORMAL);
        SetForegroundWindow(hwnd);
    }
}
