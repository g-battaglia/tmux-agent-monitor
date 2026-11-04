//! Deterministic four-agent preview with synthetic folders and conversations.
//! No real agent, tmux server, session file, or catalog is read.
use ratatui::{Terminal, backend::TestBackend};
use tmux_agent_monitor::{
    activity::{AgentActivity, MatchEvidence},
    model::*,
    tmux::PaneIdentity,
    ui::{App, render},
};
fn main() -> anyhow::Result<()> {
    let mut app = App::default();
    // Presentation only: this synthetic fixture never constructs a sender.
    app.notifications.preferences.enabled = true;
    app.notifications.owner = true;
    app.notifications
        .preferences
        .sessions
        .insert("demo-2".into(), false);
    let demos = [
        (
            Provider::Claude,
            "website-redesign",
            "/repo/acme-website",
            true,
        ),
        (
            Provider::Codex,
            "api-migration",
            "/repo/acme-website/api",
            true,
        ),
        (Provider::Pi, "mobile-spacing", "/repo/acme-website", true),
        (Provider::Opencode, "test-tooling", "/repo/tooling", true),
        (
            Provider::Claude,
            "onboarding-flow",
            "/repo/acme-website",
            false,
        ),
        (
            Provider::Codex,
            "search-tuning",
            "/repo/acme-website/api",
            false,
        ),
        (Provider::Pi, "release-notes", "/repo/acme-website", false),
        (Provider::Opencode, "perf-audit", "/repo/tooling", false),
    ];
    app.catalog.sessions = demos
        .into_iter()
        .enumerate()
        .map(|(i, (provider, name, cwd, open))| {
            let probable = if open {
                vec![PaneIdentity {
                    socket: "/synthetic/tmux".into(),
                    server: "demo".into(),
                    pane: format!("%{i}"),
                    pane_pid: 0,
                    pane_start: "synthetic".into(),
                    client_pid: 0,
                    client_start: "synthetic".into(),
                    target: format!("demo:{i}.0"),
                    session: "demo".into(),
                    window: i.to_string(),
                    cwd: cwd.into(),
                    command: provider.label().into(),
                    title: name.into(),
                    provider: Some(provider),
                    activity: Some(match i {
                        0 => AgentActivity::Working,
                        1 => AgentActivity::Finished,
                        2 => AgentActivity::Blocked,
                        _ => AgentActivity::Idle,
                    }),
                    activity_evidence: Some(MatchEvidence {
                        rule_id: "synthetic-status".into(),
                        manifest_source: "bundled".into(),
                        manifest_version: 2,
                        observation_key: format!("synthetic-opening-{i}"),
                        ..Default::default()
                    }),
                }]
            } else {
                vec![]
            };
            Session {
                id: format!("demo-{i}"),
                metadata: Metadata {
                    provider,
                    name: name.into(),
                    cwd: cwd.into(),
                    ..Default::default()
                },
                state: if i == 0 || i == 2 {
                    ResumeState::Resume
                } else {
                    ResumeState::History
                },
                note: if i == 0 {
                    "Check the hero spacing in a real browser.".into()
                } else {
                    String::new()
                },
                done_revision: 0,
                done_at: 0,
                available: true,
                presence_verified: false,
                bindings: vec![],
                probable,
            }
        })
        .collect();
    app.refresh_filter();
    app.select(0);
    app.detail_id = app.selected.clone();
    app.conversation.messages = vec![
        Message {
            role: "You".into(),
            text: "Check the new landing copy.".into(),
            time: 0,
            tool: false,
        },
        Message {
            role: "Claude".into(),
            text: "The draft is ready for review.\nMobile spacing still needs checking.".into(),
            time: 0,
            tool: false,
        },
    ];
    let args = std::env::args().skip(1).collect::<Vec<_>>();
    if args.iter().any(|a| a == "--menu" || a == "--menu-live") {
        let live = args.iter().any(|a| a == "--menu-live");
        if live {
            app.catalog.sessions[0].id = "pane-demo".into();
            app.catalog.sessions[0].state = ResumeState::History;
            app.catalog.sessions[0].note.clear();
            app.catalog.sessions[0].metadata.name =
                "Review navigation, keyboard shortcuts, and accessibility in the website mockup"
                    .into();
            app.select(0);
        }
        let mut menu = tmux_agent_monitor::ui::menu::CommandMenu::new(&app);
        if live {
            menu.selected = menu.actions.iter().position(|a| a.shortcut == "n").unwrap();
        }
        app.command_menu = Some(menu);
    }
    let mut terminal = Terminal::new(TestBackend::new(120, 32))?;
    terminal.draw(|frame| render::draw(frame, &app))?;
    let buffer = terminal.backend().buffer();
    if args.iter().any(|a| a == "--cells") {
        let cells = (0..32)
            .map(|y| {
                (0..120)
                    .map(|x| {
                        let cell = &buffer[(x, y)];
                        serde_json::json!([
                            cell.symbol(),
                            format!("{:?}", cell.fg),
                            format!("{:?}", cell.bg),
                            cell.modifier.contains(ratatui::style::Modifier::REVERSED)
                        ])
                    })
                    .collect::<Vec<_>>()
            })
            .collect::<Vec<_>>();
        println!("{}", serde_json::to_string(&cells)?);
        return Ok(());
    }
    for y in 0..32 {
        for x in 0..120 {
            print!("{}", buffer[(x, y)].symbol());
        }
        println!();
    }
    Ok(())
}
