//! Runs approved plugins off the UI thread: one worker thread per plugin,
//! started on first use. Queries are latest-wins (a query still waiting
//! when a newer one arrives is dropped); actions are never dropped. Results
//! carry the caller's generation so stale answers can be ignored.

use crate::host::{Isolation, PluginError, PluginHost, Stats};
use crate::manifest::Manifest;
use crate::protocol::{ActionResult, Item};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Mutex};

#[derive(Clone, Debug, PartialEq)]
pub enum Answer {
    Items(Vec<Item>),
    Action(ActionResult),
}

#[derive(Debug)]
pub struct Delivery {
    pub plugin: String,
    pub generation: u64,
    pub outcome: Result<Answer, PluginError>,
}

pub type Deliver = Arc<dyn Fn(Delivery) + Send + Sync>;

enum Job {
    Query { generation: u64, text: String },
    Action { generation: u64, action: String, paths: Vec<String> },
    Stop,
}

/// What diagnostics show for one plugin.
#[derive(Clone, Debug, Default)]
pub struct Health {
    pub running: bool,
    pub disabled: Option<String>,
    pub stats: Stats,
}

struct Slot {
    manifest: Manifest,
    tx: Mutex<Option<Sender<Job>>>,
    host: Arc<Mutex<Option<PluginHost>>>,
    health: Arc<Mutex<Health>>,
}

pub struct PluginService {
    slots: Vec<Slot>,
    deliver: Deliver,
}

impl PluginService {
    /// `hosts` are approved plugins; nothing starts until it is used.
    pub fn new(hosts: Vec<PluginHost>, deliver: Deliver) -> Self {
        let slots = hosts.into_iter().map(|h| Slot { manifest: h.manifest.clone(), tx: Mutex::new(None), host: Arc::new(Mutex::new(Some(h))), health: Arc::default() }).collect();
        PluginService { slots, deliver }
    }

    pub fn manifests(&self) -> impl Iterator<Item = &Manifest> {
        self.slots.iter().map(|s| &s.manifest)
    }

    /// The plugin whose prefix starts `text` (followed by a space or the
    /// end), with the rest of the text.
    pub fn route<'a>(&self, text: &'a str) -> Option<(&Manifest, &'a str)> {
        let t = text.trim_start();
        self.slots.iter().map(|s| &s.manifest).find_map(|m| {
            let p = m.prefix.as_deref()?;
            let head = t.get(..p.len())?;
            let rest = &t[p.len()..];
            (head.eq_ignore_ascii_case(p) && (rest.is_empty() || rest.starts_with(' '))).then(|| (m, rest.trim_start()))
        })
    }

    fn sender(&self, id: &str) -> Option<Sender<Job>> {
        let slot = self.slots.iter().find(|s| s.manifest.id == id)?;
        let mut tx = slot.tx.lock().unwrap();
        if tx.is_none() {
            let (t, rx) = channel();
            let (host, health, deliver, id) = (slot.host.clone(), slot.health.clone(), self.deliver.clone(), id.to_owned());
            std::thread::Builder::new().name(format!("flint-plugin-worker-{id}")).spawn(move || worker(id, host, health, rx, deliver)).ok()?;
            *tx = Some(t);
        }
        tx.clone()
    }

    pub fn query(&self, id: &str, generation: u64, text: &str) -> bool {
        self.sender(id).is_some_and(|tx| tx.send(Job::Query { generation, text: text.to_owned() }).is_ok())
    }

    pub fn action(&self, id: &str, generation: u64, action: &str, paths: Vec<String>) -> bool {
        self.sender(id).is_some_and(|tx| tx.send(Job::Action { generation, action: action.to_owned(), paths }).is_ok())
    }

    /// Stops every plugin process (they restart on next use). Used when the
    /// launcher hides, so idle plugins cost nothing.
    pub fn stop_all(&self) {
        for s in &self.slots {
            if let Some(tx) = s.tx.lock().unwrap().as_ref() {
                let _ = tx.send(Job::Stop);
            }
        }
    }

    pub fn health(&self) -> Vec<(Manifest, Health)> {
        self.slots.iter().map(|s| (s.manifest.clone(), s.health.lock().unwrap().clone())).collect()
    }
}

impl Drop for PluginService {
    fn drop(&mut self) {
        // Closing the channels ends the workers, which kill their processes.
        for s in &self.slots {
            s.tx.lock().unwrap().take();
        }
    }
}

fn worker(id: String, host: Arc<Mutex<Option<PluginHost>>>, health: Arc<Mutex<Health>>, rx: Receiver<Job>, deliver: Deliver) {
    let Some(mut h) = host.lock().unwrap().take() else { return };
    let mut pending: Option<Job> = None;
    loop {
        let job = match pending.take() {
            Some(j) => j,
            None => match rx.recv() {
                Ok(j) => j,
                Err(_) => break,
            },
        };
        // Latest wins: skip queries superseded by one already queued.
        let job = match job {
            Job::Query { generation, text } => {
                let mut latest = (generation, text);
                loop {
                    match rx.try_recv() {
                        Ok(Job::Query { generation, text }) => latest = (generation, text),
                        Ok(other) => {
                            pending = Some(other);
                            break;
                        }
                        Err(_) => break,
                    }
                }
                Job::Query { generation: latest.0, text: latest.1 }
            }
            j => j,
        };
        let (generation, outcome) = match job {
            Job::Stop => {
                h.stop();
                *health.lock().unwrap() = Health { running: false, disabled: h.disabled().map(str::to_owned), stats: h.stats.clone() };
                continue;
            }
            Job::Query { generation, text } => (generation, h.query(&text).map(Answer::Items)),
            Job::Action { generation, action, paths } => (generation, h.action(&action, &paths).map(Answer::Action)),
        };
        *health.lock().unwrap() = Health { running: h.is_running(), disabled: h.disabled().map(str::to_owned), stats: h.stats.clone() };
        deliver(Delivery { plugin: id.clone(), generation, outcome });
    }
    h.stop();
}

/// Isolation actually obtained, for display.
pub fn isolation_of(h: &Health) -> String {
    h.stats.isolation.as_ref().map(Isolation::describe).unwrap_or_else(|| "not started yet".into())
}
