//! Prints what the preview helper returns for each file given, through the
//! real helper process (`flint-preview-host` next to this executable):
//! `preview_probe.exe FILE...`. For diagnosing previews on a given machine.

use flint_preview::client::{HostClient, HostConfig};
use flint_preview::{Limits, Preview};

fn main() {
    let exe = std::env::current_exe().expect("exe path");
    let host = exe.with_file_name(if cfg!(windows) { "flint-preview-host.exe" } else { "flint-preview-host" });
    let mut c = HostClient::new(HostConfig::new(host));
    for path in std::env::args().skip(1) {
        let t = std::time::Instant::now();
        let r = c.preview(&path, &Limits::default());
        let ms = t.elapsed().as_secs_f64() * 1e3;
        match r {
            Ok(Preview::Image { format, width, height, thumbnail }) => println!("{path}: {format} {width}x{height}, thumbnail {:?} ({ms:.1} ms)", thumbnail.map(|t| (t.width, t.height, t.bgra.len()))),
            Ok(Preview::Text { text, encoding, truncated }) => println!("{path}: text {encoding}, {} chars, truncated {truncated} ({ms:.1} ms)", text.chars().count()),
            Ok(Preview::Archive { total_entries, warnings, .. }) => println!("{path}: archive, {total_entries} entries, warnings {warnings:?} ({ms:.1} ms)"),
            Ok(other) => println!("{path}: {other:?} ({ms:.1} ms)"),
            Err(e) => println!("{path}: error: {e} ({ms:.1} ms)"),
        }
    }
    println!("helper: {:?}", c.stats);
}
