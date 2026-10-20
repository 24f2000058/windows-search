use super::*;
use ed25519_dalek::{Signer, SigningKey};
use std::cell::RefCell;
use std::collections::HashMap;

fn key(seed: u8) -> (SigningKey, TrustedKey) {
    let sk = SigningKey::from_bytes(&[seed; 32]);
    let tk = TrustedKey { id: format!("k{seed}"), key: sk.verifying_key().to_bytes() };
    (sk, tk)
}

fn sign(sk: &SigningKey, id: &str, json: &[u8]) -> String {
    format!("{id} {}", hex(&sk.sign(&signed_message(json)).to_bytes()))
}

fn manifest(version: &str, data: &[u8]) -> Vec<u8> {
    serde_json::to_vec(&Manifest {
        product: "flint".into(),
        version: version.into(),
        released: "2026-11-01".into(),
        notes_url: "https://github.com/o/r/releases/tag/v2".into(),
        assets: vec![Asset { kind: AssetKind::Msi, url: "https://github.com/o/r/releases/download/v2/flint-2.0.0-x64.msi".into(), size: data.len() as u64, sha256: hex(&Sha256::digest(data)) }],
    })
    .unwrap()
}

#[test]
fn versions_order_and_reject_junk() {
    let v = |s| Version::parse(s).unwrap();
    assert!(v("0.2.0").newer_than(&v("0.1.9")));
    assert!(v("1.0.0").newer_than(&v("1.0.0-rc.2")));
    assert!(v("1.0.0-rc.10").newer_than(&v("1.0.0-rc.9")));
    assert!(!v("v0.1.0").newer_than(&v("0.1.0")));
    for bad in ["1.2", "1.2.3.4", "a.b.c", "1.2.-3", "1.2.3-", "1.2.3-x y", "01234567890.0.0"] {
        assert!(Version::parse(bad).is_none(), "{bad}");
    }
}

#[test]
fn only_a_correct_signature_from_a_trusted_key_is_accepted() {
    let (sk, tk) = key(1);
    let (other, _) = key(2);
    let json = manifest("2.0.0", b"msi");
    assert!(verify_manifest(&json, &sign(&sk, "k1", &json), std::slice::from_ref(&tk)).is_ok());
    assert_eq!(verify_manifest(&json, &sign(&sk, "k1", &json), &[]), Err(Rejected::NoTrustedKey));
    assert_eq!(verify_manifest(&json, &sign(&other, "k1", &json), std::slice::from_ref(&tk)), Err(Rejected::BadSignature));
    assert!(matches!(verify_manifest(&json, &sign(&sk, "k9", &json), std::slice::from_ref(&tk)), Err(Rejected::UnknownKey(_))));
    assert_eq!(verify_manifest(&json, "k1 zz", std::slice::from_ref(&tk)), Err(Rejected::BadSignatureFile));
    // One altered byte anywhere breaks it.
    let sig = sign(&sk, "k1", &json);
    for i in [0, json.len() / 2, json.len() - 1] {
        let mut t = json.clone();
        t[i] ^= 1;
        assert!(verify_manifest(&t, &sig, std::slice::from_ref(&tk)).is_err(), "byte {i}");
    }
    // A signature over the bare JSON (no context) is not valid.
    let bare = format!("k1 {}", hex(&sk.sign(&json).to_bytes()));
    assert_eq!(verify_manifest(&json, &bare, &[tk]), Err(Rejected::BadSignature));
}

#[test]
fn signed_but_unsafe_content_is_still_refused() {
    let (sk, tk) = key(1);
    for (from, to) in [("\"flint\"", "\"other\""), ("https://github.com/o/r/releases/download", "http://github.com/o/r/releases/download"), ("\"2.0.0\"", "\"two\"")] {
        let json = String::from_utf8(manifest("2.0.0", b"m")).unwrap().replacen(from, to, 1).into_bytes();
        assert!(verify_manifest(&json, &sign(&sk, "k1", &json), std::slice::from_ref(&tk)).is_err(), "{to}");
    }
}

#[test]
fn only_newer_releases_are_offered() {
    let m = |v: &str| serde_json::from_slice::<Manifest>(&manifest(v, b"x")).unwrap();
    assert!(matches!(decide("0.1.0", m("0.2.0")), Decision::Available(_)));
    assert!(matches!(decide("0.2.0", m("0.2.0")), Decision::UpToDate { .. }));
    assert!(matches!(decide("0.3.0", m("0.2.0")), Decision::UpToDate { .. }), "no rollback");
    assert!(matches!(decide("0.1.0", m("0.2.0-beta.1")), Decision::UpToDate { .. }), "no pre-release for releases");
    assert!(matches!(decide("0.2.0-beta.1", m("0.2.0-beta.2")), Decision::Available(_)));
}

struct FakeNet(RefCell<HashMap<String, Vec<u8>>>);

impl Fetch for FakeNet {
    fn get(&self, url: &str, max: u64) -> Result<Vec<u8>, String> {
        let d = self.0.borrow().get(url).cloned().ok_or("404")?;
        if d.len() as u64 > max {
            return Err("too large".into());
        }
        Ok(d)
    }
}

#[test]
fn check_and_download_end_to_end() {
    let (sk, tk) = key(3);
    let msi = b"pretend installer".to_vec();
    let json = manifest("2.0.0", &msi);
    let base = "https://github.com/o/r/releases/latest/download/update.json";
    let net = FakeNet(RefCell::new(HashMap::from([(base.to_owned(), json.clone()), (format!("{base}.sig"), sign(&sk, "k3", &json).into_bytes()), ("https://github.com/o/r/releases/download/v2/flint-2.0.0-x64.msi".to_owned(), msi.clone())])));
    let Decision::Available(m) = check(&net, base, "0.1.0", std::slice::from_ref(&tk)).unwrap() else { panic!() };
    let dir = tempfile::tempdir().unwrap();
    let p = download(&net, &m.assets[0], dir.path()).unwrap();
    assert_eq!(std::fs::read(&p).unwrap(), msi);
    assert!(p.ends_with("flint-2.0.0-x64.msi"));
    // A swapped download is refused and leaves no file behind.
    net.0.borrow_mut().insert(m.assets[0].url.clone(), b"pretend installeR".to_vec());
    std::fs::remove_file(&p).unwrap();
    assert!(download(&net, &m.assets[0], dir.path()).unwrap_err().contains("SHA-256"));
    assert!(!p.exists());
    // A truncated or padded download fails on size.
    net.0.borrow_mut().insert(m.assets[0].url.clone(), b"short".to_vec());
    assert!(download(&net, &m.assets[0], dir.path()).unwrap_err().contains("bytes"));
    assert!(check(&net, "http://example.org/update.json", "0.1.0", &[tk]).is_err(), "plain http refused");
}

#[test]
fn keys_file_parses_strictly_and_the_shipped_one_is_valid() {
    let (_, tk) = key(4);
    let text = format!("# comment\n\n{} {}\n", tk.id, hex(&tk.key));
    assert_eq!(parse_keys(&text).unwrap(), vec![tk]);
    assert!(parse_keys("k1 abcd").is_err());
    assert!(parse_keys("bad id! 00").is_err());
    let _ = trusted_keys();
}

#[test]
fn automatic_checks_are_spaced_out() {
    let s = CheckState { last_check: 1_000_000, skipped: None };
    assert!(!s.due(1_000_000 + 3600, 24));
    assert!(s.due(1_000_000 + 24 * 3600, 24));
    assert!(s.due(10, 24), "clock moved backwards");
}
