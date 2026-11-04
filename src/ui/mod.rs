//! Local navigation state plus rendering. No I/O here.
//!
//! `App` owns panels, search strings, filters, and the visible row cache.
//! Rendering and input only read or mutate this struct; the worker thread
//! owns the database, tmux, and Pi file reads. Three search strings exist
//! (projects, sessions, conversation text) so switching panels never
//! loses what was typed.
pub mod menu;
pub mod projects;
pub mod render;
mod runtime;
mod worker;
use crate::model::{Catalog, Conversation, Session, View, project_label};
pub use runtime::run;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Panel {
    Projects,
    Sessions,
    Detail,
}

/// Everything the TUI renders and navigates. Plain data plus filter
/// caches; pushing updates in from worker replies keeps selection stable.
pub struct App {
    pub catalog: Catalog,
    pub notifications: crate::notifications::NotificationState,
    pub project: Option<String>,
    pub project_row: usize,
    pub view: View,
    pub selected: Option<String>,
    pub row: usize,
    pub panel: Panel,
    pub search: String,
    pub project_search: String,
    pub editing_search: bool,
    pub detail_search: String,
    pub branch_selected: bool,
    pub conversation: Conversation,
    pub detail_id: Option<String>,
    pub scroll: u16,
    pub zoom: bool,
    pub status: String,
    pub no_color: bool,
    pub modal: Option<String>,
    pub modal_details: bool,
    pub modal_scroll: u16,
    pub modal_max_scroll: std::cell::Cell<u16>,
    pub command_menu: Option<menu::CommandMenu>,
    pub menu_offset: std::cell::Cell<usize>,
    pub menu_group_offset: std::cell::Cell<usize>,
    pub visible: Vec<usize>,
    pub project_paths: Vec<String>,
    pub project_tree: projects::ProjectTree,
    pub project_options: projects::ProjectOptions,
    pub max_scroll: std::cell::Cell<u16>,
    pub search_match: std::cell::Cell<u16>,
    pub project_offset: std::cell::Cell<usize>,
    pub session_offset: std::cell::Cell<usize>,
}
impl Default for App {
    fn default() -> Self {
        Self {
            catalog: Catalog::default(),
            notifications: crate::notifications::NotificationState::default(),
            project: None,
            project_row: 0,
            view: View::Open,
            selected: None,
            row: 0,
            panel: Panel::Sessions,
            search: String::new(),
            project_search: String::new(),
            editing_search: false,
            detail_search: String::new(),
            branch_selected: false,
            conversation: Conversation::default(),
            detail_id: None,
            scroll: 0,
            zoom: false,
            status: String::new(),
            no_color: std::env::var_os("NO_COLOR").is_some(),
            modal: None,
            modal_details: false,
            modal_scroll: 0,
            modal_max_scroll: std::cell::Cell::new(0),
            command_menu: None,
            menu_offset: std::cell::Cell::new(0),
            menu_group_offset: std::cell::Cell::new(0),
            visible: vec![],
            project_paths: vec![],
            project_tree: projects::ProjectTree::default(),
            project_options: projects::ProjectOptions::default(),
            max_scroll: std::cell::Cell::new(0),
            search_match: std::cell::Cell::new(0),
            project_offset: std::cell::Cell::new(0),
            session_offset: std::cell::Cell::new(0),
        }
    }
}
impl App {
    pub fn projects(&self) -> &[String] {
        &self.project_paths
    }
    pub fn search_text(&self) -> &str {
        match self.panel {
            Panel::Projects => &self.project_search,
            Panel::Sessions => &self.search,
            Panel::Detail => &self.detail_search,
        }
    }
    pub fn search_text_mut(&mut self) -> &mut String {
        match self.panel {
            Panel::Projects => &mut self.project_search,
            Panel::Sessions => &mut self.search,
            Panel::Detail => &mut self.detail_search,
        }
    }
    /// Project scope and session view are independent; All projects keeps the view.
    pub fn choose_project(&mut self, row: usize) -> bool {
        self.project_row = row;
        if row
            .checked_sub(1)
            .and_then(|i| self.project_tree.rows.get(i))
            .is_some_and(|r| r.is_group)
        {
            self.panel = Panel::Projects;
            self.toggle_project_fold();
            return false;
        }
        self.project = row
            .checked_sub(1)
            .and_then(|i| self.project_paths.get(i).cloned());
        if row == 0 {
            self.search.clear();
            self.project_search.clear();
        }
        true
    }
    /// Resolve a hidden project to its closest visible ancestor, never a sibling.
    pub fn visible_project_row(&self, cwd: &str) -> Option<usize> {
        if let Some(i) = self.project_paths.iter().position(|p| p == cwd) {
            return Some(i + 1);
        }
        self.project_paths
            .iter()
            .enumerate()
            .filter(|(_, p)| !p.is_empty() && std::path::Path::new(cwd).starts_with(p))
            .max_by_key(|(_, p)| std::path::Path::new(p).components().count())
            .map(|(i, _)| i + 1)
    }
    fn refresh_projects(&mut self) {
        let cursor = self
            .project_row
            .checked_sub(1)
            .and_then(|i| self.project_paths.get(i))
            .cloned();
        self.project_tree = projects::build_with_options(
            &self.catalog,
            self.view,
            self.project.as_deref(),
            &self.project_search,
            &self.project_options,
        );
        self.project_paths = self
            .project_tree
            .rows
            .iter()
            .map(|r| r.cwd.clone())
            .collect();
        self.project_row = cursor
            .as_deref()
            .and_then(|p| self.visible_project_row(p))
            .unwrap_or(0);
    }
    pub fn toggle_project_layout(&mut self) {
        self.project_options.layout = match self.project_options.layout {
            projects::ProjectLayout::Tree => projects::ProjectLayout::Flat,
            projects::ProjectLayout::Flat => projects::ProjectLayout::Tree,
        };
        self.refresh_projects();
        self.status = format!("Project layout: {}", self.project_options.layout.label());
    }
    pub fn toggle_project_order(&mut self) {
        self.project_options.order = match self.project_options.order {
            projects::ProjectOrder::Alphabetical => projects::ProjectOrder::Recent,
            projects::ProjectOrder::Recent => projects::ProjectOrder::Alphabetical,
        };
        self.refresh_projects();
        let known = self.project_tree.known_timestamps;
        let total = self.project_tree.project_count;
        self.status = format!(
            "Project order: {} · latest saved update known for {known}/{total} matching projects; live-only times unknown",
            self.project_options.order.label()
        );
    }
    pub fn fold_all_projects(&mut self, collapse: bool) {
        if self.project_options.layout != projects::ProjectLayout::Tree {
            self.project_options.layout = projects::ProjectLayout::Tree;
            self.refresh_projects();
        }
        if self.project_tree.branches.is_empty() {
            self.status = "No nested folders in this view; f → All includes saved history".into();
            return;
        }
        if collapse && !self.project_search.is_empty() {
            self.status = "Clear project search before collapsing branches".into();
            return;
        }
        if collapse {
            self.project_options
                .collapsed
                .extend(self.project_tree.branches.iter().cloned());
        } else {
            self.project_options.collapsed.clear();
        }
        self.refresh_projects();
        self.status = if collapse {
            "All project branches collapsed"
        } else {
            "All project branches expanded"
        }
        .into();
    }
    pub fn toggle_project_fold(&mut self) {
        if self.project_options.layout != projects::ProjectLayout::Tree {
            self.project_options.layout = projects::ProjectLayout::Tree;
            self.refresh_projects();
        }
        if self.project_row == 0 {
            let any = self
                .project_tree
                .branches
                .iter()
                .any(|p| self.project_options.collapsed.contains(p));
            self.fold_all_projects(!any);
            return;
        }
        if !self.project_search.is_empty() {
            self.status = "Clear project search before collapsing branches".into();
            return;
        }
        if let Some(row) = self
            .project_tree
            .rows
            .get(self.project_row - 1)
            .filter(|r| r.has_children)
        {
            let cwd = row.cwd.clone();
            let expanded = self.project_options.collapsed.remove(&cwd);
            if !expanded {
                self.project_options.collapsed.insert(cwd.clone());
            }
            self.refresh_projects();
            self.status = format!(
                "{} project branch: {}",
                if expanded { "Expanded" } else { "Collapsed" },
                projects::folder(&cwd)
            );
        } else {
            self.status = "No child folders here; ← goes to the parent, C/E folds all".into();
        }
    }
    pub fn project_arrow(&mut self, right: bool) {
        if self.project_options.layout != projects::ProjectLayout::Tree {
            self.panel = if right {
                Panel::Sessions
            } else {
                Panel::Projects
            };
            return;
        }
        if self.project_row == 0 {
            self.fold_all_projects(!right);
            if right && !self.project_paths.is_empty() {
                self.project_row = 1;
            }
            return;
        }
        let row = self.project_tree.rows.get(self.project_row - 1).cloned();
        if let Some(row) = row {
            if right && row.has_children {
                if row.collapsed {
                    self.toggle_project_fold();
                } else {
                    self.project_row += 1;
                }
            } else if !right {
                if row.has_children && !row.collapsed {
                    self.toggle_project_fold();
                } else {
                    self.project_row = row
                        .parent
                        .as_deref()
                        .and_then(|p| self.visible_project_row(p))
                        .unwrap_or(0);
                }
            } else {
                self.panel = Panel::Sessions;
            }
        }
    }
    pub fn refresh_filter(&mut self) {
        self.refresh_projects();
        let needle = self.search.to_lowercase();
        self.visible = self
            .catalog
            .sessions
            .iter()
            .enumerate()
            .filter(|(_, s)| {
                self.view.matches(s)
                    && self.project.as_ref().is_none_or(|p| *p == s.metadata.cwd)
                    && (needle.is_empty()
                        || format!(
                            "{} {} {} {} {} {}",
                            s.live_activity().map(|a| a.label()).unwrap_or(""),
                            s.metadata.provider.label(),
                            s.metadata.title(),
                            s.metadata.cwd,
                            project_label(&s.metadata.cwd),
                            s.note
                        )
                        .to_lowercase()
                        .contains(&needle))
            })
            .map(|(i, _)| i)
            .collect();
        if self.view == View::All {
            // Open terminals must not be buried under hundreds of historical rows.
            // Stable sorting retains newest-first history within each group.
            self.visible.sort_by_key(|i| {
                let s = &self.catalog.sessions[*i];
                s.bindings.is_empty() && s.probable.is_empty() && !s.id.starts_with("pane-")
            });
        }
    }
    pub fn rows(&self) -> Vec<&Session> {
        self.visible
            .iter()
            .filter_map(|i| self.catalog.sessions.get(*i))
            .collect()
    }
    pub fn current(&self) -> Option<&Session> {
        self.visible
            .get(self.row)
            .and_then(|i| self.catalog.sessions.get(*i))
    }
    pub fn select(&mut self, row: usize) {
        self.row = row.min(self.visible.len().saturating_sub(1));
        let id = self.current().map(|s| s.id.clone());
        if id != self.selected {
            self.scroll = 0;
        }
        self.selected = id;
    }
    pub fn apply(&mut self, catalog: Catalog) {
        let id = self.selected.clone();
        let project_cursor = self
            .project_row
            .checked_sub(1)
            .and_then(|i| self.project_paths.get(i))
            .cloned();
        self.catalog = catalog;
        self.refresh_filter();
        if self.panel != Panel::Projects {
            self.project_row = self
                .project
                .as_ref()
                .and_then(|p| self.visible_project_row(p))
                .unwrap_or(0);
        } else {
            self.project_row = project_cursor
                .and_then(|p| self.visible_project_row(&p))
                .unwrap_or(self.project_row.min(self.project_paths.len()));
        }
        let row = id
            .and_then(|id| self.rows().iter().position(|s| s.id == id))
            .unwrap_or(self.row);
        self.select(row);
    }
}
