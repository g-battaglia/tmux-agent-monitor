//! CLI entry point: human names in the explorer, stable ids in scripts.
//!
//! With no subcommand (or `pick`) this opens the interactive TUI, which
//! requires a real terminal. Every other subcommand runs headlessly and
//! prints text or JSON. Starting an agent always needs explicit confirmation
//! (`--resume` in scripts); verified/probable openings block duplicates.
use anyhow::{Context, Result, ensure};
use clap::{Parser, Subcommand};
use std::{
    io::{self, IsTerminal, Write},
    path::PathBuf,
};
use tmux_agent_monitor::{
    model::{ResumeState, View, project_name},
    paths, presence,
    service::Service,
    ui,
};

#[derive(Parser)]
#[command(
    version,
    about = "tmux-native agent monitor for Claude Code, Codex, OpenCode, and Pi"
)]
struct Cli {
    #[arg(long, global = true, env = "TMUX_AGENT_MONITOR_HOME")]
    data_dir: Option<PathBuf>,
    #[command(subcommand)]
    command: Option<Cmd>,
}
#[derive(Subcommand)]
enum Cmd {
    /// Open the explorer; nothing to create, just browse and resume.
    Pick {
        #[arg(long)]
        project: Option<String>,
    },
    /// Sessions to resume (or full history with --all).
    Sessions {
        #[arg(long)]
        project: Option<String>,
        #[arg(long,conflicts_with_all=["open","done"])]
        all: bool,
        #[arg(long, conflicts_with = "done")]
        open: bool,
        #[arg(long)]
        done: bool,
        #[arg(long, default_value = "")]
        search: String,
        #[arg(long)]
        json: bool,
    },
    /// Name, note, conversation, and openings of one session.
    Show {
        id: String,
        #[arg(long)]
        json: bool,
        #[arg(long)]
        tools: bool,
    },
    /// Inspect project folders, agent counts, storage, and session metadata.
    Details {
        #[arg(conflicts_with = "project")]
        id: Option<String>,
        #[arg(long)]
        project: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Jump to an open agent; starting one needs explicit confirmation.
    Open {
        id: String,
        #[arg(long)]
        binding: Option<String>,
        #[arg(long)]
        resume: bool,
    },
    Done {
        id: String,
    },
    Reopen {
        id: String,
    },
    Note {
        id: String,
        text: String,
    },
    Undo {
        id: String,
    },
    /// Refresh the catalog without starting or touching any agent.
    Sync,
    /// Live agent panes and screen-state debugging (terminal only).
    Agent {
        #[command(subcommand)]
        command: Agent,
    },
    Sources {
        #[command(subcommand)]
        command: Sources,
    },
    Integration {
        #[command(subcommand)]
        command: Integration,
    },
}
#[derive(Subcommand)]
enum Agent {
    /// Explain one pane's screen classification with provenance.
    /// `--file` classifies saved text offline; otherwise captures live.
    Explain {
        /// Pane target: `%id`, `socket:pane`, or agent name (unique match).
        target: Option<String>,
        #[arg(long, conflicts_with = "watch")]
        file: Option<PathBuf>,
        /// Observe transitions every two seconds until Ctrl-C.
        #[arg(long)]
        watch: bool,
        /// Bound the watcher (JSON mode emits one object per line).
        #[arg(long,requires="watch",value_parser=clap::value_parser!(u16).range(1..=1000))]
        samples: Option<u16>,
        #[arg(long)]
        agent: Option<String>,
        #[arg(long)]
        json: bool,
    },
}
#[derive(Subcommand)]
enum Sources {
    List,
    Add { path: PathBuf },
}
#[derive(Subcommand)]
enum Integration {
    Pi {
        #[command(subcommand)]
        command: PiIntegration,
    },
}
#[derive(Subcommand)]
enum PiIntegration {
    Status,
    Install,
    Remove,
}
fn main() {
    if let Err(e) = run(Cli::parse()) {
        eprintln!("tmux-agent-monitor: {}", paths::line(&format!("{e:#}")));
        std::process::exit(1);
    }
}
fn print_json(value: &impl serde::Serialize) -> Result<()> {
    println!("{}", serde_json::to_string_pretty(value)?);
    Ok(())
}
fn run(cli: Cli) -> Result<()> {
    let command = match cli.command {
        None => return ui::run(paths::root(cli.data_dir)?, None),
        Some(Cmd::Pick { project }) => return ui::run(paths::root(cli.data_dir)?, project),
        Some(Cmd::Agent {
            command:
                Agent::Explain {
                    target,
                    file,
                    agent,
                    json,
                    watch,
                    samples,
                },
        }) => return explain(target, file, agent, json, watch, samples),
        Some(command) => command,
    };
    let service = Service::open(cli.data_dir)?;
    match command {
        Cmd::Pick { .. } => unreachable!("picker handled before opening SQLite"),
        Cmd::Sources { command } => match command {
            Sources::List => {
                for source in service.store.sources()? {
                    println!(
                        "{}{}",
                        paths::line(&source.path.to_string_lossy()),
                        if source.complete {
                            ""
                        } else {
                            " (indexing incomplete)"
                        }
                    );
                }
                Ok(())
            }
            Sources::Add { path } => service.store.add_source(&path),
        },
        Cmd::Integration {
            command: Integration::Pi { command },
        } => match command {
            PiIntegration::Status => {
                let path = presence::installed_path();
                println!(
                    "{}: {}",
                    if path.exists() {
                        "Installed"
                    } else {
                        "Not installed"
                    },
                    paths::line(&path.to_string_lossy())
                );
                Ok(())
            }
            PiIntegration::Install => {
                let path = presence::install()?;
                println!(
                    "Installed: {}\nRun /reload in every Pi that is already open.",
                    paths::line(&path.to_string_lossy())
                );
                Ok(())
            }
            PiIntegration::Remove => {
                println!(
                    "Removed: {}",
                    paths::line(&presence::remove()?.to_string_lossy())
                );
                Ok(())
            }
        },
        Cmd::Sync => {
            for warning in service.sync()? {
                eprintln!("{}", paths::line(&warning));
            }
            let catalog = service.catalog()?;
            println!(
                "{} conversations, {} unassociated Pi panes",
                catalog.sessions.len(),
                catalog.unbound.len()
            );
            Ok(())
        }
        Cmd::Sessions {
            project,
            all,
            open,
            done,
            search,
            json,
        } => {
            service.sync()?;
            let catalog = service.catalog()?;
            let project = resolve_project(&catalog, project)?;
            let view = if all {
                View::All
            } else if open {
                View::Open
            } else if done {
                View::Done
            } else {
                View::Resume
            };
            let mut app = ui::App {
                catalog,
                project,
                view,
                search,
                ..Default::default()
            };
            app.refresh_filter();
            let rows = app.rows();
            if json {
                return if open {
                    print_json(
                        &serde_json::json!({"sessions":rows,"panes":app.catalog.unbound.iter().filter(|p| app.project.as_ref().is_none_or(|cwd| *cwd == p.cwd)).collect::<Vec<_>>()}),
                    )
                } else {
                    print_json(&rows)
                };
            }
            for s in &rows {
                println!(
                    "[{}] {}  {}  {}{}\n  {}",
                    s.metadata.provider.label(),
                    paths::line(&s.metadata.title()),
                    s.state.label(),
                    s.presence(),
                    s.live_activity()
                        .map(|a| format!("  [{}]", a.label()))
                        .unwrap_or_default(),
                    s.id
                );
            }
            if rows.is_empty() {
                println!("No sessions in {}. Use --all for history.", view.label());
            }
            if !app.catalog.unbound.is_empty() {
                let pi_panes = app
                    .catalog
                    .unbound
                    .iter()
                    .filter(|p| p.provider == Some(tmux_agent_monitor::model::Provider::Pi))
                    .count();
                if pi_panes > 0 {
                    println!(
                        "\n{} unassociated Pi panes: integration pi install, then /reload.",
                        pi_panes
                    );
                }
            }
            Ok(())
        }
        Cmd::Show { id, json, tools } => {
            service.sync()?;
            let catalog = service.catalog()?;
            let s = service.resolve(&catalog, &id)?;
            let leaf = s.bindings.first().and_then(|b| b.bridge.leaf.as_deref());
            let conversation = if s.available {
                service.detail(&s.id, leaf, tools)?
            } else {
                tmux_agent_monitor::model::Conversation {
                    warning: "Conversation unavailable; name and notes kept".into(),
                    ..Default::default()
                }
            };
            if json {
                return print_json(&serde_json::json!({"session":s,"conversation":conversation}));
            }
            println!(
                "{}\n{} · {}\n{}",
                paths::line(&s.metadata.title()),
                s.state.label(),
                s.presence(),
                paths::line(&s.metadata.cwd)
            );
            if s.changed_after_done() {
                println!("Updated after completion");
            }
            if !conversation.warning.is_empty() {
                println!("{}", paths::line(&conversation.warning));
            }
            if !s.note.is_empty() {
                println!("\nNext step\n{}", paths::clean(&s.note));
            }
            for message in conversation.messages {
                println!("\n{}\n{}", message.role, paths::clean(&message.text));
            }
            Ok(())
        }
        Cmd::Details { id, project, json } => {
            service.sync()?;
            let catalog = service.catalog()?;
            let details = if let Some(id) = id {
                tmux_agent_monitor::details::Details::session(
                    &catalog,
                    service.resolve(&catalog, &id)?,
                )
            } else {
                let project = resolve_project(&catalog, project)?;
                tmux_agent_monitor::details::Details::project(&catalog, project.as_deref())
            };
            if json {
                print_json(&details)
            } else {
                println!("{}", details.text());
                Ok(())
            }
        }
        Cmd::Agent { .. } => unreachable!("agent diagnostics do not open the state database"),
        Cmd::Done { id } => service.store.change(&id, Some(ResumeState::Done), None),
        Cmd::Reopen { id } => service.store.change(&id, Some(ResumeState::Resume), None),
        Cmd::Note { id, text } => service.store.change(&id, None, Some(&text)),
        Cmd::Undo { id } => service.store.undo(&id),
        Cmd::Open {
            id,
            binding,
            resume,
        } => {
            service.sync()?;
            let catalog = service.catalog()?;
            let s = service.resolve(&catalog, &id)?;
            if !s.bindings.is_empty() {
                let chosen = if let Some(nonce) = binding {
                    s.bindings
                        .iter()
                        .find(|b| b.bridge.nonce == nonce)
                        .ok_or_else(|| anyhow::anyhow!("opening not found"))?
                } else {
                    ensure!(
                        s.bindings.len() == 1,
                        "multiple openings: pass --binding with the nonce from show --json"
                    );
                    &s.bindings[0]
                };
                return presence::focus(chosen);
            }
            let spec = service.resume_spec(&s.id)?;
            if !spec.warning.is_empty() {
                eprintln!("{}", paths::line(&spec.warning));
            }
            if !resume {
                ensure!(
                    io::stdin().is_terminal() && io::stdout().is_terminal(),
                    "pass --resume to authorize a non-interactive agent start"
                );
                print!(
                    "Resume '{}' in a new tmux window/session, folder {}? [y/N] ",
                    paths::line(&s.metadata.title()),
                    paths::line(&spec.cwd)
                );
                io::stdout().flush()?;
                let mut answer = String::new();
                io::stdin().read_line(&mut answer)?;
                if !matches!(answer.trim(), "s" | "S" | "y" | "Y") {
                    return Ok(());
                }
            }
            let fresh = service.resume_spec(&s.id)?;
            ensure!(
                resume || !spec.warning.is_empty() || fresh.warning.is_empty(),
                "Presence changed: repeat the command and confirm the new situation"
            );
            fresh.launch()
        }
    }
}
/// Explain one pane's screen classification with full provenance,
/// mirroring `herdr agent explain`. Offline `--file` mode is pure and
/// scriptable; live mode captures the pane read-only and classifies it.
/// Never writes, never touches stored state.
fn explain(
    target: Option<String>,
    file: Option<PathBuf>,
    agent: Option<String>,
    json: bool,
    watch: bool,
    samples: Option<u16>,
) -> Result<()> {
    use tmux_agent_monitor::{activity, model::Provider};
    if let Some(path) = file {
        let agent = agent
            .as_deref()
            .and_then(Provider::parse)
            .context("pass --agent with pi, claude, codex, or opencode")?;
        use std::io::Read;
        let mut raw = String::new();
        std::fs::File::open(&path)?
            .take(64 * 1024 + 1)
            .read_to_string(&mut raw)?;
        ensure!(raw.len() <= 64 * 1024, "screen snapshot exceeds 64 KiB");
        let tail = paths::clean(&raw);
        let (state, evidence) = activity::classify(agent, "", &tail);
        let body = serde_json::json!({
            "agent": agent.label(),
            "state": state.label(),
            "rule": evidence.rule_id,
            "manifest": evidence.manifest_source,
            "manifest_version": evidence.manifest_version,
            "fallback": evidence.fallback_reason,
            "title_signal": evidence.title_signal,
            "evidence": evidence,
            "inferred": true,
        });
        if json {
            return print_json(&body);
        }
        println!("{}", activity::describe(agent, state, &evidence));
        return Ok(());
    }
    let wanted = agent.as_deref().and_then(Provider::parse);
    if agent.is_some() && wanted.is_none() {
        anyhow::bail!("unknown agent: use pi, claude, codex, or opencode");
    }
    let mut tracker = activity::ActivityTracker::default();
    let origin = std::time::Instant::now();
    let mut identity = None;
    let mut sample: u64 = 0;
    loop {
        let panes =
            tmux_agent_monitor::tmux::discover(&tmux_agent_monitor::tmux::TmuxConfig::default())?;
        let mut candidates: Vec<_> = panes
            .into_iter()
            .filter(|p| p.provider.is_some())
            .filter(|p| wanted.is_none_or(|w| p.provider == Some(w)))
            .collect();
        if let Some(target) = target.as_deref() {
            let trimmed: Vec<_> = candidates
                .into_iter()
                .filter(|p| {
                    p.pane == target
                        || format!("{}:{}", p.socket, p.pane) == target
                        || p.provider.is_some_and(|pr| pr.label() == target)
                })
                .collect();
            candidates = trimmed;
        }
        ensure!(
            candidates.len() == 1,
            "pane target is ambiguous or missing: use %id, socket:pane, or a unique agent name"
        );
        let pane = &candidates[0];
        let provider = pane.provider.context("pane has no recognized agent")?;
        let key = activity::pane_key(pane, "");
        if let Some(expected) = &identity {
            ensure!(
                expected == &key,
                "agent identity changed; restart the watcher"
            );
        } else {
            identity = Some(key.clone());
        }
        let (state, mut evidence, signature) =
            match tmux_agent_monitor::tmux::capture_pane(&pane.socket, &pane.pane, 80) {
                Ok(raw) => {
                    let tail = paths::clean(&raw);
                    let signature = (!tail.trim().is_empty())
                        .then(|| tmux_agent_monitor::pi::hash(tail.as_bytes()));
                    let (s, e) = activity::classify(provider, &pane.title, &tail);
                    (s, e, signature)
                }
                Err(_) => (
                    activity::AgentActivity::Unknown,
                    activity::MatchEvidence {
                        fallback_reason: "terminal-capture-unavailable".into(),
                        ..Default::default()
                    },
                    None,
                ),
            };
        evidence.sampled_at = paths::now();
        let (state, evidence) = if watch {
            tracker.observe(
                &key,
                state,
                evidence,
                signature,
                origin.elapsed().as_millis().min(u64::MAX as u128) as u64,
            )
        } else {
            (state, evidence)
        };
        let body = serde_json::json!({
            "agent": provider.label(),
            "target": pane.target,
            "title": pane.title,
            "state": state.label(),
            "rule": evidence.rule_id,
            "manifest": evidence.manifest_source,
            "manifest_version": evidence.manifest_version,
            "fallback": evidence.fallback_reason,
            "title_signal": evidence.title_signal,
            "evidence": evidence,
            "inferred": true,
        });
        if json {
            if watch {
                println!("{}", serde_json::to_string(&body)?);
            } else {
                return print_json(&body);
            }
        } else {
            println!(
                "{}  {}",
                paths::line(&pane.target),
                activity::describe(provider, state, &evidence)
            );
        }
        sample += 1;
        if !watch || samples.is_some_and(|n| sample >= u64::from(n)) {
            break;
        }
        std::thread::sleep(std::time::Duration::from_secs(2));
    }
    Ok(())
}
fn resolve_project(
    catalog: &tmux_agent_monitor::model::Catalog,
    project: Option<String>,
) -> Result<Option<String>> {
    let Some(project) = project else {
        return Ok(None);
    };
    let canonical = paths::canonical(std::path::Path::new(&project))
        .to_string_lossy()
        .into_owned();
    let paths = catalog
        .sessions
        .iter()
        .map(|s| s.metadata.cwd.clone())
        .chain(catalog.unbound.iter().map(|p| p.cwd.clone()))
        .collect::<std::collections::BTreeSet<_>>();
    let matches = paths
        .into_iter()
        .filter(|p| {
            p == &project
                || p == &canonical
                || project_name(p) == project
                || tmux_agent_monitor::model::project_label(p).eq_ignore_ascii_case(&project)
        })
        .collect::<Vec<_>>();
    ensure!(
        matches.len() == 1,
        "project not found or ambiguous: use the full path"
    );
    Ok(matches.into_iter().next())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn agent_watch_arguments_are_bounded_and_snapshot_mode_is_exclusive() {
        for args in [
            vec!["--samples", "3"],
            vec!["--watch", "--samples", "0"],
            vec!["--watch", "--samples", "1001"],
            vec!["--watch", "--file", "screen.txt"],
        ] {
            assert!(
                Cli::try_parse_from(
                    ["tmux-agent-monitor", "agent", "explain"]
                        .into_iter()
                        .chain(args)
                )
                .is_err()
            );
        }
        let cli = Cli::try_parse_from([
            "tmux-agent-monitor",
            "agent",
            "explain",
            "%1",
            "--watch",
            "--samples",
            "3",
            "--json",
        ])
        .unwrap();
        assert!(matches!(
            cli.command,
            Some(Cmd::Agent {
                command: Agent::Explain {
                    watch: true,
                    samples: Some(3),
                    json: true,
                    ..
                }
            })
        ));
    }
}
