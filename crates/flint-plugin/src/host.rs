//! One plugin process: started on first use, handshaken, given one request
//! at a time with a time limit, killed on timeout or protocol violation and
//! restarted with exponential backoff. After [`MAX_STRIKES`] failures in a
//! session the plugin is disabled until the launcher restarts; the core
//! keeps working throughout.

use crate::manifest::{Manifest, Permission, API_VERSION};
use crate::protocol::{read_frame, write_frame, ActionResult, Call, HelloResult, Item, ItemAction, QueryResult, Reply, Request, MAX_ITEMS};
use std::io::{self, Read, Write};
use std::path::PathBuf;
use std::sync::mpsc::{channel, Receiver, RecvTimeoutError};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Crashes, timeouts and protocol violations tolerated per session.
pub const MAX_STRIKES: u32 = 3;
/// Environment variable that turns on the sample plugin's failure hooks
/// (tests only).
pub const TEST_HOOKS_ENV: &str = "FLINT_PLUGIN_TEST_HOOKS";

/// How to start a plugin process.
#[derive(Clone, Debug)]
pub struct Launch {
    pub exe: PathBuf,
    pub dir: PathBuf,
    pub env: Vec<(String, String)>,
    pub memory_limit: u64,
    pub allow_children: bool,
    /// Hard CPU cap for the process (Windows Job Object), percent of all cores.
    pub cpu_percent: u32,
}

/// The confinement a plugin process actually got; shown in diagnostics.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Isolation {
    pub low_integrity: bool,
    pub job_object: bool,
    pub note: String,
}

impl Isolation {
    pub fn describe(&self) -> String {
        let mut v = Vec::new();
        v.push(if self.low_integrity { "low integrity" } else { "normal integrity (runs with your rights)" });
        if self.job_object {
            v.push("Job Object limits");
        }
        let mut s = v.join(", ");
        if !self.note.is_empty() {
            s.push_str(&format!(" ({})", self.note));
        }
        s
    }
}

pub trait Process: Send {
    fn kill(&mut self);
}

pub struct Spawned {
    pub stdin: Box<dyn Write + Send>,
    pub stdout: Box<dyn Read + Send>,
    pub process: Box<dyn Process>,
    pub isolation: Isolation,
}

pub type Spawner = Arc<dyn Fn(&Launch) -> io::Result<Spawned> + Send + Sync>;

struct StdProcess(std::process::Child);

impl Process for StdProcess {
    fn kill(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// Plain child process (no confinement): used off Windows and in tests.
pub fn std_spawn(l: &Launch) -> io::Result<Spawned> {
    let mut cmd = std::process::Command::new(&l.exe);
    cmd.current_dir(&l.dir).stdin(std::process::Stdio::piped()).stdout(std::process::Stdio::piped()).stderr(std::process::Stdio::null());
    for (k, v) in &l.env {
        cmd.env(k, v);
    }
    let mut child = cmd.spawn()?;
    let stdin = child.stdin.take().expect("piped");
    let stdout = child.stdout.take().expect("piped");
    Ok(Spawned { stdin: Box::new(stdin), stdout: Box::new(stdout), process: Box::new(StdProcess(child)), isolation: Isolation { note: "no confinement on this platform".into(), ..Default::default() } })
}

/// Low-integrity process in a Job Object on Windows; a plain child elsewhere.
pub fn default_spawner() -> Spawner {
    #[cfg(windows)]
    return Arc::new(crate::win::spawn_confined);
    #[cfg(not(windows))]
    Arc::new(std_spawn)
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PluginError {
    Timeout,
    Crashed(String),
    /// Malformed or unexpected reply; the process was stopped.
    Protocol(String),
    /// Cannot start now (missing, changed since approval, backing off).
    Unavailable(String),
    /// Disabled for this session after repeated failures.
    Disabled(String),
    /// The plugin answered with an error (not a failure of the plugin).
    Plugin(String),
}

impl std::fmt::Display for PluginError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PluginError::Timeout => write!(f, "did not answer in time and was stopped"),
            PluginError::Crashed(e) => write!(f, "stopped unexpectedly ({e})"),
            PluginError::Protocol(e) => write!(f, "sent an invalid reply ({e}) and was stopped"),
            PluginError::Unavailable(e) => write!(f, "unavailable: {e}"),
            PluginError::Disabled(e) => write!(f, "disabled for this session: {e}"),
            PluginError::Plugin(e) => write!(f, "{e}"),
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Stats {
    pub starts: u64,
    pub requests: u64,
    pub timeouts: u64,
    pub crashes: u64,
    pub protocol_errors: u64,
    /// Result actions removed because the plugin lacks the permission.
    pub denied: u64,
    pub last_error: Option<String>,
    pub last_latency: Option<Duration>,
    pub isolation: Option<Isolation>,
}

struct Running {
    stdin: Box<dyn Write + Send>,
    rx: Receiver<io::Result<Option<Vec<u8>>>>,
    process: Box<dyn Process>,
}

impl Drop for Running {
    fn drop(&mut self) {
        self.process.kill();
    }
}

/// Checked before every start: the plugin's files still match what the
/// user approved.
pub type Verify = Box<dyn Fn() -> Result<(), String> + Send>;

pub struct PluginHost {
    pub manifest: Manifest,
    pub granted: Vec<Permission>,
    launch: Launch,
    spawner: Spawner,
    verify: Option<Verify>,
    running: Option<Running>,
    next_id: u64,
    failures: u32,
    strikes: u32,
    retry_at: Option<Instant>,
    disabled: Option<String>,
    pub backoff_min: Duration,
    pub backoff_max: Duration,
    pub stats: Stats,
}

impl PluginHost {
    pub fn new(manifest: Manifest, granted: Vec<Permission>, launch: Launch, spawner: Spawner, verify: Option<Verify>) -> Self {
        PluginHost {
            manifest,
            granted,
            launch,
            spawner,
            verify,
            running: None,
            next_id: 1,
            failures: 0,
            strikes: 0,
            retry_at: None,
            disabled: None,
            backoff_min: Duration::from_millis(250),
            backoff_max: Duration::from_secs(30),
            stats: Stats::default(),
        }
    }

    fn timeout(&self) -> Duration {
        Duration::from_millis(self.manifest.limits.timeout_ms)
    }

    pub fn disabled(&self) -> Option<&str> {
        self.disabled.as_deref()
    }

    pub fn is_running(&self) -> bool {
        self.running.is_some()
    }

    /// Stops the process (it restarts on the next request).
    pub fn stop(&mut self) {
        self.running = None;
    }

    fn start(&mut self) -> Result<(), PluginError> {
        if let Some(r) = &self.disabled {
            return Err(PluginError::Disabled(r.clone()));
        }
        if self.retry_at.is_some_and(|t| Instant::now() < t) {
            return Err(PluginError::Unavailable(format!("restarting after {} failure(s)", self.failures)));
        }
        if let Some(v) = &self.verify {
            v().map_err(PluginError::Unavailable)?;
        }
        let sp = (self.spawner)(&self.launch).map_err(|e| PluginError::Unavailable(format!("cannot start {}: {e}", self.launch.exe.display())))?;
        let Spawned { stdin, mut stdout, process, isolation } = sp;
        let (tx, rx) = channel();
        std::thread::Builder::new()
            .name(format!("flint-plugin-{}", self.manifest.id))
            .spawn(move || loop {
                let r = read_frame(&mut stdout);
                let end = !matches!(r, Ok(Some(_)));
                if tx.send(r).is_err() || end {
                    return;
                }
            })
            .map_err(|e| PluginError::Unavailable(e.to_string()))?;
        self.stats.starts += 1;
        self.stats.isolation = Some(isolation);
        self.running = Some(Running { stdin, rx, process });
        let hello = self.exchange(Call::Hello { api: API_VERSION, permissions: self.granted.clone() })?;
        let h: HelloResult = serde_json::from_value(hello).map_err(|e| self.fail(PluginError::Protocol(format!("hello: {e}"))))?;
        if h.api != API_VERSION {
            let why = format!("speaks plugin API {} but Flint speaks {API_VERSION}", h.api);
            self.running = None;
            self.disabled = Some(why.clone());
            return Err(PluginError::Disabled(why));
        }
        Ok(())
    }

    fn fail(&mut self, e: PluginError) -> PluginError {
        self.running = None; // kills
        self.failures += 1;
        self.strikes += 1;
        let backoff = self.backoff_min.saturating_mul(1 << self.failures.min(16).saturating_sub(1)).min(self.backoff_max);
        self.retry_at = Some(Instant::now() + backoff);
        match &e {
            PluginError::Timeout => self.stats.timeouts += 1,
            PluginError::Crashed(_) => self.stats.crashes += 1,
            PluginError::Protocol(_) => self.stats.protocol_errors += 1,
            _ => {}
        }
        self.stats.last_error = Some(e.to_string());
        if self.strikes >= MAX_STRIKES {
            self.disabled = Some(format!("{} failures; last: {e}", self.strikes));
        }
        e
    }

    /// Sends one call and waits for its reply (the process must be running).
    fn exchange(&mut self, call: Call) -> Result<serde_json::Value, PluginError> {
        let id = self.next_id;
        self.next_id += 1;
        self.stats.requests += 1;
        let t0 = Instant::now();
        let body = serde_json::to_vec(&Request { id, call }).expect("requests serialize");
        let timeout = self.timeout();
        let run = self.running.as_mut().expect("started");
        if let Err(e) = write_frame(&mut run.stdin, &body) {
            return Err(self.fail(PluginError::Crashed(format!("write: {e}"))));
        }
        let frame = match run.rx.recv_timeout(timeout) {
            Ok(Ok(Some(f))) => f,
            Ok(Ok(None)) | Err(RecvTimeoutError::Disconnected) => return Err(self.fail(PluginError::Crashed("exited".into()))),
            Ok(Err(e)) if e.kind() == io::ErrorKind::InvalidData => return Err(self.fail(PluginError::Protocol(e.to_string()))),
            Ok(Err(e)) => return Err(self.fail(PluginError::Crashed(e.to_string()))),
            Err(RecvTimeoutError::Timeout) => return Err(self.fail(PluginError::Timeout)),
        };
        let reply: Reply = match serde_json::from_slice(&frame) {
            Ok(r) => r,
            Err(e) => return Err(self.fail(PluginError::Protocol(e.to_string()))),
        };
        if reply.id != id {
            return Err(self.fail(PluginError::Protocol(format!("answered request {} instead of {id}", reply.id))));
        }
        self.failures = 0;
        self.retry_at = None;
        self.stats.last_latency = Some(t0.elapsed());
        match (reply.result, reply.error) {
            (_, Some(e)) => Err(PluginError::Plugin(crate::protocol::clean(&e, 300))),
            (Some(v), None) => Ok(v),
            (None, None) => Err(self.fail(PluginError::Protocol("reply has neither result nor error".into()))),
        }
    }

    fn call(&mut self, call: Call) -> Result<serde_json::Value, PluginError> {
        if let Some(r) = &self.disabled {
            return Err(PluginError::Disabled(r.clone()));
        }
        if self.running.is_none() {
            self.start()?;
        }
        self.exchange(call)
    }

    /// Runs a query; results are clamped and stripped of actions the plugin
    /// is not permitted.
    pub fn query(&mut self, text: &str) -> Result<Vec<Item>, PluginError> {
        let v = self.call(Call::Query { text: text.chars().take(512).collect() })?;
        let r: QueryResult = serde_json::from_value(v).map_err(|e| self.fail(PluginError::Protocol(format!("query result: {e}"))))?;
        let open_urls = self.granted.contains(&Permission::OpenUrls);
        let mut items: Vec<Item> = r.items.into_iter().take(MAX_ITEMS).map(Item::clamp).collect();
        for it in &mut items {
            if matches!(it.action, ItemAction::OpenUrl { .. }) && !open_urls {
                it.action = ItemAction::None;
                self.stats.denied += 1;
            }
        }
        Ok(items)
    }

    /// Runs a declared result action. Paths are passed only with the
    /// `selection_paths` permission.
    pub fn action(&mut self, action: &str, paths: &[String]) -> Result<ActionResult, PluginError> {
        if !self.manifest.actions.iter().any(|a| a.id == action) {
            return Err(PluginError::Plugin(format!("unknown action {action}")));
        }
        let paths = if self.granted.contains(&Permission::SelectionPaths) { paths.to_vec() } else { Vec::new() };
        let v = self.call(Call::Action { action: action.to_owned(), paths })?;
        let r: ActionResult = serde_json::from_value(v).map_err(|e| self.fail(PluginError::Protocol(format!("action result: {e}"))))?;
        Ok(r.clamp())
    }
}
