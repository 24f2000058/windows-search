//! The real helper process: isolation, crash recovery, timeouts, backoff.

use flint_preview::client::{HostClient, HostConfig, HostError};
use flint_preview::host::TEST_HOOKS_ENV;
use flint_preview::{Extracted, Limits, Op, Outcome, Preview};
use std::path::PathBuf;
use std::time::{Duration, Instant};

fn client(timeout_ms: u64) -> HostClient {
    let mut cfg = HostConfig::new(PathBuf::from(env!("CARGO_BIN_EXE_flint-preview-host")));
    cfg.timeout = Duration::from_millis(timeout_ms);
    cfg.backoff_min = Duration::from_millis(50);
    cfg.env.push((TEST_HOOKS_ENV.into(), "1".into()));
    HostClient::new(cfg)
}

fn file(name: &str, data: &[u8]) -> String {
    let d = std::env::temp_dir().join(format!("flint-host-test-{}", std::process::id()));
    std::fs::create_dir_all(&d).unwrap();
    let p = d.join(name);
    std::fs::write(&p, data).unwrap();
    p.to_string_lossy().into_owned()
}

#[test]
fn previews_and_extracts_through_the_helper() {
    let mut c = client(5000);
    assert!(!c.is_running(), "nothing starts until the first request");
    let p = file("hello.txt", b"hello from a file");
    assert_eq!(c.preview(&p, &Limits::default()).unwrap(), Preview::Text { text: "hello from a file".into(), encoding: "UTF-8".into(), truncated: false });
    assert_eq!(c.extract(&p, &Limits::default()).unwrap().unwrap(), Extracted::Text { text: "hello from a file".into(), truncated: false });
    assert!(c.preview("/no/such/file", &Limits::default()).is_err(), "a missing file is a per-file error");
    assert_eq!(c.stats.starts, 1, "one helper served all requests");
}

#[test]
fn a_crash_costs_one_file_and_the_helper_restarts() {
    let mut c = client(5000);
    let ok = file("ok.txt", b"fine");
    assert!(c.preview(&ok, &Limits::default()).is_ok());
    let r = c.request(Op::Preview, "/x/__flint_crash__", &Limits::default());
    assert!(matches!(r, Err(HostError::Crashed(_))), "{r:?}");
    // Within the backoff window the helper is not restarted.
    assert!(matches!(c.request(Op::Preview, &ok, &Limits::default()), Err(HostError::Unavailable(_))));
    std::thread::sleep(Duration::from_millis(80));
    assert!(matches!(c.request(Op::Preview, &ok, &Limits::default()), Ok(Outcome::Preview(_))), "restarted after backoff");
    assert_eq!((c.stats.crashes, c.stats.starts), (1, 2));
}

#[test]
fn a_hang_is_killed_at_the_timeout() {
    let mut c = client(300);
    let t = Instant::now();
    let r = c.request(Op::Extract, "/x/__flint_hang__", &Limits::default());
    let took = t.elapsed();
    assert_eq!(r, Err(HostError::Timeout));
    assert!(took >= Duration::from_millis(300) && took < Duration::from_secs(3), "{took:?}");
    std::thread::sleep(Duration::from_millis(80));
    let ok = file("after.txt", b"still works");
    assert!(c.preview(&ok, &Limits::default()).is_ok());
    assert_eq!(c.stats.timeouts, 1);
}

#[test]
fn repeated_failures_back_off_exponentially() {
    let mut c = client(5000);
    let mut gaps = Vec::new();
    for _ in 0..3 {
        // Wait until the helper may start again, then crash it.
        let t = Instant::now();
        while matches!(c.request(Op::Preview, "/x/__flint_crash__", &Limits::default()), Err(HostError::Unavailable(_))) {
            std::thread::sleep(Duration::from_millis(5));
        }
        gaps.push(t.elapsed());
    }
    assert!(gaps[2] > gaps[1] && gaps[1] >= Duration::from_millis(50), "backoff grows: {gaps:?}");
}

#[test]
fn missing_helper_is_reported_not_fatal() {
    let mut c = HostClient::new(HostConfig::new(PathBuf::from("/definitely/not/a/helper")));
    assert!(matches!(c.request(Op::Preview, "x", &Limits::default()), Err(HostError::Unavailable(_))));
}
