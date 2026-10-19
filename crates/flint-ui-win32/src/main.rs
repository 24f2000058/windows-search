//! Flint launcher — direct Win32 UI spike.

mod model;
#[cfg(windows)]
mod win;

fn main() {
    #[cfg(windows)]
    win::run();
    #[cfg(not(windows))]
    eprintln!("flint-ui-win32 runs on Windows; on other platforms only its model tests build");
}
