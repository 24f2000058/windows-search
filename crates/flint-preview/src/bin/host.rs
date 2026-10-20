//! `flint-preview-host`: the isolated helper process (see the crate docs).
//! It is started by the launcher with piped stdin/stdout and is not meant
//! to be run by hand.

fn main() {
    #[cfg(windows)]
    flint_preview::win::harden_helper();
    #[cfg(windows)]
    let thumb: Option<flint_preview::backends::ThumbFn> = Some(flint_preview::win::shell_thumbnail);
    #[cfg(not(windows))]
    let thumb: Option<flint_preview::backends::ThumbFn> = None;
    let (stdin, stdout) = (std::io::stdin().lock(), std::io::stdout().lock());
    if let Err(e) = flint_preview::host::serve(stdin, stdout, thumb) {
        eprintln!("flint-preview-host: {e}");
        std::process::exit(2);
    }
}
