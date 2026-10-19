//! Light/dark/high-contrast palette selection (Windows).

use crate::model::{palette, Palette};
use flint_config::Theme;
use windows_sys::Win32::Graphics::Gdi::*;
use windows_sys::Win32::System::Registry::{RegGetValueW, HKEY_CURRENT_USER, RRF_RT_REG_DWORD};
use windows_sys::Win32::UI::Accessibility::{HCF_HIGHCONTRASTON, HIGHCONTRASTW};
use windows_sys::Win32::UI::WindowsAndMessaging::{SystemParametersInfoW, SPI_GETHIGHCONTRAST};

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// `AppsUseLightTheme == 0` in the user's personalization settings.
pub fn system_prefers_dark() -> bool {
    let key = wide("Software\\Microsoft\\Windows\\CurrentVersion\\Themes\\Personalize");
    let val = wide("AppsUseLightTheme");
    let (mut data, mut size) = (1u32, 4u32);
    let r = unsafe { RegGetValueW(HKEY_CURRENT_USER, key.as_ptr(), val.as_ptr(), RRF_RT_REG_DWORD, std::ptr::null_mut(), &mut data as *mut u32 as *mut _, &mut size) };
    r == 0 && data == 0
}

pub fn high_contrast() -> bool {
    let mut hc: HIGHCONTRASTW = unsafe { std::mem::zeroed() };
    hc.cbSize = std::mem::size_of::<HIGHCONTRASTW>() as u32;
    let ok = unsafe { SystemParametersInfoW(SPI_GETHIGHCONTRAST, hc.cbSize, &mut hc as *mut _ as *mut _, 0) };
    ok != 0 && hc.dwFlags & HCF_HIGHCONTRASTON != 0
}

/// High contrast always wins and uses the user's system colors.
pub fn current(setting: Theme) -> Palette {
    if high_contrast() {
        unsafe {
            return Palette {
                bg: GetSysColor(COLOR_WINDOW),
                edit_bg: GetSysColor(COLOR_WINDOW),
                text: GetSysColor(COLOR_WINDOWTEXT),
                dim: GetSysColor(COLOR_GRAYTEXT),
                sel_bg: GetSysColor(COLOR_HIGHLIGHT),
                sel_text: GetSysColor(COLOR_HIGHLIGHTTEXT),
                accent: GetSysColor(COLOR_HOTLIGHT),
            };
        }
    }
    palette(match setting {
        Theme::Dark => true,
        Theme::Light => false,
        Theme::System => system_prefers_dark(),
    })
}
