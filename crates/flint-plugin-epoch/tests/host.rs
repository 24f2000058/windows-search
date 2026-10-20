//! The plugin host against a real plugin process: protocol, limits,
//! permission enforcement, crash/timeout recovery, strikes, approvals.

use flint_plugin::host::{std_spawn, Launch, PluginError, PluginHost, MAX_STRIKES, TEST_HOOKS_ENV};
use flint_plugin::protocol::{ItemAction, MAX_ITEMS};
use flint_plugin::registry::{self, Approvals, Standing};
use flint_plugin::service::{Answer, Delivery, PluginService};
use flint_plugin::{Manifest, Permission};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

fn exe() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_flint-plugin-epoch"))
}

fn manifest(timeout_ms: u64, perms: &[Permission]) -> Manifest {
    let mut m = Manifest::parse(include_str!("../plugin.toml")).unwrap();
    m.limits.timeout_ms = timeout_ms;
    m.permissions = perms.to_vec();
    m
}

fn host_with(m: Manifest, env: Vec<(String, String)>) -> PluginHost {
    let launch = Launch { exe: exe(), dir: std::env::temp_dir(), env, memory_limit: 64 << 20, allow_children: false, cpu_percent: 25 };
    let granted = m.permission_set();
    let mut h = PluginHost::new(m, granted, launch, Arc::new(std_spawn), None);
    h.backoff_min = Duration::from_millis(40);
    h
}

fn host(timeout_ms: u64, perms: &[Permission]) -> PluginHost {
    host_with(manifest(timeout_ms, perms), vec![(TEST_HOOKS_ENV.into(), "1".into())])
}

fn wait_backoff() {
    std::thread::sleep(Duration::from_millis(120));
}

#[test]
fn queries_and_actions_round_trip_through_one_process() {
    let mut h = host(3000, &[Permission::SelectionPaths]);
    assert!(!h.is_running(), "nothing starts before first use");
    let items = h.query("1700000000").unwrap();
    assert_eq!(items[0].title, "2023-11-14 22:13:20 UTC");
    assert_eq!(items[0].action, ItemAction::Copy { text: "2023-11-14 22:13:20 UTC".into() });
    let f = std::env::temp_dir().join(format!("flint-plugin-hash-{}.txt", std::process::id()));
    std::fs::write(&f, b"abc").unwrap();
    let r = h.action("sha256", &[f.to_string_lossy().into_owned()]).unwrap();
    assert!(r.copy.unwrap().starts_with("ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"));
    assert!(matches!(h.action("undeclared", &[]), Err(PluginError::Plugin(_))), "only declared actions are sent");
    assert_eq!(h.stats.starts, 1);
}

#[test]
fn paths_are_withheld_without_the_permission() {
    let mut m = manifest(3000, &[Permission::SelectionPaths]);
    m.permissions.clear(); // granted set is what counts
    let launch = Launch { exe: exe(), dir: std::env::temp_dir(), env: vec![], memory_limit: 0, allow_children: false, cpu_percent: 0 };
    let mut h = PluginHost::new(m, vec![], launch, Arc::new(std_spawn), None);
    let r = h.action("sha256", &["C:\\secret.txt".into()]);
    assert!(matches!(r, Err(PluginError::Plugin(e)) if e.contains("no files selected")), "the plugin saw no paths");
}

#[test]
fn a_crash_costs_one_request_then_the_plugin_restarts() {
    let mut h = host(3000, &[]);
    assert!(h.query("now").is_ok());
    assert!(matches!(h.query("__crash__"), Err(PluginError::Crashed(_))));
    assert!(matches!(h.query("now"), Err(PluginError::Unavailable(_))), "backing off");
    wait_backoff();
    assert!(h.query("now").is_ok(), "restarted");
    assert_eq!((h.stats.crashes, h.stats.starts), (1, 2));
}

#[test]
fn a_hang_is_stopped_at_the_time_limit() {
    let mut h = host(300, &[]);
    let t = Instant::now();
    assert_eq!(h.query("__hang__"), Err(PluginError::Timeout));
    assert!(t.elapsed() < Duration::from_secs(3), "{:?}", t.elapsed());
    wait_backoff();
    assert!(h.query("now").is_ok());
}

#[test]
fn protocol_violations_stop_the_plugin() {
    for q in ["__garbage__", "__huge__", "__wrongid__"] {
        let mut h = host(3000, &[]);
        assert!(matches!(h.query(q), Err(PluginError::Protocol(_))), "{q}");
        assert!(!h.is_running(), "{q}: process stopped");
        assert_eq!(h.stats.protocol_errors, 1);
    }
}

#[test]
fn repeated_failures_disable_the_plugin_for_the_session() {
    let mut h = host(3000, &[]);
    for _ in 0..MAX_STRIKES {
        while matches!(h.query("__crash__"), Err(PluginError::Unavailable(_))) {
            std::thread::sleep(Duration::from_millis(10));
        }
    }
    wait_backoff();
    assert!(matches!(h.query("now"), Err(PluginError::Disabled(_))));
    assert!(h.disabled().unwrap().contains("failures"));
}

#[test]
fn a_plugin_error_answer_is_not_a_failure() {
    let mut h = host(3000, &[]);
    for _ in 0..MAX_STRIKES + 2 {
        assert_eq!(h.query("__error__"), Err(PluginError::Plugin("nothing to convert".into())));
    }
    assert!(h.disabled().is_none() && h.is_running());
}

#[test]
fn output_is_bounded_and_cleaned() {
    let mut h = host(3000, &[]);
    let items = h.query("__flood__").unwrap();
    assert_eq!(items.len(), MAX_ITEMS);
    assert!(items.iter().all(|i| !i.title.contains('\u{202E}') && i.title.chars().count() <= 301));
}

#[test]
fn url_actions_need_the_permission() {
    let mut h = host(3000, &[]);
    assert_eq!(h.query("__url__").unwrap()[0].action, ItemAction::None);
    assert_eq!(h.stats.denied, 1);
    let mut h = host(3000, &[Permission::OpenUrls]);
    assert!(matches!(&h.query("__url__").unwrap()[0].action, ItemAction::OpenUrl { url } if url == "https://example.org/"));
}

#[test]
fn an_incompatible_api_disables_at_handshake() {
    let mut h = host_with(manifest(3000, &[]), vec![(TEST_HOOKS_ENV.into(), "1".into()), ("FLINT_PLUGIN_TEST_OLDAPI".into(), "1".into())]);
    assert!(matches!(h.query("now"), Err(PluginError::Disabled(e)) if e.contains("API 0")));
}

#[test]
fn changed_files_are_not_started() {
    let launch = Launch { exe: exe(), dir: std::env::temp_dir(), env: vec![], memory_limit: 0, allow_children: false, cpu_percent: 0 };
    let mut h = PluginHost::new(manifest(3000, &[]), vec![], launch, Arc::new(std_spawn), Some(Box::new(|| Err("its files changed since you approved it".into()))));
    assert!(matches!(h.query("now"), Err(PluginError::Unavailable(e)) if e.contains("changed")));
    assert_eq!(h.stats.starts, 0);
}

#[test]
fn service_is_latest_wins_and_survives_a_broken_plugin() {
    let got: Arc<Mutex<Vec<Delivery>>> = Arc::default();
    let sink = got.clone();
    let mut broken = manifest(3000, &[]);
    broken.id = "broken".into();
    broken.prefix = Some("broken".into());
    let svc = PluginService::new(vec![host(3000, &[]), host_with(broken, vec![(TEST_HOOKS_ENV.into(), "1".into())])], Arc::new(move |d| sink.lock().unwrap().push(d)));
    assert_eq!(svc.route("Epoch 5").map(|(m, rest)| (m.id.as_str(), rest)), Some(("epoch", "5")));
    assert!(svc.route("epochs").is_none() && svc.route("report.pdf").is_none());
    // The broken plugin crashes; the other keeps answering.
    assert!(svc.query("broken", 1, "__crash__"));
    for g in 2..=40 {
        svc.query("epoch", g, &g.to_string());
    }
    let t = Instant::now();
    // Each plugin answers on its own worker thread: wait for both.
    let done = |v: &[Delivery]| v.iter().any(|d| d.plugin == "epoch" && d.generation == 40) && v.iter().any(|d| d.plugin == "broken");
    while !done(&got.lock().unwrap()) && t.elapsed() < Duration::from_secs(20) {
        std::thread::sleep(Duration::from_millis(10));
    }
    let v = got.lock().unwrap();
    let epoch: Vec<&Delivery> = v.iter().filter(|d| d.plugin == "epoch").collect();
    assert!(epoch.len() < 39, "superseded queries were skipped ({} answered)", epoch.len());
    assert!(matches!(&epoch.last().unwrap().outcome, Ok(Answer::Items(i)) if i[0].subtitle.contains("40 as Unix")));
    let broken: Vec<_> = v.iter().filter(|d| d.plugin == "broken").map(|d| &d.outcome).collect();
    assert!(matches!(broken.as_slice(), [Err(PluginError::Crashed(_))]), "{broken:?}");
    drop(v);
    svc.stop_all();
}

#[test]
fn load_runs_only_approved_unchanged_plugins() {
    let t = tempfile::tempdir().unwrap();
    let dir = t.path().join("epoch");
    std::fs::create_dir_all(&dir).unwrap();
    let text = include_str!("../plugin.toml").replace("flint-plugin-epoch.exe", exe().file_name().unwrap().to_str().unwrap());
    std::fs::write(dir.join("plugin.toml"), text).unwrap();
    std::fs::copy(exe(), dir.join(exe().file_name().unwrap())).unwrap();
    let spawner: flint_plugin::Spawner = Arc::new(std_spawn);
    let (hosts, all) = flint_plugin::load(t.path(), &Approvals::default(), spawner.clone(), &[]);
    assert!(hosts.is_empty());
    let Standing::NeedsApproval { sha256, .. } = &all[0].1 else { panic!("{:?}", all[0].1) };
    let mut ap = Approvals::default();
    ap.approve(all[0].0.manifest.as_ref().unwrap(), sha256.clone(), 0);
    let (mut hosts, _) = flint_plugin::load(t.path(), &ap, spawner, &[]);
    assert_eq!(hosts[0].query("0").unwrap()[0].title, "1970-01-01 00:00:00 UTC");
    hosts[0].stop();
    // Swapped after approval: refused at the next start.
    std::fs::write(dir.join("extra.dll"), b"x").unwrap();
    assert!(matches!(hosts[0].query("0"), Err(PluginError::Unavailable(e)) if e.contains("changed")));
    assert!(matches!(registry::standing(&registry::discover(t.path())[0], &ap), Standing::NeedsApproval { .. }));
}

#[cfg(windows)]
#[test]
fn confined_spawn_uses_a_job_and_blocks_child_processes() {
    let run = |allow_children: bool| {
        let m = manifest(5000, &[]);
        let launch = Launch { exe: exe(), dir: std::env::temp_dir(), env: vec![(TEST_HOOKS_ENV.into(), "1".into())], memory_limit: 64 << 20, allow_children, cpu_percent: 25 };
        let mut h = PluginHost::new(m, vec![], launch, flint_plugin::host::default_spawner(), None);
        let title = h.query("__spawn__").unwrap()[0].title.clone();
        let iso = h.stats.isolation.clone().unwrap();
        assert!(iso.job_object, "{iso:?}");
        println!("isolation: {}", iso.describe());
        title
    };
    // Wine accepts but does not enforce the job's active-process limit.
    let wine = std::path::Path::new("C:\\windows\\system32\\winecfg.exe").exists();
    let refused = run(false);
    if wine {
        println!("under Wine: child-process limit not enforced (got {refused:?})");
    } else {
        assert_eq!(refused, "spawn refused");
    }
    assert_eq!(run(true), "spawned");
}
