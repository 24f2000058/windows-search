//! Launcher side of the helper: spawns `flint-preview-host`, enforces a
//! per-request timeout by killing it, and restarts it with exponential
//! backoff after a crash or timeout. A failure costs the one request that
//! caused it.

use crate::wire::{read_frame, write_frame};
use crate::{Extracted, Limits, Op, Outcome, Preview, Request, Response};
use std::io;
use std::path::PathBuf;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{channel, Receiver, RecvTimeoutError};
use std::time::{Duration, Instant};

#[derive(Clone, Debug)]
pub struct HostConfig {
    pub exe: PathBuf,
    /// Longest a single request may take before the helper is killed.
    pub timeout: Duration,
    /// Memory limit for the helper process (Windows Job Object; 0 = none).
    pub memory_limit: u64,
    pub env: Vec<(String, String)>,
    pub backoff_min: Duration,
    pub backoff_max: Duration,
}

impl HostConfig {
    pub fn new(exe: PathBuf) -> Self {
        HostConfig { exe, timeout: Duration::from_secs(3), memory_limit: 256 << 20, env: Vec::new(), backoff_min: Duration::from_millis(100), backoff_max: Duration::from_secs(30) }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HostError {
    /// The helper did not answer in time and was killed.
    Timeout,
    /// The helper exited or broke the protocol while handling the request.
    Crashed(String),
    /// The helper cannot be started (missing, or backing off after failures).
    Unavailable(String),
}

impl std::fmt::Display for HostError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            HostError::Timeout => write!(f, "took too long and was stopped"),
            HostError::Crashed(e) => write!(f, "the preview helper failed on this file ({e})"),
            HostError::Unavailable(e) => write!(f, "preview helper unavailable: {e}"),
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct HostStats {
    pub requests: u64,
    pub timeouts: u64,
    pub crashes: u64,
    pub starts: u64,
}

struct Running {
    child: Child,
    stdin: ChildStdin,
    rx: Receiver<io::Result<Option<Vec<u8>>>>,
    #[cfg(windows)]
    _job: Option<crate::win::Job>,
}

impl Drop for Running {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

pub struct HostClient {
    cfg: HostConfig,
    running: Option<Running>,
    next_id: u64,
    failures: u32,
    retry_at: Option<Instant>,
    pub stats: HostStats,
}

impl HostClient {
    /// Does not start the helper; that happens on the first request.
    pub fn new(cfg: HostConfig) -> Self {
        HostClient { cfg, running: None, next_id: 1, failures: 0, retry_at: None, stats: HostStats::default() }
    }

    fn start(&mut self) -> Result<(), HostError> {
        if let Some(t) = self.retry_at {
            if Instant::now() < t {
                return Err(HostError::Unavailable(format!("restarting after {} failure(s)", self.failures)));
            }
        }
        let mut cmd = Command::new(&self.cfg.exe);
        cmd.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::null());
        for (k, v) in &self.cfg.env {
            cmd.env(k, v);
        }
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            const CREATE_NO_WINDOW: u32 = 0x0800_0000;
            cmd.creation_flags(CREATE_NO_WINDOW);
        }
        let mut child = cmd.spawn().map_err(|e| HostError::Unavailable(format!("{}: {e}", self.cfg.exe.display())))?;
        #[cfg(windows)]
        let job = crate::win::Job::confine(&child, self.cfg.memory_limit);
        let stdin = child.stdin.take().expect("piped");
        let mut stdout = child.stdout.take().expect("piped");
        let (tx, rx) = channel();
        std::thread::Builder::new()
            .name("flint-preview-reader".into())
            .spawn(move || loop {
                let r = read_frame(&mut stdout);
                let end = !matches!(r, Ok(Some(_)));
                if tx.send(r).is_err() || end {
                    return;
                }
            })
            .map_err(|e| HostError::Unavailable(e.to_string()))?;
        self.stats.starts += 1;
        self.running = Some(Running {
            child,
            stdin,
            rx,
            #[cfg(windows)]
            _job: job,
        });
        Ok(())
    }

    fn fail(&mut self, e: HostError) -> HostError {
        self.running = None; // kills and reaps
        self.failures += 1;
        let backoff = self.cfg.backoff_min.saturating_mul(1 << self.failures.min(16).saturating_sub(1)).min(self.cfg.backoff_max);
        self.retry_at = Some(Instant::now() + backoff);
        match e {
            HostError::Timeout => self.stats.timeouts += 1,
            HostError::Crashed(_) => self.stats.crashes += 1,
            HostError::Unavailable(_) => {}
        }
        e
    }

    pub fn request(&mut self, op: Op, path: &str, limits: &Limits) -> Result<Outcome, HostError> {
        if self.running.is_none() {
            self.start()?;
        }
        self.stats.requests += 1;
        let id = self.next_id;
        self.next_id += 1;
        let req = Request { id, op, path: path.to_owned(), limits: limits.clone() };
        let run = self.running.as_mut().unwrap();
        if let Err(e) = write_frame(&mut run.stdin, &req.encode()) {
            return Err(self.fail(HostError::Crashed(format!("write: {e}"))));
        }
        let r = run.rx.recv_timeout(self.cfg.timeout);
        let frame = match r {
            Ok(Ok(Some(f))) => f,
            Ok(Ok(None)) | Err(RecvTimeoutError::Disconnected) => return Err(self.fail(HostError::Crashed("exited".into()))),
            Ok(Err(e)) => return Err(self.fail(HostError::Crashed(e.to_string()))),
            Err(RecvTimeoutError::Timeout) => return Err(self.fail(HostError::Timeout)),
        };
        match Response::decode(&frame) {
            Ok(resp) if resp.id == id => {
                self.failures = 0;
                self.retry_at = None;
                Ok(resp.outcome)
            }
            Ok(_) => Err(self.fail(HostError::Crashed("answered a different request".into()))),
            Err(e) => Err(self.fail(HostError::Crashed(e.to_string()))),
        }
    }

    pub fn preview(&mut self, path: &str, limits: &Limits) -> Result<Preview, String> {
        match self.request(Op::Preview, path, limits) {
            Ok(Outcome::Preview(p)) => Ok(p),
            Ok(Outcome::Failed(e)) => Err(e),
            Ok(_) => Err("unexpected reply".into()),
            Err(e) => Err(e.to_string()),
        }
    }

    pub fn extract(&mut self, path: &str, limits: &Limits) -> Result<Result<Extracted, String>, HostError> {
        match self.request(Op::Extract, path, limits)? {
            Outcome::Extracted(x) => Ok(Ok(x)),
            Outcome::Failed(e) => Ok(Err(e)),
            Outcome::Preview(_) => Ok(Err("unexpected reply".into())),
        }
    }

    pub fn is_running(&self) -> bool {
        self.running.is_some()
    }
}
