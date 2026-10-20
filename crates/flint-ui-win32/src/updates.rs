//! Update checks in the launcher (ADR-020).
//!
//! `/update` checks now. In `notify` mode a check also runs at most every
//! `check_interval_hours` and only puts a line in the status strip. Every
//! step that downloads or installs asks first. Nothing runs unless it
//! matched the signed release information.

use crate::win::{Modal, State, WM_APP_UPDATE};
use flint_config::UpdateMode;
use flint_update::{AssetKind, CheckState, Decision, Manifest};
use std::path::PathBuf;
use windows_sys::Win32::Foundation::*;
use windows_sys::Win32::UI::WindowsAndMessaging::*;

pub const RELEASES_PAGE: &str = "https://github.com/24f2000058/windows-search/releases";
pub const CURRENT: &str = env!("CARGO_PKG_VERSION");

pub enum UpdateEvent {
    Checked { interactive: bool, result: Result<Decision, String> },
    Downloaded { version: String, kind: AssetKind, result: Result<PathBuf, String> },
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

fn post(hwnd: usize, ev: UpdateEvent) {
    let p = Box::into_raw(Box::new(ev));
    unsafe {
        if PostMessageW(hwnd as HWND, WM_APP_UPDATE, 0, p as LPARAM) == 0 {
            drop(Box::from_raw(p));
        }
    }
}

fn ask(s: &State, text: &str, flags: u32) -> i32 {
    if !s.selftest.is_empty() {
        // --selftest: print the dialog and answer yes, so runs never block.
        println!("dialog: {}", text.replace('\n', " | "));
        return IDYES;
    }
    let _m = Modal::new(s);
    let (t, c) = (wide(text), wide("Flint update"));
    unsafe { MessageBoxW(s.hwnd, t.as_ptr(), c.as_ptr(), flags | MB_TOPMOST) }
}

fn open_url(url: &str) {
    if flint_commands::websearch::is_safe_url(url) {
        let (op, u) = (wide("open"), wide(url));
        unsafe { windows_sys::Win32::UI::Shell::ShellExecuteW(std::ptr::null_mut(), op.as_ptr(), u.as_ptr(), std::ptr::null(), std::ptr::null(), SW_SHOWNORMAL) };
    }
}

fn now() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs() as i64)
}

/// Starts a check off the UI thread.
pub fn check(s: &State, interactive: bool) {
    let cfg = &s.updates_cfg;
    if cfg.mode == UpdateMode::Off {
        if interactive {
            s.say("update checks are off (updates.mode = \"off\" in config.toml)");
        }
        return;
    }
    if flint_update::trusted_keys().is_empty() {
        *s.update_status.borrow_mut() = "this build has no update-signing key; updates cannot be verified".into();
        if interactive && ask(s, &format!("This build of Flint {CURRENT} has no update-signing key, so it cannot verify or install updates.\n\nOpen the releases page to check manually?"), MB_YESNO | MB_ICONINFORMATION) == IDYES {
            open_url(RELEASES_PAGE);
            s.leave();
        }
        return;
    }
    let (url, hwnd) = (cfg.manifest_url.clone(), s.hwnd as usize);
    *s.update_status.borrow_mut() = "checking…".into();
    if interactive {
        s.say("checking for updates…");
    }
    std::thread::Builder::new()
        .name("flint-update".into())
        .spawn(move || {
            let result = flint_update::check(&flint_update::win::WinHttp::default(), &url, CURRENT, &flint_update::trusted_keys());
            post(hwnd, UpdateEvent::Checked { interactive, result });
        })
        .ok();
}

/// Notify mode: called from a timer; checks if the interval has passed.
pub fn tick(s: &State) {
    if s.updates_cfg.mode != UpdateMode::Notify {
        return;
    }
    let path = s.data_dir.join("update-state.toml");
    let mut st = CheckState::load(&path);
    if st.due(now(), s.updates_cfg.check_interval_hours) {
        st.last_check = now();
        let _ = st.save(&path);
        check(s, false);
    }
}

fn pick(m: &Manifest, portable: bool) -> Option<flint_update::Asset> {
    let want = if portable { AssetKind::Portable } else { AssetKind::Msi };
    m.assets.iter().find(|a| a.kind == want).cloned()
}

pub fn on_event(s: &State, ev: UpdateEvent) {
    match ev {
        UpdateEvent::Checked { interactive, result } => match result {
            Ok(Decision::UpToDate { latest }) => {
                *s.update_status.borrow_mut() = format!("up to date ({CURRENT}; latest release {latest})");
                if interactive {
                    s.say(&format!("Flint is up to date ({CURRENT})"));
                }
            }
            Ok(Decision::Available(m)) => {
                *s.update_status.borrow_mut() = format!("Flint {} is available (you have {CURRENT})", m.version);
                if !interactive {
                    *s.notice.borrow_mut() = format!("Flint {} is available — type /update", m.version);
                    unsafe { windows_sys::Win32::Graphics::Gdi::InvalidateRect(s.hwnd, std::ptr::null(), 0) };
                    return;
                }
                offer(s, m);
            }
            Err(e) => {
                *s.update_status.borrow_mut() = format!("last check failed: {e}");
                if interactive {
                    s.say(&format!("update check failed: {e}"));
                }
            }
        },
        UpdateEvent::Downloaded { version, kind, result } => match result {
            Err(e) => {
                ask(s, &format!("The download of Flint {version} was rejected and deleted:\n{e}\n\nNothing was installed."), MB_OK | MB_ICONWARNING);
            }
            Ok(path) if kind == AssetKind::Msi => {
                let q = format!("Flint {version} was downloaded and verified (signature and SHA-256 match).\n\nInstall it now? Flint will close and the Windows installer will start.");
                if ask(s, &q, MB_YESNO | MB_ICONQUESTION) == IDYES {
                    let (op, exe, args) = (wide("open"), wide("msiexec.exe"), wide(&format!("/i \"{}\"", path.display())));
                    let r = unsafe { windows_sys::Win32::UI::Shell::ShellExecuteW(std::ptr::null_mut(), op.as_ptr(), exe.as_ptr(), args.as_ptr(), std::ptr::null(), SW_SHOWNORMAL) };
                    if r as isize > 32 {
                        unsafe { PostMessageW(s.hwnd, WM_CLOSE, 0, 0) };
                    } else {
                        s.say("could not start the installer");
                    }
                }
            }
            Ok(path) => {
                ask(s, &format!("Flint {version} was downloaded and verified:\n{}\n\nThis is a portable copy: close Flint and extract the archive over your Flint folder (your data folder is kept).", path.display()), MB_OK | MB_ICONINFORMATION);
                let _ = flint_ops::win::reveal(&[path.display().to_string()]);
            }
        },
    }
}

fn offer(s: &State, m: Manifest) {
    let Some(asset) = pick(&m, s.portable) else {
        if ask(s, &format!("Flint {} is available (you have {CURRENT}), but not as a {} download.\n\nOpen the release notes?", m.version, if s.portable { "portable" } else { "installer" }), MB_YESNO | MB_ICONINFORMATION) == IDYES {
            open_url(&m.notes_url);
        }
        return;
    };
    let q = format!(
        "Flint {} is available (you have {CURRENT}), released {}.\nRelease notes: {}\n\nDownload it ({:.1} MB) and verify it? Nothing is installed until you confirm again.",
        m.version,
        if m.released.is_empty() { "recently" } else { &m.released },
        m.notes_url,
        asset.size as f64 / 1e6
    );
    if ask(s, &q, MB_YESNO | MB_ICONQUESTION) == IDYES {
        let (dir, hwnd, version, kind) = (s.data_dir.join("updates"), s.hwnd as usize, m.version.clone(), asset.kind);
        s.say(&format!("downloading Flint {version}…"));
        std::thread::spawn(move || {
            let result = flint_update::download(&flint_update::win::WinHttp::default(), &asset, &dir);
            post(hwnd, UpdateEvent::Downloaded { version, kind, result });
        });
    }
}
