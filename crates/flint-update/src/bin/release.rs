//! `flint-release`: maintainer tool for update signing.
//!
//! ```text
//! flint-release keygen <secret-key-file> [--id ID]
//!     New ed25519 key. Writes the secret (hex) to the file, which must not
//!     exist, and prints the line to add to crates/flint-update/update-keys.txt.
//! flint-release manifest --version V --notes-url URL [--released YYYY-MM-DD]
//!                        [--msi FILE URL] [--portable FILE URL]
//!     Prints update.json for the given release files.
//! flint-release sign <file> --id ID (--key-file F | env FLINT_UPDATE_SIGNING_KEY)
//!     Writes <file>.sig.
//! flint-release verify <file> [--keys update-keys.txt]
//!     Checks <file>.sig against the given keys (default: this build's).
//! ```

use ed25519_dalek::{Signer, SigningKey};
use flint_update::{hex, parse_keys, signed_message, trusted_keys, unhex, verify_manifest, Asset, AssetKind, Manifest};
use sha2::{Digest, Sha256};
use std::process::exit;

fn die(msg: &str) -> ! {
    eprintln!("flint-release: {msg}");
    exit(2)
}

fn flag(args: &[String], name: &str) -> Option<String> {
    args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).cloned()
}

fn flag2(args: &[String], name: &str) -> Option<(String, String)> {
    let i = args.iter().position(|a| a == name)?;
    Some((args.get(i + 1)?.clone(), args.get(i + 2)?.clone()))
}

fn secret(args: &[String]) -> SigningKey {
    let text = match flag(args, "--key-file") {
        Some(f) => std::fs::read_to_string(&f).unwrap_or_else(|e| die(&format!("{f}: {e}"))),
        None => std::env::var("FLINT_UPDATE_SIGNING_KEY").unwrap_or_else(|_| die("give --key-file or set FLINT_UPDATE_SIGNING_KEY")),
    };
    let b: [u8; 32] = unhex(text.trim()).and_then(|v| v.try_into().ok()).unwrap_or_else(|| die("secret key must be 64 hex characters"));
    SigningKey::from_bytes(&b)
}

fn asset(kind: AssetKind, file: &str, url: &str) -> Asset {
    let data = std::fs::read(file).unwrap_or_else(|e| die(&format!("{file}: {e}")));
    Asset { kind, url: url.to_owned(), size: data.len() as u64, sha256: hex(&Sha256::digest(&data)) }
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("keygen") => {
            let out = args.get(1).unwrap_or_else(|| die("keygen <secret-key-file>"));
            let id = flag(&args, "--id").unwrap_or_else(|| "release-1".into());
            let mut seed = [0u8; 32];
            getrandom::fill(&mut seed).unwrap_or_else(|e| die(&e.to_string()));
            let sk = SigningKey::from_bytes(&seed);
            let mut f = std::fs::OpenOptions::new().write(true).create_new(true).open(out).unwrap_or_else(|e| die(&format!("{out}: {e}")));
            std::io::Write::write_all(&mut f, hex(&seed).as_bytes()).unwrap_or_else(|e| die(&e.to_string()));
            println!("{id} {}", hex(&sk.verifying_key().to_bytes()));
            eprintln!("secret key written to {out}; keep it offline or in the release secret store");
        }
        Some("manifest") => {
            let version = flag(&args, "--version").unwrap_or_else(|| die("--version required"));
            let notes_url = flag(&args, "--notes-url").unwrap_or_else(|| die("--notes-url required"));
            let mut assets = Vec::new();
            if let Some((f, u)) = flag2(&args, "--msi") {
                assets.push(asset(AssetKind::Msi, &f, &u));
            }
            if let Some((f, u)) = flag2(&args, "--portable") {
                assets.push(asset(AssetKind::Portable, &f, &u));
            }
            let m = Manifest { product: "flint".into(), version, released: flag(&args, "--released").unwrap_or_default(), notes_url, assets };
            println!("{}", serde_json::to_string_pretty(&m).unwrap());
        }
        Some("sign") => {
            let file = args.get(1).unwrap_or_else(|| die("sign <file>"));
            let id = flag(&args, "--id").unwrap_or_else(|| die("--id required"));
            let data = std::fs::read(file).unwrap_or_else(|e| die(&format!("{file}: {e}")));
            let sig = secret(&args).sign(&signed_message(&data));
            std::fs::write(format!("{file}.sig"), format!("{id} {}\n", hex(&sig.to_bytes()))).unwrap_or_else(|e| die(&e.to_string()));
        }
        Some("verify") => {
            let file = args.get(1).unwrap_or_else(|| die("verify <file>"));
            let keys = match flag(&args, "--keys") {
                Some(k) => parse_keys(&std::fs::read_to_string(&k).unwrap_or_else(|e| die(&e.to_string()))).unwrap_or_else(|e| die(&e)),
                None => trusted_keys(),
            };
            let data = std::fs::read(file).unwrap_or_else(|e| die(&format!("{file}: {e}")));
            let sig = std::fs::read_to_string(format!("{file}.sig")).unwrap_or_else(|e| die(&format!("{file}.sig: {e}")));
            match verify_manifest(&data, &sig, &keys) {
                Ok(m) => println!("valid: {} {} ({} assets)", m.product, m.version, m.assets.len()),
                Err(e) => die(&e.to_string()),
            }
        }
        _ => die("usage: flint-release keygen|manifest|sign|verify (see source for options)"),
    }
}
