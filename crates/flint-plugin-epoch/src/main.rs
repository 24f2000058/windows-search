//! `flint-plugin-epoch`: sample plugin and reference implementation of the
//! plugin protocol (docs/plugins.md).
//!
//! * `epoch 1700000000` → the UTC date (seconds or milliseconds)
//! * `epoch 2024-03-01 12:30` → the Unix timestamp (UTC)
//! * `epoch` / `epoch now` → the current timestamp
//! * result action `sha256`: hashes the selected files (bounded) and copies
//!   the digests.
//!
//! With `FLINT_PLUGIN_TEST_HOOKS=1`, special queries simulate faults:
//! `__crash__`, `__hang__`, `__garbage__`, `__huge__`, `__wrongid__`,
//! `__flood__`, `__spawn__`, `__error__`, `__url__`, `__oldapi__`
//! (handshake).

use flint_plugin::protocol::{read_frame, write_frame, Call, Request};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::io::{Read, Write};

const HASH_LIMIT: u64 = 256 << 20;

fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (yoe + era * 400 + i64::from(m <= 2), m, d)
}

fn format_utc(secs: i64) -> String {
    let (y, m, d) = civil_from_days(secs.div_euclid(86_400));
    let s = secs.rem_euclid(86_400);
    format!("{y:04}-{m:02}-{d:02} {:02}:{:02}:{:02} UTC", s / 3600, s / 60 % 60, s % 60)
}

/// `YYYY-MM-DD[ HH:MM[:SS]]` (or `T` separator), as UTC.
fn parse_utc(s: &str) -> Option<i64> {
    let s = s.trim().trim_end_matches('Z');
    let (date, time) = s.split_once([' ', 'T']).unwrap_or((s, ""));
    let mut dp = date.split('-').map(|p| p.parse::<i64>().ok());
    let (y, m, d) = (dp.next()??, dp.next()??, dp.next()??);
    if dp.next().is_some() || !(1..=12).contains(&m) || !(1..=31).contains(&d) {
        return None;
    }
    let mut tp = time.split(':').filter(|p| !p.is_empty()).map(|p| p.parse::<i64>().ok());
    let (hh, mm, ss) = (tp.next().unwrap_or(Some(0))?, tp.next().unwrap_or(Some(0))?, tp.next().unwrap_or(Some(0))?);
    if hh > 23 || mm > 59 || ss > 60 {
        return None;
    }
    Some(days_from_civil(y, m, d) * 86_400 + hh * 3600 + mm * 60 + ss)
}

fn item(title: String, subtitle: &str, copy: String) -> Value {
    json!({"title": title, "subtitle": subtitle, "action": {"type": "copy", "text": copy}})
}

fn query(text: &str, now: i64) -> Value {
    let t = text.trim();
    let mut items = Vec::new();
    if t.is_empty() || t.eq_ignore_ascii_case("now") {
        items.push(item(now.to_string(), &format!("now: {} · Enter copies", format_utc(now)), now.to_string()));
        items.push(item((now * 1000).to_string(), "now, in milliseconds", (now * 1000).to_string()));
    } else if let Ok(n) = t.parse::<i64>() {
        // 13+ digits are milliseconds.
        let (secs, unit) = if t.trim_start_matches('-').len() >= 13 { (n.div_euclid(1000), "milliseconds") } else { (n, "seconds") };
        if secs.unsigned_abs() < 253_402_300_800 {
            let date = format_utc(secs);
            items.push(item(date.clone(), &format!("{n} as Unix {unit} · Enter copies"), date));
        }
    } else if let Some(secs) = parse_utc(t) {
        items.push(item(secs.to_string(), &format!("{} as Unix seconds · Enter copies", format_utc(secs)), secs.to_string()));
    } else {
        items.push(json!({"title": "Type a Unix timestamp or a date (YYYY-MM-DD HH:MM)", "subtitle": "epoch converter"}));
    }
    json!({ "items": items })
}

fn sha256_file(path: &str) -> Result<String, String> {
    let mut f = std::fs::File::open(path).map_err(|e| e.to_string())?;
    if f.metadata().map_err(|e| e.to_string())?.len() > HASH_LIMIT {
        return Err("larger than 256 MB".into());
    }
    let mut h = Sha256::new();
    let mut buf = vec![0u8; 1 << 16];
    loop {
        let n = f.read(&mut buf).map_err(|e| e.to_string())?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    Ok(h.finalize().iter().map(|b| format!("{b:02x}")).collect())
}

fn action(name: &str, paths: &[String]) -> Result<Value, String> {
    if name != "sha256" {
        return Err(format!("unknown action {name}"));
    }
    if paths.is_empty() {
        return Err("no files selected (or the selection_paths permission was not granted)".into());
    }
    let mut lines = Vec::new();
    let mut failed = 0;
    for p in paths.iter().take(32) {
        match sha256_file(p) {
            Ok(h) => lines.push(format!("{h}  {}", p.rsplit(['\\', '/']).next().unwrap_or(p))),
            Err(_) => failed += 1,
        }
    }
    let msg = format!("SHA-256 of {} file(s) copied{}", lines.len(), if failed > 0 { format!("; {failed} could not be read") } else { String::new() });
    Ok(json!({ "message": msg, "copy": lines.join("\r\n") }))
}

fn send(out: &mut impl Write, v: &Value) -> std::io::Result<()> {
    write_frame(out, &serde_json::to_vec(v).unwrap())
}

fn main() {
    let hooks = std::env::var(flint_plugin::host::TEST_HOOKS_ENV).as_deref() == Ok("1");
    let (mut input, mut out) = (std::io::stdin().lock(), std::io::stdout().lock());
    while let Ok(Some(frame)) = read_frame(&mut input) {
        let Ok(req) = serde_json::from_slice::<Request>(&frame) else { return };
        let id = req.id;
        let reply = match &req.call {
            Call::Hello { .. } => {
                let api = if hooks && std::env::var("FLINT_PLUGIN_TEST_OLDAPI").is_ok() { 0 } else { flint_plugin::API_VERSION };
                json!({"id": id, "result": {"api": api}})
            }
            Call::Query { text } if hooks && text.starts_with("__") => match text.as_str() {
                "__crash__" => std::process::abort(),
                "__hang__" => loop {
                    std::thread::sleep(std::time::Duration::from_secs(3600));
                },
                "__garbage__" => {
                    let _ = write_frame(&mut out, b"this is not json");
                    continue;
                }
                "__huge__" => {
                    let _ = out.write_all(&(u32::MAX).to_le_bytes()).and_then(|_| out.flush());
                    continue;
                }
                "__wrongid__" => json!({"id": id + 100, "result": {"items": []}}),
                "__error__" => json!({"id": id, "error": "nothing to convert"}),
                "__url__" => json!({"id": id, "result": {"items": [{"title": "site", "action": {"type": "open_url", "url": "https://example.org/"}}]}}),
                "__flood__" => {
                    let items: Vec<Value> = (0..500).map(|i| json!({"title": format!("{i}{}", "\u{202E}x".repeat(400)), "subtitle": "s"})).collect();
                    json!({"id": id, "result": {"items": items}})
                }
                "__spawn__" => {
                    // Tries to start a child (refused by the Job Object on
                    // Windows unless launch_processes is granted).
                    let me = std::env::current_exe().unwrap();
                    let ok = std::process::Command::new(me).stdin(std::process::Stdio::null()).stdout(std::process::Stdio::null()).spawn().map(|mut c| {
                        let _ = c.kill();
                    });
                    json!({"id": id, "result": {"items": [{"title": if ok.is_ok() { "spawned" } else { "spawn refused" }}]}})
                }
                _ => json!({"id": id, "result": query(text, 0)}),
            },
            Call::Query { text } => {
                let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs() as i64);
                json!({"id": id, "result": query(text, now)})
            }
            Call::Action { action: a, paths } => match action(a, paths) {
                Ok(r) => json!({"id": id, "result": r}),
                Err(e) => json!({"id": id, "error": e}),
            },
        };
        if send(&mut out, &reply).is_err() {
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn converts_both_ways() {
        assert_eq!(format_utc(0), "1970-01-01 00:00:00 UTC");
        assert_eq!(format_utc(1_700_000_000), "2023-11-14 22:13:20 UTC");
        assert_eq!(parse_utc("2023-11-14 22:13:20"), Some(1_700_000_000));
        assert_eq!(parse_utc("2024-02-29T00:00Z"), Some(1_709_164_800));
        assert_eq!(parse_utc("1969-12-31 23:59:59"), Some(-1));
        assert_eq!(parse_utc("2024-13-01"), None);
        let r = query("1700000000000", 0);
        assert_eq!(r["items"][0]["title"], "2023-11-14 22:13:20 UTC");
        assert!(r["items"][0]["subtitle"].as_str().unwrap().contains("milliseconds"));
    }
}
