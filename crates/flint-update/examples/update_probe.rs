//! Manual probe: `update_probe <manifest-url> <current-version> <keys-file> <download-dir>`
//! checks for an update through the platform fetcher and, if one is
//! offered, downloads and verifies its first asset.

#[cfg(not(windows))]
fn main() {
    eprintln!("the probe uses WinHTTP; run it on Windows");
    std::process::exit(2);
}

#[cfg(windows)]
fn main() {
    let a: Vec<String> = std::env::args().skip(1).collect();
    if a.len() < 4 {
        eprintln!("usage: update_probe <manifest-url> <current-version> <keys-file> <download-dir>");
        std::process::exit(2);
    }
    let fetch = flint_update::win::WinHttp::default();
    let keys = flint_update::parse_keys(&std::fs::read_to_string(&a[2]).unwrap()).unwrap();
    match flint_update::check(&fetch, &a[0], &a[1], &keys) {
        Ok(flint_update::Decision::Available(m)) => {
            println!("available: {} ({} assets)", m.version, m.assets.len());
            if let Some(asset) = m.assets.first() {
                match flint_update::download(&fetch, asset, std::path::Path::new(&a[3])) {
                    Ok(p) => println!("downloaded and verified: {}", p.display()),
                    Err(e) => println!("download rejected: {e}"),
                }
            }
        }
        Ok(flint_update::Decision::UpToDate { latest }) => println!("up to date (latest {latest})"),
        Err(e) => println!("check failed: {e}"),
    }
}
