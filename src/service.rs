//! Shared CLI/TUI operations: indexing, catalog assembly, and resume specs.
//!
//! `catalog()` merges durable SQLite rows with live tmux/extension
//! presence plus ephemeral screen activity (Herdr-style, terminal only).
//! Activity is recomputed per call and never persisted: screen state is
//! a guess about panes, never a fact about conversations.
use crate::{
    activity,
    model::*,
    opencode, paths, pi, presence,
    store::{Source, Store},
    tmux,
};
use anyhow::{Context, Result, ensure};
use std::{
    collections::VecDeque,
    fs,
    os::unix::fs::MetadataExt,
    path::{Path, PathBuf},
};

/// Opened state root plus SQLite store.
pub struct Service {
    pub root: PathBuf,
    pub store: Store,
    activity: std::cell::RefCell<activity::ActivityTracker>,
    activity_started: std::time::Instant,
}

/// Incremental scan state.
pub struct Indexer {
    queue: VecDeque<(Source, PathBuf, usize)>,
    sources: Vec<Source>,
    pub warnings: Vec<String>,
    pub initial: bool,
    failed: std::collections::HashSet<PathBuf>,
    names: std::collections::HashMap<PathBuf, std::collections::HashMap<String, String>>,
}
impl Indexer {
    pub fn pending(&self) -> bool {
        !self.queue.is_empty()
    }
}
impl Service {
    pub fn open(root: Option<PathBuf>) -> Result<Self> {
        let root = paths::root(root)?;
        let store = Store::open(&root)?;
        let dir = default_source()?;
        store.add_source(&dir)?;
        let standard = paths::pi_dir().join("sessions");
        if standard.is_dir() {
            store.add_source(&standard)?;
        }
        for path in other_sources() {
            if path.exists() {
                store.add_source(&path)?;
            }
        }
        Ok(Self {
            root,
            store,
            activity: Default::default(),
            activity_started: std::time::Instant::now(),
        })
    }
    pub fn begin_index(&self) -> Result<Indexer> {
        let sources = self.store.sources()?;
        let mut indexer = Indexer {
            queue: VecDeque::new(),
            initial: sources.iter().any(|s| !s.complete),
            sources,
            warnings: vec![],
            failed: Default::default(),
            names: Default::default(),
        };
        for source in &indexer.sources {
            if let Some(parent) = source.path.parent() {
                let titles = parent.join("session_index.jsonl");
                if titles.is_file() {
                    match crate::formats::codex_names(&titles) {
                        Ok(names) => {
                            indexer.names.insert(source.path.clone(), names);
                        }
                        Err(e) => indexer.warnings.push(format!(
                            "Codex titles unavailable: {}",
                            paths::line(&e.to_string())
                        )),
                    }
                }
            }
            match if source.path.is_file() {
                Ok(vec![paths::canonical(&source.path)])
            } else {
                pi::files(&source.path)
            } {
                Ok(mut files) => {
                    let own = std::env::current_dir().ok();
                    files.sort_by_key(|p| {
                        !own.as_ref().is_some_and(|c| {
                            p.to_string_lossy()
                                .contains(&paths::line(&c.to_string_lossy()).replace('/', "-"))
                        })
                    });
                    indexer
                        .queue
                        .extend(files.into_iter().map(|p| (source.clone(), p, 0)));
                }
                Err(_) => {
                    indexer.failed.insert(source.path.clone());
                    indexer.warnings.push(format!(
                        "Unreadable source: {}",
                        paths::line(&source.path.to_string_lossy())
                    ));
                }
            }
        }
        Ok(indexer)
    }
    pub fn index_step(&self, indexer: &mut Indexer) -> Result<()> {
        if let Some((source, path, offset)) = indexer.queue.pop_front() {
            if path.extension().is_some_and(|e| e == "db") {
                match opencode::batch(&path, offset) {
                    Ok((cursors, more)) => {
                        for cursor in cursors {
                            self.store.ingest(&source, &cursor)?;
                        }
                        if more {
                            indexer.queue.push_front((source, path, offset + 100));
                        }
                    }
                    Err(e) => {
                        indexer.failed.insert(source.path.clone());
                        indexer.warnings.push(format!(
                            "OpenCode source unavailable: {}",
                            paths::line(&e.to_string())
                        ));
                    }
                }
            } else {
                let previous = self.store.cursor(&path)?;
                let recognized = previous.is_some() || pi::is_session(&path).unwrap_or(true);
                let stat = fs::metadata(&path);
                let unchanged = previous
                    .as_ref()
                    .zip(stat.as_ref().ok())
                    .is_some_and(|(c, m)| {
                        c.offset == m.len()
                            && c.size == m.len()
                            && c.inode == m.ino()
                            && c.device == m.dev()
                            && c.mtime == m.mtime() * 1_000_000_000 + m.mtime_nsec()
                    });
                if recognized && !unchanged {
                    match pi::scan(&path, previous.as_ref()) {
                        Ok((mut cursor, more)) => {
                            if cursor.metadata.provider == Provider::Codex
                                && let Some(name) = indexer
                                    .names
                                    .get(&source.path)
                                    .and_then(|names| names.get(&cursor.metadata.native_id))
                            {
                                cursor.metadata.name = name.clone();
                            }
                            self.store.ingest(&source, &cursor)?;
                            if more {
                                indexer.queue.push_front((source, path, 0));
                            }
                        }
                        Err(_) => {
                            indexer.failed.insert(source.path.clone());
                            indexer.warnings.push(format!(
                                "Unreadable or incomplete session: {}",
                                paths::line(&path.to_string_lossy())
                            ));
                        }
                    }
                } else if unchanged
                    && let Some(mut cursor) = previous
                    && cursor.metadata.provider == Provider::Codex
                    && let Some(name) = indexer
                        .names
                        .get(&source.path)
                        .and_then(|names| names.get(&cursor.metadata.native_id))
                    && cursor.metadata.name != *name
                {
                    cursor.metadata.name = name.clone();
                    self.store.ingest(&source, &cursor)?;
                }
            }
        }
        if !indexer.pending() {
            for source in &indexer.sources {
                if !indexer.failed.contains(&source.path) {
                    self.store.complete(&source.path)?;
                }
            }
            for session in self.store.list()? {
                if !session.metadata.source_file().exists()
                    || session.metadata.database.as_ref().is_some_and(|db| {
                        opencode::contains(db, &session.metadata.native_id)
                            .is_ok_and(|present| !present)
                    })
                {
                    self.store.mark_missing(&session.metadata.file)?;
                }
            }
        }
        Ok(())
    }
    pub fn sync(&self) -> Result<Vec<String>> {
        let mut indexer = self.begin_index()?;
        loop {
            self.index_step(&mut indexer)?;
            if !indexer.pending() {
                break;
            }
        }
        Ok(indexer.warnings)
    }
    pub fn catalog(&self) -> Result<Catalog> {
        let mut warnings = vec![];
        let mut panes = match tmux::discover(&tmux::TmuxConfig::default()) {
            Ok(p) => p,
            Err(_) => {
                warnings.push("tmux presence is unverified".into());
                vec![]
            }
        };
        let mut bindings = match presence::records(&self.root, &panes) {
            Ok(b) => b,
            Err(e) => {
                warnings.push(format!(
                    "Pi presence is unverified: {}",
                    paths::line(&e.to_string())
                ));
                vec![]
            }
        };
        attach_activity(
            &mut panes,
            &bindings,
            &mut self.activity.borrow_mut(),
            self.activity_started
                .elapsed()
                .as_millis()
                .min(u64::MAX as u128) as u64,
        );
        for binding in &mut bindings {
            if let Some(old) = &binding.pane
                && let Some(fresh) = panes.iter().find(|p| {
                    p.socket == old.socket
                        && p.pane == old.pane
                        && p.client_pid == old.client_pid
                        && p.client_start == old.client_start
                })
            {
                binding.pane = Some(fresh.clone());
            }
        }
        // Project settings reveal additional storage roots, not session identity.
        for cwd in panes
            .iter()
            .filter(|p| p.provider == Some(Provider::Pi))
            .map(|p| &p.cwd)
            .collect::<std::collections::BTreeSet<_>>()
        {
            if let Some(dir) = project_source(Path::new(cwd)) {
                self.store.add_source(&dir)?;
            }
        }
        // External --session/--session-dir files are exact sources, never cwd guesses.
        for binding in &bindings {
            if let Some(file) = &binding.bridge.file
                && file.exists()
            {
                let path = paths::canonical(file);
                if self.store.cursor(&path)?.is_none() {
                    self.store.add_source(&path)?;
                    let source = self
                        .store
                        .sources()?
                        .into_iter()
                        .find(|s| s.path == path)
                        .context("source is missing")?;
                    if let Ok((cursor, _)) = pi::scan(&path, None) {
                        self.store.ingest(&source, &cursor)?;
                    }
                }
            }
        }
        if tmux::unreported_pi(&bindings.iter().map(|b| b.bridge.pid).collect::<Vec<_>>())? > 0 {
            warnings
                .push("Some Pi runs have no extension: probable openings are marked [~]".into());
        }
        let mut sessions = self.store.list()?;
        for session in &mut sessions {
            session.presence_verified = warnings.is_empty();
            session.bindings = bindings
                .iter()
                .filter(|b| {
                    session.metadata.provider == Provider::Pi
                        && b.bridge.native_id == session.metadata.native_id
                        && b.bridge
                            .file
                            .as_ref()
                            .is_some_and(|p| paths::canonical(p) == session.metadata.file)
                })
                .cloned()
                .collect();
            session.live_name();
        }
        for binding in &bindings {
            if !sessions.iter().any(|s| {
                s.bindings
                    .iter()
                    .any(|b| b.bridge.nonce == binding.bridge.nonce)
            }) {
                sessions.push(Session {
                    id: format!("live-{}", binding.bridge.nonce),
                    metadata: Metadata {
                        native_id: binding.bridge.native_id.clone(),
                        file: binding.bridge.file.clone().unwrap_or_default(),
                        cwd: binding.bridge.cwd.clone(),
                        name: binding.bridge.name.clone(),
                        warning: "Session not saved by Pi yet (or --no-session)".into(),
                        ..Default::default()
                    },
                    state: ResumeState::History,
                    note: String::new(),
                    done_revision: 0,
                    done_at: 0,
                    available: false,
                    presence_verified: true,
                    bindings: vec![binding.clone()],
                    probable: vec![],
                });
            }
        }
        // Every recognized agent without a verified binding stays visible.
        // Pi-only consumers (associate_probable, project-source scan,
        // unreported_pi, [~] hints) filter explicitly below; nothing here
        // is Pi-specific anymore.
        let unbound: Vec<tmux::PaneIdentity> = panes
            .into_iter()
            .filter(|p| {
                p.provider.is_some()
                    && !bindings.iter().any(|b| {
                        b.pane
                            .as_ref()
                            .is_some_and(|bp| bp.socket == p.socket && bp.pane == p.pane)
                    })
            })
            .collect();
        if !unbound.is_empty() {
            for session in &mut sessions {
                session.presence_verified = false;
            }
        }
        associate_probable(&mut sessions, &unbound);
        append_live_panes(&mut sessions, &unbound);
        Ok(Catalog {
            sessions,
            unbound,
            warnings,
            scanning: false,
        })
    }
    pub fn detail(&self, target: &str, leaf: Option<&str>, tools: bool) -> Result<Conversation> {
        self.detail_page(target, leaf, tools, 0)
    }
    pub fn detail_page(
        &self,
        target: &str,
        leaf: Option<&str>,
        tools: bool,
        page: usize,
    ) -> Result<Conversation> {
        let session = self.store.resolve(target)?;
        ensure!(session.available, "conversation is unavailable");
        let current = session_identity(&session.metadata)?;
        ensure!(
            current.native_id == session.metadata.native_id
                && current.provider == session.metadata.provider,
            "session identity changed"
        );
        if let Some(db) = &session.metadata.database {
            opencode::conversation(db, &session.metadata.native_id, tools, page)
        } else {
            pi::conversation_page(&session.metadata.file, leaf, tools, page)
        }
    }
    pub fn resolve<'a>(&self, catalog: &'a Catalog, target: &str) -> Result<&'a Session> {
        let matches = catalog
            .sessions
            .iter()
            .filter(|s| s.id == target || s.metadata.native_id == target)
            .collect::<Vec<_>>();
        ensure!(matches.len() <= 1, "ambiguous id: choose the exact session");
        matches.into_iter().next().context("session not found")
    }
    /// Build a resume plan without starting anything. The UI/CLI shows it,
    /// asks for confirmation, then rebuilds and rechecks it right before launch.
    pub fn resume_spec(&self, target: &str) -> Result<ResumeSpec> {
        let catalog = self.catalog()?;
        let session = self.resolve(&catalog, target)?;
        ensure!(
            session.bindings.is_empty(),
            "session is already open: use the existing pane"
        );
        ensure!(
            session.probable.is_empty(),
            "Probable opening: pick the pane in the monitor instead of starting a possible duplicate"
        );
        // Unidentified runs of the same provider require an explicit warning;
        // unrelated providers cannot hold this conversation.
        let provider_unbound = catalog
            .unbound
            .iter()
            .any(|p| p.provider == Some(session.metadata.provider));
        let warning = if provider_unbound || !catalog.warnings.is_empty() {
            format!(
                "Some {} runs are unidentified: another opening of this conversation cannot be ruled out.",
                session.metadata.provider.label()
            )
        } else {
            String::new()
        };
        ensure!(
            session.available && session.metadata.source_file().is_file(),
            "session file is unavailable"
        );
        ensure!(
            Path::new(&session.metadata.cwd).is_dir(),
            "project directory is unavailable"
        );
        let identity = session_identity(&session.metadata)?;
        ensure!(
            identity.native_id == session.metadata.native_id
                && identity.provider == session.metadata.provider,
            "session identity changed"
        );
        Ok(ResumeSpec {
            file: session.metadata.source_file().to_path_buf(),
            provider: session.metadata.provider,
            cwd: session.metadata.cwd.clone(),
            title: session.metadata.title(),
            native_id: session.metadata.native_id.clone(),
            warning,
        })
    }
}
/// Identity verification and screen activity are independent. Capture ALL
/// known agent panes, including verified Pi openings, without keeping text.
fn attach_activity(
    panes: &mut [tmux::PaneIdentity],
    bindings: &[Binding],
    tracker: &mut activity::ActivityTracker,
    now_ms: u64,
) {
    attach_activity_sampled(panes, bindings, tracker, now_ms, |pane| {
        let tail = paths::clean(&tmux::capture_pane(&pane.socket, &pane.pane, 80)?);
        let signature = (!tail.trim().is_empty()).then(|| pi::hash(tail.as_bytes()));
        let (state, evidence) = activity::classify(
            pane.provider.context("agent identity missing")?,
            &pane.title,
            &tail,
        );
        Ok((state, evidence, signature))
    });
}
fn attach_activity_sampled(
    panes: &mut [tmux::PaneIdentity],
    bindings: &[Binding],
    tracker: &mut activity::ActivityTracker,
    now_ms: u64,
    mut sample: impl FnMut(
        &tmux::PaneIdentity,
    ) -> Result<(
        activity::AgentActivity,
        activity::MatchEvidence,
        Option<u64>,
    )>,
) {
    let mut active = std::collections::HashSet::new();
    for pane in panes {
        let binding = bindings.iter().find(|b| {
            b.pane
                .as_ref()
                .is_some_and(|p| p.socket == pane.socket && p.pane == pane.pane)
        });
        if binding.is_some() && pane.provider.is_none() {
            pane.provider = Some(Provider::Pi);
        }
        if pane.provider.is_none() {
            continue;
        }
        let generation = binding
            .map(|b| {
                format!(
                    "{}:{}:{}",
                    b.bridge.nonce, b.bridge.generation, b.bridge.native_id
                )
            })
            .unwrap_or_default();
        let key = activity::pane_key(pane, &generation);
        active.insert(key.clone());
        let (state, mut evidence, signature) = sample(pane).unwrap_or_else(|_| {
            (
                activity::AgentActivity::Unknown,
                activity::MatchEvidence {
                    fallback_reason: "terminal-capture-unavailable".into(),
                    ..Default::default()
                },
                None,
            )
        });
        evidence.sampled_at = paths::now();
        let (state, evidence) = tracker.observe(&key, state, evidence, signature, now_ms);
        pane.activity = Some(state);
        pane.activity_evidence = Some(evidence);
    }
    tracker.retain(&active);
}

/// Reattach ephemeral presence to fresh store rows between full catalog
/// rebuilds. Bindings and verification are carried over by session id;
/// store rows never hold live state. Returns the live-only (`live-*`)
/// sessions so the caller can append them. Callers must still run
/// `associate_probable` after this.
pub fn reattach_presence(current: &mut [Session], previous: &[Session]) -> Vec<Session> {
    let known: std::collections::HashMap<&str, &Session> =
        previous.iter().map(|s| (s.id.as_str(), s)).collect();
    for session in current.iter_mut() {
        if let Some(old) = known.get(session.id.as_str()) {
            session.bindings = old.bindings.clone();
            session.presence_verified = old.presence_verified;
            session.live_name();
        }
    }
    previous
        .iter()
        .filter(|s| s.id.starts_with("live-"))
        .cloned()
        .collect()
}

/// Attach passive title/cwd hints. Exact unique matches only; duplicates
/// stay unassociated and stored state is never changed.
pub fn associate_probable(sessions: &mut [Session], panes: &[tmux::PaneIdentity]) {
    for session in sessions.iter_mut() {
        session.probable.clear();
    }
    for pane in panes.iter().filter(|p| p.provider == Some(Provider::Pi)) {
        let cwd = paths::canonical(Path::new(&pane.cwd))
            .to_string_lossy()
            .into_owned();
        let title = paths::line(&pane.title);
        let Some(name) = title
            .strip_prefix("π - ")
            .or_else(|| title.strip_prefix("pi - "))
            .and_then(|s| s.strip_suffix(&format!(" - {}", project_name(&cwd))))
        else {
            continue;
        };
        if name.is_empty() {
            continue;
        }
        let matches = sessions
            .iter()
            .enumerate()
            .filter(|(_, s)| {
                s.available
                    && s.metadata.provider == Provider::Pi
                    && s.metadata.cwd == cwd
                    && s.metadata.name == name
            })
            .map(|(i, _)| i)
            .collect::<Vec<_>>();
        if matches.len() == 1 && sessions[matches[0]].bindings.is_empty() {
            sessions[matches[0]].probable.push(pane.clone());
        }
    }
}
/// Unidentified terminals are selectable rows, not hidden in leftover space.
/// These rows are ephemeral and never inherit a saved conversation's decisions.
pub fn append_live_panes(sessions: &mut Vec<Session>, panes: &[tmux::PaneIdentity]) {
    for pane in panes {
        if sessions.iter().any(|s| {
            s.probable.iter().any(|p| {
                tmux::socket_key(&p.socket) == tmux::socket_key(&pane.socket) && p.pane == pane.pane
            })
        }) {
            continue;
        }
        let Some(provider) = pane.provider else {
            continue;
        };
        sessions.push(Session {
            id: format!("pane-{:x}-{}-{}-{:x}", pi::hash(tmux::socket_key(&pane.socket).as_bytes()), pane.pane, pane.client_pid, pi::hash(pane.client_start.as_bytes())),
            metadata: Metadata { provider, cwd: paths::canonical(Path::new(&pane.cwd)).to_string_lossy().into_owned(),
                name: paths::line(&pane.title), warning: "Live terminal; saved conversation identity is unverified. Enter focuses the pane without starting another agent.".into(), ..Default::default() },
            state: ResumeState::History, note: String::new(), done_revision: 0, done_at: 0,
            available: false, presence_verified: false, bindings: vec![], probable: vec![pane.clone()],
        });
    }
}
fn session_identity(meta: &Metadata) -> Result<Metadata> {
    if let Some(db) = &meta.database {
        opencode::identity(db, &meta.native_id)
    } else {
        pi::header(&meta.file)
    }
}
fn other_sources() -> Vec<PathBuf> {
    let home = paths::expand_home(Path::new("~"));
    let claude = std::env::var_os("CLAUDE_CONFIG_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| home.join(".claude"));
    let codex = std::env::var_os("CODEX_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| home.join(".codex"));
    let data = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| home.join(".local/share"));
    vec![
        paths::canonical(&claude.join("projects")),
        paths::canonical(&codex.join("sessions")),
        paths::canonical(&data.join("opencode/opencode.db")),
    ]
}
fn project_source(cwd: &Path) -> Option<PathBuf> {
    use std::io::Read;
    let file = fs::File::open(cwd.join(".pi/settings.json")).ok()?;
    let mut text = String::new();
    file.take(1024 * 1024).read_to_string(&mut text).ok()?;
    let settings: serde_json::Value = serde_json::from_str(&text).ok()?;
    let path = paths::expand_home(Path::new(settings["sessionDir"].as_str()?));
    let path = paths::canonical(&if path.is_absolute() {
        path
    } else {
        cwd.join(path)
    });
    path.is_dir().then_some(path)
}

#[derive(Debug, Clone)]
pub struct ResumeSpec {
    pub file: PathBuf,
    pub provider: Provider,
    pub cwd: String,
    pub title: String,
    pub native_id: String,
    pub warning: String,
}
impl ResumeSpec {
    pub fn argv(&self) -> Vec<String> {
        match self.provider {
            Provider::Pi => vec![
                "pi".into(),
                "--session".into(),
                self.file.to_string_lossy().into_owned(),
            ],
            Provider::Claude => vec!["claude".into(), "--resume".into(), self.native_id.clone()],
            Provider::Codex => vec!["codex".into(), "resume".into(), self.native_id.clone()],
            Provider::Opencode => vec![
                "opencode".into(),
                "--session".into(),
                self.native_id.clone(),
            ],
        }
    }
    pub fn launch(&self) -> Result<()> {
        let identity = if self.provider == Provider::Opencode {
            opencode::identity(&self.file, &self.native_id)?
        } else {
            pi::header(&self.file)?
        };
        ensure!(
            identity.provider == self.provider
                && identity.native_id == self.native_id
                && paths::canonical(Path::new(&identity.cwd))
                    == paths::canonical(Path::new(&self.cwd)),
            "session identity changed before launch"
        );
        ensure!(self.file.to_str().is_some(), "session path is not UTF-8");
        ensure!(
            Path::new(&self.cwd).is_dir(),
            "project directory is unavailable"
        );
        crate::tmux::resume_command(&self.argv(), Path::new(&self.cwd), &self.title)
    }
    #[cfg(test)]
    fn launch_program(&self, program: &std::ffi::OsStr) -> Result<()> {
        let status = std::process::Command::new(program)
            .args(self.argv().into_iter().skip(1))
            .current_dir(&self.cwd)
            .status()?;
        ensure!(status.success(), "Pi exited with an error");
        Ok(())
    }
}
fn default_source() -> Result<PathBuf> {
    if let Some(dir) = std::env::var_os("PI_CODING_AGENT_SESSION_DIR") {
        return absolute(PathBuf::from(dir));
    }
    let agent = paths::pi_dir();
    for settings in [
        std::env::current_dir()?.join(".pi/settings.json"),
        agent.join("settings.json"),
    ] {
        if let Ok(file) = fs::File::open(settings) {
            use std::io::Read;
            let mut data = String::new();
            file.take(1024 * 1024).read_to_string(&mut data)?;
            if let Ok(value) = serde_json::from_str::<serde_json::Value>(&data)
                && let Some(dir) = value["sessionDir"].as_str()
            {
                return absolute(PathBuf::from(dir));
            }
        }
    }
    absolute(agent.join("sessions"))
}
fn absolute(path: PathBuf) -> Result<PathBuf> {
    let path = paths::expand_home(&path);
    Ok(paths::canonical(&if path.is_absolute() {
        path
    } else {
        std::env::current_dir()?.join(path)
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    fn pane(title: &str) -> tmux::PaneIdentity {
        tmux::PaneIdentity {
            socket: "/fake/socket".into(),
            server: "1".into(),
            pane: "%1".into(),
            pane_pid: 1,
            pane_start: "start".into(),
            client_pid: 2,
            client_start: "start".into(),
            target: "project:1.0".into(),
            session: "project".into(),
            window: "1".into(),
            cwd: "/repo/project".into(),
            command: "pi".into(),
            title: title.into(),
            provider: Some(Provider::Pi),
            activity: None,
            activity_evidence: None,
        }
    }
    fn session() -> Session {
        Session {
            id: "one".into(),
            metadata: Metadata {
                name: "release-notes".into(),
                cwd: "/repo/project".into(),
                ..Default::default()
            },
            state: ResumeState::History,
            note: "retained".into(),
            done_revision: 0,
            done_at: 0,
            available: true,
            presence_verified: false,
            bindings: vec![],
            probable: vec![],
        }
    }
    #[test]
    fn non_pi_panes_stay_visible_while_pi_only_paths_ignore_them() {
        // attach_activity shells out to tmux, so test the widening rule
        // directly: any recognized agent without a binding is unbound.
        let mut codex = pane("anything");
        codex.provider = Some(Provider::Codex);
        codex.command = "codex".into();
        let unbound: Vec<_> = [pane("π - release-notes - project"), codex]
            .into_iter()
            .filter(|p| p.provider.is_some())
            .collect();
        assert_eq!(unbound.len(), 2);
        // …while Pi-only consumers still filter explicitly.
        let mut sessions = vec![session()];
        associate_probable(&mut sessions, &unbound);
        assert_eq!(sessions[0].probable.len(), 1);
        // reattach_presence carries bindings like the old inline block.
        let mut fresh = vec![session()];
        let live = reattach_presence(&mut fresh, &sessions);
        fresh.extend(live);
        assert_eq!(fresh.len(), 1);
    }
    #[test]
    fn verified_panes_are_sampled_and_generations_failures_and_live_states_do_not_change_decisions()
    {
        let mut panes = vec![pane("π - release-notes")];
        let mut bindings = vec![Binding {
            bridge: Bridge {
                version: 1,
                nonce: "fixture".into(),
                generation: 1,
                pid: 2,
                process_start: "start".into(),
                native_id: "native".into(),
                file: None,
                cwd: "/repo/project".into(),
                name: "release-notes".into(),
                leaf: None,
                socket: Some("/fake/socket".into()),
                pane: Some("%1".into()),
                seen: 0,
            },
            record: "/fake/record.json".into(),
            pane: Some(panes[0].clone()),
        }];
        let mut tracker = activity::ActivityTracker::default();
        let sample = |text: &'static str| {
            move |p: &tmux::PaneIdentity| {
                let (state, evidence) = activity::classify_with_override(
                    Provider::Pi,
                    &p.title,
                    text,
                    Some(include_str!("../assets/activity/pi.toml")),
                );
                Ok((state, evidence, Some(pi::hash(text.as_bytes()))))
            }
        };
        attach_activity_sampled(&mut panes, &bindings, &mut tracker, 0, sample("⠼ Working"));
        assert_eq!(panes[0].activity, Some(activity::AgentActivity::Working));
        attach_activity_sampled(&mut panes, &bindings, &mut tracker, 2000, sample(">"));
        attach_activity_sampled(&mut panes, &bindings, &mut tracker, 4000, sample(">"));
        assert_eq!(panes[0].activity, Some(activity::AgentActivity::Finished));
        bindings[0].pane = Some(panes[0].clone());
        let mut saved = session();
        saved.state = ResumeState::Done;
        saved.note = "Keep my note".into();
        saved.bindings = bindings.clone();
        assert_eq!(
            saved.live_activity(),
            Some(activity::AgentActivity::Finished)
        );
        assert_eq!(saved.state, ResumeState::Done);
        assert_eq!(saved.note, "Keep my note");
        saved.probable.push({
            let mut p = panes[0].clone();
            p.activity = Some(activity::AgentActivity::Working);
            p
        });
        assert_eq!(saved.live_activity(), Some(activity::AgentActivity::Mixed));
        bindings[0].bridge.generation = 2;
        attach_activity_sampled(&mut panes, &bindings, &mut tracker, 6000, sample(">"));
        assert_eq!(panes[0].activity, Some(activity::AgentActivity::Idle));
        attach_activity_sampled(&mut panes, &bindings, &mut tracker, 8000, |_| {
            anyhow::bail!("fake capture failure")
        });
        assert_eq!(panes[0].activity, Some(activity::AgentActivity::Unknown));
        assert_eq!(
            panes[0].activity_evidence.as_ref().unwrap().fallback_reason,
            "terminal-capture-unavailable"
        );
        assert_eq!(saved.state, ResumeState::Done);
    }
    #[test]
    fn passive_titles_are_hints_and_duplicates_are_never_guessed() {
        let pane = pane("π - release-notes - project");
        let mut sessions = vec![session()];
        associate_probable(&mut sessions, std::slice::from_ref(&pane));
        assert_eq!(sessions[0].probable.len(), 1);
        assert!(sessions[0].bindings.is_empty());
        assert_eq!(sessions[0].state, ResumeState::History);
        assert_eq!(sessions[0].note, "retained");
        assert_eq!(sessions[0].presence(), "Probable opening");
        assert!(View::Open.matches(&sessions[0]));
        sessions.push(session());
        associate_probable(&mut sessions, std::slice::from_ref(&pane));
        assert!(sessions.iter().all(|s| s.probable.is_empty()));
        sessions.pop();
        let mut other = pane.clone();
        other.cwd = "/repo/other".into();
        associate_probable(&mut sessions, &[other]);
        assert!(sessions[0].probable.is_empty());
        associate_probable(&mut sessions, &[pane.clone(), pane]);
        assert_eq!(sessions[0].probable.len(), 2);
        associate_probable(&mut sessions, &[]);
        assert!(sessions[0].probable.is_empty());
    }
    #[test]
    fn codex_title_refresh_preserves_notes_done_and_content_revision() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("sessions");
        fs::create_dir(&source).unwrap();
        let file = source.join("rollout.jsonl");
        let header = serde_json::json!({"type":"session_meta","payload":{"id":"native","cwd":dir.path(),"timestamp":"2026-01-01T00:00:00Z"}});
        let msg = serde_json::json!({"type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"Fix tests"}]}});
        fs::write(&file, format!("{header}\n{msg}\n")).unwrap();
        let titles = dir.path().join("session_index.jsonl");
        fs::write(
            &titles,
            "{\"id\":\"native\",\"thread_name\":\"Test repair\"}\n",
        )
        .unwrap();
        let root = dir.path().join("state");
        let store = Store::open(&root).unwrap();
        store.add_source(&source).unwrap();
        let service = Service {
            root,
            store,
            activity: Default::default(),
            activity_started: std::time::Instant::now(),
        };
        assert!(service.sync().unwrap().is_empty());
        service
            .store
            .change("native", Some(ResumeState::Done), Some("Keep this note"))
            .unwrap();
        let before = service.store.resolve("native").unwrap();
        assert_eq!(before.metadata.title(), "Test repair");
        fs::write(
            &titles,
            "{\"id\":\"native\",\"thread_name\":\"Renamed test repair\"}\n",
        )
        .unwrap();
        assert!(service.sync().unwrap().is_empty());
        let after = service.store.resolve("native").unwrap();
        assert_eq!(after.metadata.title(), "Renamed test repair");
        assert_eq!(after.metadata.revision, before.metadata.revision);
        assert_eq!(after.state, ResumeState::Done);
        assert_eq!(after.note, "Keep this note");
        assert!(!after.changed_after_done());
    }
    #[test]
    fn every_unidentified_provider_is_a_searchable_ephemeral_row() {
        let providers = [
            Provider::Pi,
            Provider::Claude,
            Provider::Codex,
            Provider::Opencode,
        ];
        let panes = providers
            .iter()
            .enumerate()
            .map(|(i, provider)| {
                let mut p = pane(provider.label());
                p.provider = Some(*provider);
                p.pane = format!("%{i}");
                p.client_pid = i as u32;
                p
            })
            .collect::<Vec<_>>();
        let mut rows = vec![session()];
        append_live_panes(&mut rows, &panes);
        assert_eq!(rows.len(), 5);
        append_live_panes(&mut rows, &panes);
        assert_eq!(rows.len(), 5);
        assert!(
            rows.iter()
                .skip(1)
                .all(|s| s.id.starts_with("pane-") && !s.available && View::Open.matches(s))
        );
        let mut app = crate::ui::App {
            catalog: Catalog {
                sessions: rows,
                ..Default::default()
            },
            search: "claude".into(),
            ..Default::default()
        };
        app.refresh_filter();
        assert_eq!(app.rows().len(), 1);
        assert_eq!(app.rows()[0].metadata.provider, Provider::Claude);
        // Live-only rows vanish on a store refresh unless their pane still exists.
        let mut saved = vec![session()];
        assert!(reattach_presence(&mut saved, &app.catalog.sessions).is_empty());
    }
    #[test]
    fn resume_arguments_use_the_correct_provider_and_exact_identity() {
        for (provider, expected) in [
            (
                Provider::Pi,
                vec!["pi", "--session", "/tmp/session with spaces;$.jsonl"],
            ),
            (Provider::Claude, vec!["claude", "--resume", "native-id"]),
            (Provider::Codex, vec!["codex", "resume", "native-id"]),
            (
                Provider::Opencode,
                vec!["opencode", "--session", "native-id"],
            ),
        ] {
            let spec = ResumeSpec {
                provider,
                file: "/tmp/session with spaces;$.jsonl".into(),
                cwd: "/tmp".into(),
                title: "name".into(),
                native_id: "native-id".into(),
                warning: String::new(),
            };
            assert_eq!(spec.argv(), expected);
        }
    }
    #[test]
    fn project_settings_find_custom_storage_without_an_extension() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir(dir.path().join(".pi")).unwrap();
        fs::create_dir(dir.path().join("history")).unwrap();
        fs::write(
            dir.path().join(".pi/settings.json"),
            r#"{"sessionDir":"history"}"#,
        )
        .unwrap();
        assert_eq!(
            project_source(dir.path()).unwrap(),
            paths::canonical(&dir.path().join("history"))
        );
    }
    #[test]
    fn changed_native_identity_is_rejected_before_any_tmux_creation() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("session.jsonl");
        std::fs::write(
            &file,
            r#"{"type":"session","version":3,"id":"replacement","cwd":"/tmp"}
"#,
        )
        .unwrap();
        let spec = ResumeSpec {
            file,
            provider: Provider::Pi,
            cwd: dir.path().to_string_lossy().into_owned(),
            title: "original".into(),
            native_id: "original".into(),
            warning: String::new(),
        };
        assert!(
            spec.launch()
                .unwrap_err()
                .to_string()
                .contains("session identity changed")
        );
    }
    #[test]
    fn launch_uses_exact_argv_and_cwd_not_a_shell_command() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path().join("project with spaces");
        fs::create_dir(&cwd).unwrap();
        let script = dir.path().join("fake-pi");
        fs::write(
            &script,
            "#!/bin/sh\nprintf '%s\\n' \"$PWD\" \"$@\" > \"$PWD/result\"\n",
        )
        .unwrap();
        fs::set_permissions(&script, fs::Permissions::from_mode(0o700)).unwrap();
        let file = cwd.join("session; name.jsonl");
        let spec = ResumeSpec {
            file: file.clone(),
            provider: Provider::Pi,
            cwd: cwd.to_string_lossy().into(),
            title: "Synthetic session".into(),
            native_id: String::new(),
            warning: String::new(),
        };
        spec.launch_program(script.as_os_str()).unwrap();
        let output = fs::read_to_string(cwd.join("result")).unwrap();
        let lines = output.lines().collect::<Vec<_>>();
        assert_eq!(lines[0], paths::canonical(&cwd).to_string_lossy());
        assert_eq!(lines[1], "--session");
        assert_eq!(lines[2], file.to_string_lossy());
        assert_eq!(lines.len(), 3);
    }
}
