//! Read-only tmux discovery plus explicit, revalidated navigation.
//!
//! Discovery only lists panes and matches same-user agent processes.
//! Moving to a pane, or opening a fresh window/session for resume,
//! always happens after an explicit user confirmation. This module never
//! closes panes, kills processes, or edits tmux configuration.
use crate::{model::Provider, paths::clean};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::{collections::HashMap, path::Path, process::Command};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PaneIdentity {
    pub socket: String,
    pub server: String,
    pub pane: String,
    pub pane_pid: u32,
    pub pane_start: String,
    pub client_pid: u32,
    pub client_start: String,
    pub target: String,
    pub session: String,
    pub window: String,
    pub cwd: String,
    pub command: String,
    #[serde(default)]
    pub title: String,
    pub provider: Option<Provider>,
    /// Screen-inferred activity (Herdr-style). Ephemeral: recomputed each
    /// presence tick, never persisted. `None` = not sampled (unrecognized
    /// provider); failed captures report Unknown, even for verified bindings.
    #[serde(default)]
    pub activity: Option<crate::activity::AgentActivity>,
    /// Why the activity came out that way (rule id or fallback reason).
    /// Metadata-only provenance; no captured screen text.
    #[serde(default)]
    pub activity_evidence: Option<crate::activity::MatchEvidence>,
}

#[derive(Debug, Clone, Default)]
pub struct TmuxConfig {
    pub sockets: Vec<String>,
}

/// One same-user OS process, with the fields needed for ancestry checks.
struct Proc {
    parent: u32,
    start: String,
    command: String,
}

/// Run one bounded `tmux -S <socket> …` command and return stdout.
fn tmux(socket: &str, args: &[&str]) -> Result<String> {
    let output = crate::paths::output(Command::new("tmux").args(["-S", socket]).args(args))?;
    ensure!(
        output.status.success(),
        "tmux: {}",
        clean(&String::from_utf8_lossy(&output.stderr))
    );
    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}

/// Canonicalize socket aliases (`/tmp` versus `/private/tmp` on macOS).
/// Comparisons elsewhere must use this key, not the raw env string.
pub fn socket_key(socket: &str) -> String {
    std::fs::canonicalize(socket)
        .map(|path| path.to_string_lossy().into_owned())
        .unwrap_or_else(|_| socket.to_string())
}

/// Snapshot same-user processes once per discovery pass.
/// Other users are ignored before any matching happens.
///
/// Uses `args=` (full command line), not `comm=` (basename, truncated to
/// 15 chars): the wrapper hint (`TMUX_AGENT_MONITOR_AGENT=` on the command)
/// is only recoverable from the full line. The executable basename is
/// derived from the first whitespace-separated token; the whole line is
/// kept for hint parsing.
fn processes() -> Result<HashMap<u32, Proc>> {
    let output = crate::paths::output(
        Command::new("ps")
            .env("LC_ALL", "C")
            .env("TZ", "UTC")
            .args(["-axo", "pid=,ppid=,uid=,lstart=,args="]),
    )?;
    ensure!(output.status.success(), "cannot inspect processes");
    let mut result = HashMap::new();
    for line in String::from_utf8_lossy(&output.stdout).lines() {
        let parts: Vec<_> = line.split_whitespace().collect();
        if parts.len() < 9 || parts[2].parse::<u32>().ok() != Some(unsafe { libc::geteuid() }) {
            continue;
        }
        if let (Ok(pid), Ok(parent)) = (parts[0].parse(), parts[1].parse()) {
            result.insert(
                pid,
                Proc {
                    parent,
                    start: parts[3..8].join(" "),
                    command: parts[8..].join(" "),
                },
            );
        }
    }
    Ok(result)
}

/// Ancestor depth from `pid` up to `root`, capped to avoid loops.
/// Used to decide whether an agent process belongs to a given pane.
fn distance(pid: u32, root: u32, all: &HashMap<u32, Proc>) -> Option<usize> {
    let mut current = pid;
    for depth in 0..64 {
        if current == root {
            return Some(depth);
        }
        let proc = all.get(&current)?;
        if proc.parent == current {
            return None;
        }
        current = proc.parent;
    }
    None
}

/// Recognize supported agents from a full `ps args=` command line.
///
/// The executable is the first whitespace-separated token; only its exact
/// basename counts (`not-codex` never matches). Wrapper hint: when that
/// executable is an opaque sandbox/VM wrapper (see `WRAPPERS`), and the
/// same command line carries `TMUX_AGENT_MONITOR_AGENT=<agent>`, the pane is
/// treated as that agent. The hint is per-command only — never read from
/// the global environment — so exporting it globally cannot mislabel
/// unrelated panes.
const WRAPPERS: &[&str] = &["fence", "nono", "ssh", "docker", "kubectl"];
fn provider(command: &str) -> Option<Provider> {
    // Shells launch tools as `ENV=… executable args…`: leading `NAME=value`
    // tokens are environment, not the program. The executable is the first
    // token without `=`; its exact basename is what counts.
    let mut words = command.split_whitespace();
    let executable = words.by_ref().find(|w| !w.contains('='))?;
    let base = Path::new(executable).file_name()?.to_str()?;
    if let Some(agent) = direct_provider(base) {
        return Some(agent);
    }
    if WRAPPERS.contains(&base) {
        return wrapper_hint(command).and_then(direct_provider);
    }
    None
}
fn direct_provider(base: &str) -> Option<Provider> {
    match base {
        "pi" | "pi-coding-agent" => Some(Provider::Pi),
        "claude" => Some(Provider::Claude),
        "codex" => Some(Provider::Codex),
        "opencode" => Some(Provider::Opencode),
        _ => None,
    }
}
/// Extract a per-command wrapper hint; retain the legacy prefix alias.
/// Only NAME=value and --env NAME=value count, never global environment.
fn wrapper_hint(command: &str) -> Option<&str> {
    fn value(word: &str) -> Option<&str> {
        ["TMUX_AGENT_MONITOR_AGENT=", "AGENT_MONITOR_AGENT="]
            .iter()
            .find_map(|key| word.strip_prefix(key))
    }
    let mut words = command.split_whitespace();
    let mut expect_value = false;
    for word in words.by_ref() {
        if expect_value {
            expect_value = false;
            if let Some(hint) = value(word) {
                return Some(hint);
            }
        } else if word == "--env" {
            expect_value = true;
        } else if let Some(hint) = value(word) {
            return Some(hint);
        }
    }
    None
}

fn client_hint() -> Option<String> {
    std::env::var("TMUX_AGENT_MONITOR_TMUX_CLIENT")
        .ok()
        .or_else(|| std::env::var("AGENT_MONITOR_TMUX_CLIENT").ok())
}

/// Which tmux servers to query: explicit config first, otherwise the
/// current `$TMUX` socket plus every socket in the user tmp directory.
/// Results are canonicalized, sorted, and deduplicated.
pub fn sockets(config: &TmuxConfig) -> Vec<String> {
    if !config.sockets.is_empty() {
        let mut sockets: Vec<_> = config.sockets.iter().map(|s| socket_key(s)).collect();
        sockets.sort();
        sockets.dedup();
        return sockets;
    }
    let mut sockets = vec![];
    if let Ok(tmux) = std::env::var("TMUX")
        && let Some((value, _)) = tmux.rsplit_once(',')
        && let Some((path, _)) = value.rsplit_once(',')
    {
        sockets.push(path.to_string());
    }
    let uid = unsafe { libc::geteuid() };
    let root = std::env::var_os("TMUX_TMPDIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| "/tmp".into())
        .join(format!("tmux-{uid}"));
    if let Ok(entries) = std::fs::read_dir(root) {
        for entry in entries.flatten() {
            use std::os::unix::fs::FileTypeExt;
            if !entry.file_type().is_ok_and(|t| t.is_socket()) {
                continue;
            }
            if let Some(path) = entry.path().to_str() {
                sockets.push(path.to_string());
            }
        }
    }
    let mut sockets: Vec<_> = sockets.into_iter().map(|s| socket_key(&s)).collect();
    sockets.sort();
    sockets.dedup();
    sockets
}

/// List every pane on every observed tmux server.
///
/// For each pane shell, the nearest descendant agent process wins over
/// nested helpers, so a wrapper around `pi` does not hide the real tool.
/// Panes whose shell has no agent descendant keep the pane command as a
/// fallback guess for the provider field.
pub fn discover(config: &TmuxConfig) -> Result<Vec<PaneIdentity>> {
    let all = processes()?;
    let mut panes = vec![];
    for socket in sockets(config) {
        let server = match tmux(&socket, &["display-message", "-p", "#{pid}:#{start_time}"]) {
            Ok(server) => server.trim().to_string(),
            Err(_) => anyhow::bail!(
                "tmux server is not verifiable: {}",
                crate::paths::line(&socket)
            ),
        };
        let output = tmux(
            &socket,
            &[
                "list-panes",
                "-a",
                "-F",
                "#{pane_id}\t#{pane_pid}\t#{pane_current_command}\t#{pane_current_path}\t#{session_name}\t#{window_index}\t#{session_name}:#{window_index}.#{pane_index}\t#{pane_title}",
            ],
        )?;
        for line in output.lines() {
            let fields: Vec<_> = line.split('\t').collect();
            if fields.len() != 8 {
                continue;
            }
            let Ok(pid) = fields[1].parse::<u32>() else {
                continue;
            };
            let Some(root) = all.get(&pid) else {
                continue;
            };
            let mut identified: Vec<_> = all
                .iter()
                .filter(|(id, proc)| {
                    distance(**id, pid, &all).is_some() && provider(&proc.command).is_some()
                })
                .collect();
            identified
                .sort_by_key(|(id, _)| (distance(**id, pid, &all).unwrap_or(usize::MAX), **id));
            let (client_pid, client_start, tool) = identified
                .first()
                .map(|(id, proc)| (**id, proc.start.clone(), provider(&proc.command)))
                .unwrap_or((pid, root.start.clone(), provider(fields[2])));
            panes.push(PaneIdentity {
                socket: socket.clone(),
                server: server.clone(),
                pane: fields[0].to_string(),
                pane_pid: pid,
                pane_start: root.start.clone(),
                client_pid,
                client_start,
                target: clean(fields[6]),
                session: fields[4].to_string(),
                window: fields[5].to_string(),
                cwd: fields[3].to_string(),
                command: clean(fields[2]),
                title: clean(fields[7]),
                provider: tool,
                activity: None,
                activity_evidence: None,
            });
        }
    }
    Ok(panes)
}

/// Read-only snapshot of the live bottom of a pane's terminal buffer.
///
/// Exact argv: `capture-pane -p -t <pane> -S 0` — current viewport only,
/// no writes, no input. Output is capped (`MAX_CAPTURE`) and the caller
/// maps failures to `Unknown`, never to status-line errors. Use
/// `paths::clean` on the result before matching or display.
const MAX_CAPTURE: usize = 64 * 1024;
pub fn capture_pane(socket: &str, pane: &str, lines: u32) -> Result<String> {
    capture_with(std::ffi::OsStr::new("tmux"), socket, pane, lines)
}
/// Same as `capture_pane` with an injectable binary for tests.
/// Production always passes the real `tmux`; tests pass a fake script.
fn capture_with(program: &std::ffi::OsStr, socket: &str, pane: &str, lines: u32) -> Result<String> {
    let lines = lines.clamp(1, 200) as usize;
    let mut command = Command::new(program);
    command.args(["-S", socket]);
    let output = crate::paths::output(command.args(["capture-pane", "-p", "-t", pane, "-S", "0"]))?;
    ensure!(output.status.success(), "capture-pane failed");
    let output = String::from_utf8_lossy(&output.stdout).into_owned();
    ensure!(output.len() <= MAX_CAPTURE, "pane capture exceeds 64 KiB");
    let all = output.lines().collect::<Vec<_>>();
    if all.len() <= lines {
        return Ok(output);
    }
    let mut tail = all[all.len() - lines..].join("\n");
    if output.ends_with('\n') {
        tail.push('\n');
    }
    Ok(tail)
}

/// Count live Pi processes with no extension record.
/// Nonzero means some openings can only be guessed, never proven.
pub fn unreported_pi(reported: &[u32]) -> Result<usize> {
    Ok(processes()?
        .iter()
        .filter(|(pid, p)| provider(&p.command) == Some(Provider::Pi) && !reported.contains(pid))
        .count())
}

/// True when `pid` (with this exact start time) is still a descendant
/// of the recorded pane shell. Catches PID reuse across restarts.
pub fn contains_process(pane: &PaneIdentity, pid: u32, start: &str) -> Result<bool> {
    let all = processes()?;
    Ok(all.get(&pid).is_some_and(|p| p.start == start)
        && distance(pid, pane.pane_pid, &all).is_some())
}

/// `lstart` plus UID check for one process.
/// Rejects missing processes and other users' PIDs outright.
pub fn process_start(pid: u32) -> Result<String> {
    let output = crate::paths::output(
        Command::new("ps")
            .env("LC_ALL", "C")
            .env("TZ", "UTC")
            .args(["-p", &pid.to_string(), "-o", "lstart=,uid="]),
    )?;
    let text = String::from_utf8_lossy(&output.stdout);
    let parts = text.split_whitespace().collect::<Vec<_>>();
    ensure!(
        output.status.success()
            && parts.len() == 6
            && parts[5].parse::<u32>().ok() == Some(unsafe { libc::geteuid() }),
        "process is gone or belongs to another user"
    );
    Ok(parts[..5].join(" "))
}

/// Revalidate immediately before moving: same server, socket, pane,
/// pane shell, agent PID, and both process start tokens.
pub fn valid(identity: &PaneIdentity, config: &TmuxConfig) -> bool {
    if process_start(identity.client_pid).ok().as_deref() != Some(&identity.client_start) {
        return false;
    }
    discover(config).ok().is_some_and(|panes| {
        panes.iter().any(|pane| {
            socket_key(&pane.socket) == socket_key(&identity.socket)
                && pane.server == identity.server
                && pane.pane == identity.pane
                && pane.pane_pid == identity.pane_pid
                && pane.pane_start == identity.pane_start
                && contains_process(identity, identity.client_pid, &identity.client_start)
                    .unwrap_or(false)
        })
    })
}

/// Move to a probable pane after rechecking title, cwd, and processes.
///
/// This is explicitly pane navigation, not proof of which conversation
/// the process holds. Renamed panes and reused PIDs are rejected.
pub fn focus_probable(identity: &PaneIdentity) -> Result<()> {
    let config = TmuxConfig {
        sockets: vec![identity.socket.clone()],
    };
    let panes = discover(&config)?;
    ensure!(
        panes.iter().any(|p| same_hint(p, identity)),
        "The pane or its title changed: refresh and choose again"
    );
    focus(identity, &config)
}
pub(crate) fn same_hint(current: &PaneIdentity, selected: &PaneIdentity) -> bool {
    socket_key(&current.socket) == socket_key(&selected.socket)
        && current.server == selected.server
        && current.pane == selected.pane
        && current.client_pid == selected.client_pid
        && current.client_start == selected.client_start
        && current.title == selected.title
        && current.cwd == selected.cwd
        && current.provider.is_some()
        && current.provider == selected.provider
}

pub fn focus(identity: &PaneIdentity, config: &TmuxConfig) -> Result<()> {
    ensure!(
        valid(identity, config),
        "Opening is no longer verifiable; wait for the bridge or run /reload"
    );
    // Switch only the caller's own tmux client, never another attached client.
    let own = std::env::var("TMUX").context("Run the monitor inside tmux to jump to a pane")?;
    let own_socket = own
        .rsplit_once(',')
        .and_then(|(value, _)| value.rsplit_once(','))
        .map(|(socket, _)| socket_key(socket));
    ensure!(
        own_socket.as_deref() == Some(socket_key(&identity.socket).as_str()),
        "pane belongs to another server; attach manually with tmux -S {} attach",
        identity.socket
    );
    let monitor_pane = std::env::var("TMUX_PANE").context("Monitor pane is not identifiable")?;
    let session = tmux(
        &identity.socket,
        &[
            "display-message",
            "-p",
            "-t",
            &monitor_pane,
            "#{session_name}",
        ],
    )?;
    let clients = tmux(
        &identity.socket,
        &["list-clients", "-F", "#{client_tty}\t#{client_session}"],
    )?;
    let hint = client_hint();
    let client = requesting_client(&clients, session.trim(), hint.as_deref())?;
    tmux(
        &identity.socket,
        &["switch-client", "-c", &client, "-t", &identity.pane],
    )?;
    tmux(&identity.socket, &["select-pane", "-t", &identity.pane])?;
    Ok(())
}

/// Resume the SAME Pi JSONL in a fresh detached window/session, then reach it.
///
/// Prefers a new window in the one tmux session already rooted in the
/// project directory; otherwise creates a fresh detached session with a
/// unique name. Every tmux invocation passes argv directly (execvp):
/// file paths and titles are never interpolated into a shell command.
pub fn resume(file: &Path, cwd: &Path, title: &str) -> Result<()> {
    let file = file.to_str().context("session path is not UTF-8")?;
    resume_command(&["pi".into(), "--session".into(), file.into()], cwd, title)
}
fn current_socket() -> Option<String> {
    std::env::var("TMUX").ok().and_then(|v| {
        v.rsplit_once(',')
            .and_then(|(v, _)| v.rsplit_once(','))
            .map(|(s, _)| s.to_owned())
    })
}

/// Create a detached opening and return its exact identity. Deliberately omits
/// the monitor pane: bulk recovery cannot attach or move any tmux client.
pub fn resume_command_detached(
    argv: &[String],
    cwd: &Path,
    title: &str,
) -> Result<DetachedOpening> {
    resume_command_with(
        std::ffi::OsStr::new("tmux"),
        current_socket().as_deref(),
        None,
        argv,
        cwd,
        title,
    )?
    .context("tmux did not report the detached session")
}

pub fn resume_command(argv: &[String], cwd: &Path, title: &str) -> Result<()> {
    let socket = current_socket();
    let monitor = std::env::var("TMUX_PANE").ok();
    let opened = resume_command_with(
        std::ffi::OsStr::new("tmux"),
        socket.as_deref(),
        monitor.as_deref(),
        argv,
        cwd,
        title,
    )?;
    if let Some(opening) = opened {
        let target = opening.session;
        use std::io::IsTerminal;
        if !std::io::stdin().is_terminal() || !std::io::stdout().is_terminal() {
            println!(
                "Conversation reopened in tmux session {}",
                crate::paths::line(&target)
            );
            return Ok(());
        }
        let mut command = Command::new("tmux");
        if let Some(socket) = socket {
            command.args(["-S", &socket]);
        }
        let status = command.args(["attach-session", "-t", &target]).status()?;
        ensure!(status.success(), "Could not attach to the new tmux session");
    }
    Ok(())
}
#[cfg(test)]
fn resume_with(
    program: &std::ffi::OsStr,
    socket: Option<&str>,
    monitor: Option<&str>,
    file: &Path,
    cwd: &Path,
    title: &str,
) -> Result<Option<String>> {
    Ok(resume_command_with(
        program,
        socket,
        monitor,
        &[
            "pi".into(),
            "--session".into(),
            file.to_string_lossy().into_owned(),
        ],
        cwd,
        title,
    )?
    .map(|opening| opening.session))
}

/// Identity returned by our own create command. tmux pane ids do not repeat
/// within the same server lifetime. Never infer ownership from a title/cwd.
#[derive(Debug)]
pub struct DetachedOpening {
    pub session: String,
    pane: String,
    socket: String,
    server: String,
}
impl DetachedOpening {
    pub fn owns(&self, pane: &PaneIdentity) -> bool {
        self.pane == pane.pane
            && self.server == pane.server
            && socket_key(&self.socket) == socket_key(&pane.socket)
    }
}

fn resume_command_with(
    program: &std::ffi::OsStr,
    socket: Option<&str>,
    monitor: Option<&str>,
    argv: &[String],
    cwd: &Path,
    title: &str,
) -> Result<Option<DetachedOpening>> {
    ensure!(!argv.is_empty(), "missing agent command");
    let run = |args: &[&str]| -> Result<String> {
        let mut command = Command::new(program);
        if let Some(socket) = socket {
            command.args(["-S", socket]);
        }
        let out = crate::paths::output(command.args(args))?;
        ensure!(
            out.status.success(),
            "tmux: {}",
            crate::paths::line(&String::from_utf8_lossy(&out.stderr))
        );
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    };
    // Resolve the requesting client BEFORE creating anything. Two clients
    // on the monitor session is an error, never permission to guess
    // which terminal to move.
    let client = if let Some(pane) = monitor {
        if let Ok(session) = run(&["display-message", "-p", "-t", pane, "#{session_name}"]) {
            Some(requesting_client(
                &run(&["list-clients", "-F", "#{client_tty}\t#{client_session}"])?,
                session.trim(),
                client_hint().as_deref(),
            )?)
        } else {
            None
        }
    } else {
        None
    };
    let listing = run(&[
        "list-panes",
        "-a",
        "-F",
        "#{session_id}\t#{pane_current_path}",
    ])
    .unwrap_or_default();
    let mut matching = listing
        .lines()
        .filter_map(|line| line.split_once('\t'))
        .filter(|(_, path)| {
            crate::paths::canonical(Path::new(path)) == crate::paths::canonical(cwd)
        })
        .map(|(id, _)| id.to_owned())
        .collect::<Vec<_>>();
    matching.sort();
    matching.dedup();
    let name = crate::model::project_name(&cwd.to_string_lossy())
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '-' | '_') {
                c
            } else {
                '-'
            }
        })
        .take(40)
        .collect::<String>();
    let unique = format!(
        "{}-{}",
        if name.is_empty() { "pi" } else { &name },
        &uuid::Uuid::new_v4().to_string()[..8]
    );
    let cwd = cwd.to_str().context("Project directory is not UTF-8")?;
    let target = matching.first().map(|id| format!("{id}:"));
    let name = crate::paths::line(title);
    let mut args = if matching.len() == 1 {
        vec![
            "new-window",
            "-d",
            "-P",
            "-F",
            "#{pane_id}\t#{session_id}\t#{socket_path}\t#{pid}:#{start_time}",
            "-t",
            target.as_deref().unwrap(),
            "-n",
            &name,
            "-c",
            cwd,
        ]
    } else {
        vec![
            "new-session",
            "-d",
            "-P",
            "-F",
            "#{pane_id}\t#{session_id}\t#{socket_path}\t#{pid}:#{start_time}",
            "-s",
            &unique,
            "-n",
            &name,
            "-c",
            cwd,
        ]
    };
    args.extend(argv.iter().map(String::as_str));
    let output = run(&args)?;
    let fields = output.trim().split('\t').collect::<Vec<_>>();
    ensure!(
        fields.len() == 4 && fields.iter().all(|f| !f.is_empty()),
        "tmux did not report the new opening"
    );
    let (pane, session) = (fields[0], fields[1]);
    if let Some(client) = client {
        run(&["switch-client", "-c", &client, "-t", pane])?;
        run(&["select-pane", "-t", pane])?;
        Ok(None)
    } else {
        Ok(Some(DetachedOpening {
            session: session.to_owned(),
            pane: pane.to_owned(),
            socket: fields[2].to_owned(),
            server: fields[3].to_owned(),
        }))
    }
}

fn requesting_client(text: &str, session: &str, hint: Option<&str>) -> Result<String> {
    let candidates: Vec<_> = text
        .lines()
        .filter_map(|line| line.split_once('\t'))
        .filter(|(_, name)| *name == session)
        .map(|(tty, _)| tty)
        .collect();
    if let Some(tty) = hint {
        ensure!(
            candidates.contains(&tty),
            "The requested client is not attached to the monitor tmux session"
        );
        Ok(tty.to_string())
    } else {
        ensure!(
            candidates.len() == 1,
            "requesting tmux client is ambiguous; set TMUX_AGENT_MONITOR_TMUX_CLIENT to its client_tty"
        );
        Ok(candidates[0].to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn probable_focus_rejects_reused_pids_and_renamed_panes() {
        let pane = PaneIdentity {
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
            title: "π - release-notes - project".into(),
            provider: Some(Provider::Pi),
            activity: None,
            activity_evidence: None,
        };
        assert!(same_hint(&pane, &pane));
        let owned = DetachedOpening {
            session: "$1".into(),
            pane: pane.pane.clone(),
            socket: pane.socket.clone(),
            server: pane.server.clone(),
        };
        assert!(owned.owns(&pane));
        for field in ["pane", "socket", "server"] {
            let mut foreign = pane.clone();
            match field {
                "pane" => foreign.pane = "%different".into(),
                "socket" => foreign.socket = "/other/socket".into(),
                _ => foreign.server = "restarted-server".into(),
            }
            assert!(!owned.owns(&foreign));
        }
        for provider in [Provider::Claude, Provider::Codex, Provider::Opencode] {
            let mut other = pane.clone();
            other.provider = Some(provider);
            assert!(same_hint(&other, &other));
            assert!(!same_hint(&other, &pane));
        }
        let mut changed = pane.clone();
        changed.title = "π - another - project".into();
        assert!(!same_hint(&changed, &pane));
        changed = pane.clone();
        changed.client_start = "new start".into();
        assert!(!same_hint(&changed, &pane));
        changed = pane.clone();
        changed.cwd = "/repo/elsewhere".into();
        assert!(!same_hint(&changed, &pane));
        changed = pane.clone();
        changed.socket = "/other/server".into();
        assert!(!same_hint(&changed, &pane));
    }
    #[test]
    fn resume_creates_only_new_windows_or_sessions_with_exact_argv() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("fake-tmux");
        fs_fixture(&script);
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o700)).unwrap();
        let cwd = dir.path().join("project with spaces");
        std::fs::create_dir(&cwd).unwrap();
        let file = cwd.join("file;$(not-a-command).jsonl");
        // No server/session: a fresh detached session is created, with the
        // original JSONL as one argv element; attaching belongs to the caller.
        let target = resume_with(
            script.as_os_str(),
            None,
            None,
            &file,
            &cwd,
            "release ; name",
        )
        .unwrap();
        assert_eq!(target.as_deref(), Some("$9"));
        let log = std::fs::read_to_string(dir.path().join("log")).unwrap();
        let commands = log
            .lines()
            .map(|l| serde_json::from_str::<Vec<String>>(l).unwrap())
            .collect::<Vec<_>>();
        let create = commands
            .iter()
            .find(|args| args[0] == "new-session")
            .unwrap();
        assert!(
            create
                .windows(3)
                .any(|w| w == ["pi", "--session", file.to_str().unwrap()])
        );
        assert!(create.contains(&cwd.to_string_lossy().into_owned()));
        assert!(create.contains(&"release ; name".into()));
        assert!(
            !commands
                .iter()
                .any(|args| args[0].starts_with("kill-") || args[0] == "send-keys")
        );
        std::fs::write(dir.path().join("panes"), format!("$1\t{}\n", cwd.display())).unwrap();
        std::fs::write(dir.path().join("log"), "").unwrap();
        resume_with(script.as_os_str(), None, None, &file, &cwd, "release").unwrap();
        let log = std::fs::read_to_string(dir.path().join("log")).unwrap();
        assert!(log.contains("new-window"));
        assert!(!log.contains("new-session"));
        std::fs::write(dir.path().join("clients"), "/dev/fakeA\tmonitor\n").unwrap();
        assert!(
            resume_with(
                script.as_os_str(),
                None,
                Some("%monitor"),
                &file,
                &cwd,
                "release"
            )
            .unwrap()
            .is_none()
        );
        let log = std::fs::read_to_string(dir.path().join("log")).unwrap();
        assert!(log.contains("switch-client"));
        assert!(log.contains("/dev/fakeA"));
        std::fs::write(
            dir.path().join("clients"),
            "/dev/fakeA\tmonitor\n/dev/fakeB\tmonitor\n",
        )
        .unwrap();
        std::fs::write(dir.path().join("log"), "").unwrap();
        assert!(
            resume_with(
                script.as_os_str(),
                None,
                Some("%monitor"),
                &file,
                &cwd,
                "release"
            )
            .is_err()
        );
        let log = std::fs::read_to_string(dir.path().join("log")).unwrap();
        assert!(!log.contains("new-window"));
        assert!(!log.contains("new-session"));
        for argv in [
            vec!["claude", "--resume", "native-id"],
            vec!["codex", "resume", "native-id"],
            vec!["opencode", "--session", "ses_one"],
        ] {
            std::fs::write(dir.path().join("log"), "").unwrap();
            let command = argv.iter().map(|s| s.to_string()).collect::<Vec<_>>();
            resume_command_with(script.as_os_str(), None, None, &command, &cwd, "fixture").unwrap();
            let log = std::fs::read_to_string(dir.path().join("log")).unwrap();
            let commands = log
                .lines()
                .map(|l| serde_json::from_str::<Vec<String>>(l).unwrap())
                .collect::<Vec<_>>();
            let create = commands.iter().find(|a| a[0] == "new-window").unwrap();
            assert_eq!(&create[create.len() - command.len()..], command.as_slice());
            assert!(!log.contains("send-keys") && !log.contains("kill-"));
        }
    }
    fn fs_fixture(script: &Path) {
        std::fs::write(
            script,
            r#"#!/usr/bin/env python3
import json,sys
from pathlib import Path
root=Path(__file__).parent
args=sys.argv[1:]
with (root/'log').open('a') as file: file.write(json.dumps(args)+'\n')
if args[0]=='list-panes':
    if (root/'panes').exists(): print((root/'panes').read_text(),end='')
    else: sys.exit(1)
elif args[0] in ('new-session','new-window'): print('%99\t$9\t/fake/socket\t1:1234')
elif args[0]=='display-message': print('monitor')
elif args[0]=='list-clients': print((root/'clients').read_text(),end='')
elif args[0] in ('switch-client','select-pane'): pass
else: sys.exit(1)
"#,
        )
        .unwrap();
    }
    #[test]
    fn provider_matches_exact_names_and_per_command_wrapper_hints() {
        // Exact basenames only: substrings and unknown tools are ignored.
        // Full `args=` lines, as processes() stores them.
        assert_eq!(provider("/usr/local/bin/pi"), Some(Provider::Pi));
        assert_eq!(
            provider("pi-coding-agent --session f.jsonl"),
            Some(Provider::Pi)
        );
        assert_eq!(provider("codex"), Some(Provider::Codex));
        assert_eq!(provider("not-codex"), None);
        assert_eq!(provider("zsh"), None);
        // Truncated-basename lookalikes from comm= must not match.
        assert_eq!(provider("pi-coding-age"), None);
        // Opaque wrappers: only per-command hints (including legacy aliases).
        assert_eq!(
            provider("TMUX_AGENT_MONITOR_AGENT=claude fence -- claude"),
            Some(Provider::Claude)
        );
        assert_eq!(
            provider("ssh --env TMUX_AGENT_MONITOR_AGENT=codex host"),
            Some(Provider::Codex)
        );
        assert_eq!(provider("fence -- claude"), None);
        assert_eq!(
            provider("AGENT_MONITOR_AGENT=claude fence -- claude"),
            Some(Provider::Claude)
        );
        assert_eq!(
            provider("/usr/bin/ssh --env AGENT_MONITOR_AGENT=codex host"),
            Some(Provider::Codex)
        );
        // The process environment is never consulted: only the command line
        // carries the hint, so a global export cannot mislabel panes.
        // (provider() takes no env input by construction.)
        assert_eq!(provider("fence -- claude"), None);
        // Unknown wrapper values never guess.
        assert_eq!(provider("AGENT_MONITOR_AGENT=bogus fence"), None);
    }
    #[test]
    fn capture_pane_is_read_only_bounded_and_capped() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("fake-tmux"),
            "#!/usr/bin/env python3\nimport json,sys\nfrom pathlib import Path\nroot=Path(__file__).parent\nargs=sys.argv[1:]\nwith (root/'log').open('a') as f: f.write(json.dumps(args)+'\\n')\nif args[2:4]==['capture-pane','-p']: print('line1\\nline2')\nelse: sys.exit(1)\n",
        )
        .unwrap();
        let fake = dir.path().join("fake-tmux");
        std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o700)).unwrap();
        let out = capture_with(fake.as_os_str(), "/fake/sock", "%9", 40).unwrap();
        assert_eq!(out, "line1\nline2\n");
        // 0 clamps to 1, huge values clamp to 200.
        assert_eq!(
            capture_with(fake.as_os_str(), "/fake/sock", "%9", 0).unwrap(),
            "line2\n"
        );
        capture_with(fake.as_os_str(), "/fake/sock", "%9", 9999).unwrap();
        // Over-cap output is refused, never truncated-and-matched.
        std::fs::write(&fake, "#!/bin/sh\nhead -c 70000 /dev/zero | tr '\\0' 'x'\n").unwrap();
        assert!(capture_with(fake.as_os_str(), "/fake/sock", "%9", 40).is_err());
        let commands = std::fs::read_to_string(dir.path().join("log")).unwrap();
        assert!(commands.contains("\"-S\", \"0\""));
        assert!(!commands.contains("\"-40\"") && !commands.contains("\"-200\""));
        // Read-only: capture must never send input or touch panes.
        for forbidden in [
            "send-keys",
            "send-prefix",
            "kill-pane",
            "kill-window",
            "new-window",
            "new-session",
            "set-option",
        ] {
            assert!(!commands.contains(forbidden), "forbidden: {forbidden}");
        }
    }
    #[test]
    fn focus_never_guesses_between_attached_clients() {
        let clients = "/dev/ttyA\tboard\n/dev/ttyB\tboard\n/dev/ttyC\tother\n";
        assert!(requesting_client(clients, "board", None).is_err());
        assert_eq!(
            requesting_client(clients, "board", Some("/dev/ttyB")).unwrap(),
            "/dev/ttyB"
        );
        assert!(requesting_client(clients, "board", Some("/dev/ttyC")).is_err());
        assert_eq!(
            requesting_client(clients, "other", None).unwrap(),
            "/dev/ttyC"
        );
    }
}
