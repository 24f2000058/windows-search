//! Plugin host (ADR-008, ADR-019).
//!
//! A plugin is a separate executable in its own folder with a
//! `plugin.toml` manifest ([`manifest`]). It speaks a versioned,
//! length-prefixed JSON protocol over stdin/stdout ([`protocol`]). It runs
//! only after the user approves its files and permissions ([`registry`]).
//! Each plugin runs in its own process with per-request time limits, crash
//! recovery with backoff, and a session strike limit ([`host`]). On Windows
//! it also runs at low integrity in a Job Object with memory, CPU,
//! child-process and UI limits ([`win`]). The [`service`] keeps all of this
//! off the UI thread.
//!
//! Plugins are disabled unless `plugins.enabled` is set, and the launcher
//! works the same with every plugin disabled. Isolation reduces the harm a
//! faulty plugin can do; it is **not** a sandbox and is never described as
//! one.

pub mod host;
pub mod manifest;
pub mod protocol;
pub mod registry;
pub mod service;
#[cfg(windows)]
pub mod win;

pub use host::{Launch, PluginError, PluginHost, Spawner};
pub use manifest::{Manifest, Permission, API_VERSION};

use registry::{Approvals, Discovered, Standing};
use std::path::Path;

/// Every discovered plugin with its standing, plus hosts for the approved
/// ones. A host re-hashes its folder before every process start and
/// refuses to start if the files changed after approval.
pub fn load(plugins_dir: &Path, approvals: &Approvals, spawner: Spawner, env: &[(String, String)]) -> (Vec<PluginHost>, Vec<(Discovered, Standing)>) {
    let mut hosts = Vec::new();
    let mut all = Vec::new();
    for d in registry::discover(plugins_dir) {
        let st = registry::standing(&d, approvals);
        if let (Standing::Approved { sha256 }, Ok(m)) = (&st, &d.manifest) {
            let launch = Launch {
                exe: m.exe_path(&d.dir),
                dir: d.dir.clone(),
                env: env.to_vec(),
                memory_limit: m.limits.memory_mb << 20,
                allow_children: m.permissions.contains(&Permission::LaunchProcesses),
                cpu_percent: 25,
            };
            let (dir, expected) = (d.dir.clone(), sha256.clone());
            let verify: host::Verify = Box::new(move || match registry::hash_folder(&dir) {
                Ok(h) if h == expected => Ok(()),
                Ok(_) => Err("its files changed since you approved it; approve it again".into()),
                Err(e) => Err(e),
            });
            hosts.push(PluginHost::new(m.clone(), m.permission_set(), launch, spawner.clone(), Some(verify)));
        }
        all.push((d, st));
    }
    (hosts, all)
}
