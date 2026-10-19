//! File-type icons from the system image list, cached per extension.
//!
//! `SHGFI_USEFILEATTRIBUTES` resolves the icon from the *type* only, so no
//! file is opened and no per-file icon handler (third-party shell extension
//! code) runs against user files. Per-file icons (e.g. an .exe's own icon)
//! are therefore not shown; that is deliberate.

use std::collections::HashMap;
use windows_sys::Win32::Storage::FileSystem::{FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_NORMAL};
use windows_sys::Win32::System::Com::{CoInitializeEx, COINIT_APARTMENTTHREADED};
use windows_sys::Win32::UI::Controls::HIMAGELIST;
use windows_sys::Win32::UI::Shell::{SHGetFileInfoW, SHFILEINFOW, SHGFI_SMALLICON, SHGFI_SYSICONINDEX, SHGFI_USEFILEATTRIBUTES};

pub struct Icons {
    pub list: HIMAGELIST,
    by_ext: HashMap<String, i32>,
    dir: Option<i32>,
}

fn query(name: &str, attrs: u32) -> (HIMAGELIST, i32) {
    let w: Vec<u16> = name.encode_utf16().chain(std::iter::once(0)).collect();
    let mut fi: SHFILEINFOW = unsafe { std::mem::zeroed() };
    let h = unsafe { SHGetFileInfoW(w.as_ptr(), attrs, &mut fi, std::mem::size_of::<SHFILEINFOW>() as u32, SHGFI_SYSICONINDEX | SHGFI_SMALLICON | SHGFI_USEFILEATTRIBUTES) };
    (h as HIMAGELIST, if h == 0 { -1 } else { fi.iIcon })
}

impl Icons {
    /// Must be called on the UI thread (initializes COM there).
    pub fn new() -> Self {
        unsafe { CoInitializeEx(std::ptr::null(), COINIT_APARTMENTTHREADED as u32) };
        let (list, dir) = query("folder", FILE_ATTRIBUTE_DIRECTORY);
        Icons { list, by_ext: HashMap::new(), dir: (dir >= 0).then_some(dir) }
    }

    pub fn index(&mut self, name: &str, is_dir: bool) -> Option<i32> {
        if is_dir {
            return self.dir;
        }
        let ext = name.rsplit_once('.').map_or(String::new(), |(_, e)| e.to_ascii_lowercase());
        if ext.len() > 16 {
            return None;
        }
        let i = *self.by_ext.entry(ext.clone()).or_insert_with(|| query(&format!("x.{ext}"), FILE_ATTRIBUTE_NORMAL).1);
        (i >= 0).then_some(i)
    }
}
