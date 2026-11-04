//! TUI-only completion notifications. Preferences are durable; observations,
//! opening overrides, queues and ownership are private to this monitor run.
//! No transcript text is accepted by the desktop payload builder.
pub mod desktop;
use crate::{
    model::{Catalog, Session, project_label},
    paths,
    store::Store,
    tmux::PaneIdentity,
};
use anyhow::{Result, ensure};
use desktop::DesktopBackend;
use std::{
    collections::{HashMap, HashSet},
    fs::{File, OpenOptions},
    os::{
        fd::AsRawFd,
        unix::fs::{MetadataExt, OpenOptionsExt},
    },
    path::Path,
    sync::{Arc, Mutex, mpsc},
    thread::JoinHandle,
};

const MASTER: &str = "notifications.enabled";
const SESSION_PREFIX: &str = "notifications.session.";
const MAX_OPENINGS: usize = 4096;
const QUEUE_SIZE: usize = 32;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Preferences {
    pub enabled: bool,
    pub sessions: HashMap<String, bool>,
}
impl Preferences {
    fn load(store: &Store) -> Result<(Self, String)> {
        let mut preferences = Self::default();
        let mut diagnostic = String::new();
        let rows = store.prefixed_preferences("notifications.")?;
        ensure!(
            rows.len() <= MAX_OPENINGS,
            "too many notification preferences"
        );
        for (key, value) in rows {
            if key != MASTER && !key.starts_with(SESSION_PREFIX) {
                continue;
            }
            let enabled = match value.as_str() {
                "true" => true,
                "false" => false,
                _ => {
                    diagnostic =
                        "Invalid notification preference; using off for that setting".into();
                    false
                }
            };
            if key == MASTER {
                preferences.enabled = enabled;
            } else if let Some(id) = key.strip_prefix(SESSION_PREFIX) {
                preferences.sessions.insert(id.into(), enabled);
            }
        }
        Ok((preferences, diagnostic))
    }
}

/// Frozen input target. No later UI selection can retarget a queued toggle.
#[derive(Debug, Clone)]
pub struct Target {
    pub session: Option<String>,
    pub openings: Vec<String>,
}
impl Target {
    pub fn from_session(session: &Session) -> Self {
        let mut openings = session
            .openings()
            .filter_map(opening_key)
            .map(str::to_owned)
            .collect::<Vec<_>>();
        openings.sort();
        openings.dedup();
        Self {
            session: saved(session).then(|| session.id.clone()),
            openings,
        }
    }
    pub fn available(&self) -> bool {
        self.session.is_some() || !self.openings.is_empty()
    }
}
fn saved(session: &Session) -> bool {
    !session.id.starts_with("pane-") && !session.id.starts_with("live-")
}
pub fn opening_key(pane: &PaneIdentity) -> Option<&str> {
    pane.activity_evidence
        .as_ref()
        .map(|e| e.observation_key.as_str())
        .filter(|key| !key.is_empty())
}

/// Read-only UI snapshot. Rendering never opens preferences or probes a backend.
#[derive(Debug, Clone, Default)]
pub struct NotificationState {
    pub preferences: Preferences,
    pub temporary: HashMap<String, bool>,
    pub backend: String,
    pub owner: bool,
    pub diagnostic: String,
}
impl NotificationState {
    pub fn target_enabled(&self, target: &Target) -> bool {
        let default = target
            .session
            .as_ref()
            .and_then(|id| self.preferences.sessions.get(id))
            .copied()
            .unwrap_or(true);
        if target.openings.is_empty() {
            return default;
        }
        // A session-wide command aligns differing openings to a single setting.
        target
            .openings
            .iter()
            .all(|key| self.temporary.get(key).copied().unwrap_or(default))
    }
    pub fn session_enabled(&self, session: &Session) -> bool {
        self.target_enabled(&Target::from_session(session))
    }
    pub fn can_change_target(&self, target: &Target) -> bool {
        target.available() && (target.session.is_some() || self.owner)
    }
    pub fn summary(&self) -> &'static str {
        if !self.preferences.enabled {
            "Alerts off"
        } else if !self.owner {
            "Alerts on (other instance/unavailable)"
        } else {
            "Alerts on"
        }
    }
    pub fn details(&self, session: Option<&Session>) -> String {
        let mut lines = vec![
            format!("Notifications: {}", self.summary()),
            format!("Desktop backend: {}", self.backend),
        ];
        if let Some(session) = session {
            lines.push(format!(
                "Session notifications: {}",
                if self.session_enabled(session) {
                    "on"
                } else {
                    "off"
                }
            ));
            lines.push(if saved(session) {
                "Preference: saved for this catalog session and all its openings".into()
            } else {
                "Preference: temporary; only for this process/session identity".into()
            });
            let overrides = session
                .openings()
                .filter_map(opening_key)
                .filter(|key| self.temporary.contains_key(*key))
                .count();
            if saved(session) && overrides > 0 {
                lines.push(format!("Temporary overrides on {overrides} opening(s); a aligns them with the saved setting."));
            }
            if !self.preferences.enabled {
                lines.push("All desktop alerts are paused by the global switch".into());
            }
        }
        if !self.owner {
            lines.push(
                "Temporary terminal overrides must be changed in the sending TUI instance.".into(),
            );
        }
        if !self.diagnostic.is_empty() {
            lines.push(format!("Notification diagnostic: {}", self.diagnostic));
        }
        lines.push("Finished means an inferred turn end, not task completion or Done.".into());
        lines.join("\n")
    }
}

#[derive(Debug, Clone)]
pub struct DesktopNotification {
    pub title: String,
    pub body: String,
}
impl DesktopNotification {
    pub fn from_pane(pane: &PaneIdentity) -> Self {
        let bounded = |s: &str, max| paths::line(s).chars().take(max).collect::<String>();
        Self {
            title: format!(
                "{}: turn finished",
                pane.provider.map(|p| p.label()).unwrap_or("agent")
            ),
            body: format!(
                "{} · tmux {} ({})\nTurn end detected; task is not marked Done.",
                bounded(&project_label(&pane.cwd), 80),
                bounded(&pane.target, 100),
                bounded(&pane.pane, 24)
            ),
        }
    }
}

/// Lock lifetime follows the last Arc, including an in-flight desktop command.
/// Never unlink the lock file: doing so could create two different lock inodes.
struct SenderLock(File);
impl SenderLock {
    fn acquire(root: &Path) -> Result<Option<Arc<Self>>> {
        paths::secure_dir(root)?;
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK)
            .open(root.join("notifications.lock"))?;
        let meta = file.metadata()?;
        ensure!(
            meta.is_file() && meta.uid() == unsafe { libc::geteuid() } && meta.mode() & 0o077 == 0,
            "unsafe notification lock file"
        );
        let result = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
        if result == 0 {
            return Ok(Some(Arc::new(Self(file))));
        }
        let error = std::io::Error::last_os_error();
        if error.kind() == std::io::ErrorKind::WouldBlock {
            Ok(None)
        } else {
            Err(error.into())
        }
    }
}
impl Drop for SenderLock {
    fn drop(&mut self) {
        // File close releases flock; an explicit unlock documents that ownership
        // is tied to this open file description, not to a pid stored on disk.
        unsafe {
            libc::flock(self.0.as_raw_fd(), libc::LOCK_UN);
        }
    }
}

#[derive(Default)]
struct DeliveryPolicy {
    alive: bool,
    revision: u64,
    preferences: Preferences,
    temporary: HashMap<String, bool>,
    active: HashSet<String>,
    owner: Option<Arc<SenderLock>>,
}
impl DeliveryPolicy {
    fn permits(&self, key: &str, session: Option<&str>) -> bool {
        self.alive
            && self.owner.is_some()
            && self.preferences.enabled
            && self.active.contains(key)
            && self.temporary.get(key).copied().unwrap_or_else(|| {
                session
                    .and_then(|id| self.preferences.sessions.get(id))
                    .copied()
                    .unwrap_or(true)
            })
    }
}
struct Job {
    key: String,
    session: Option<String>,
    revision: u64,
    notification: DesktopNotification,
}

/// One bounded sender thread per interactive monitor. Side effects are kept out
/// of Service, rendering, input, and all headless CLI paths.
pub struct Notifications {
    state: NotificationState,
    policy: Arc<Mutex<DeliveryPolicy>>,
    feedback: Arc<Mutex<Option<String>>>,
    sender: Option<mpsc::SyncSender<Job>>,
    thread: Option<JoinHandle<()>>,
}
impl Notifications {
    pub fn new(backend: impl DesktopBackend + 'static) -> Self {
        let state = NotificationState {
            backend: backend.label(),
            ..Default::default()
        };
        let policy = Arc::new(Mutex::new(DeliveryPolicy {
            alive: true,
            ..Default::default()
        }));
        let feedback = Arc::new(Mutex::new(None));
        let (sender, jobs) = mpsc::sync_channel::<Job>(QUEUE_SIZE);
        let gate = policy.clone();
        let result = feedback.clone();
        let thread = std::thread::spawn(move || {
            let mut backend = backend;
            while let Ok(job) = jobs.recv() {
                // Clone ownership so it cannot transfer while this command is in
                // flight. A later toggle cancels queued jobs, not delivered ones.
                let owner = {
                    let policy = gate.lock().unwrap();
                    if policy.revision != job.revision
                        || !policy.permits(&job.key, job.session.as_deref())
                    {
                        continue;
                    }
                    policy.owner.clone()
                };
                if let Err(error) = backend.send(&job.notification) {
                    *result.lock().unwrap() = Some(
                        paths::line(&format!("Desktop notification failed: {error:#}"))
                            .chars()
                            .take(512)
                            .collect(),
                    );
                }
                drop(owner);
            }
        });
        Self {
            state,
            policy,
            feedback,
            sender: Some(sender),
            thread: Some(thread),
        }
    }
    pub fn snapshot(&mut self) -> NotificationState {
        if let Some(message) = self.feedback.lock().unwrap().take() {
            self.state.diagnostic = message;
        }
        self.state.clone()
    }
    /// Refresh on real presence ticks, not UI catalog publications. Fail closed
    /// on preference read errors, and retire overrides for vanished identities.
    pub fn refresh(&mut self, store: &Store, root: &Path, catalog: &Catalog) {
        match Preferences::load(store) {
            Ok((preferences, diagnostic)) => {
                self.state.preferences = preferences;
                if !diagnostic.is_empty() {
                    self.state.diagnostic = diagnostic;
                }
            }
            Err(error) => {
                self.state.preferences.enabled = false;
                self.state.diagnostic =
                    paths::line(&format!("Cannot read notification preferences: {error:#}"));
            }
        }
        let active = catalog
            .sessions
            .iter()
            .flat_map(|s| s.openings())
            .chain(catalog.unbound.iter())
            .filter_map(opening_key)
            .take(MAX_OPENINGS)
            .map(str::to_owned)
            .collect::<HashSet<_>>();
        self.state.temporary.retain(|key, _| active.contains(key));
        let mut policy = self.policy.lock().unwrap();
        if policy.owner.is_none() {
            match SenderLock::acquire(root) {
                Ok(owner) => policy.owner = owner,
                Err(error) => {
                    self.state.diagnostic =
                        paths::line(&format!("Notifications unavailable: {error:#}"))
                            .chars()
                            .take(512)
                            .collect()
                }
            }
        }
        self.state.owner = policy.owner.is_some();
        policy.active = active;
        drop(policy);
        self.publish_policy();
    }
    fn publish_policy(&self) {
        let mut policy = self.policy.lock().unwrap();
        if policy.preferences != self.state.preferences || policy.temporary != self.state.temporary
        {
            // Invalidate the small backlog on a setting change, including an
            // off/on cycle that would otherwise revive an old eligible job.
            policy.revision = policy.revision.wrapping_add(1);
            policy.preferences = self.state.preferences.clone();
            policy.temporary = self.state.temporary.clone();
        }
    }
    pub fn set_global(&mut self, store: &Store, enabled: bool) -> Result<()> {
        store.set_preference(MASTER, if enabled { "true" } else { "false" })?;
        self.state.preferences.enabled = enabled;
        self.publish_policy();
        Ok(())
    }
    pub fn set_target(&mut self, store: &Store, target: &Target, enabled: bool) -> Result<()> {
        ensure!(
            target.available(),
            "Wait for a sampled process identity before changing live alerts"
        );
        if let Some(id) = &target.session {
            // Resolve the frozen catalog id, never a title or probable pane id.
            let session = store.resolve(id)?;
            ensure!(
                session.id == *id && saved(&session),
                "saved session required"
            );
            ensure!(
                self.state.preferences.sessions.contains_key(id)
                    || self.state.preferences.sessions.len() < MAX_OPENINGS - 1,
                "too many saved notification preferences"
            );
            store.set_preference(
                &format!("{SESSION_PREFIX}{id}"),
                if enabled { "true" } else { "false" },
            )?;
            self.state.preferences.sessions.insert(id.clone(), enabled);
            for key in &target.openings {
                self.state.temporary.remove(key);
            }
        } else {
            let policy = self.policy.lock().unwrap();
            ensure!(
                policy.owner.is_some(),
                "Temporary alerts are managed by the sending TUI instance; use its controls"
            );
            ensure!(
                target
                    .openings
                    .iter()
                    .all(|key| policy.active.contains(key)),
                "Opening changed or closed; select it again"
            );
            ensure!(
                self.state.temporary.len() + target.openings.len() <= MAX_OPENINGS,
                "too many temporary alert preferences"
            );
            for key in &target.openings {
                self.state.temporary.insert(key.clone(), enabled);
            }
        }
        self.publish_policy();
        Ok(())
    }
    /// Called once with a fresh sample batch. Events rejected here are consumed;
    /// enabling later never replays an already finished turn.
    pub fn consume(&mut self, panes: Vec<PaneIdentity>, catalog: &Catalog) {
        let mut seen = HashSet::new();
        for pane in panes.into_iter().take(MAX_OPENINGS) {
            let Some(key) = opening_key(&pane) else {
                continue;
            };
            if !seen.insert(key.to_string()) {
                continue;
            }
            let mut matches = catalog
                .sessions
                .iter()
                .filter(|s| saved(s) && s.openings().any(|p| opening_key(p) == Some(key)));
            let session = matches.next().map(|s| s.id.clone());
            // Ambiguous history is never resolved by arbitrary association.
            let session = if matches.next().is_none() {
                session
            } else {
                None
            };
            let policy = self.policy.lock().unwrap();
            if !policy.permits(key, session.as_deref()) {
                continue;
            }
            let job = Job {
                key: key.into(),
                session,
                revision: policy.revision,
                notification: DesktopNotification::from_pane(&pane),
            };
            if self
                .sender
                .as_ref()
                .is_some_and(|sender| sender.try_send(job).is_err())
            {
                self.state.diagnostic = "Notification queue busy; alert dropped (no retry)".into();
            }
        }
    }
    /// A failed overall refresh cannot leave previously sampled openings eligible.
    pub fn unavailable(&mut self) {
        self.policy.lock().unwrap().active.clear();
    }
}
#[cfg(test)]
mod tests;

impl Drop for Notifications {
    fn drop(&mut self) {
        {
            let mut policy = self.policy.lock().unwrap();
            policy.alive = false;
            policy.owner = None;
        }
        self.sender.take();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}
