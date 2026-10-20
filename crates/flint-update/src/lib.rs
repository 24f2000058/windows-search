//! Update checks (ADR-020).
//!
//! * **Nothing automatic.** `updates.mode` is `manual` by default: the
//!   network is contacted only when the user types `/update`. In `notify`
//!   mode a check runs at most once per interval and only *tells* the user.
//!   Downloading and installing always need explicit approval, and nothing
//!   is ever installed silently.
//! * **Signed manifest.** A release publishes `update.json` and
//!   `update.json.sig`. The signature is ed25519 over a domain-separated
//!   message, made with a key whose public half is compiled into the build
//!   (`update-keys.txt`). An unsigned, badly signed or unknown-key manifest
//!   is rejected.
//! * **Verified download.** The manifest names each asset's size and
//!   SHA-256. The download is bounded by that size and must match the
//!   digest before anything is offered to run.
//! * **No rollback.** Only a strictly newer version is offered, and only
//!   for this product. Pre-releases are offered only to pre-release builds.
//!
//! A build without a trusted key can still look for a newer version and
//! link to the release page, but it will not download or install.

use ed25519_dalek::{Signature, VerifyingKey};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::path::Path;

#[cfg(windows)]
pub mod win;

pub const PRODUCT: &str = "flint";
/// Prefix of every signed message, so a signature over anything else
/// (another file, another protocol) is never valid here.
pub const SIGNING_CONTEXT: &[u8] = b"flint-update-manifest-v1\n";
const MAX_MANIFEST: usize = 64 << 10;
pub const MAX_ASSET: u64 = 512 << 20;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Version {
    pub major: u64,
    pub minor: u64,
    pub patch: u64,
    /// Pre-release tag present (sorts before the release).
    pub pre: Option<PreTag>,
}

/// Ordered so that `None` (a release) sorts above any pre-release.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct PreTag(pub u64);

impl Version {
    /// `1.2.3` or `1.2.3-beta.4` / `1.2.3-rc4` (the number after the
    /// letters orders pre-releases). A leading `v` is allowed.
    pub fn parse(s: &str) -> Option<Version> {
        let s = s.trim().trim_start_matches('v');
        let (core, pre) = match s.split_once('-') {
            Some((c, p)) => (c, Some(p)),
            None => (s, None),
        };
        let mut it = core.split('.').map(|p| if p.is_empty() || p.len() > 9 || !p.bytes().all(|b| b.is_ascii_digit()) { None } else { p.parse().ok() });
        let v = (it.next()??, it.next()??, it.next()??);
        if it.next().is_some() {
            return None;
        }
        let pre = match pre {
            None => None,
            Some(p) if !p.is_empty() && p.len() <= 32 && p.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'.') => {
                let digits: String = p.chars().rev().take_while(|c| c.is_ascii_digit()).collect::<Vec<_>>().into_iter().rev().collect();
                Some(PreTag(digits.parse().unwrap_or(0)))
            }
            Some(_) => return None,
        };
        Some(Version { major: v.0, minor: v.1, patch: v.2, pre })
    }

    fn key(&self) -> (u64, u64, u64, bool, u64) {
        (self.major, self.minor, self.patch, self.pre.is_none(), self.pre.map_or(0, |p| p.0))
    }

    pub fn newer_than(&self, other: &Version) -> bool {
        self.key() > other.key()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AssetKind {
    Msi,
    Portable,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Asset {
    pub kind: AssetKind,
    pub url: String,
    pub size: u64,
    pub sha256: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    pub product: String,
    pub version: String,
    /// ISO date, informational.
    pub released: String,
    pub notes_url: String,
    pub assets: Vec<Asset>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TrustedKey {
    pub id: String,
    pub key: [u8; 32],
}

pub fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

pub fn unhex(s: &str) -> Option<Vec<u8>> {
    let s = s.trim();
    if !s.len().is_multiple_of(2) || !s.is_ascii() {
        return None;
    }
    (0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16).ok()).collect()
}

fn valid_key_id(id: &str) -> bool {
    (1..=32).contains(&id.len()) && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
}

/// Parses `update-keys.txt` (`<id> <64 hex>` lines; `#` comments).
pub fn parse_keys(text: &str) -> Result<Vec<TrustedKey>, String> {
    let mut keys = Vec::new();
    for (n, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let (id, k) = line.split_once(char::is_whitespace).ok_or(format!("line {}: expected '<id> <key>'", n + 1))?;
        let key: [u8; 32] = unhex(k).and_then(|v| v.try_into().ok()).ok_or(format!("line {}: key must be 64 hex characters", n + 1))?;
        if !valid_key_id(id) || VerifyingKey::from_bytes(&key).is_err() {
            return Err(format!("line {}: invalid key", n + 1));
        }
        keys.push(TrustedKey { id: id.to_owned(), key });
    }
    Ok(keys)
}

/// Keys this build trusts (compiled in).
pub fn trusted_keys() -> Vec<TrustedKey> {
    parse_keys(include_str!("../update-keys.txt")).expect("update-keys.txt is valid")
}

/// The exact bytes that are signed.
pub fn signed_message(manifest_json: &[u8]) -> Vec<u8> {
    [SIGNING_CONTEXT, manifest_json].concat()
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Rejected {
    NoTrustedKey,
    BadSignatureFile,
    UnknownKey(String),
    BadSignature,
    Malformed(String),
    WrongProduct(String),
}

impl std::fmt::Display for Rejected {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Rejected::NoTrustedKey => write!(f, "this build has no update-signing key, so no update can be verified"),
            Rejected::BadSignatureFile => write!(f, "the signature file is malformed"),
            Rejected::UnknownKey(k) => write!(f, "signed with key \"{k}\", which this build does not trust"),
            Rejected::BadSignature => write!(f, "the signature does not match: the update information was altered or corrupted"),
            Rejected::Malformed(e) => write!(f, "the update information is malformed: {e}"),
            Rejected::WrongProduct(p) => write!(f, "the update information is for \"{p}\", not Flint"),
        }
    }
}

/// Verifies `update.json` against `update.json.sig` (`<key-id> <128 hex>`)
/// and only then parses it.
pub fn verify_manifest(json: &[u8], sig_file: &str, keys: &[TrustedKey]) -> Result<Manifest, Rejected> {
    if keys.is_empty() {
        return Err(Rejected::NoTrustedKey);
    }
    if json.len() > MAX_MANIFEST {
        return Err(Rejected::Malformed("too large".into()));
    }
    let (id, sig) = sig_file.trim().split_once(' ').ok_or(Rejected::BadSignatureFile)?;
    let sig: [u8; 64] = unhex(sig).and_then(|v| v.try_into().ok()).ok_or(Rejected::BadSignatureFile)?;
    let key = keys.iter().find(|k| k.id == id).ok_or_else(|| Rejected::UnknownKey(id.chars().take(32).collect()))?;
    let vk = VerifyingKey::from_bytes(&key.key).map_err(|_| Rejected::NoTrustedKey)?;
    vk.verify_strict(&signed_message(json), &Signature::from_bytes(&sig)).map_err(|_| Rejected::BadSignature)?;
    let m: Manifest = serde_json::from_slice(json).map_err(|e| Rejected::Malformed(e.to_string()))?;
    if m.product != PRODUCT {
        return Err(Rejected::WrongProduct(m.product.chars().take(32).collect()));
    }
    if Version::parse(&m.version).is_none() {
        return Err(Rejected::Malformed(format!("version \"{}\"", m.version.chars().take(32).collect::<String>())));
    }
    for a in &m.assets {
        if !(safe_url(&a.url) || test_url_allowed(&a.url)) || unhex(&a.sha256).is_none_or(|d| d.len() != 32) || a.size == 0 || a.size > MAX_ASSET {
            return Err(Rejected::Malformed("asset entry".into()));
        }
    }
    if !safe_url(&m.notes_url) {
        return Err(Rejected::Malformed("notes_url".into()));
    }
    Ok(m)
}

/// https only, no credentials, printable ASCII.
pub fn safe_url(u: &str) -> bool {
    let Some(rest) = u.strip_prefix("https://") else { return false };
    let host = rest.split(['/', '?', '#']).next().unwrap_or("");
    u.len() <= 2048 && !host.is_empty() && !host.contains('@') && u.bytes().all(|b| b.is_ascii_graphic())
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Decision {
    UpToDate { latest: String },
    /// A newer version; `assets` may be empty (then only the notes page).
    Available(Manifest),
}

/// What to offer a build at `current`.
pub fn decide(current: &str, m: Manifest) -> Decision {
    let (Some(cur), Some(new)) = (Version::parse(current), Version::parse(&m.version)) else { return Decision::UpToDate { latest: m.version } };
    // Pre-releases are offered only to pre-release builds.
    if new.pre.is_some() && cur.pre.is_none() {
        return Decision::UpToDate { latest: m.version };
    }
    if new.newer_than(&cur) { Decision::Available(m) } else { Decision::UpToDate { latest: m.version } }
}

/// Checks that `data` is the asset the signed manifest describes.
pub fn verify_asset(a: &Asset, data: &[u8]) -> Result<(), String> {
    if data.len() as u64 != a.size {
        return Err(format!("expected {} bytes, got {}", a.size, data.len()));
    }
    let got = hex(&Sha256::digest(data));
    if !got.eq_ignore_ascii_case(a.sha256.trim()) {
        return Err("SHA-256 does not match the signed update information".into());
    }
    Ok(())
}

/// HTTP GET with a size cap (Windows: WinHTTP; tests: fakes).
pub trait Fetch {
    fn get(&self, url: &str, max_bytes: u64) -> Result<Vec<u8>, String>;
}

/// Downloads the manifest and its signature and decides.
pub fn check(fetch: &dyn Fetch, manifest_url: &str, current: &str, keys: &[TrustedKey]) -> Result<Decision, String> {
    if !safe_url(manifest_url) && !test_url_allowed(manifest_url) {
        return Err("the update address must be https".into());
    }
    let json = fetch.get(manifest_url, MAX_MANIFEST as u64)?;
    let sig = fetch.get(&format!("{manifest_url}.sig"), 1024)?;
    let sig = String::from_utf8(sig).map_err(|_| Rejected::BadSignatureFile.to_string())?;
    let m = verify_manifest(&json, &sig, keys).map_err(|e| e.to_string())?;
    Ok(decide(current, m))
}

/// Downloads an asset into `dir` and verifies it; the file exists only if
/// it verified.
pub fn download(fetch: &dyn Fetch, a: &Asset, dir: &Path) -> Result<std::path::PathBuf, String> {
    let data = fetch.get(&a.url, a.size)?;
    verify_asset(a, &data)?;
    let name = a.url.rsplit('/').next().filter(|n| !n.is_empty() && n.len() <= 100 && n.bytes().all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))).unwrap_or("flint-update.bin");
    std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    let path = dir.join(name);
    let tmp = dir.join(format!("{name}.partial"));
    std::fs::write(&tmp, &data).map_err(|e| e.to_string())?;
    std::fs::rename(&tmp, &path).map_err(|e| e.to_string())?;
    Ok(path)
}

/// Debug builds may use `http://127.0.0.1` for tests; release builds never.
pub fn test_url_allowed(u: &str) -> bool {
    cfg!(debug_assertions) && u.starts_with("http://127.0.0.1:")
}

/// When the last automatic check ran (seconds since the Unix epoch).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CheckState {
    pub last_check: i64,
    /// A version the user chose to skip.
    #[serde(default)]
    pub skipped: Option<String>,
}

impl CheckState {
    pub fn load(path: &Path) -> CheckState {
        std::fs::read_to_string(path).ok().and_then(|t| toml::from_str(&t).ok()).unwrap_or_default()
    }

    pub fn save(&self, path: &Path) -> Result<(), String> {
        std::fs::write(path, toml::to_string(self).map_err(|e| e.to_string())?).map_err(|e| e.to_string())
    }

    /// Whether an automatic (notify-mode) check is due. A clock that moved
    /// backwards counts as due, so a wrong clock cannot stop checks forever.
    pub fn due(&self, now: i64, interval_hours: u64) -> bool {
        let interval = (interval_hours.max(1) * 3600) as i64;
        now < self.last_check || now - self.last_check >= interval
    }
}

#[cfg(test)]
mod tests;
