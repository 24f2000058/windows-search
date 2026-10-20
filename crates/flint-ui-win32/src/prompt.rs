//! Small modal text prompt (rename, new folder) built from plain controls,
//! with its own message loop. `IsDialogMessageW` provides Tab, Enter
//! (default button) and Esc (Cancel) handling and keyboard accessibility.

use std::cell::RefCell;
use std::ptr::{null, null_mut};
use windows_sys::Win32::Foundation::*;
use windows_sys::Win32::Graphics::Gdi::*;
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::UI::Controls::EM_SETSEL;
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{EnableWindow, SetFocus};
use windows_sys::Win32::UI::WindowsAndMessaging::*;

const ID_OK: isize = 1; // IDOK: Enter activates it
const ID_CANCEL: isize = 2; // IDCANCEL: Esc activates it
const ID_EDIT: isize = 10;

#[derive(Default)]
struct PromptState {
    edit: HWND,
    done: bool,
    result: Option<String>,
}

thread_local! {
    static PROMPT: RefCell<PromptState> = RefCell::new(PromptState::default());
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

unsafe extern "system" fn proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match msg {
        WM_COMMAND => {
            let id = (wp & 0xFFFF) as isize;
            if id == ID_OK || id == ID_CANCEL {
                let text = (id == ID_OK).then(|| {
                    let edit = PROMPT.with(|p| p.borrow().edit);
                    let mut buf = vec![0u16; 1024];
                    let n = GetWindowTextW(edit, buf.as_mut_ptr(), buf.len() as i32) as usize;
                    String::from_utf16_lossy(&buf[..n])
                });
                PROMPT.with(|p| {
                    let mut p = p.borrow_mut();
                    p.result = text;
                    p.done = true;
                });
            }
            0
        }
        WM_CLOSE => {
            PROMPT.with(|p| p.borrow_mut().done = true);
            0
        }
        WM_CTLCOLORSTATIC | WM_CTLCOLORDLG => GetSysColorBrush(COLOR_WINDOW) as LRESULT,
        _ => DefWindowProcW(hwnd, msg, wp, lp),
    }
}

/// Shows the prompt centred over `owner`; returns the text, or None if
/// cancelled. `select_stem` preselects the name without its extension, as
/// Explorer does when renaming.
pub fn prompt(owner: HWND, title: &str, label: &str, initial: &str, select_stem: bool, font: HFONT, dpi: u32) -> Option<String> {
    let s = |v: i32| v * dpi as i32 / 96;
    unsafe {
        let hinst = GetModuleHandleW(null());
        let class = wide("FlintPrompt");
        let wc = WNDCLASSEXW {
            cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
            style: 0,
            lpfnWndProc: Some(proc),
            cbClsExtra: 0,
            cbWndExtra: 0,
            hInstance: hinst,
            hIcon: null_mut(),
            hCursor: LoadCursorW(null_mut(), IDC_ARROW),
            hbrBackground: GetSysColorBrush(COLOR_WINDOW),
            lpszMenuName: null(),
            lpszClassName: class.as_ptr(),
            hIconSm: null_mut(),
        };
        RegisterClassExW(&wc); // fails harmlessly when already registered
        let mut orc: RECT = std::mem::zeroed();
        GetWindowRect(owner, &mut orc);
        let (w, h) = (s(420), s(150));
        let x = orc.left + ((orc.right - orc.left) - w) / 2;
        let y = orc.top + s(60);
        let t = wide(title);
        let hwnd = CreateWindowExW(WS_EX_DLGMODALFRAME | WS_EX_TOPMOST, class.as_ptr(), t.as_ptr(), WS_POPUP | WS_CAPTION | WS_SYSMENU, x, y, w, h, owner, null_mut(), hinst, null());
        if hwnd.is_null() {
            return None;
        }
        let mk = |cls: &str, text: &str, style: u32, ex: u32, id: isize, r: (i32, i32, i32, i32)| {
            let (c, tx) = (wide(cls), wide(text));
            let ctl = CreateWindowExW(ex, c.as_ptr(), tx.as_ptr(), WS_CHILD | WS_VISIBLE | style, r.0, r.1, r.2, r.3, hwnd, id as HMENU, hinst, null());
            SendMessageW(ctl, WM_SETFONT, font as WPARAM, 1);
            ctl
        };
        mk("STATIC", label, 0, 0, 0, (s(12), s(10), s(390), s(20)));
        let edit = mk("EDIT", initial, WS_TABSTOP | WS_BORDER | ES_AUTOHSCROLL as u32, 0, ID_EDIT, (s(12), s(34), s(390), s(26)));
        mk("BUTTON", "OK", WS_TABSTOP | BS_DEFPUSHBUTTON as u32, 0, ID_OK, (s(222), s(74), s(86), s(28)));
        mk("BUTTON", "Cancel", WS_TABSTOP, 0, ID_CANCEL, (s(316), s(74), s(86), s(28)));
        PROMPT.with(|p| *p.borrow_mut() = PromptState { edit, done: false, result: None });
        let sel_end = if select_stem { initial.rfind('.').filter(|&i| i > 0).map_or(initial.encode_utf16().count(), |i| initial[..i].encode_utf16().count()) } else { initial.encode_utf16().count() };
        SendMessageW(edit, EM_SETSEL, 0, sel_end as LPARAM);
        EnableWindow(owner, 0);
        ShowWindow(hwnd, SW_SHOW);
        SetForegroundWindow(hwnd);
        SetFocus(edit);
        let mut msg: MSG = std::mem::zeroed();
        while !PROMPT.with(|p| p.borrow().done) {
            if GetMessageW(&mut msg, null_mut(), 0, 0) <= 0 {
                // WM_QUIT: re-post it for the main loop and give up.
                PostQuitMessage(msg.wParam as i32);
                break;
            }
            if IsDialogMessageW(hwnd, &msg) == 0 {
                TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }
        // Re-enable the owner before destroying, so activation returns to it.
        EnableWindow(owner, 1);
        DestroyWindow(hwnd);
        SetForegroundWindow(owner);
        PROMPT.with(|p| p.borrow_mut().result.take())
    }
}
