//! Viewer pane (Windows): shows the preview of the focused result.
//!
//! Off by default (`viewer.enabled`; F9 toggles it for the session). While
//! off, and whenever the launcher is hidden, no viewer thread or helper
//! process exists, so no file is read. Requests carry a selection
//! generation; only the latest result is shown (`flint-preview::scheduler`).

use crate::win::{State, WM_APP_PREVIEW};
use flint_fuzzy::table::flags;
use flint_preview::client::{HostClient, HostConfig};
use flint_preview::scheduler::{Delivered, FileKey, Viewer, ViewerOptions};
use flint_preview::{Limits, Preview};
use std::ptr::null_mut;
use std::time::Duration;
use windows_sys::Win32::Foundation::*;
use windows_sys::Win32::Graphics::Gdi::*;
use windows_sys::Win32::UI::WindowsAndMessaging::*;

pub const PANE_W: i32 = 380;

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Starts the viewer thread (the helper itself starts on the first request).
pub fn start(s: &State) {
    if s.viewer.borrow().is_some() {
        return;
    }
    let v = &s.viewer_cfg;
    let mut cfg = HostConfig::new(s.host_exe.clone());
    cfg.timeout = Duration::from_millis(v.timeout_ms.max(200));
    cfg.memory_limit = v.helper_memory_mb << 20;
    let limits = Limits { max_read: v.max_file_mb.max(1) << 20, thumb: if v.thumbnails { 256 } else { 0 }, ..Default::default() };
    let hwnd = s.hwnd as usize;
    let viewer = Viewer::new(HostClient::new(cfg), ViewerOptions { limits, ..Default::default() }, move |g, r: Delivered| {
        let p = Box::into_raw(Box::new((g, r)));
        unsafe {
            if PostMessageW(hwnd as HWND, WM_APP_PREVIEW, 0, p as LPARAM) == 0 {
                drop(Box::from_raw(p));
            }
        }
    });
    *s.viewer.borrow_mut() = Some(viewer);
    s.last_preview.borrow_mut().take();
}

/// Stops the viewer thread and the helper process.
pub fn stop(s: &State) {
    // Dropping joins the thread, which drops the client and kills the helper.
    let v = s.viewer.borrow_mut().take();
    drop(v);
    s.last_preview.borrow_mut().take();
}

pub fn set_bitmap(s: &State, bmp: HBITMAP, w: i32, h: i32) {
    let (old, _, _) = s.pane_bitmap.replace((bmp, w, h));
    if !old.is_null() {
        unsafe { DeleteObject(old) };
    }
}

fn show_text(s: &State, header: &str, body: &str) {
    *s.pane_header.borrow_mut() = header.to_owned();
    // EDIT controls need CRLF line breaks.
    let body = body.replace("\r\n", "\n").replace('\n', "\r\n");
    let w = wide(&body);
    unsafe {
        SetWindowTextW(s.pane_edit, w.as_ptr());
        crate::win::layout(s);
        InvalidateRect(s.hwnd, null_mut(), 0);
    }
}

/// The focused result changed: request its preview.
pub fn selection_changed(s: &State) {
    if !s.viewer_on.get() || s.viewer.borrow().is_none() {
        return;
    }
    let id = s.model.borrow().selected_id();
    let Some(id) = id else {
        set_bitmap(s, null_mut(), 0, 0);
        show_text(s, "", "");
        return;
    };
    let (key, is_dir) = {
        let t = s.table.read().unwrap();
        if !t.is_live(id) {
            return;
        }
        let m = t.meta(id);
        (FileKey { path: t.path(id), size: m.size, mtime: m.mtime, file_id: t.key(id) }, m.flags & flags::DIR != 0)
    };
    if s.last_preview.borrow().as_ref() == Some(&key) {
        return;
    }
    *s.last_preview.borrow_mut() = Some(key.clone());
    let g = s.preview_gen.get() + 1;
    s.preview_gen.set(g);
    if is_dir {
        // Folders are described from the index; nothing is read.
        if let Some(v) = s.viewer.borrow().as_ref() {
            v.clear(g);
        }
        set_bitmap(s, null_mut(), 0, 0);
        show_text(s, "Folder", &key.path);
        return;
    }
    *s.pane_header.borrow_mut() = format!("{} …", key.path.rsplit('\\').next().unwrap_or(&key.path));
    unsafe { InvalidateRect(s.hwnd, null_mut(), 0) };
    if let Some(v) = s.viewer.borrow().as_ref() {
        v.show(g, key);
    }
}

/// A preview arrived (on the UI thread).
pub fn on_result(s: &State, g: u64, r: Delivered) {
    if g != s.preview_gen.get() || !s.viewer_on.get() {
        return; // the selection moved on
    }
    match r {
        Ok(p) => {
            let (header, body) = crate::ux::preview_text(&p);
            let bmp = match &*p {
                Preview::Image { thumbnail: Some(t), .. } => unsafe { bitmap_from_bgra(t.width as i32, t.height as i32, &t.bgra) },
                _ => null_mut(),
            };
            let (w, h) = match &*p {
                Preview::Image { thumbnail: Some(t), .. } if !bmp.is_null() => (t.width as i32, t.height as i32),
                _ => (0, 0),
            };
            set_bitmap(s, bmp, w, h);
            show_text(s, &header, &body);
        }
        Err(e) => {
            set_bitmap(s, null_mut(), 0, 0);
            show_text(s, "No preview", &e);
        }
    }
}

unsafe fn bitmap_from_bgra(w: i32, h: i32, bgra: &[u8]) -> HBITMAP {
    if w <= 0 || h <= 0 || bgra.len() != (w * h * 4) as usize {
        return null_mut();
    }
    let mut bi: BITMAPINFO = std::mem::zeroed();
    bi.bmiHeader.biSize = std::mem::size_of::<BITMAPINFOHEADER>() as u32;
    bi.bmiHeader.biWidth = w;
    bi.bmiHeader.biHeight = -h; // top-down
    bi.bmiHeader.biPlanes = 1;
    bi.bmiHeader.biBitCount = 32;
    bi.bmiHeader.biCompression = BI_RGB;
    let mut bits = null_mut();
    let bmp = CreateDIBSection(null_mut(), &bi, DIB_RGB_COLORS, &mut bits, null_mut(), 0);
    if !bmp.is_null() && !bits.is_null() {
        std::ptr::copy_nonoverlapping(bgra.as_ptr(), bits as *mut u8, bgra.len());
    }
    bmp
}

/// Paints the pane header and thumbnail into the parent's client area.
pub unsafe fn paint(s: &State, hdc: HDC, pane: RECT) {
    let p = s.palette.get();
    let d = s.dpi.get();
    let bg = CreateSolidBrush(p.edit_bg);
    FillRect(hdc, &pane, bg);
    DeleteObject(bg);
    let mut rc = RECT { left: pane.left + d as i32 * 6 / 96, top: pane.top, right: pane.right - d as i32 * 6 / 96, bottom: pane.top + d as i32 * 24 / 96 };
    let header = s.pane_header.borrow().clone();
    let w: Vec<u16> = header.encode_utf16().collect();
    if !w.is_empty() {
        SelectObject(hdc, s.fonts.get().0);
        SetBkMode(hdc, TRANSPARENT as i32);
        SetTextColor(hdc, p.dim);
        DrawTextW(hdc, w.as_ptr(), w.len() as i32, &mut rc, DT_SINGLELINE | DT_VCENTER | DT_NOPREFIX | DT_END_ELLIPSIS);
    }
    let (bmp, bw, bh) = s.pane_bitmap.get();
    if !bmp.is_null() {
        let mem = CreateCompatibleDC(hdc);
        let old = SelectObject(mem, bmp);
        let x = pane.left + ((pane.right - pane.left) - bw).max(0) / 2;
        BitBlt(hdc, x, rc.bottom, bw.min(pane.right - pane.left), bh, mem, 0, 0, SRCCOPY);
        SelectObject(mem, old);
        DeleteDC(mem);
    }
}

/// Height taken above the text body (header + thumbnail).
pub fn top_height(s: &State) -> i32 {
    let d = s.dpi.get() as i32;
    d * 24 / 96 + s.pane_bitmap.get().2
}
