//! Durable tmux topology checkpoints, separate from the To resume work list.
//!
//! Capture is read-only. Restoration is explicit, refuses name collisions and
//! unresolved identities, and targets only windows/panes created by this run.
//! Process memory, shell history and arbitrary foreground jobs are NOT backups.
use crate::{model::*, paths, service::ResumeSpec, tmux};
use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, HashMap, HashSet},
    ffi::OsString,
    fs,
    io::{Read, Write},
    os::{
        fd::AsRawFd,
        unix::fs::{MetadataExt, OpenOptionsExt},
    },
    path::{Path, PathBuf},
    process::Command,
};

#[cfg(test)]
mod tests;

const VERSION: u32 = 1;
const MAX_BYTES: u64 = 8 * 1024 * 1024;
const KEEP_CHECKPOINTS: usize = 20;
const PLACEHOLDER: &[&str] = &["sleep", "86400"];

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Workspace {
    pub version: u32,
    pub captured_at: i64,
    pub servers: Vec<Server>,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Server {
    pub socket: String,
    pub identity: String,
    pub sessions: Vec<SavedSession>,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SavedSession {
    pub id: String,
    pub name: String,
    pub base_index: u32,
    pub renumber: bool,
    pub group: String,
    pub default_shell: String,
    pub windows: Vec<SavedWindow>,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SavedWindow {
    pub id: String,
    pub index: u32,
    pub name: String,
    pub layout: String,
    pub width: u32,
    pub height: u32,
    pub active: bool,
    pub zoomed: bool,
    pub linked: bool,
    pub pane_base_index: u32,
    pub automatic_rename: bool,
    pub allow_rename: bool,
    pub size_policy: String,
    pub panes: Vec<SavedPane>,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SavedPane {
    pub id: String,
    pub index: u32,
    pub active: bool,
    pub cwd: String,
    pub title: String,
    pub current_command: String,
    pub start_command: String,
    pub stamp: Option<PaneStamp>,
    pub content: PaneContent,
}
/// A manual association expires when the process, server, pane or title changes.
/// Pi's extension is still needed to prove in-process switches with equal titles.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PaneStamp {
    server: String,
    pane: String,
    pane_pid: u32,
    client_pid: u32,
    client_start: String,
    title: String,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PaneContent {
    Chat {
        spec: ResumeSpec,
        confidence: Confidence,
    },
    Shell,
    /// The original tmux launch string, not inferred from a process basename.
    Command {
        command: String,
    },
    Unresolved {
        reason: String,
    },
}
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum Confidence {
    Verified,
    Title,
    User,
}

/// All subprocesses pass through an injectable executable; tests use strict
/// stubs which cannot invoke the real tmux or any agent client.
struct Client {
    program: OsString,
}
impl Client {
    fn system() -> Self {
        Self {
            program: "tmux".into(),
        }
    }
    fn run(&self, socket: &str, args: &[&str]) -> Result<String> {
        let out = paths::output(Command::new(&self.program).args(["-S", socket]).args(args))?;
        ensure!(
            out.status.success(),
            "tmux {}: {}",
            args[0],
            paths::line(&String::from_utf8_lossy(&out.stderr))
        );
        String::from_utf8(out.stdout).context("tmux returned non-UTF-8 metadata")
    }
}

const SESSION_FORMAT: &str = "#{session_id}\t#{session_name}\t#{base-index}\t#{renumber-windows}\t#{session_group}\t#{default-shell}";
const WINDOW_FORMAT: &str = "#{session_id}\t#{window_id}\t#{window_index}\t#{window_name}\t#{window_layout}\t#{window_width}\t#{window_height}\t#{window_active}\t#{window_zoomed_flag}\t#{window_linked}\t#{pane-base-index}\t#{automatic-rename}\t#{allow-rename}\t#{window-size}";
const PANE_FORMAT: &str = "#{session_id}\t#{window_id}\t#{pane_id}\t#{pane_index}\t#{pane_active}\t#{pane_current_path}\t#{pane_title}\t#{pane_current_command}\t#{pane_start_command}\t#{pane_pid}";

/// `bindings` are explicit pane=id choices. They never run or rename anything.
pub fn capture(
    catalog: &Catalog,
    previous: Option<&Workspace>,
    bindings: &[String],
) -> Result<Workspace> {
    capture_with(
        &Client::system(),
        &tmux::sockets(&tmux::TmuxConfig::default()),
        catalog,
        previous,
        bindings,
    )
}
fn capture_with(
    client: &Client,
    sockets: &[String],
    catalog: &Catalog,
    previous: Option<&Workspace>,
    bindings: &[String],
) -> Result<Workspace> {
    let mut workspace = Workspace {
        version: VERSION,
        captured_at: paths::now(),
        servers: vec![],
    };
    let mut choices = HashMap::new();
    for binding in bindings {
        let (pane, id) = binding
            .split_once('=')
            .context("binding must be %pane=catalog-id (or socket:%pane=catalog-id)")?;
        ensure!(
            !pane.is_empty()
                && !id.is_empty()
                && choices.insert(pane.to_owned(), id.to_owned()).is_none(),
            "invalid or duplicate pane binding"
        );
    }
    let mut used = HashSet::new();
    for socket in sockets {
        // Bracket the three listings. Layout changes during capture must never
        // replace a good checkpoint with a mixed topology.
        let raw = topology(client, socket)?;
        let mut server = Server {
            socket: tmux::socket_key(socket),
            identity: raw.0.clone(),
            sessions: vec![],
        };
        for line in raw.1.lines() {
            let f = fields(line, 6)?;
            server.sessions.push(SavedSession {
                id: f[0].into(),
                name: f[1].into(),
                base_index: f[2].parse()?,
                renumber: flag(f[3])?,
                group: f[4].into(),
                default_shell: f[5].into(),
                windows: vec![],
            });
        }
        for line in raw.2.lines() {
            let f = fields(line, 14)?;
            let session = server
                .sessions
                .iter_mut()
                .find(|s| s.id == f[0])
                .context("window belongs to an unknown session")?;
            session.windows.push(SavedWindow {
                id: f[1].into(),
                index: f[2].parse()?,
                name: f[3].into(),
                layout: f[4].into(),
                width: f[5].parse()?,
                height: f[6].parse()?,
                active: flag(f[7])?,
                zoomed: flag(f[8])?,
                linked: flag(f[9])?,
                pane_base_index: f[10].parse()?,
                automatic_rename: flag(f[11])?,
                allow_rename: flag(f[12])?,
                size_policy: f[13].into(),
                panes: vec![],
            });
        }
        for line in raw.3.lines() {
            let f = fields(line, 10)?;
            let window = server
                .sessions
                .iter_mut()
                .find(|s| s.id == f[0])
                .and_then(|s| s.windows.iter_mut().find(|w| w.id == f[1]))
                .context("pane belongs to an unknown window")?;
            let pane_pid: u32 = f[9].parse()?;
            let live = catalog
                .sessions
                .iter()
                .flat_map(|s| s.openings())
                .chain(catalog.unbound.iter())
                .find(|p| {
                    tmux::socket_key(&p.socket) == server.socket
                        && p.server == server.identity
                        && p.pane == f[2]
                        && p.pane_pid == pane_pid
                        && p.title == f[6]
                });
            let stamp = live.map(|p| PaneStamp {
                server: p.server.clone(),
                pane: p.pane.clone(),
                pane_pid: p.pane_pid,
                client_pid: p.client_pid,
                client_start: p.client_start.clone(),
                title: p.title.clone(),
            });
            let target = format!("{}:{}", server.socket, f[2]);
            let choice = choices.get(&target).or_else(|| choices.get(f[2]));
            let content = if let Some(id) = choice {
                let matches = catalog
                    .sessions
                    .iter()
                    .filter(|s| s.persistent() && (s.id == *id || s.metadata.native_id == *id))
                    .collect::<Vec<_>>();
                ensure!(
                    matches.len() == 1,
                    "binding has an ambiguous or missing saved chat: {id}"
                );
                let session = matches[0];
                ensure!(
                    live.is_some_and(|p| p.provider == Some(session.metadata.provider))
                        && paths::canonical(Path::new(f[5])) == Path::new(&session.metadata.cwd),
                    "binding provider/project does not match pane {}",
                    f[2]
                );
                ensure!(session.available, "bound chat has no saved transcript");
                ensure!(
                    used.insert(if choices.contains_key(&target) {
                        target
                    } else {
                        f[2].to_owned()
                    }),
                    "pane binding is ambiguous across tmux servers; use socket:%pane=chat-id"
                );
                chat(session, Confidence::User)
            } else if let Some(session) = catalog.sessions.iter().find(|s| {
                s.persistent()
                    && s.available
                    && s.openings().any(|p| {
                        live.is_some_and(|l| {
                            p.socket == l.socket
                                && p.server == l.server
                                && p.pane == l.pane
                                && p.client_pid == l.client_pid
                                && p.client_start == l.client_start
                        })
                    })
            }) {
                let verified = session.bindings.iter().find(|b| {
                    b.pane.as_ref().is_some_and(|p| {
                        live.is_some_and(|l| p.pane == l.pane && p.socket == l.socket)
                    })
                });
                if let Some(binding) = verified {
                    let fresh = crate::presence::read(&binding.record)?;
                    ensure!(
                        fresh.nonce == binding.bridge.nonce
                            && fresh.generation == binding.bridge.generation
                            && fresh.native_id == binding.bridge.native_id
                            && fresh.file == binding.bridge.file,
                        "Pi switched conversations while taking the checkpoint; retry"
                    );
                }
                if verified.is_none()
                    && let Some(old) = previous_assignment(
                        previous,
                        &server.socket,
                        &server.identity,
                        stamp.as_ref(),
                    )
                {
                    old.content.clone()
                } else {
                    chat(
                        session,
                        if verified.is_some() {
                            Confidence::Verified
                        } else {
                            Confidence::Title
                        },
                    )
                }
            } else if let Some(old) =
                previous_assignment(previous, &server.socket, &server.identity, stamp.as_ref())
            {
                old.content.clone()
            } else if live.is_some_and(|p| p.provider.is_some()) {
                PaneContent::Unresolved { reason: "Agent conversation identity is unknown; install the Pi identity integration or snapshot --bind %pane=chat-id".into() }
            } else if !f[8].is_empty() {
                PaneContent::Command {
                    command: f[8].into(),
                }
            } else if is_shell(f[7]) {
                PaneContent::Shell
            } else {
                PaneContent::Unresolved {
                    reason: format!(
                        "Foreground program '{}' has no recoverable tmux launch command",
                        paths::line(f[7])
                    ),
                }
            };
            window.panes.push(SavedPane {
                id: f[2].into(),
                index: f[3].parse()?,
                active: flag(f[4])?,
                cwd: f[5].into(),
                title: f[6].into(),
                current_command: f[7].into(),
                start_command: f[8].into(),
                stamp,
                content,
            });
        }
        ensure!(
            topology(client, socket)? == raw,
            "tmux topology changed during snapshot; retry"
        );
        server.sessions.sort_by(|a, b| a.id.cmp(&b.id));
        for session in &mut server.sessions {
            session.windows.sort_by_key(|w| w.index);
            for window in &mut session.windows {
                window.panes.sort_by_key(|p| p.index);
            }
        }
        workspace.servers.push(server);
    }
    ensure!(
        used.len() == choices.len(),
        "a pane binding did not match a live pane uniquely"
    );
    workspace.validate()?;
    Ok(workspace)
}
/// A later title heuristic must not silently downgrade or replace an explicit
/// user choice. An extension-proven switch, however, always takes precedence.
fn previous_assignment<'a>(
    previous: Option<&'a Workspace>,
    socket: &str,
    server: &str,
    stamp: Option<&PaneStamp>,
) -> Option<&'a SavedPane> {
    let stamp = stamp?;
    previous?
        .servers
        .iter()
        .find(|s| s.socket == socket && s.identity == server)?
        .sessions
        .iter()
        .flat_map(|s| &s.windows)
        .flat_map(|w| &w.panes)
        .find(|p| {
            p.stamp.as_ref() == Some(stamp)
                && matches!(
                    p.content,
                    PaneContent::Chat {
                        confidence: Confidence::User,
                        ..
                    }
                )
        })
}

fn topology(client: &Client, socket: &str) -> Result<(String, String, String, String)> {
    Ok((
        client
            .run(socket, &["display-message", "-p", "#{pid}:#{start_time}"])?
            .trim()
            .into(),
        client.run(socket, &["list-sessions", "-F", SESSION_FORMAT])?,
        client.run(socket, &["list-windows", "-a", "-F", WINDOW_FORMAT])?,
        client.run(socket, &["list-panes", "-a", "-F", PANE_FORMAT])?,
    ))
}
fn fields(line: &str, count: usize) -> Result<Vec<&str>> {
    let fields = line.split('\t').collect::<Vec<_>>();
    ensure!(
        fields.len() == count,
        "unsupported newline/tab in tmux metadata; checkpoint not replaced"
    );
    Ok(fields)
}
fn flag(raw: &str) -> Result<bool> {
    match raw {
        "1" | "on" => Ok(true),
        "0" | "off" => Ok(false),
        _ => bail!("invalid tmux boolean"),
    }
}
fn is_shell(command: &str) -> bool {
    matches!(
        Path::new(command)
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or(command)
            .trim_start_matches('-'),
        "sh" | "bash" | "zsh" | "fish" | "dash" | "ksh" | "nu"
    )
}
fn chat(session: &Session, confidence: Confidence) -> PaneContent {
    PaneContent::Chat {
        spec: ResumeSpec {
            file: session.metadata.source_file().into(),
            provider: session.metadata.provider,
            cwd: session.metadata.cwd.clone(),
            title: session.metadata.title(),
            native_id: session.metadata.native_id.clone(),
            warning: String::new(),
        },
        confidence,
    }
}

impl Workspace {
    pub fn counts(&self) -> (usize, usize, usize) {
        (
            self.servers.iter().map(|s| s.sessions.len()).sum(),
            self.servers
                .iter()
                .flat_map(|s| &s.sessions)
                .map(|s| s.windows.len())
                .sum(),
            self.servers
                .iter()
                .flat_map(|s| &s.sessions)
                .flat_map(|s| &s.windows)
                .map(|w| w.panes.len())
                .sum(),
        )
    }
    fn validate(&self) -> Result<()> {
        ensure!(
            self.version == VERSION,
            "unsupported workspace snapshot version"
        );
        ensure!(self.captured_at >= 0, "invalid snapshot timestamp");
        let (sessions, windows, panes) = self.counts();
        ensure!(
            self.servers.len() <= 32 && sessions <= 512 && windows <= 4096 && panes <= 8192,
            "workspace snapshot exceeds limits"
        );
        let mut sockets = HashSet::new();
        for server in &self.servers {
            ensure!(
                Path::new(&server.socket).is_absolute() && sockets.insert(&server.socket),
                "invalid/duplicate tmux socket"
            );
            let mut names = HashSet::new();
            for session in &server.sessions {
                ensure!(
                    !session.name.is_empty()
                        && session.name.len() <= 512
                        && !session.name.contains([':', '.', '\n', '\t'])
                        && names.insert(&session.name),
                    "invalid/duplicate tmux session name"
                );
                ensure!(
                    Path::new(&session.default_shell).is_absolute() && !session.windows.is_empty(),
                    "invalid session shell/windows"
                );
                ensure!(
                    session.windows.iter().filter(|w| w.active).count() == 1,
                    "session must have exactly one active window"
                );
                let mut indexes = HashSet::new();
                for window in &session.windows {
                    ensure!(
                        indexes.insert(window.index)
                            && window.name.len() <= 512
                            && !window.panes.is_empty(),
                        "invalid window metadata"
                    );
                    ensure!(
                        (2..=10000).contains(&window.width) && (2..=10000).contains(&window.height),
                        "invalid window size"
                    );
                    ensure!(
                        matches!(
                            window.size_policy.as_str(),
                            "largest" | "smallest" | "manual" | "latest"
                        ),
                        "invalid window size policy"
                    );
                    ensure!(
                        window.panes.iter().filter(|p| p.active).count() == 1,
                        "window must have exactly one active pane"
                    );
                    let layout = Layout::parse(&window.layout)?;
                    ensure!(
                        layout.width == window.width && layout.height == window.height,
                        "layout/window dimensions disagree"
                    );
                    let mut leaves = vec![];
                    layout.leaves(&mut leaves);
                    let ids = window
                        .panes
                        .iter()
                        .map(|p| p.id.trim_start_matches('%').parse::<u32>())
                        .collect::<std::result::Result<HashSet<_>, _>>()?;
                    ensure!(
                        ids.len() == window.panes.len()
                            && leaves.len() == ids.len()
                            && leaves.iter().copied().collect::<HashSet<_>>() == ids,
                        "layout does not match pane identities"
                    );
                    for (i, pane) in window.panes.iter().enumerate() {
                        ensure!(
                            window.pane_base_index.checked_add(i as u32) == Some(pane.index)
                                && leaves[i] == pane.id.trim_start_matches('%').parse::<u32>()?,
                            "nonstandard pane-index/layout order cannot be restored exactly"
                        );
                        ensure!(
                            Path::new(&pane.cwd).is_absolute()
                                && pane.cwd.len() <= 4096
                                && pane.start_command.len() <= 65536
                                && pane.title.len() <= 16384,
                            "invalid pane metadata"
                        );
                    }
                }
            }
        }
        Ok(())
    }
}

/// Owner-only, versioned, atomic checkpoints. Empty/unverifiable discovery never
/// overwrites the last nonempty workspace after a server has disappeared.
pub fn save(root: &Path, workspace: &Workspace) -> Result<Option<PathBuf>> {
    let _lock = workspace_lock(root)?;
    save_locked(root, workspace)
}

/// Hold one lock over discovery AND publication, so an observer cannot save
/// recovery's temporary placeholders, nor race another checkpoint writer.
pub fn checkpoint(
    root: &Path,
    catalog: &Catalog,
    previous: Option<&Workspace>,
    bindings: &[String],
) -> Result<(Workspace, Option<PathBuf>)> {
    let _lock = workspace_lock(root)?;
    let snapshot = capture(catalog, previous, bindings)?;
    let path = save_locked(root, &snapshot)?;
    Ok((snapshot, path))
}

struct WorkspaceLock(fs::File);
impl Drop for WorkspaceLock {
    fn drop(&mut self) {
        // Explicit unlock is important with concurrent subprocess creation:
        // a forked child may briefly hold the same open file description before
        // CLOEXEC takes effect. Parent close alone would defer lock release.
        unsafe {
            libc::flock(self.0.as_raw_fd(), libc::LOCK_UN);
        }
    }
}

fn workspace_lock(root: &Path) -> Result<WorkspaceLock> {
    let dir = root.join("workspace");
    paths::secure_dir(&dir)?;
    let file = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK)
        .open(dir.join("workspace.lock"))?;
    let metadata = file.metadata()?;
    ensure!(
        metadata.is_file()
            && metadata.uid() == unsafe { libc::geteuid() }
            && metadata.mode() & 0o077 == 0,
        "unsafe workspace lock"
    );
    ensure!(
        unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0,
        "Another workspace checkpoint/recovery is active; retry when it finishes"
    );
    // Never unlink this inode while another TUI could have it open.
    // CLOEXEC and the explicit RAII unlock prevent ownership leaking to tools.
    Ok(WorkspaceLock(file))
}

fn save_locked(root: &Path, workspace: &Workspace) -> Result<Option<PathBuf>> {
    workspace.validate()?;
    if workspace.counts().0 == 0 {
        return Ok(None);
    }
    let dir = root.join("workspace");
    paths::secure_dir(&dir)?;
    let latest = dir.join("latest.json");
    if latest.exists() {
        let old = load(root, None)?;
        let mut comparable = old.clone();
        comparable.captured_at = workspace.captured_at;
        if comparable == *workspace {
            return Ok(Some(latest));
        }
    }
    let bytes = serde_json::to_vec_pretty(workspace)?;
    ensure!(
        bytes.len() as u64 <= MAX_BYTES,
        "workspace snapshot exceeds 8 MiB"
    );
    let archive = dir.join(format!(
        "{:020}-{}.json",
        workspace.captured_at,
        uuid::Uuid::new_v4()
    ));
    private_write(&archive, &bytes)?;
    // Refuse a malicious destination even though rename itself does not follow it.
    if fs::symlink_metadata(&latest).is_ok() {
        paths::private_file(&latest)?;
    }
    let temporary = dir.join(format!(".{}.tmp", uuid::Uuid::new_v4()));
    private_write(&temporary, &bytes)?;
    let result = fs::rename(&temporary, &latest);
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result?;
    let mut archives = checkpoints(root)?;
    archives.sort();
    let remove = archives.len().saturating_sub(KEEP_CHECKPOINTS);
    for file in archives.into_iter().take(remove) {
        paths::private_file(&file)?;
        fs::remove_file(file)?;
    }
    Ok(Some(archive))
}
fn private_write(path: &Path, bytes: &[u8]) -> Result<()> {
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    Ok(())
}
pub fn checkpoints(root: &Path) -> Result<Vec<PathBuf>> {
    let dir = root.join("workspace");
    if !dir.exists() {
        return Ok(vec![]);
    }
    paths::secure_dir(&dir)?;
    Ok(fs::read_dir(dir)?
        .filter_map(|entry| entry.ok())
        .filter(|entry| entry.file_type().is_ok_and(|t| t.is_file()))
        .map(|entry| entry.path())
        .filter(|p| {
            // Retention may remove only files with our generated naming scheme,
            // never an arbitrary user JSON file or a symlink in this directory.
            p.extension().is_some_and(|s| s == "json")
                && p.file_stem()
                    .and_then(|s| s.to_str())
                    .and_then(|s| s.split_once('-'))
                    .is_some_and(|(time, id)| {
                        time.parse::<u64>().is_ok() && uuid::Uuid::parse_str(id).is_ok()
                    })
        })
        .collect())
}
pub fn load(root: &Path, path: Option<&Path>) -> Result<Workspace> {
    let path = path
        .map(Path::to_path_buf)
        .unwrap_or_else(|| root.join("workspace/latest.json"));
    paths::private_file(&path)
        .context("No usable workspace checkpoint; run snapshot before closing tmux")?;
    let file = fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)?;
    ensure!(
        file.metadata()?.is_file() && file.metadata()?.len() <= MAX_BYTES,
        "invalid snapshot file"
    );
    let mut bytes = vec![];
    file.take(MAX_BYTES + 1).read_to_end(&mut bytes)?;
    ensure!(bytes.len() as u64 <= MAX_BYTES, "snapshot is too large");
    let snapshot: Workspace = serde_json::from_slice(&bytes)?;
    snapshot.validate()?;
    Ok(snapshot)
}

#[derive(Debug, Clone, Copy, Default)]
pub struct RestoreOptions {
    pub layout_only: bool,
    pub commands: bool,
    pub allow_probable: bool,
}

impl Workspace {
    /// Return every blocker up front. There is no silent partial/full fallback.
    pub fn blockers(&self, catalog: &Catalog, options: RestoreOptions) -> Vec<String> {
        let mut issues = vec![];
        if let Err(e) = self.validate() {
            return vec![format!("{e:#}")];
        }
        for server in &self.servers {
            for session in &server.sessions {
                if !session.group.is_empty() {
                    issues.push(format!(
                        "{}: grouped sessions are not supported",
                        session.name
                    ));
                }
                if !Path::new(&session.default_shell).is_file() {
                    issues.push(format!("{}: saved shell is unavailable", session.name));
                }
                for window in &session.windows {
                    if window.linked {
                        issues.push(format!(
                            "{}:{}: linked windows are not supported",
                            session.name, window.index
                        ));
                    }
                    for pane in &window.panes {
                        let label = format!("{}:{}.{}", session.name, window.index, pane.index);
                        if !Path::new(&pane.cwd).is_dir() {
                            issues.push(format!(
                                "{label}: directory is unavailable: {}",
                                paths::line(&pane.cwd)
                            ));
                        }
                        if options.layout_only {
                            continue;
                        }
                        match &pane.content {
                            PaneContent::Chat { spec, confidence } => {
                                if !spec.file.is_absolute()
                                    || paths::canonical(Path::new(&spec.cwd))
                                        != paths::canonical(Path::new(&pane.cwd))
                                {
                                    issues.push(format!("{label}: saved chat and pane project/file metadata disagree"));
                                }
                                if *confidence == Confidence::Title && !options.allow_probable {
                                    issues.push(format!("{label}: title-only chat identity; bind it explicitly with r/snapshot --bind, or authorize --allow-probable"));
                                }
                                if let Err(e) = spec.validate_launch() {
                                    issues.push(format!("{label}: {e:#}"));
                                }
                                if catalog.sessions.iter().any(|s| {
                                    s.metadata.provider == spec.provider
                                        && s.metadata.native_id == spec.native_id
                                        && (!s.bindings.is_empty() || !s.probable.is_empty())
                                }) {
                                    issues.push(format!("{label}: chat is already/probably open"));
                                }
                                if catalog
                                    .unbound
                                    .iter()
                                    .any(|p| p.provider == Some(spec.provider))
                                    || !catalog.warnings.is_empty()
                                {
                                    issues.push(format!(
                                        "{label}: external agent presence is uncertain"
                                    ));
                                }
                            }
                            PaneContent::Command { .. } if !options.commands => {
                                issues.push(format!(
                                    "{label}: restarting the saved launch command needs --commands"
                                ))
                            }
                            PaneContent::Unresolved { reason } => {
                                issues.push(format!("{label}: {reason}"))
                            }
                            _ => {}
                        }
                    }
                }
            }
        }
        issues
    }

    pub fn summary(&self) -> String {
        let mut lines = vec![format!(
            "Workspace checkpoint {}",
            paths::date(self.captured_at)
        )];
        for server in &self.servers {
            lines.push(format!("Server {}", paths::line(&server.socket)));
            for session in &server.sessions {
                lines.push(format!("  Session {}", paths::line(&session.name)));
                for window in &session.windows {
                    lines.push(format!(
                        "    Window {}: {} ({}x{}, {} panes{})",
                        window.index,
                        paths::line(&window.name),
                        window.width,
                        window.height,
                        window.panes.len(),
                        if window.zoomed { ", zoomed" } else { "" }
                    ));
                    for pane in &window.panes {
                        let content = match &pane.content {
                            PaneContent::Chat { spec, confidence } => format!(
                                "{} chat: {} ({confidence:?})",
                                spec.provider.label(),
                                paths::line(&spec.title)
                            ),
                            PaneContent::Shell => "shell".into(),
                            PaneContent::Command { command } => {
                                format!("launch command: {}", paths::line(command))
                            }
                            PaneContent::Unresolved { reason } => {
                                format!("UNRESOLVED: {}", paths::line(reason))
                            }
                        };
                        lines.push(format!(
                            "      Pane {}: {} — {}",
                            pane.index,
                            content,
                            paths::line(&pane.cwd)
                        ));
                    }
                }
            }
        }
        lines.join("\n")
    }
}

/// Restore into the original sockets with original session names, but only if
/// those names are absent. Nothing is merged into or removed from live sessions.
pub fn restore(
    root: &Path,
    workspace: &Workspace,
    catalog: &Catalog,
    options: RestoreOptions,
) -> Result<PathBuf> {
    restore_with(&Client::system(), root, workspace, catalog, options)
}
fn restore_with(
    client: &Client,
    root: &Path,
    workspace: &Workspace,
    catalog: &Catalog,
    options: RestoreOptions,
) -> Result<PathBuf> {
    let blockers = workspace.blockers(catalog, options);
    ensure!(
        blockers.is_empty(),
        "Workspace is not fully recoverable:\n{}",
        blockers.join("\n")
    );
    ensure!(workspace.counts().0 > 0, "workspace snapshot is empty");
    let _lock = workspace_lock(root)?;
    // Complete collision preflight across ALL servers before creating anything.
    for server in &workspace.servers {
        if Path::new(&server.socket).exists() {
            let live = client.run(&server.socket, &["list-sessions", "-F", "#{session_name}"])?;
            for session in &server.sessions {
                ensure!(
                    !live.lines().any(|s| s == session.name),
                    "Session '{}' already exists; refusing to overwrite or duplicate it",
                    session.name
                );
            }
        }
    }
    let dir = root.join("workspace");
    paths::secure_dir(&dir)?;
    let receipt = dir.join(format!("restore-{}.jsonl", uuid::Uuid::new_v4()));
    let mut journal = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(&receipt)?;
    let result = restore_topology(client, workspace, options, &mut journal);
    result.with_context(|| format!("Recovery interrupted. Only newly created placeholders may remain; no rollback was attempted. Created-object journal: {}", receipt.display()))?;
    Ok(receipt)
}
#[derive(Clone)]
struct CreatedPane {
    id: String,
    pid: u32,
}
struct CreatedWindow {
    id: String,
    server: String,
    panes: HashMap<u32, CreatedPane>,
    root: CreatedPane,
}

fn record(journal: &mut fs::File, value: serde_json::Value) -> Result<()> {
    serde_json::to_writer(&mut *journal, &value)?;
    journal.write_all(b"\n")?;
    journal.sync_all()?;
    Ok(())
}
/// A respawn may replace ONLY the exact placeholder process created by us.
/// If another client has reused/respawned that pane, abort instead of killing it.
fn guard(
    client: &Client,
    socket: &str,
    server: &str,
    pane: &CreatedPane,
    placeholder: bool,
) -> Result<()> {
    let raw = client.run(
        socket,
        &[
            "display-message",
            "-p",
            "-t",
            &pane.id,
            "#{pid}:#{start_time}\t#{pane_id}\t#{pane_pid}\t#{pane_current_command}",
        ],
    )?;
    let f = fields(raw.trim_end_matches('\n'), 4)?;
    ensure!(
        f[0] == server && f[1] == pane.id,
        "Created pane/server identity changed"
    );
    if placeholder {
        ensure!(
            f[2].parse::<u32>()? == pane.pid && f[3] == "sleep",
            "Placeholder changed; refusing to replace another process"
        );
    }
    Ok(())
}
fn restore_topology(
    client: &Client,
    workspace: &Workspace,
    options: RestoreOptions,
    journal: &mut fs::File,
) -> Result<()> {
    let mut created = BTreeMap::<(usize, usize, usize), CreatedWindow>::new();
    let format = "#{session_id}\t#{window_id}\t#{pane_id}\t#{window_index}\t#{pid}:#{start_time}\t#{pane_pid}";
    // Build geometry with inert, detached placeholders BEFORE starting any chats.
    for (si, server) in workspace.servers.iter().enumerate() {
        let parent = Path::new(&server.socket)
            .parent()
            .context("tmux socket has no parent")?;
        paths::secure_dir(parent)?;
        for (sj, session) in server.sessions.iter().enumerate() {
            let first = &session.windows[0];
            let raw = client.run(
                &server.socket,
                &[
                    "new-session",
                    "-d",
                    "-P",
                    "-F",
                    format,
                    "-s",
                    &session.name,
                    "-n",
                    &first.name,
                    "-x",
                    &first.width.to_string(),
                    "-y",
                    &first.height.to_string(),
                    "-c",
                    &first.panes[0].cwd,
                    PLACEHOLDER[0],
                    PLACEHOLDER[1],
                ],
            )?;
            let f = fields(raw.trim_end_matches('\n'), 6)?;
            let session_id = f[0].to_owned();
            let server_id = f[4].to_owned();
            let first_pane = CreatedPane {
                id: f[2].into(),
                pid: f[5].parse()?,
            };
            record(
                journal,
                serde_json::json!({"event":"session_created","socket":server.socket,"name":session.name,"session":session_id,"window":f[1],"pane":first_pane.id,"server":server_id}),
            )?;
            guard(client, &server.socket, &server_id, &first_pane, true)?;
            client.run(
                &server.socket,
                &["set-option", "-t", &session_id, "renumber-windows", "off"],
            )?;
            client.run(
                &server.socket,
                &[
                    "set-option",
                    "-t",
                    &session_id,
                    "base-index",
                    &session.base_index.to_string(),
                ],
            )?;
            client.run(
                &server.socket,
                &[
                    "set-option",
                    "-t",
                    &session_id,
                    "default-shell",
                    &session.default_shell,
                ],
            )?;
            if f[3].parse::<u32>()? != first.index {
                client.run(
                    &server.socket,
                    &[
                        "move-window",
                        "-s",
                        f[1],
                        "-t",
                        &format!("{session_id}:{}", first.index),
                    ],
                )?;
            }
            for (wi, window) in session.windows.iter().enumerate() {
                let (window_id, root) = if wi == 0 {
                    (f[1].to_owned(), first_pane.clone())
                } else {
                    let raw = client.run(
                        &server.socket,
                        &[
                            "new-window",
                            "-d",
                            "-P",
                            "-F",
                            format,
                            "-t",
                            &format!("{session_id}:{}", window.index),
                            "-n",
                            &window.name,
                            "-c",
                            &window.panes[0].cwd,
                            PLACEHOLDER[0],
                            PLACEHOLDER[1],
                        ],
                    )?;
                    let wf = fields(raw.trim_end_matches('\n'), 6)?;
                    ensure!(
                        wf[0] == session_id
                            && wf[4] == server_id
                            && wf[3].parse::<u32>()? == window.index,
                        "tmux created a window in an unexpected location"
                    );
                    let pane = CreatedPane {
                        id: wf[2].into(),
                        pid: wf[5].parse()?,
                    };
                    record(
                        journal,
                        serde_json::json!({"event":"window_created","socket":server.socket,"session":session_id,"window":wf[1],"pane":pane.id}),
                    )?;
                    (wf[1].to_owned(), pane)
                };
                guard(client, &server.socket, &server_id, &root, true)?;
                for (option, value) in [
                    ("automatic-rename", "off".to_owned()),
                    ("allow-rename", "off".to_owned()),
                    ("pane-base-index", window.pane_base_index.to_string()),
                ] {
                    client.run(
                        &server.socket,
                        &["set-window-option", "-t", &window_id, option, &value],
                    )?;
                }
                client.run(
                    &server.socket,
                    &[
                        "resize-window",
                        "-t",
                        &window_id,
                        "-x",
                        &window.width.to_string(),
                        "-y",
                        &window.height.to_string(),
                    ],
                )?;
                let layout = Layout::parse(&window.layout)?;
                let mut panes = HashMap::new();
                split_layout(
                    client,
                    &server.socket,
                    &server_id,
                    &layout,
                    &root,
                    &mut panes,
                    journal,
                )?;
                let ids = panes
                    .iter()
                    .map(|(old, pane)| Ok((*old, pane.id.trim_start_matches('%').parse::<u32>()?)))
                    .collect::<Result<HashMap<_, _>>>()?;
                client.run(
                    &server.socket,
                    &["select-layout", "-t", &window_id, &layout.remap(&ids)?],
                )?;
                created.insert(
                    (si, sj, wi),
                    CreatedWindow {
                        id: window_id,
                        server: server_id.clone(),
                        panes,
                        root,
                    },
                );
            }
        }
    }
    // All topology now exists. Start saved chats/authorized commands in place.
    // A failure is reported with the journal; never kill/overwrite other panes.
    for ((si, sj, wi), window) in &created {
        let server = &workspace.servers[*si];
        let session = &server.sessions[*sj];
        let saved = &session.windows[*wi];
        for pane in &saved.panes {
            let old: u32 = pane.id.trim_start_matches('%').parse()?;
            let target = window.panes.get(&old).context("missing created pane")?;
            guard(client, &server.socket, &window.server, target, true)?;
            let argv = if options.layout_only {
                vec![session.default_shell.clone(), "-l".into()]
            } else {
                match &pane.content {
                    PaneContent::Chat { spec, .. } => {
                        spec.validate_launch()?;
                        spec.argv()
                    }
                    PaneContent::Shell => vec![session.default_shell.clone(), "-l".into()],
                    PaneContent::Command { command } => {
                        ensure!(options.commands, "command replay not authorized");
                        vec![command.clone()]
                    }
                    PaneContent::Unresolved { .. } => bail!("unresolved pane escaped preflight"),
                }
            };
            record(
                journal,
                serde_json::json!({"event":"pane_starting","socket":server.socket,"pane":target.id,"original_pane":pane.id}),
            )?;
            let mut args = vec!["respawn-pane", "-k", "-t", &target.id, "-c", &pane.cwd];
            args.extend(argv.iter().map(String::as_str));
            client.run(&server.socket, &args)?;
            client.run(
                &server.socket,
                &["select-pane", "-t", &target.id, "-T", &pane.title],
            )?;
        }
        let active = saved
            .panes
            .iter()
            .find(|p| p.active)
            .context("missing active pane")?;
        let active = &window.panes[&active.id.trim_start_matches('%').parse::<u32>()?];
        guard(client, &server.socket, &window.server, &window.root, false)?;
        client.run(&server.socket, &["select-pane", "-t", &active.id])?;
        if saved.zoomed {
            client.run(&server.socket, &["resize-pane", "-Z", "-t", &active.id])?;
        }
        client.run(
            &server.socket,
            &["rename-window", "-t", &window.id, &saved.name],
        )?;
        for (option, value) in [
            ("window-size", saved.size_policy.as_str()),
            (
                "automatic-rename",
                if saved.automatic_rename { "on" } else { "off" },
            ),
            (
                "allow-rename",
                if saved.allow_rename { "on" } else { "off" },
            ),
        ] {
            client.run(
                &server.socket,
                &["set-window-option", "-t", &window.id, option, value],
            )?;
        }
    }
    for (si, server) in workspace.servers.iter().enumerate() {
        for (sj, session) in server.sessions.iter().enumerate() {
            let first = &created[&(si, sj, 0)];
            guard(client, &server.socket, &first.server, &first.root, false)?;
            let active_index = session
                .windows
                .iter()
                .position(|w| w.active)
                .context("missing active window")?;
            client.run(
                &server.socket,
                &["select-window", "-t", &created[&(si, sj, active_index)].id],
            )?;
            // Resolve the session from our stable window id, not a user name.
            let id = client.run(
                &server.socket,
                &["display-message", "-p", "-t", &first.id, "#{session_id}"],
            )?;
            client.run(
                &server.socket,
                &[
                    "set-option",
                    "-t",
                    id.trim(),
                    "renumber-windows",
                    if session.renumber { "on" } else { "off" },
                ],
            )?;
        }
    }
    // tmux may resize a requested layout to fit options/client constraints.
    // Report that honestly instead of claiming an exact restore on a changed grid.
    for ((si, sj, wi), made) in &created {
        let server = &workspace.servers[*si];
        let saved = &server.sessions[*sj].windows[*wi];
        let raw = client.run(
            &server.socket,
            &[
                "display-message",
                "-p",
                "-t",
                &made.id,
                "#{window_layout}\t#{window_width}\t#{window_height}\t#{window_zoomed_flag}",
            ],
        )?;
        let f = fields(raw.trim_end_matches('\n'), 4)?;
        let ids = made
            .panes
            .iter()
            .map(|(old, p)| Ok((*old, p.id.trim_start_matches('%').parse::<u32>()?)))
            .collect::<Result<HashMap<_, _>>>()?;
        let expected = Layout::parse(&Layout::parse(&saved.layout)?.remap(&ids)?)?;
        ensure!(
            Layout::parse(f[0])? == expected
                && f[1].parse::<u32>()? == saved.width
                && f[2].parse::<u32>()? == saved.height
                && flag(f[3])? == saved.zoomed,
            "tmux changed the restored dimensions/layout; exact recovery could not be verified"
        );
    }
    record(
        journal,
        serde_json::json!({"event":"complete","layout_only":options.layout_only}),
    )?;
    Ok(())
}
fn split_layout(
    client: &Client,
    socket: &str,
    server: &str,
    layout: &Layout,
    root: &CreatedPane,
    panes: &mut HashMap<u32, CreatedPane>,
    journal: &mut fs::File,
) -> Result<()> {
    match &layout.cell {
        Cell::Pane(id) => {
            ensure!(
                panes.insert(*id, root.clone()).is_none(),
                "duplicate original pane id"
            );
        }
        Cell::Split {
            horizontal,
            children,
        } => {
            let mut targets = vec![root.clone(); children.len()];
            // Split the retained first child from right/bottom to left/top. This
            // preserves tmux's pane-index order even for mixed nested layouts.
            for i in (1..children.len()).rev() {
                guard(client, socket, server, root, true)?;
                let size = if *horizontal {
                    children[i].width
                } else {
                    children[i].height
                };
                let raw = client.run(
                    socket,
                    &[
                        "split-window",
                        "-d",
                        if *horizontal { "-h" } else { "-v" },
                        "-P",
                        "-F",
                        "#{pane_id}\t#{pane_pid}",
                        "-t",
                        &root.id,
                        "-l",
                        &size.to_string(),
                        PLACEHOLDER[0],
                        PLACEHOLDER[1],
                    ],
                )?;
                let f = fields(raw.trim_end_matches('\n'), 2)?;
                targets[i] = CreatedPane {
                    id: f[0].into(),
                    pid: f[1].parse()?,
                };
                record(
                    journal,
                    serde_json::json!({"event":"pane_created","socket":socket,"pane":targets[i].id}),
                )?;
            }
            for (child, target) in children.iter().zip(&targets) {
                split_layout(client, socket, server, child, target, panes, journal)?;
            }
        }
    }
    Ok(())
}

// tmux layout grammar: WxH,X,Y,pane or WxH,X,Y{children} / [...].
// Parse it structurally, never replace arbitrary numeric substrings: offsets,
// dimensions and pane ids may have exactly the same values.
#[derive(Debug, Clone, PartialEq)]
struct Layout {
    width: u32,
    height: u32,
    x: u32,
    y: u32,
    cell: Cell,
}
#[derive(Debug, Clone, PartialEq)]
enum Cell {
    Pane(u32),
    Split {
        horizontal: bool,
        children: Vec<Layout>,
    },
}
impl Layout {
    fn parse(raw: &str) -> Result<Self> {
        ensure!(raw.len() <= 128 * 1024, "layout exceeds limits");
        let (checksum, body) = raw.split_once(',').context("missing layout checksum")?;
        ensure!(
            u16::from_str_radix(checksum, 16).ok() == Some(layout_checksum(body)),
            "layout checksum mismatch"
        );
        let mut parser = LayoutParser {
            bytes: body.as_bytes(),
            pos: 0,
            nodes: 0,
        };
        let layout = parser.cell(0)?;
        ensure!(parser.pos == parser.bytes.len(), "trailing layout data");
        Ok(layout)
    }
    fn leaves(&self, leaves: &mut Vec<u32>) {
        match &self.cell {
            Cell::Pane(id) => leaves.push(*id),
            Cell::Split { children, .. } => {
                for child in children {
                    child.leaves(leaves);
                }
            }
        }
    }
    fn remap(&self, ids: &HashMap<u32, u32>) -> Result<String> {
        fn body(node: &Layout, ids: &HashMap<u32, u32>) -> Result<String> {
            let prefix = format!("{}x{},{},{}", node.width, node.height, node.x, node.y);
            Ok(match &node.cell {
                Cell::Pane(id) => format!(
                    "{prefix},{}",
                    ids.get(id).context("missing replacement pane id")?
                ),
                Cell::Split {
                    horizontal,
                    children,
                } => {
                    let (open, close) = if *horizontal { ('{', '}') } else { ('[', ']') };
                    format!(
                        "{prefix}{open}{}{close}",
                        children
                            .iter()
                            .map(|c| body(c, ids))
                            .collect::<Result<Vec<_>>>()?
                            .join(",")
                    )
                }
            })
        }
        let body = body(self, ids)?;
        Ok(format!("{:04x},{body}", layout_checksum(&body)))
    }
}
fn layout_checksum(body: &str) -> u16 {
    body.bytes().fold(0u16, |sum, byte| {
        sum.rotate_right(1).wrapping_add(byte as u16)
    })
}
struct LayoutParser<'a> {
    bytes: &'a [u8],
    pos: usize,
    nodes: usize,
}
impl LayoutParser<'_> {
    fn number(&mut self) -> Result<u32> {
        let start = self.pos;
        while self.bytes.get(self.pos).is_some_and(u8::is_ascii_digit) {
            self.pos += 1;
        }
        ensure!(
            self.pos > start && self.pos - start <= 10,
            "invalid layout number"
        );
        Ok(std::str::from_utf8(&self.bytes[start..self.pos])?.parse()?)
    }
    fn expect(&mut self, byte: u8) -> Result<()> {
        ensure!(
            self.bytes.get(self.pos) == Some(&byte),
            "invalid layout syntax"
        );
        self.pos += 1;
        Ok(())
    }
    fn cell(&mut self, depth: usize) -> Result<Layout> {
        self.nodes += 1;
        ensure!(
            depth < 64 && self.nodes <= 16384,
            "layout nesting/node limit exceeded"
        );
        let width = self.number()?;
        self.expect(b'x')?;
        let height = self.number()?;
        self.expect(b',')?;
        let x = self.number()?;
        self.expect(b',')?;
        let y = self.number()?;
        ensure!(
            (1..=10000).contains(&width)
                && (1..=10000).contains(&height)
                && x <= 10000
                && y <= 10000,
            "invalid layout geometry"
        );
        let cell = match self.bytes.get(self.pos).copied() {
            Some(b',') => {
                self.pos += 1;
                Cell::Pane(self.number()?)
            }
            Some(open @ (b'{' | b'[')) => {
                self.pos += 1;
                let mut children = vec![self.cell(depth + 1)?];
                while self.bytes.get(self.pos) == Some(&b',') {
                    self.pos += 1;
                    children.push(self.cell(depth + 1)?);
                }
                self.expect(if open == b'{' { b'}' } else { b']' })?;
                ensure!(
                    children.len() >= 2,
                    "layout split has fewer than two children"
                );
                let horizontal = open == b'{';
                let mut offset = if horizontal { x } else { y };
                for child in &children {
                    ensure!(
                        child.x == if horizontal { offset } else { x }
                            && child.y == if horizontal { y } else { offset }
                            && if horizontal {
                                child.height == height
                            } else {
                                child.width == width
                            },
                        "inconsistent split geometry"
                    );
                    offset = offset
                        .checked_add(if horizontal {
                            child.width
                        } else {
                            child.height
                        })
                        .and_then(|v| v.checked_add(1))
                        .context("layout geometry overflow")?;
                }
                ensure!(
                    offset - 1 == if horizontal { x + width } else { y + height },
                    "split dimensions do not add up"
                );
                Cell::Split {
                    horizontal,
                    children,
                }
            }
            _ => bail!("invalid layout node"),
        };
        Ok(Layout {
            width,
            height,
            x,
            y,
            cell,
        })
    }
}
