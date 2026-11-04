//! Metadata-only details shared by the CLI and TUI. No filesystem I/O.
use crate::{model::*, paths};
use serde::Serialize;
use std::collections::BTreeSet;

#[derive(Debug, Serialize)]
pub struct AgentCount {
    pub agent: Provider,
    pub sessions: usize,
    pub open: usize,
}
#[derive(Debug, Serialize)]
pub struct ProjectDetails {
    pub name: String,
    pub folder: Option<String>,
    pub includes_descendants: bool,
    pub saved_sessions: usize,
    pub live_terminals: usize,
    pub open: usize,
    pub to_resume: usize,
    pub done: usize,
    pub last_exchange: i64,
    pub agents: Vec<AgentCount>,
    pub storage_locations: Vec<String>,
}
#[derive(Debug, Serialize)]
pub struct SessionDetails {
    pub id: String,
    pub native_id: String,
    pub title: String,
    pub agent: Provider,
    pub state: ResumeState,
    pub presence: String,
    pub live_activity: Option<crate::activity::AgentActivity>,
    pub activity_evidence: Vec<String>,
    pub folder: String,
    pub source_file: Option<String>,
    pub parent: Option<String>,
    pub created: i64,
    pub updated: i64,
    pub available: bool,
    pub changed_after_done: bool,
    pub warning: String,
    pub note: String,
    pub openings: Vec<String>,
}
#[derive(Debug, Serialize)]
pub struct Details {
    pub project: ProjectDetails,
    pub session: Option<SessionDetails>,
}
fn live(s: &Session) -> bool {
    s.id.starts_with("pane-") || s.id.starts_with("live-")
}
impl Details {
    pub fn project(catalog: &Catalog, folder: Option<&str>) -> Self {
        Self::project_scope(catalog, folder, false)
    }
    pub fn folder_group(catalog: &Catalog, folder: &str) -> Self {
        Self::project_scope(catalog, Some(folder), true)
    }
    fn project_scope(catalog: &Catalog, folder: Option<&str>, includes_descendants: bool) -> Self {
        let rows = catalog
            .sessions
            .iter()
            .filter(|s| {
                folder.is_none_or(|cwd| {
                    if includes_descendants {
                        std::path::Path::new(&s.metadata.cwd).starts_with(cwd)
                    } else {
                        s.metadata.cwd == cwd
                    }
                })
            })
            .collect::<Vec<_>>();
        let mut storage = BTreeSet::new();
        for s in rows.iter().filter(|s| !live(s)) {
            let path = s.metadata.source_file();
            if !path.as_os_str().is_empty() {
                let location = if s.metadata.database.is_some() {
                    path
                } else {
                    path.parent().unwrap_or(path)
                };
                storage.insert(paths::line(&location.to_string_lossy()));
            }
        }
        Self {
            project: ProjectDetails {
                name: folder
                    .map(project_label)
                    .unwrap_or_else(|| "All projects".into()),
                folder: folder.map(str::to_string),
                includes_descendants,
                saved_sessions: rows.iter().filter(|s| !live(s)).count(),
                live_terminals: rows.iter().filter(|s| live(s)).count(),
                open: rows.iter().filter(|s| View::Open.matches(s)).count(),
                to_resume: rows
                    .iter()
                    .filter(|s| !live(s) && s.state == ResumeState::Resume)
                    .count(),
                done: rows
                    .iter()
                    .filter(|s| !live(s) && s.state == ResumeState::Done)
                    .count(),
                last_exchange: rows.iter().map(|s| s.metadata.updated).max().unwrap_or(0),
                agents: [
                    Provider::Claude,
                    Provider::Codex,
                    Provider::Pi,
                    Provider::Opencode,
                ]
                .into_iter()
                .filter_map(|agent| {
                    let sessions = rows.iter().filter(|s| s.metadata.provider == agent).count();
                    (sessions > 0).then(|| AgentCount {
                        agent,
                        sessions,
                        open: rows
                            .iter()
                            .filter(|s| s.metadata.provider == agent && View::Open.matches(s))
                            .count(),
                    })
                })
                .collect(),
                storage_locations: storage.into_iter().collect(),
            },
            session: None,
        }
    }
    pub fn session(catalog: &Catalog, session: &Session) -> Self {
        let mut details = Self::project(catalog, Some(&session.metadata.cwd));
        let mut openings = session
            .bindings
            .iter()
            .filter_map(|b| b.pane.as_ref())
            .chain(session.probable.iter())
            .map(|p| {
                format!(
                    "{} · {} · {}",
                    paths::line(&p.target),
                    paths::line(&p.title),
                    p.activity.map(|a| a.label()).unwrap_or("unsampled")
                )
            })
            .collect::<Vec<_>>();
        openings.sort();
        openings.dedup();
        details.session = Some(SessionDetails {
            id: session.id.clone(),
            native_id: session.metadata.native_id.clone(),
            title: session.metadata.title(),
            agent: session.metadata.provider,
            state: session.state,
            presence: session.presence().into(),
            live_activity: session.live_activity(),
            activity_evidence: session
                .openings()
                .filter_map(|p| {
                    Some(format!(
                        "{}: {}",
                        paths::line(&p.target),
                        crate::activity::describe(
                            p.provider?,
                            p.activity?,
                            p.activity_evidence.as_ref()?,
                        )
                    ))
                })
                .collect(),
            folder: session.metadata.cwd.clone(),
            source_file: (!session.metadata.source_file().as_os_str().is_empty()).then(|| {
                session
                    .metadata
                    .source_file()
                    .to_string_lossy()
                    .into_owned()
            }),
            parent: session.metadata.parent.clone(),
            created: session.metadata.created,
            updated: session.metadata.updated,
            available: session.available,
            changed_after_done: session.changed_after_done(),
            warning: session.metadata.warning.clone(),
            note: session.note.clone(),
            openings,
        });
        details
    }
    pub fn text(&self) -> String {
        let p = &self.project;
        let folder = p
            .folder
            .as_deref()
            .map(|s| {
                if s.is_empty() {
                    "Unknown (not recorded)"
                } else {
                    s
                }
            })
            .unwrap_or("All indexed folders");
        let mut lines = vec![
            format!(
                "{}: {}",
                if p.includes_descendants {
                    "Folder group (includes nested projects)"
                } else {
                    "Project"
                },
                p.name
            ),
            format!("Folder: {folder}"),
            format!(
                "Saved sessions: {}    Live terminals: {}",
                p.saved_sessions, p.live_terminals
            ),
            format!(
                "Open: {}    To resume: {}    Done: {}",
                p.open, p.to_resume, p.done
            ),
            format!("Last exchange: {}", paths::date(p.last_exchange)),
            "Agents:".into(),
        ];
        lines.extend(p.agents.iter().map(|a| {
            format!(
                "  [{}] {} session(s), {} open",
                a.agent.label(),
                a.sessions,
                a.open
            )
        }));
        if let Some(s) = &self.session {
            lines.extend([
                String::new(),
                format!("Session: {}", s.title),
                format!("Agent: [{}]", s.agent.label()),
                format!("State: {}    {}", s.state.label(), s.presence),
                format!(
                    "Created: {}    Updated: {}",
                    paths::date(s.created),
                    paths::date(s.updated)
                ),
                format!(
                    "Source: {}",
                    s.source_file
                        .as_deref()
                        .unwrap_or("Live terminal (no transcript association)")
                ),
                format!(
                    "Session ID: {}",
                    if s.native_id.is_empty() {
                        "Unverified"
                    } else {
                        &s.native_id
                    }
                ),
                format!("Catalog ID: {}", s.id),
                format!(
                    "Transcript available: {}",
                    if s.available { "yes" } else { "no" }
                ),
            ]);
            if let Some(activity) = s.live_activity {
                lines.push(format!(
                    "Live activity: {} (terminal inference; separate from saved work state)",
                    activity.label()
                ));
                lines.extend(s.activity_evidence.iter().map(|e| format!("  {e}")));
            }
            if let Some(parent) = &s.parent {
                lines.push(format!("Parent: {parent}"));
            }
            if s.changed_after_done {
                lines.push("Updated after completion".into());
            }
            if !s.openings.is_empty() {
                lines.push("Openings (association may be unverified):".into());
                lines.extend(s.openings.iter().map(|p| format!("  {p}")));
            }
            if !s.warning.is_empty() {
                lines.push(format!("Warning: {}", s.warning));
            }
            if !s.note.is_empty() {
                lines.extend([String::new(), "Next step:".into(), s.note.clone()]);
            }
        }
        if !p.storage_locations.is_empty() {
            lines.push(String::new());
            lines.push("History storage:".into());
            lines.extend(p.storage_locations.iter().map(|p| format!("  {p}")));
        }
        paths::clean(&lines.join("\n"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;
    #[test]
    fn details_keep_full_paths_and_provider_identity_without_reading_files() {
        let session = Session {
            id: "catalog-id".into(),
            metadata: Metadata {
                provider: Provider::Claude,
                native_id: "native-id".into(),
                name: "Parser fix".into(),
                cwd: "/repo/T/Python".into(),
                file: Path::new("/history/claude.jsonl").into(),
                ..Default::default()
            },
            state: ResumeState::Resume,
            note: "Check tests".into(),
            done_revision: 0,
            done_at: 0,
            available: false,
            presence_verified: false,
            bindings: vec![],
            probable: vec![],
        };
        let catalog = Catalog {
            sessions: vec![session],
            ..Default::default()
        };
        let details = Details::session(&catalog, &catalog.sessions[0]);
        let text = details.text();
        assert!(text.contains("Folder: /repo/T/Python"));
        assert!(text.contains("Agent: [claude]"));
        assert!(text.contains("Source: /history/claude.jsonl"));
        assert!(text.contains("Check tests"));
        assert!(!details.project.includes_descendants);
        let mut grouped = catalog.clone();
        let mut outside = grouped.sessions[0].clone();
        outside.id = "outside".into();
        outside.metadata.cwd = "/repo/Together".into();
        grouped.sessions.push(outside);
        let group = Details::folder_group(&grouped, "/repo/T");
        assert!(group.project.includes_descendants);
        assert_eq!(group.project.saved_sessions, 1);
        assert_eq!(
            Details::project(&grouped, Some("/repo/T"))
                .project
                .saved_sessions,
            0
        );
        assert_eq!(details.project.saved_sessions, 1);
        assert_eq!(details.project.storage_locations, vec!["/history"]);
        assert_eq!(
            Details::project(&catalog, None).project.name,
            "All projects"
        );
    }
}
