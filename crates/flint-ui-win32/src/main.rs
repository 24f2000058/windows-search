//! Flint launcher — direct Win32 UI spike.

// Release builds (feature "gui") have no console window; development
// builds keep the console for timing output.
#![cfg_attr(all(windows, feature = "gui"), windows_subsystem = "windows")]
// Off Windows only the model (and its tests) are built.
#![cfg_attr(not(windows), allow(dead_code))]

#[cfg(windows)]
mod actions;
mod diag;
#[cfg(windows)]
mod diagview;
mod discovery;
#[cfg(windows)]
mod icons;
mod model;
#[cfg(windows)]
mod pane;
#[cfg(windows)]
mod plugins;
#[cfg(windows)]
mod prompt;
mod ux;
#[cfg(windows)]
mod resources;
#[cfg(windows)]
mod theme;
#[cfg(windows)]
mod updates;
#[cfg(windows)]
mod volumes;
#[cfg(windows)]
mod win;

fn main() {
    #[cfg(windows)]
    win::run();
    #[cfg(not(windows))]
    eprintln!("flint-ui-win32 runs on Windows; on other platforms only its model tests build");
}
