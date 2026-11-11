//! Sessions are conversations, not panes or task tickets.
//!
//! This module holds the plain data the explorer shows: what kind of
//! agent process is running, whether a conversation is new work or
//! finished history, and which openings (if any) it currently has.
//! Identity always comes from stable file IDs, never from display names.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// Which agent owns a terminal process, guessed from the executable name.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Provider {
    #[default]
    Pi,
    Claude,
    Codex,
    Opencode,
}

impl Provider {
    /// Lowercase name used in manifests, overrides, CLI, and display.
    pub fn label(self) -> &'static str {
        match self {
            Self::Pi => "pi",
            Self::Claude => "claude",
            Self::Codex => "codex",
            Self::Opencode => "opencode",
        }
    }

    /// Parse a manifest/override/CLI agent name. Case-insensitive;
    /// unknown names are rejected, never guessed.
    pub fn parse(text: &str) -> Option<Self> {
        match text.trim().to_ascii_lowercase().as_str() {
            "pi" | "pi-coding-agent" => Some(Self::Pi),
            "claude" => Some(Self::Claude),
            "codex" => Some(Self::Codex),
            "opencode" => Some(Self::Opencode),
            _ => None,
        }
    }
}

/// The user's own working decision for a conversation.
///
/// History is the import baseline, Resume means "I still want this",
/// Done is explicit and never inferred from idle panes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResumeState {
    #[default]
    History,
    Resume,
    Done,
}

impl ResumeState {
    pub fn label(self) -> &'static str {
        match self {
            Self::History => "History",
            Self::Resume => "To resume",
            Self::Done => "Done",
        }
    }
}

/// Facts read from the Pi JSONL header, plus small fallbacks for display.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Metadata {
    #[serde(default)]
    pub provider: Provider,
    /// OpenCode stores several sessions in one read-only database.
    #[serde(default)]
    pub database: Option<PathBuf>,
    /// Stable provider session id from the `session` record.
    pub native_id: String,
    /// Canonical transcript path. The JSONL file stays authoritative.
    pub file: PathBuf,
    /// Canonical working directory. Projects are grouped by this path.
    pub cwd: String,
    /// Explicit Pi name, or empty when Pi never named the session.
    pub name: String,
    /// First user message, used only when no name exists.
    pub first_message: String,
    pub parent: Option<String>,
    pub created: i64,
    pub updated: i64,
    /// Content hash of the messages seen so far. Used to notice real
    /// follow-ups after a session was marked done.
    pub revision: u64,
    pub leaf: Option<String>,
    pub warning: String,
}

impl Metadata {
    pub fn source_file(&self) -> &Path {
        self.database.as_deref().unwrap_or(&self.file)
    }

    /// Human title with clear fallbacks. Never used as an identity key.
    pub fn title(&self) -> String {
        if !self.name.is_empty() {
            return self.name.chars().take(512).collect();
        }

        if !self.first_message.is_empty() {
            return self.first_message.chars().take(100).collect();
        }

        format!("Unnamed session ({})", crate::paths::date(self.created))
    }
}

/// One catalog row: durable user state plus live presence attachments.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Session {
    /// Catalog id. Stable for imported files, `live-*` for unsaved Pi runs.
    pub id: String,
    pub metadata: Metadata,
    pub state: ResumeState,
    /// The user's own next-step note, capped at 4000 characters.
    pub note: String,
    pub done_revision: u64,
    #[serde(default)]
    pub done_at: i64,
    /// False when the transcript file is gone. Notes survive regardless.
    pub available: bool,
    #[serde(default)]
    pub presence_verified: bool,
    /// Exact openings, proven by the optional Pi extension record.
    #[serde(default)]
    pub bindings: Vec<Binding>,
    /// Passive title/cwd hints. Visible as probable, never authoritative.
    #[serde(default)]
    pub probable: Vec<crate::tmux::PaneIdentity>,
}

impl Session {
    /// Only imported conversations have a durable catalog row. Terminal and
    /// unsaved-run ids must never be sent to SQLite mutation operations.
    pub fn persistent(&self) -> bool {
        !self.id.starts_with("live-") && !self.id.starts_with("pane-")
    }

    pub fn openings(&self) -> impl Iterator<Item = &crate::tmux::PaneIdentity> {
        self.bindings
            .iter()
            .filter_map(|b| b.pane.as_ref())
            .chain(self.probable.iter())
    }
    /// No activity for closed history; conflicting live instances are explicit.
    pub fn live_activity(&self) -> Option<crate::activity::AgentActivity> {
        use crate::activity::AgentActivity;
        if self.bindings.is_empty() && self.probable.is_empty() {
            return None;
        }
        let mut states = self
            .openings()
            .map(|p| p.activity.unwrap_or(AgentActivity::Unknown));
        let first = states.next().unwrap_or(AgentActivity::Unknown);
        Some(if states.any(|s| s != first) {
            AgentActivity::Mixed
        } else {
            first
        })
    }
    /// Adopt the live Pi name only when every verified opening agrees.
    /// Conflicting instances keep the saved file's name instead of
    /// arbitrarily picking one process's local state.
    pub fn live_name(&mut self) {
        if let Some(first) = self.bindings.first()
            && self
                .bindings
                .iter()
                .all(|b| b.bridge.name == first.bridge.name)
        {
            self.metadata.name = crate::paths::line(&first.bridge.name)
                .chars()
                .take(512)
                .collect();
        }
    }

    /// True only when genuinely new content arrived after completion:
    /// the content hash changed AND the transcript timestamp moved past
    /// the moment the user pressed done. Slow imports of old history
    /// must never look like fresh work.
    pub fn changed_after_done(&self) -> bool {
        self.state == ResumeState::Done
            && self.metadata.revision != self.done_revision
            && self.metadata.updated > self.done_at
    }

    /// Short presence line for the detail panel.
    pub fn presence(&self) -> &'static str {
        if self.id.starts_with("pane-") {
            return "Open terminal (session unverified)";
        }
        if self.bindings.is_empty() {
            if !self.probable.is_empty() {
                return "Probable opening";
            }
            if self.presence_verified {
                "Not open"
            } else {
                "Presence unverified"
            }
        } else {
            "Open"
        }
    }
}

/// Small liveness record written by the optional Pi extension.
/// Metadata only: it proves which file a process holds, nothing more.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Bridge {
    pub version: u32,
    /// Random per-process token, so `/resume` in the same PID is visible.
    pub nonce: String,
    pub generation: u64,
    pub pid: u32,
    /// Process start time, which defeats PID-reuse false positives.
    pub process_start: String,
    pub native_id: String,
    pub file: Option<PathBuf>,
    pub cwd: String,
    pub name: String,
    pub leaf: Option<String>,
    pub socket: Option<String>,
    pub pane: Option<String>,
    pub seen: i64,
}

/// A verified opening: bridge record plus the revalidated tmux pane.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Binding {
    pub bridge: Bridge,
    pub record: PathBuf,
    pub pane: Option<crate::tmux::PaneIdentity>,
}

/// Everything the UI renders in one pass.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Catalog {
    pub sessions: Vec<Session>,
    /// Agent panes with no verifiable conversation identity.
    pub unbound: Vec<crate::tmux::PaneIdentity>,
    pub warnings: Vec<String>,
    pub scanning: bool,
}

/// Which slice of the catalog the session list shows.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, clap::ValueEnum)]
#[serde(rename_all = "snake_case")]
pub enum View {
    #[default]
    Resume,
    Open,
    All,
    Done,
}

impl View {
    pub const ALL: [View; 4] = [Self::Resume, Self::Open, Self::All, Self::Done];

    pub fn label(self) -> &'static str {
        match self {
            Self::Resume => "To resume",
            Self::Open => "Open",
            Self::All => "All",
            Self::Done => "Done",
        }
    }

    pub fn matches(self, session: &Session) -> bool {
        match self {
            Self::Resume => session.state == ResumeState::Resume,
            Self::Open => !session.bindings.is_empty() || !session.probable.is_empty(),
            Self::All => true,
            Self::Done => session.state == ResumeState::Done,
        }
    }
}

/// Raw folder name. This is the grouping key used for discovery.
pub fn project_name(cwd: &str) -> String {
    Path::new(cwd)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or(cwd)
        .to_string()
}

/// Friendly display name derived from the folder name.
///
/// Dashes, dots and underscores become spaces and each word is
/// capitalized, so `acme-website` reads as `Acme Website`.
/// Display only: grouping and identity always use the raw path.
pub fn project_label(cwd: &str) -> String {
    if cwd.is_empty() {
        return "Unknown folder".into();
    }
    if cwd == "/" {
        return "/".into();
    }
    if Path::new(cwd) == crate::paths::expand_home(Path::new("~")) {
        return "Home".into();
    }
    let pretty = project_name(cwd)
        .split(['.', '-', '_'])
        .filter(|word| !word.is_empty())
        .map(|word| {
            if word.chars().all(|c| c.is_ascii_uppercase()) {
                word.to_string()
            } else {
                let mut chars = word.chars();
                match chars.next() {
                    None => String::new(),
                    Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
                }
            }
        })
        .collect::<Vec<_>>()
        .join(" ");

    if pretty.is_empty() {
        project_name(cwd)
    } else {
        pretty
    }
}

/// One readable page of a conversation, newest entries first.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Conversation {
    pub messages: Vec<Message>,
    /// Alternative branch tips as `(entry_id, request_preview)` pairs.
    pub branches: Vec<(String, String)>,
    pub leaf: Option<String>,
    pub warning: String,
    pub page: usize,
    pub pages: usize,
    /// True when the preview stopped early on purpose (size caps).
    pub partial: bool,
}

/// A single rendered message. Thinking blocks and images are skipped
/// upstream, so `text` is always something a human can read.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    pub role: String,
    pub text: String,
    pub time: i64,
    pub tool: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provider_labels_parse_round_trip() {
        for provider in [
            Provider::Pi,
            Provider::Claude,
            Provider::Codex,
            Provider::Opencode,
        ] {
            assert_eq!(Provider::parse(provider.label()), Some(provider));
        }
        assert_eq!(Provider::parse("CODEX"), Some(Provider::Codex));
        assert_eq!(Provider::parse(" pi "), Some(Provider::Pi));
        assert_eq!(Provider::parse("herdr"), None);
    }
}
