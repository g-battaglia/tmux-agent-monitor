//! The contextual action menu behind `?`.
use super::{App, Panel};
use crate::model::{ResumeState, project_label};
use crossterm::event::KeyCode;

#[derive(Clone)]
pub struct MenuAction {
    pub group: &'static str,
    pub label: &'static str,
    pub shortcut: &'static str,
    pub key: KeyCode,
    pub description: &'static str,
    pub enabled: bool,
    pub disabled_reason: &'static str,
}
#[derive(Clone)]
pub struct CommandMenu {
    pub title: &'static str,
    pub context: String,
    pub subtitle: String,
    pub notice: Option<String>,
    pub target: Option<String>,
    pub alert_target: Option<crate::notifications::Target>,
    pub alert_enabled: bool,
    pub actions: Vec<MenuAction>,
    pub selected: usize,
}
impl CommandMenu {
    pub fn new(app: &App) -> Self {
        let mut menu = Self {
            title: match app.panel {
                Panel::Projects => "Project actions",
                Panel::Sessions => "Session actions",
                Panel::Detail => "Conversation actions",
            },
            context: app
                .current()
                .map(|s| s.metadata.title())
                .unwrap_or_else(|| "Choose what to do".into()),
            subtitle: app
                .current()
                .map(|s| {
                    format!(
                        "[{}]  {}  ·  {}",
                        s.metadata.provider.label(),
                        if s.id.starts_with("pane-") || s.id.starts_with("live-") {
                            "Live terminal"
                        } else {
                            s.state.label()
                        },
                        project_label(&s.metadata.cwd)
                    )
                })
                .unwrap_or_else(|| "No session selected".into()),
            notice: None,
            target: app.selected.clone(),
            alert_target: app
                .current()
                .map(crate::notifications::Target::from_session),
            alert_enabled: app
                .current()
                .is_some_and(|s| app.notifications.session_enabled(s)),
            actions: vec![],
            selected: 0,
        };
        menu.add(
            "Find",
            match app.panel {
                Panel::Projects => "Search projects",
                Panel::Sessions => "Search sessions",
                Panel::Detail => "Search the text",
            },
            "/",
            KeyCode::Char('/'),
            "Type in the search bar at the top.",
            true,
        );
        if app.panel == Panel::Projects {
            menu.context = app
                .project_row
                .checked_sub(1)
                .and_then(|i| app.projects().get(i))
                .map(|p| project_label(p))
                .unwrap_or_else(|| "All projects".into());
            menu.subtitle = format!(
                "{} view  ·  {}  ·  {}",
                app.view.label(),
                app.project_options.layout.label(),
                app.project_options.order.label()
            );
            let group = app
                .project_row
                .checked_sub(1)
                .and_then(|i| app.project_tree.rows.get(i))
                .is_some_and(|r| r.is_group);
            menu.add(
                "Project",
                if group {
                    "Toggle folder group"
                } else {
                    "Open project"
                },
                "Enter",
                KeyCode::Enter,
                if group {
                    "Open or close this navigation folder; keep the active project unchanged."
                } else {
                    "Show this project's conversations."
                },
                true,
            );
            menu.add(
                "Project",
                "Details",
                "i",
                KeyCode::Char('i'),
                "Full folder, agent counts, and history storage for this project.",
                true,
            );
            let tree = app.project_options.layout == super::projects::ProjectLayout::Tree;
            let branches = !app.project_tree.branches.is_empty();
            let branch = app
                .project_row
                .checked_sub(1)
                .and_then(|i| app.project_tree.rows.get(i))
                .is_some_and(|r| r.has_children);
            menu.add("Folders","Toggle branch","Space",KeyCode::Char(' '),"On All projects, toggles all branches. Arrow keys navigate and fold; clicking a triangle also folds.",tree && (branch || app.project_row == 0 && branches) && app.project_search.is_empty());
        } else {
            let session = app.current();
            let persistent =
                session.is_some_and(|s| !s.id.starts_with("live-") && !s.id.starts_with("pane-"));
            menu.add(
                "Session",
                if session.is_some_and(|s| s.id.starts_with("pane-") || s.id.starts_with("live-")) {
                    "Focus live terminal"
                } else if session.is_some_and(|s| !s.bindings.is_empty() || !s.probable.is_empty())
                {
                    "Open agent pane"
                } else {
                    "Resume conversation"
                },
                "Enter",
                KeyCode::Enter,
                if session.is_some_and(|s| !s.bindings.is_empty() || !s.probable.is_empty()) {
                    "Choose an existing pane. No agent is started."
                } else {
                    "Review and confirm before reopening this saved conversation in tmux."
                },
                session.is_some(),
            );
            menu.add(
                "Session",
                "Edit next-step note",
                "n",
                KeyCode::Char('n'),
                "Save a note for when you return to this conversation.",
                persistent,
            );
            menu.add(
                "Session",
                "Mark Done",
                "d",
                KeyCode::Char('d'),
                "Never closes agents or tmux: only changes the work state.",
                persistent && session.is_some_and(|s| s.state != ResumeState::Done),
            );
            menu.add(
                "Session",
                "Mark To resume",
                "r",
                KeyCode::Char('r'),
                "Keep this conversation in the to-resume list.",
                persistent && session.is_some_and(|s| s.state != ResumeState::Resume),
            );
            menu.add(
                "Session",
                "Undo last change",
                "u",
                KeyCode::Char('u'),
                "Restore the previous note or state decision.",
                persistent,
            );
            menu.add(
                "Session",
                "Details",
                "i",
                KeyCode::Char('i'),
                "Project folder, agent, transcript path, session identity, and notes.",
                session.is_some(),
            );
            if app.panel == Panel::Detail {
                menu.add(
                    "Reading",
                    if app.zoom {
                        "Exit full-screen preview"
                    } else {
                        "Full-screen preview"
                    },
                    "z",
                    KeyCode::Char('z'),
                    "Use the whole terminal to read the text.",
                    session.is_some(),
                );
                menu.add(
                    "Reading",
                    "Choose branch",
                    "b",
                    KeyCode::Char('b'),
                    "Explore alternatives without mixing branch history.",
                    app.detail_id == app.selected && !app.conversation.branches.is_empty(),
                );
                menu.add(
                    "Reading",
                    "Toggle tool output",
                    "t",
                    KeyCode::Char('t'),
                    "Internal reasoning always stays hidden.",
                    session.is_some(),
                );
                menu.add(
                    "Reading",
                    "Older page",
                    "[",
                    KeyCode::Char('['),
                    "Move toward older exchanges.",
                    app.conversation.page + 1 < app.conversation.pages,
                );
                menu.add(
                    "Reading",
                    "Newer page",
                    "]",
                    KeyCode::Char(']'),
                    "Move back toward recent exchanges.",
                    app.conversation.page > 0,
                );
            }
        }
        menu.project_display_actions(app);
        menu.add(
            "Navigate",
            "Change view",
            "f",
            KeyCode::Char('f'),
            "All, to resume, open, or done.",
            true,
        );
        menu.add(
            "Navigate",
            "Choose project",
            "p",
            KeyCode::Char('p'),
            "Projects come automatically from all four agents' saved sessions.",
            true,
        );
        menu.add(
            "Navigate",
            "Browse open panes",
            "o",
            KeyCode::Char('o'),
            "Pick an existing pane: Pi, Claude, Codex, or Opencode.",
            true,
        );
        menu.add(
            "Catalog",
            "Refresh catalog",
            "R",
            KeyCode::Char('R'),
            "Re-read names, conversations, and openings without starting agents.",
            true,
        );
        // Append the new category so established keyboard/mouse category order
        // remains intact for existing tmux workflows.
        if app.panel != Panel::Projects {
            menu.add(
                "Notifications",
                if menu.alert_enabled {
                    "Disable session notifications"
                } else {
                    "Enable session notifications"
                },
                "a",
                KeyCode::Char('a'),
                "Turn-end desktop alerts for this session; the global switch still applies.",
                menu.alert_target
                    .as_ref()
                    .is_some_and(|target| app.notifications.can_change_target(target)),
            );
        }
        menu.add(
            "Notifications",
            if app.notifications.preferences.enabled { "Disable all notifications" } else { "Enable all notifications" },
            "A", KeyCode::Char('A'),
            "Pause or enable desktop alerts while this monitor stays open. Individual settings are preserved.",
            true,
        );
        menu.explain_unavailable(app);
        let initial = match app.panel {
            Panel::Projects => "Project",
            Panel::Sessions => "Session",
            Panel::Detail => "Reading",
        };
        menu.selected = menu
            .actions
            .iter()
            .position(|a| a.group == initial && a.enabled)
            .or_else(|| menu.actions.iter().position(|a| a.group == initial))
            .unwrap_or(0);
        menu
    }
    fn project_display_actions(&mut self, app: &App) {
        let tree = app.project_options.layout == super::projects::ProjectLayout::Tree;
        let folders = app
            .project_tree
            .rows
            .iter()
            .any(|r| std::path::Path::new(&r.cwd).is_absolute());
        self.add("Folders","Expand all folders","E",KeyCode::Char('E'),"Switches to Tree if needed, restores every nested folder, and keeps the active project unchanged.",folders);
        self.add("Folders","Collapse all folders","C",KeyCode::Char('C'),"Switches to Tree if needed. Navigation-only folder groups also work in Open; sessions are unchanged.",folders && app.project_search.is_empty());
        self.add("Folders",if tree {"Flat project list"} else {"Folder tree"},"v",KeyCode::Char('v'),"Tree includes ancestor folder groups; Flat shows only actual projects. Works from every panel.",true);
        self.add("Folders",if app.project_options.order == super::projects::ProjectOrder::Alphabetical {"Sort: recent first"} else {"Sort: alphabetical"},"s",KeyCode::Char('s'),"Recent uses saved updates, not live activity. Tree sorts siblings; Flat sorts individual projects. Unknown timestamps cannot imply recency.",true);
    }
    fn add(
        &mut self,
        group: &'static str,
        label: &'static str,
        shortcut: &'static str,
        key: KeyCode,
        description: &'static str,
        enabled: bool,
    ) {
        self.actions.push(MenuAction {
            group,
            label,
            shortcut,
            key,
            description,
            enabled,
            disabled_reason: "Unavailable for this selection.",
        });
    }
    fn explain_unavailable(&mut self, app: &App) {
        let live = app
            .current()
            .is_some_and(|s| s.id.starts_with("pane-") || s.id.starts_with("live-"));
        for a in &mut self.actions {
            if a.enabled {
                continue;
            }
            a.disabled_reason = match a.key {
                KeyCode::Char('a')
                    if self
                        .alert_target
                        .as_ref()
                        .is_some_and(|target| target.available() && target.session.is_none())
                        && !app.notifications.owner =>
                {
                    "Temporary alerts are managed by the sending TUI instance; use its controls."
                }
                KeyCode::Char('a') if app.current().is_some() => {
                    "Wait for a sampled process identity before changing live alerts."
                }
                KeyCode::Char('n' | 'd' | 'r' | 'u') if live => {
                    "Saved session required for notes and decisions."
                }
                KeyCode::Char('d') if app.current().is_some() => {
                    "Already marked Done. Use To resume to reopen the work decision."
                }
                KeyCode::Char('r') if app.current().is_some() => "Already marked To resume.",
                KeyCode::Char(' ' | 'C') if !app.project_search.is_empty() => {
                    "Clear project search with Esc before folding folders."
                }
                KeyCode::Char(' ') => {
                    "Select a folder with children. E/C opens or closes all folders."
                }
                KeyCode::Char('E' | 'C') => {
                    "No known folders in this view. Use f → All to browse saved history."
                }
                KeyCode::Char('b') => "No saved branches available in the current preview.",
                KeyCode::Char('[') => "No older page available.",
                KeyCode::Char(']') => "You are on the newest page.",
                _ => "Select a session first.",
            };
        }
    }
    pub fn groups(&self) -> Vec<&'static str> {
        let mut groups = vec![];
        for a in &self.actions {
            if !groups.contains(&a.group) {
                groups.push(a.group);
            }
        }
        groups
    }
    pub fn group_index(&self) -> usize {
        let group = self.actions.get(self.selected).map(|a| a.group);
        self.groups()
            .iter()
            .position(|g| Some(*g) == group)
            .unwrap_or(0)
    }
    pub fn choose_group(&mut self, index: usize) {
        if let Some(group) = self.groups().get(index)
            && let Some(i) = self
                .actions
                .iter()
                .position(|a| a.group == *group && a.enabled)
                .or_else(|| self.actions.iter().position(|a| a.group == *group))
        {
            self.selected = i;
            self.notice = None;
        }
    }
    pub fn step_group(&mut self, delta: isize) {
        let count = self.groups().len();
        if count > 0 {
            self.choose_group(
                (self.group_index() as isize + delta).rem_euclid(count as isize) as usize,
            );
        }
    }
    pub fn step(&mut self, delta: isize) {
        let rows = self.rows();
        let current = rows
            .iter()
            .position(|i| *i == Some(self.selected))
            .unwrap_or(0);
        if !rows.is_empty() {
            self.selected = rows
                [(current as isize + delta).clamp(0, rows.len() as isize - 1) as usize]
                .unwrap();
            self.notice = None;
        }
    }
    /// Action indices in the current category; used by rendering and mouse hits.
    pub fn rows(&self) -> Vec<Option<usize>> {
        let group = self.actions.get(self.selected).map(|a| a.group);
        self.actions
            .iter()
            .enumerate()
            .filter(|(_, a)| Some(a.group) == group)
            .map(|(i, _)| Some(i))
            .collect()
    }
    pub fn display_height(&self) -> usize {
        self.groups()
            .iter()
            .map(|g| self.actions.iter().filter(|a| a.group == *g).count())
            .max()
            .unwrap_or(0)
            .max(self.groups().len())
    }
}
