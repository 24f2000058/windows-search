//! Plugins in the launcher (ADR-019).
//!
//! Off unless `plugins.enabled`. Typing a plugin's prefix sends the rest of
//! the text to it (latest wins) and shows its results as rows. Enter
//! performs the row's action, but only within the plugin's permissions.
//! Unapproved plugins never run: their prefix shows a row that opens the
//! approval prompt. `/plugins` lists every plugin with its standing and
//! health. Approved result actions appear in the context menu. While the
//! launcher is hidden no plugin process runs.

use crate::model::{Row, RowAction};
use crate::win::{Modal, State, WM_APP_PLUGIN};
use flint_plugin::protocol::ItemAction;
use flint_plugin::registry::{self, Approvals, Discovered, Standing};
use flint_plugin::service::{Answer, Delivery, PluginService};
use flint_plugin::Permission;
use std::cell::{Cell, RefCell};
use std::path::PathBuf;
use windows_sys::Win32::Foundation::*;
use windows_sys::Win32::UI::WindowsAndMessaging::*;

/// Context-menu ids for plugin actions: FIRST_MENU_ID + index.
pub const FIRST_MENU_ID: u32 = 9000;

pub struct Plugins {
    pub enabled: bool,
    pub dir: PathBuf,
    approvals_path: PathBuf,
    pub service: RefCell<Option<PluginService>>,
    pub found: RefCell<Vec<(Discovered, Standing)>>,
    pub notes: RefCell<Vec<String>>,
    /// Generation of the plugin query whose results may be shown.
    pub generation: Cell<u64>,
    /// Plugin whose results are on screen (or awaited).
    pub showing: RefCell<Option<String>>,
    menu: RefCell<Vec<(String, String)>>,
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

fn now() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs() as i64)
}

impl Plugins {
    pub fn new(enabled: bool, data_dir: &std::path::Path) -> Plugins {
        Plugins {
            enabled,
            dir: data_dir.join("plugins"),
            approvals_path: data_dir.join("plugin-approvals.toml"),
            service: RefCell::new(None),
            found: RefCell::new(Vec::new()),
            notes: RefCell::new(Vec::new()),
            generation: Cell::new(0),
            showing: RefCell::new(None),
            menu: RefCell::new(Vec::new()),
        }
    }

    /// (Re)discovers plugins and restarts the service with the approved
    /// ones. Nothing is started until a plugin is used.
    pub fn reload(&self, hwnd: HWND) {
        if !self.enabled {
            return;
        }
        let _ = std::fs::create_dir_all(&self.dir);
        let (approvals, note) = Approvals::load(&self.approvals_path);
        self.notes.borrow_mut().clear();
        self.notes.borrow_mut().extend(note);
        let hwnd = hwnd as usize;
        let deliver: flint_plugin::service::Deliver = std::sync::Arc::new(move |d: Delivery| {
            let p = Box::into_raw(Box::new(d));
            unsafe {
                if PostMessageW(hwnd as HWND, WM_APP_PLUGIN, 0, p as LPARAM) == 0 {
                    drop(Box::from_raw(p));
                }
            }
        });
        let (hosts, found) = flint_plugin::load(&self.dir, &approvals, flint_plugin::host::default_spawner(), &[]);
        for (d, st) in &found {
            println!("plugin {}: {}", d.label(), match st {
                Standing::Approved { .. } => "approved".to_owned(),
                Standing::NeedsApproval { reason, .. } => format!("needs approval ({reason})"),
                Standing::Invalid(e) => format!("invalid: {e}"),
            });
            if let Standing::Invalid(e) = st {
                self.notes.borrow_mut().push(format!("plugin {} ignored: {e}", d.label()));
            }
        }
        // Dropping the old service stops its processes.
        *self.service.borrow_mut() = Some(PluginService::new(hosts, deliver));
        *self.found.borrow_mut() = found;
    }

    pub fn stop(&self) {
        if let Some(s) = self.service.borrow().as_ref() {
            s.stop_all();
        }
        self.showing.borrow_mut().take();
    }

    fn name_of(&self, id: &str) -> String {
        self.found.borrow().iter().filter_map(|(d, _)| d.manifest.as_ref().ok()).find(|m| m.id == id).map_or(id.to_owned(), |m| m.name.clone())
    }

    fn granted(&self, id: &str, p: Permission) -> bool {
        self.service.borrow().as_ref().is_some_and(|s| s.manifests().any(|m| m.id == id && m.permissions.contains(&p)))
    }

    /// Result actions of approved plugins, for the context menu.
    pub fn menu_items(&self) -> Vec<(u32, String, bool)> {
        let mut map = self.menu.borrow_mut();
        map.clear();
        let svc = self.service.borrow();
        let Some(svc) = svc.as_ref() else { return Vec::new() };
        let mut items = Vec::new();
        for m in svc.manifests() {
            for a in &m.actions {
                items.push((FIRST_MENU_ID + map.len() as u32, format!("{}  ({})", a.title, m.name), true));
                map.push((m.id.clone(), a.id.clone()));
            }
        }
        items
    }

    pub fn menu_action(&self, id: u32) -> Option<(String, String)> {
        self.menu.borrow().get(id.checked_sub(FIRST_MENU_ID)? as usize).cloned()
    }
}

/// Sends the text to a plugin if it starts with a plugin prefix. Returns
/// true if the input is a plugin's (no file search then).
pub fn route(s: &State, text: &str) -> bool {
    let p = &s.plugins;
    if !p.enabled {
        return false;
    }
    let routed = p.service.borrow().as_ref().and_then(|svc| svc.route(text).map(|(m, rest)| (m.id.clone(), m.name.clone(), rest.to_owned())));
    if let Some((id, name, rest)) = routed {
        let g = p.generation.get() + 1;
        p.generation.set(g);
        let same = p.showing.borrow().as_deref() == Some(id.as_str());
        if !same {
            s.model.borrow_mut().show_rows(Vec::new());
            crate::win::refresh_list(s);
        }
        *p.showing.borrow_mut() = Some(id.clone());
        if let Some(svc) = p.service.borrow().as_ref() {
            svc.query(&id, g, &rest);
        }
        *s.command_line.borrow_mut() = format!("{name} (plugin)");
        return true;
    }
    // A prefix of a plugin that is not approved: offer the review, never run it.
    let t = text.trim_start().to_ascii_lowercase();
    let pending = p.found.borrow().iter().enumerate().find_map(|(i, (d, st))| {
        let m = d.manifest.as_ref().ok()?;
        let pre = m.prefix.as_deref()?;
        let hit = t == pre || t.starts_with(&format!("{pre} "));
        match st {
            Standing::NeedsApproval { reason, .. } if hit => Some((i, m.name.clone(), reason.clone())),
            _ => None,
        }
    });
    if let Some((i, name, reason)) = pending {
        p.showing.borrow_mut().take();
        s.model.borrow_mut().show_rows(vec![Row { title: format!("{name} needs your approval before it can run"), subtitle: format!("{reason} · Enter to review what it asks for"), source: "plugins".into(), action: RowAction::Approve(i) }]);
        crate::win::refresh_list(s);
        return true;
    }
    p.showing.borrow_mut().take();
    false
}

/// `/plugins`: every discovered plugin with its standing.
pub fn list(s: &State) {
    let p = &s.plugins;
    let mut rows = Vec::new();
    if !p.enabled {
        rows.push(Row { title: "Plugins are off".into(), subtitle: "set plugins.enabled = true in config.toml and restart Flint".into(), source: "plugins".into(), action: RowAction::None });
    } else {
        let health = p.service.borrow().as_ref().map(|s| s.health()).unwrap_or_default();
        for (i, (d, st)) in p.found.borrow().iter().enumerate() {
            let (sub, action) = match st {
                Standing::Approved { .. } => {
                    let id = d.manifest.as_ref().map(|m| m.id.clone()).unwrap_or_default();
                    let h = health.iter().find(|(m, _)| m.id == id).map(|(_, h)| h.clone()).unwrap_or_default();
                    let state = match &h.disabled {
                        Some(r) => format!("disabled this session: {r}"),
                        None if h.running => "approved, running".into(),
                        None => "approved".into(),
                    };
                    (format!("{state} · Enter for details"), RowAction::About(i))
                }
                Standing::NeedsApproval { reason, .. } => (format!("needs approval ({reason}) · Enter to review"), RowAction::Approve(i)),
                Standing::Invalid(e) => (format!("not loaded: {e}"), RowAction::About(i)),
            };
            rows.push(Row { title: d.label(), subtitle: sub, source: "plugins".into(), action });
        }
        if rows.is_empty() {
            rows.push(Row { title: "No plugins installed".into(), subtitle: format!("put each plugin in its own folder under {}", p.dir.display()), source: "plugins".into(), action: RowAction::None });
        }
    }
    p.showing.borrow_mut().take();
    s.model.borrow_mut().show_rows(rows);
    crate::win::refresh_list(s);
}

fn message(s: &State, text: &str, flags: u32) -> i32 {
    if !s.selftest.is_empty() {
        // --selftest: print the dialog and answer yes, so runs never block.
        println!("dialog: {}", text.replace('\n', " | "));
        return IDYES;
    }
    let _m = Modal::new(s);
    let (t, c) = (wide(text), wide("Flint plugins"));
    unsafe { MessageBoxW(s.hwnd, t.as_ptr(), c.as_ptr(), flags | MB_TOPMOST) }
}

fn approve(s: &State, i: usize) {
    let (d, st) = match s.plugins.found.borrow().get(i) {
        Some(x) => x.clone(),
        None => return,
    };
    // Re-evaluated now: what the prompt shows is what is on disk.
    let st = match st {
        Standing::NeedsApproval { .. } => registry::standing(&d, &Approvals::load(&s.plugins.approvals_path).0),
        other => other,
    };
    let (Ok(m), Standing::NeedsApproval { reason, sha256 }) = (&d.manifest, &st) else {
        return s.say("this plugin no longer needs approval");
    };
    let text = registry::approval_text(&d, m, sha256, reason);
    if message(s, &text, MB_YESNO | MB_ICONWARNING | MB_DEFBUTTON2) != IDYES {
        return s.say(&format!("{} was not approved and will not run", m.name));
    }
    let (mut ap, _) = Approvals::load(&s.plugins.approvals_path);
    ap.approve(m, sha256.clone(), now());
    match ap.save(&s.plugins.approvals_path) {
        Ok(()) => {
            s.plugins.reload(s.hwnd);
            s.say(&format!("approved {}; type \"{} …\" to use it", m.name, m.prefix.as_deref().unwrap_or("")));
        }
        Err(e) => s.say(&format!("could not save the approval: {e}")),
    }
}

fn about(s: &State, i: usize) {
    let Some((d, st)) = s.plugins.found.borrow().get(i).cloned() else { return };
    let mut text = format!("{}\nFolder: {}\n", d.label(), d.dir.display());
    match (&d.manifest, &st) {
        (Ok(m), Standing::Approved { .. }) => {
            let h = s.plugins.service.borrow().as_ref().and_then(|svc| svc.health().into_iter().find(|(x, _)| x.id == m.id)).map(|(_, h)| h).unwrap_or_default();
            text.push_str(&format!("Version {}\nPermissions: {}\n", m.version, if m.permissions.is_empty() { "none".into() } else { m.permission_set().iter().map(|p| p.describe()).collect::<Vec<_>>().join("; ") }));
            text.push_str(&format!("Isolation: {}\n", flint_plugin::service::isolation_of(&h)));
            text.push_str(&format!("Requests {}, timeouts {}, crashes {}, invalid replies {}\n", h.stats.requests, h.stats.timeouts, h.stats.crashes, h.stats.protocol_errors));
            if let Some(r) = &h.disabled {
                text.push_str(&format!("Disabled for this session: {r}\n"));
            }
            text.push_str("\nRevoke its approval? It will not run again until you approve it.");
            if message(s, &text, MB_YESNO | MB_ICONINFORMATION | MB_DEFBUTTON2) == IDYES {
                let (mut ap, _) = Approvals::load(&s.plugins.approvals_path);
                ap.revoke(&m.id);
                match ap.save(&s.plugins.approvals_path) {
                    Ok(()) => {
                        s.plugins.reload(s.hwnd);
                        s.say(&format!("revoked {}", m.name));
                    }
                    Err(e) => s.say(&format!("could not save: {e}")),
                }
            }
        }
        (_, Standing::Invalid(e)) => {
            text.push_str(&format!("\nNot loaded: {e}"));
            message(s, &text, MB_OK | MB_ICONWARNING);
        }
        _ => {}
    }
}

/// Enter on a row.
pub fn activate(s: &State, row: &Row) {
    match &row.action {
        RowAction::Approve(i) => approve(s, *i),
        RowAction::About(i) => about(s, *i),
        RowAction::Plugin { plugin, action } => match action {
            ItemAction::Copy { text } => match flint_ops::win::copy_text(s.hwnd as usize, text) {
                Ok(()) => s.say("copied"),
                Err(e) => s.say(&format!("copy failed: {e}")),
            },
            ItemAction::OpenUrl { url } => {
                // Checked again here, at the moment of acting.
                if s.plugins.granted(plugin, Permission::OpenUrls) && flint_commands::websearch::is_safe_url(url) {
                    let (op, u) = (wide("open"), wide(url));
                    unsafe { windows_sys::Win32::UI::Shell::ShellExecuteW(std::ptr::null_mut(), op.as_ptr(), u.as_ptr(), std::ptr::null(), std::ptr::null(), SW_SHOWNORMAL) };
                    s.leave();
                } else {
                    s.say("this plugin may not open links");
                }
            }
            ItemAction::Search { query } => crate::win::set_query(s, query),
            ItemAction::None => {}
        },
        RowAction::None => {}
    }
}

/// A plugin answered (UI thread).
pub fn on_delivery(s: &State, d: Delivery) {
    let name = s.plugins.name_of(&d.plugin);
    println!("plugin {} answered generation {} (current {}): {}", d.plugin, d.generation, s.plugins.generation.get(), match &d.outcome {
        Ok(Answer::Items(i)) => format!("{} result(s)", i.len()),
        Ok(Answer::Action(_)) => "action done".into(),
        Err(e) => e.to_string(),
    });
    match d.outcome {
        Ok(Answer::Items(items)) => {
            if d.generation != s.plugins.generation.get() || s.plugins.showing.borrow().as_deref() != Some(d.plugin.as_str()) {
                return; // the query moved on
            }
            let mut rows: Vec<Row> = items.into_iter().map(|it| Row { title: it.title, subtitle: it.subtitle, source: name.clone(), action: RowAction::Plugin { plugin: d.plugin.clone(), action: it.action } }).collect();
            if rows.is_empty() {
                rows.push(Row { title: "no results".into(), subtitle: String::new(), source: name, action: RowAction::None });
            }
            s.model.borrow_mut().show_rows(rows);
            crate::win::refresh_list(s);
        }
        Ok(Answer::Action(r)) => {
            if let Some(t) = &r.copy {
                let _ = flint_ops::win::copy_text(s.hwnd as usize, t);
            }
            s.say(&format!("{name}: {}", if r.message.is_empty() { "done" } else { &r.message }));
        }
        Err(e) => {
            let msg = format!("{name} {e}");
            if d.generation == s.plugins.generation.get() && s.plugins.showing.borrow().as_deref() == Some(d.plugin.as_str()) {
                s.model.borrow_mut().show_rows(vec![Row { title: msg.clone(), subtitle: "the launcher is unaffected; /plugins shows details".into(), source: name, action: RowAction::None }]);
                crate::win::refresh_list(s);
            }
            s.say(&msg);
        }
    }
}

/// A plugin action chosen in the context menu, on the selected results.
pub fn run_menu_action(s: &State, id: u32, paths: Vec<String>) {
    let Some((plugin, action)) = s.plugins.menu_action(id) else { return };
    let g = s.plugins.generation.get();
    if let Some(svc) = s.plugins.service.borrow().as_ref() {
        svc.action(&plugin, g, &action, paths);
    }
    s.say(&format!("{}: working…", s.plugins.name_of(&plugin)));
}
