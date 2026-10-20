//! Windows specifics: helper hardening, shell thumbnails (run inside the
//! helper, because thumbnail handlers are third-party code), and the Job
//! Object that confines the helper.
//!
//! Status: compile-checked and run under Wine; not validated on Windows
//! hardware.

use crate::Thumbnail;
use std::os::windows::io::AsRawHandle;
use windows::core::PCWSTR;
use windows::Win32::Foundation::{CloseHandle, HANDLE, SIZE};
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::System::Com::{CoInitializeEx, COINIT_APARTMENTTHREADED};
use windows::Win32::System::Diagnostics::Debug::{SetErrorMode, SEM_FAILCRITICALERRORS, SEM_NOGPFAULTERRORBOX, SEM_NOOPENFILEERRORBOX};
use windows::Win32::System::JobObjects::*;
use windows::Win32::UI::Shell::*;

/// Called first in the helper: no crash or "insert disk" dialogs (a crash
/// must end the process quietly so the client can restart it), and COM
/// for the shell.
pub fn harden_helper() {
    unsafe {
        SetErrorMode(SEM_FAILCRITICALERRORS | SEM_NOGPFAULTERRORBOX | SEM_NOOPENFILEERRORBOX);
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
    }
}

/// Thumbnail from the shell's thumbnail cache/handlers (`IShellItemImageFactory`,
/// thumbnails only: never an icon), as top-down BGRA.
pub fn shell_thumbnail(path: &str, edge: u32) -> Option<Thumbnail> {
    unsafe {
        let w: Vec<u16> = path.encode_utf16().chain(std::iter::once(0)).collect();
        let f: IShellItemImageFactory = SHCreateItemFromParsingName(PCWSTR(w.as_ptr()), None).ok()?;
        let bmp = f.GetImage(SIZE { cx: edge as i32, cy: edge as i32 }, SIIGBF_THUMBNAILONLY | SIIGBF_BIGGERSIZEOK).ok()?;
        let mut info = BITMAP::default();
        let ok = GetObjectW(HGDIOBJ(bmp.0), std::mem::size_of::<BITMAP>() as i32, Some(&mut info as *mut _ as *mut _)) != 0;
        let (bw, bh) = (info.bmWidth.unsigned_abs(), info.bmHeight.unsigned_abs());
        // Tiny bitmaps are placeholders, not thumbnails (some shells return 1×1).
        let result = if ok && (bw >= 8 || bh >= 8) && bw <= 4096 && bh <= 4096 {
            let mut bi = BITMAPINFO { bmiHeader: BITMAPINFOHEADER { biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32, biWidth: bw as i32, biHeight: -(bh as i32), biPlanes: 1, biBitCount: 32, biCompression: BI_RGB.0, ..Default::default() }, ..Default::default() };
            let mut bgra = vec![0u8; (bw * bh * 4) as usize];
            let dc = GetDC(None);
            let lines = GetDIBits(dc, bmp, 0, bh, Some(bgra.as_mut_ptr() as *mut _), &mut bi, DIB_RGB_COLORS);
            ReleaseDC(None, dc);
            (lines == bh as i32).then_some(Thumbnail { width: bw, height: bh, bgra })
        } else {
            None
        };
        let _ = DeleteObject(HGDIOBJ(bmp.0));
        result
    }
}

/// A Job Object holding the helper: memory limit, no crash dialogs, killed
/// when the launcher closes the job (including if the launcher crashes).
pub struct Job(HANDLE);

// SAFETY: a job handle may be used and closed from any thread.
unsafe impl Send for Job {}

impl Job {
    pub fn confine(child: &std::process::Child, memory_limit: u64) -> Option<Job> {
        unsafe {
            let h = CreateJobObjectW(None, PCWSTR::null()).ok()?;
            let mut info = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
            info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE | JOB_OBJECT_LIMIT_DIE_ON_UNHANDLED_EXCEPTION;
            if memory_limit > 0 {
                info.BasicLimitInformation.LimitFlags |= JOB_OBJECT_LIMIT_PROCESS_MEMORY;
                info.ProcessMemoryLimit = memory_limit as usize;
            }
            let job = Job(h);
            SetInformationJobObject(h, JobObjectExtendedLimitInformation, &info as *const _ as *const _, std::mem::size_of_val(&info) as u32).ok()?;
            AssignProcessToJobObject(h, HANDLE(child.as_raw_handle())).ok()?;
            Some(job)
        }
    }
}

impl Drop for Job {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseHandle(self.0);
        }
    }
}
