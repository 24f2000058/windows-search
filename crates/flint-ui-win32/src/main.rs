//! Flint launcher — direct Win32 UI spike.

// Off Windows only the model (and its tests) are built.
#![cfg_attr(not(windows), allow(dead_code))]

mod discovery;
#[cfg(windows)]
mod icons;
mod model;
#[cfg(windows)]
mod theme;
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
