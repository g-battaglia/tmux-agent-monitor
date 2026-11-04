//! Local, read-only terminal inference inspired by Herdr's detector.
//! Snapshot rules are scoped to current terminal UI; finished requires an
//! observed working → settled idle transition. This never changes durable Done.
use crate::{model::Provider, paths};
use serde::{Deserialize, Serialize};
use std::{
    collections::{HashMap, HashSet},
    io::Read,
    path::{Path, PathBuf},
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentActivity {
    Working,
    Finished,
    Idle,
    Blocked,
    Waiting,
    Error,
    Unknown,
    Mixed,
}
impl AgentActivity {
    pub fn label(self) -> &'static str {
        match self {
            Self::Working => "working",
            Self::Finished => "finished",
            Self::Idle => "idle",
            Self::Blocked => "blocked",
            Self::Waiting => "waiting",
            Self::Error => "error",
            Self::Unknown => "unknown",
            Self::Mixed => "mixed",
        }
    }
}
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct MatchEvidence {
    pub rule_id: String,
    pub manifest_source: String,
    pub manifest_version: u64,
    pub fallback_reason: String,
    pub title_signal: bool,
    #[serde(default)]
    pub transition: String,
    #[serde(default)]
    pub observations: u8,
    #[serde(default)]
    pub sampled_at: i64,
    #[serde(default)]
    pub warning: String,
    /// Full process/session identity for ephemeral UI and notification policy.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub observation_key: String,
    /// Monotonic duration at the latest sample, never inferred from transcripts.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state_elapsed_ms: Option<u64>,
}
#[derive(Debug, Deserialize)]
struct Manifest {
    #[serde(default)]
    manifest_version: u64,
    #[serde(default)]
    rule: Vec<Rule>,
}
#[derive(Debug, Deserialize)]
struct Rule {
    id: String,
    state: AgentActivity,
    #[serde(default)]
    priority: u16,
    #[serde(default)]
    title_any: Vec<String>,
    #[serde(default)]
    title_spinner: bool,
    #[serde(default)]
    screen_any: Vec<String>,
    #[serde(default)]
    screen_all: Vec<String>,
    #[serde(default)]
    screen_none: Vec<String>,
    /// When present, screen_any must match on the SAME prefixed line.
    #[serde(default)]
    line_starts_any: Vec<String>,
    #[serde(default)]
    bottom_lines: usize,
    #[serde(default)]
    bottom_non_empty_lines: usize,
    #[serde(default)]
    case_insensitive: bool,
    /// Discard older output before the latest non-menu composer/prompt.
    #[serde(default)]
    after_prompt: bool,
    #[serde(default)]
    no_later_response: bool,
}
const CODEX: &str = include_str!("../assets/activity/codex.toml");
const CLAUDE: &str = include_str!("../assets/activity/claude.toml");
const OPENCODE: &str = include_str!("../assets/activity/opencode.toml");
const PI: &str = include_str!("../assets/activity/pi.toml");
const MAX_MANIFEST: u64 = 128 * 1024;
fn bundled(provider: Provider) -> (&'static str, &'static str) {
    match provider {
        Provider::Codex => (CODEX, "codex"),
        Provider::Claude => (CLAUDE, "claude"),
        Provider::Opencode => (OPENCODE, "opencode"),
        Provider::Pi => (PI, "pi"),
    }
}
fn parse(raw: &str) -> Result<Manifest, String> {
    if raw.len() > MAX_MANIFEST as usize {
        return Err("manifest exceeds 128 KiB".into());
    }
    let manifest =
        toml::from_str::<Manifest>(raw).map_err(|_| "invalid manifest TOML".to_string())?;
    if manifest.rule.len() > 128 {
        return Err("too many detection rules".into());
    }
    for r in &manifest.rule {
        if matches!(r.state, AgentActivity::Finished | AgentActivity::Mixed) {
            return Err("finished and mixed are derived states, not snapshot rules".into());
        }
        let strings = r
            .title_any
            .iter()
            .chain(&r.screen_any)
            .chain(&r.screen_all)
            .chain(&r.screen_none)
            .chain(&r.line_starts_any);
        if r.id.len() > 128
            || strings.clone().count() > 128
            || strings.clone().any(|s| s.is_empty() || s.len() > 512)
        {
            return Err("invalid detection pattern size".into());
        }
        if !r.title_spinner && strings.count() == 0 {
            return Err("detection rule has no predicates".into());
        }
    }
    Ok(manifest)
}
fn override_path(home: &Path, name: &str) -> PathBuf {
    let dir = home.join(".config/tmux-agent-monitor/agent-detection");
    let primary = dir.join(format!("{name}.toml"));
    if std::fs::symlink_metadata(&primary).is_ok()
        || std::fs::symlink_metadata(&dir).is_ok_and(|m| m.file_type().is_symlink())
    {
        primary
    } else {
        home.join(".config/agent-monitor/agent-detection")
            .join(format!("{name}.toml"))
    }
}
fn load(provider: Provider, injected: Option<&str>) -> (Manifest, String, String) {
    let (default, name) = bundled(provider);
    let raw = injected.map(str::to_owned).or_else(|| {
        let home = std::env::var_os("HOME").map(PathBuf::from)?;
        let path = override_path(&home, name);
        if std::fs::symlink_metadata(path.parent()?).is_ok_and(|m| m.file_type().is_symlink()) {
            return None;
        }
        let meta = std::fs::symlink_metadata(&path).ok()?;
        if !meta.is_file() || meta.file_type().is_symlink() {
            return None;
        }
        let file = std::fs::File::open(path).ok()?;
        let mut raw = String::new();
        file.take(MAX_MANIFEST + 1).read_to_string(&mut raw).ok()?;
        Some(raw)
    });
    if let Some(raw) = raw {
        match parse(&raw) {
            Ok(manifest) => return (manifest, "override".into(), String::new()),
            Err(warning) => {
                return (
                    parse(default).expect("bundled manifest"),
                    "bundled".into(),
                    warning,
                );
            }
        }
    }
    (
        parse(default).expect("bundled manifest"),
        "bundled".into(),
        String::new(),
    )
}
fn spinner(title: &str) -> bool {
    title.trim_start().chars().next().is_some_and(|c| {
        ('\u{2801}'..='\u{28ff}').contains(&c) || matches!(c, '◐' | '◓' | '◑' | '◒')
    })
}
fn prompt(line: &str, provider: Provider) -> bool {
    let line = line.trim_start();
    let marker = match provider {
        Provider::Claude => '❯',
        Provider::Codex => '›',
        Provider::Pi => '>',
        Provider::Opencode => '>',
    };
    let Some(rest) = line.strip_prefix(marker) else {
        return false;
    };
    // Numbered choices are permission menus, not a new composer.
    !rest
        .trim_start()
        .chars()
        .next()
        .is_some_and(|c| c.is_ascii_digit())
}
pub fn classify(provider: Provider, title: &str, tail: &str) -> (AgentActivity, MatchEvidence) {
    classify_with_override(provider, title, tail, None)
}
pub fn classify_with_override(
    provider: Provider,
    title: &str,
    tail: &str,
    injected: Option<&str>,
) -> (AgentActivity, MatchEvidence) {
    let (manifest, source, warning) = load(provider, injected);
    let mut evidence = MatchEvidence {
        manifest_source: source,
        manifest_version: manifest.manifest_version,
        warning,
        ..Default::default()
    };
    let clean = paths::clean(tail);
    let title = paths::line(title);
    let mut rules = manifest.rule.iter().enumerate().collect::<Vec<_>>();
    rules.sort_by_key(|(i, r)| (std::cmp::Reverse(r.priority), *i));
    for (_, rule) in rules {
        if rule.title_spinner && !spinner(&title) {
            continue;
        }
        let normalize = |s: &str| {
            if rule.case_insensitive {
                s.to_lowercase()
            } else {
                s.to_owned()
            }
        };
        if !rule.title_any.is_empty()
            && !rule
                .title_any
                .iter()
                .any(|n| normalize(&title).contains(&normalize(n)))
        {
            continue;
        }
        let mut lines = clean.lines().collect::<Vec<_>>();
        if rule.after_prompt
            && let Some(i) = lines.iter().rposition(|s| prompt(s, provider))
        {
            lines = lines[i..].to_vec();
        }
        if rule.bottom_non_empty_lines > 0 {
            lines.retain(|l| !l.trim().is_empty());
            let first = lines.len().saturating_sub(rule.bottom_non_empty_lines);
            lines = lines[first..].to_vec();
        } else if rule.bottom_lines > 0 {
            let first = lines.len().saturating_sub(rule.bottom_lines);
            lines = lines[first..].to_vec();
        }
        let scope = normalize(&lines.join("\n"));
        if !rule
            .screen_all
            .iter()
            .all(|n| scope.contains(&normalize(n)))
            || rule
                .screen_none
                .iter()
                .any(|n| scope.contains(&normalize(n)))
        {
            continue;
        }
        let any = |s: &str| {
            rule.screen_any.is_empty() || rule.screen_any.iter().any(|n| s.contains(&normalize(n)))
        };
        if rule.line_starts_any.is_empty() {
            if !any(&scope) {
                continue;
            }
        } else if !lines.iter().enumerate().any(|(index, line)| {
            if rule.no_later_response
                && lines[index + 1..].iter().any(|l| {
                    let l = l.trim_start();
                    match provider {
                        Provider::Codex => l.starts_with(['•', '■', '✓', '✗']),
                        Provider::Claude => l.starts_with('⏺'),
                        _ => false,
                    }
                })
            {
                return false;
            }
            let line = normalize(line.trim_start_matches([' ', '─', '│']));
            rule.line_starts_any
                .iter()
                .any(|p| line.starts_with(&normalize(p)))
                && any(&line)
        }) {
            continue;
        }
        evidence.rule_id = paths::line(&rule.id);
        evidence.title_signal = rule.title_spinner || !rule.title_any.is_empty();
        return (rule.state, evidence);
    }
    if provider == Provider::Codex && spinner(&title) {
        evidence.rule_id = "codex-title-spinner".into();
        evidence.title_signal = true;
        return (AgentActivity::Working, evidence);
    }
    if clean.trim().is_empty() && (provider != Provider::Codex || title.trim().is_empty()) {
        evidence.fallback_reason = "empty-terminal-snapshot".into();
        return (AgentActivity::Unknown, evidence);
    }
    if provider == Provider::Codex {
        if title.trim().is_empty() {
            evidence.fallback_reason = "no-rule-matched-no-title".into();
            return (AgentActivity::Unknown, evidence);
        }
        evidence.title_signal = true;
        evidence.fallback_reason = "codex-title-without-spinner".into();
    } else {
        evidence.fallback_reason = "default_known_agent_idle_fallback".into();
    }
    (AgentActivity::Idle, evidence)
}

#[derive(Default)]
struct Observation {
    working_seen: bool,
    finished: bool,
    idle: Option<(u64, u64, u8)>,
    completion_emitted: bool,
    last_state: Option<AgentActivity>,
    changed_at: u64,
}
/// An observation is a level; completion is a one-shot edge for this work cycle.
pub struct ActivitySample {
    pub state: AgentActivity,
    pub evidence: MatchEvidence,
    pub finished_now: bool,
}
/// Per monitor-run, memory-only history. No screen text or durable state.
#[derive(Default)]
pub struct ActivityTracker {
    records: HashMap<String, Observation>,
}
impl ActivityTracker {
    pub fn retain(&mut self, active: &HashSet<String>) {
        self.records.retain(|key, _| active.contains(key));
    }
    /// Compatibility path for callers interested only in the current state.
    pub fn observe(
        &mut self,
        key: &str,
        state: AgentActivity,
        evidence: MatchEvidence,
        signature: Option<u64>,
        now_ms: u64,
    ) -> (AgentActivity, MatchEvidence) {
        let sample = self.observe_sample(key, state, evidence, signature, now_ms);
        (sample.state, sample.evidence)
    }
    pub fn observe_sample(
        &mut self,
        key: &str,
        state: AgentActivity,
        evidence: MatchEvidence,
        signature: Option<u64>,
        now_ms: u64,
    ) -> ActivitySample {
        let (state, mut evidence) = self.observe_state(key, state, evidence, signature, now_ms);
        let observation = self
            .records
            .get_mut(key)
            .expect("observation just inserted");
        let finished_now = state == AgentActivity::Finished && !observation.completion_emitted;
        if finished_now {
            observation.completion_emitted = true;
        }
        if observation.last_state != Some(state) {
            observation.changed_at = now_ms;
            observation.last_state = Some(state);
        }
        evidence.observation_key = key.into();
        evidence.state_elapsed_ms = Some(now_ms.saturating_sub(observation.changed_at));
        ActivitySample {
            state,
            evidence,
            finished_now,
        }
    }
    fn observe_state(
        &mut self,
        key: &str,
        state: AgentActivity,
        mut evidence: MatchEvidence,
        signature: Option<u64>,
        now_ms: u64,
    ) -> (AgentActivity, MatchEvidence) {
        if self.records.len() >= 4096 && !self.records.contains_key(key) {
            self.records.clear();
        }
        let observation = self.records.entry(key.into()).or_default();
        match state {
            AgentActivity::Working => {
                observation.working_seen = true;
                observation.completion_emitted = false;
                observation.finished = false;
                observation.idle = None;
            }
            AgentActivity::Idle if signature.is_some() => {
                if observation.working_seen {
                    let signature = signature.unwrap();
                    let idle = observation.idle.get_or_insert((signature, now_ms, 0));
                    if idle.0 != signature {
                        *idle = (signature, now_ms, 0);
                    }
                    idle.2 = idle.2.saturating_add(1);
                    evidence.observations = idle.2;
                    if idle.2 >= 2 && now_ms.saturating_sub(idle.1) >= 1500 {
                        observation.finished = true;
                    }
                    if observation.finished {
                        evidence.transition = "observed-working-to-settled-idle (inferred turn end, not task completion)".into();
                        return (AgentActivity::Finished, evidence);
                    }
                }
            }
            AgentActivity::Blocked | AgentActivity::Waiting => {
                observation.finished = false;
                observation.idle = None;
            }
            _ => {
                observation.working_seen = false;
                observation.finished = false;
                observation.idle = None;
                observation.completion_emitted = false;
            }
        }
        (state, evidence)
    }
}
/// Identity includes process start and optional verified session generation.
pub fn pane_key(p: &crate::tmux::PaneIdentity, generation: &str) -> String {
    serde_json::to_string(&(
        crate::tmux::socket_key(&p.socket),
        &p.server,
        &p.pane,
        p.pane_pid,
        &p.pane_start,
        p.client_pid,
        &p.client_start,
        p.provider,
        &p.cwd,
        generation,
    ))
    .expect("pane identity is serializable")
}
pub fn describe(provider: Provider, state: AgentActivity, evidence: &MatchEvidence) -> String {
    let why = if !evidence.transition.is_empty() {
        &evidence.transition
    } else if !evidence.rule_id.is_empty() {
        &evidence.rule_id
    } else {
        &evidence.fallback_reason
    };
    let timing = evidence
        .state_elapsed_ms
        .map(|ms| {
            format!(
                "; state observed for {}s at {}",
                ms / 1000,
                paths::date(evidence.sampled_at)
            )
        })
        .unwrap_or_default();
    format!(
        "{} · {} ({}; {} manifest v{}; inferred{})",
        provider.label(),
        state.label(),
        why,
        evidence.manifest_source,
        evidence.manifest_version,
        timing,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    const TEST: &str = r#"
manifest_version = 7
[[rule]]
id = "approval"
state = "blocked"
screen_any = ["Approve", "(y/n)"]
[[rule]]
id = "spinner"
state = "working"
screen_any = ["Working", "⠼"]
screen_none = ["error"]
[[rule]]
id = "scoped-tail"
state = "working"
screen_any = ["done-marker"]
bottom_lines = 3
"#;
    #[test]
    fn ordered_rules_win_and_blocked_is_strict() {
        let (s, e) =
            classify_with_override(Provider::Codex, "", "⠼ Working\nApprove? (y/n)", Some(TEST));
        assert_eq!(s, AgentActivity::Blocked);
        assert_eq!(e.rule_id, "approval");
        let (s, e) = classify_with_override(Provider::Codex, "", "⠼ Working on it", Some(TEST));
        assert_eq!(s, AgentActivity::Working);
        assert_eq!(e.rule_id, "spinner");
        assert_eq!(
            classify_with_override(
                Provider::Codex,
                "",
                "Something needs okay-ish confirmation",
                Some(TEST)
            )
            .0,
            AgentActivity::Unknown
        );
        assert_eq!(
            classify_with_override(
                Provider::Claude,
                "",
                "Something needs okay-ish confirmation",
                Some(TEST)
            )
            .0,
            AgentActivity::Idle
        );
        assert_eq!(
            classify_with_override(Provider::Codex, "", "Working error", Some(TEST)).0,
            AgentActivity::Unknown
        );
    }
    #[test]
    fn bottom_lines_scopes_the_match() {
        assert_eq!(
            classify_with_override(
                Provider::Codex,
                "",
                "done-marker\nfiller\nfiller\nfiller\nfiller",
                Some(TEST)
            )
            .0,
            AgentActivity::Unknown
        );
        assert_eq!(
            classify_with_override(
                Provider::Codex,
                "",
                "filler\nfiller\ndone-marker",
                Some(TEST)
            )
            .0,
            AgentActivity::Working
        );
    }
    #[test]
    fn codex_title_signal_matrix() {
        assert_eq!(
            classify_with_override(Provider::Codex, "⠼ codex", "", Some(TEST)).0,
            AgentActivity::Working
        );
        assert_eq!(
            classify_with_override(Provider::Codex, "codex", "", Some(TEST)).0,
            AgentActivity::Idle
        );
        assert_eq!(
            classify_with_override(Provider::Codex, "filename ⠼", "", Some(TEST)).0,
            AgentActivity::Idle
        );
        assert_eq!(
            classify_with_override(Provider::Codex, "", "", Some(TEST)).0,
            AgentActivity::Unknown
        );
    }
    #[test]
    fn invalid_manifest_falls_back_to_bundled() {
        let (s, e) = classify_with_override(Provider::Codex, "", "⠼ Working", Some("not = [valid"));
        assert_eq!(s, AgentActivity::Working);
        assert_eq!(e.manifest_source, "bundled");
        assert!(!e.warning.is_empty());
    }
    #[test]
    fn renamed_overrides_take_precedence_without_losing_legacy_rules() {
        let home = tempfile::tempdir().unwrap();
        let old = home
            .path()
            .join(".config/agent-monitor/agent-detection/pi.toml");
        std::fs::create_dir_all(old.parent().unwrap()).unwrap();
        std::fs::write(&old, PI).unwrap();
        assert_eq!(override_path(home.path(), "pi"), old);
        let new = home
            .path()
            .join(".config/tmux-agent-monitor/agent-detection/pi.toml");
        std::fs::create_dir_all(new.parent().unwrap()).unwrap();
        std::fs::write(&new, PI).unwrap();
        assert_eq!(override_path(home.path(), "pi"), new);
    }
    #[test]
    fn bundled_manifests_all_parse() {
        for p in [
            Provider::Pi,
            Provider::Claude,
            Provider::Codex,
            Provider::Opencode,
        ] {
            assert!(!parse(bundled(p).0).unwrap().rule.is_empty());
        }
    }
    #[test]
    fn describe_always_shows_provenance() {
        let (_, e) = classify_with_override(Provider::Codex, "", "⠼ Working", Some(TEST));
        assert!(
            describe(Provider::Codex, AgentActivity::Working, &e)
                .contains("spinner; override manifest v7")
        );
    }
    #[test]
    fn provider_signal_matrix_rejects_prose_and_stale_approvals() {
        for (p, title, screen) in [
            (Provider::Pi, "pi", "── ⠼ Working ───\n>"),
            (
                Provider::Claude,
                "claude",
                "✻ Thinking… (3s · esc to interrupt)\n❯",
            ),
            (
                Provider::Codex,
                "codex",
                "• Working (2s • esc to interrupt)\n›",
            ),
            (Provider::Opencode, "opencode", "press esc to interrupt"),
        ] {
            assert_eq!(
                classify_with_override(p, title, screen, Some(bundled(p).0)).0,
                AgentActivity::Working,
                "{p:?}"
            );
        }
        for (p, title, screen, state) in [
            (
                Provider::Pi,
                "pi",
                "Choose an option\nenter to select\nesc to cancel",
                AgentActivity::Waiting,
            ),
            (
                Provider::Claude,
                "⠼ claude",
                "❯ 1. Yes, proceed\n2. No\nEsc to cancel",
                AgentActivity::Blocked,
            ),
            (
                Provider::Codex,
                "⠼ codex",
                "› inspect\nWould you like to proceed?\n› 1. Yes\nEsc to cancel",
                AgentActivity::Blocked,
            ),
            (
                Provider::Opencode,
                "opencode",
                "△ Permission required\npress esc to interrupt",
                AgentActivity::Blocked,
            ),
        ] {
            assert_eq!(
                classify_with_override(p, title, screen, Some(bundled(p).0)).0,
                state,
                "{p:?}"
            );
        }
        for p in [
            Provider::Pi,
            Provider::Claude,
            Provider::Codex,
            Provider::Opencode,
        ] {
            assert_eq!(
                classify_with_override(
                    p,
                    p.label(),
                    "The docs mention Working, Thinking, Allow, Approve and (y/n).",
                    Some(bundled(p).0)
                )
                .0,
                AgentActivity::Idle
            );
            assert_eq!(
                classify_with_override(
                    p,
                    p.label(),
                    "API Error: authentication failed",
                    Some(bundled(p).0)
                )
                .0,
                AgentActivity::Error
            );
            assert_eq!(
                classify_with_override(p, "", "", Some(bundled(p).0)).0,
                AgentActivity::Unknown
            );
        }
        let old = format!("Working...\n{}\n>", "a settled response\n".repeat(20));
        assert_eq!(
            classify_with_override(Provider::Pi, "pi", &old, Some(PI)).0,
            AgentActivity::Idle
        );
        assert_eq!(
            classify_with_override(
                Provider::Codex,
                "codex",
                "• Working (2s • esc to interrupt)\n• Here is the answer.\n›",
                Some(CODEX)
            )
            .0,
            AgentActivity::Idle
        );
        let old = "Yes, proceed\nesc to cancel\n› a later prompt\nThe action is complete.";
        assert_eq!(
            classify_with_override(Provider::Codex, "codex", old, Some(CODEX)).0,
            AgentActivity::Idle
        );
    }
    #[test]
    fn priorities_are_explicit_and_legacy_ties_keep_first_match() {
        let raw = r#"[[rule]]
id = "first"
state = "working"
screen_any = ["signal"]
[[rule]]
id = "second"
state = "blocked"
priority = 100
screen_any = ["signal"]"#;
        assert_eq!(
            classify_with_override(Provider::Pi, "", "signal", Some(raw)).0,
            AgentActivity::Blocked
        );
        let derived = "[[rule]]\nid='untrusted-finish'\nstate='finished'\nscreen_any=['done']";
        assert_ne!(
            classify_with_override(Provider::Pi, "", "done", Some(derived)).0,
            AgentActivity::Finished
        );
    }
    #[test]
    fn finished_requires_observed_work_and_two_settled_samples() {
        let mut t = ActivityTracker::default();
        let e = MatchEvidence::default();
        assert_eq!(
            t.observe("pane", AgentActivity::Idle, e.clone(), Some(1), 0)
                .0,
            AgentActivity::Idle
        );
        t.observe("pane", AgentActivity::Working, e.clone(), Some(2), 2000);
        assert_eq!(
            t.observe("pane", AgentActivity::Idle, e.clone(), Some(3), 4000)
                .0,
            AgentActivity::Idle
        );
        let (s, e2) = t.observe("pane", AgentActivity::Idle, e.clone(), Some(3), 6000);
        assert_eq!(s, AgentActivity::Finished);
        assert!(!e2.transition.is_empty());
        assert_eq!(
            t.observe("pane", AgentActivity::Working, e.clone(), Some(4), 8000)
                .0,
            AgentActivity::Working
        );
        assert_eq!(
            t.observe("new-pid", AgentActivity::Idle, e, Some(3), 10000)
                .0,
            AgentActivity::Idle
        );
    }
    #[test]
    fn completion_edges_are_once_per_work_cycle_and_times_are_monotonic() {
        let mut tracker = ActivityTracker::default();
        let mut sample = |state, now| {
            tracker.observe_sample("identity", state, MatchEvidence::default(), Some(1), now)
        };
        assert!(!sample(AgentActivity::Idle, 0).finished_now);
        assert!(!sample(AgentActivity::Working, 2000).finished_now);
        assert!(!sample(AgentActivity::Idle, 4000).finished_now);
        let first = sample(AgentActivity::Idle, 6000);
        assert!(first.finished_now);
        assert_eq!(first.evidence.state_elapsed_ms, Some(0));
        let later = sample(AgentActivity::Idle, 8000);
        assert!(!later.finished_now);
        assert_eq!(later.evidence.observation_key, "identity");
        assert_eq!(later.evidence.state_elapsed_ms, Some(2000));
        // A transient menu must not rearm a turn that already completed.
        assert!(!sample(AgentActivity::Waiting, 10000).finished_now);
        assert!(!sample(AgentActivity::Idle, 12000).finished_now);
        assert!(!sample(AgentActivity::Idle, 14000).finished_now);
        sample(AgentActivity::Working, 16000);
        sample(AgentActivity::Blocked, 18000);
        sample(AgentActivity::Idle, 20000);
        assert!(sample(AgentActivity::Idle, 22000).finished_now);
        sample(AgentActivity::Unknown, 24000);
        sample(AgentActivity::Idle, 26000);
        assert!(!sample(AgentActivity::Idle, 28000).finished_now);
    }
    #[test]
    fn failures_errors_and_changing_snapshots_cannot_complete_work() {
        let mut t = ActivityTracker::default();
        let e = MatchEvidence::default();
        t.observe("p", AgentActivity::Working, e.clone(), Some(1), 0);
        t.observe("p", AgentActivity::Idle, e.clone(), Some(2), 2000);
        assert_eq!(
            t.observe("p", AgentActivity::Idle, e.clone(), Some(3), 4000)
                .0,
            AgentActivity::Idle
        );
        t.observe("p", AgentActivity::Unknown, e.clone(), None, 6000);
        assert_eq!(
            t.observe("p", AgentActivity::Idle, e.clone(), Some(3), 8000)
                .0,
            AgentActivity::Idle
        );
        t.observe("p", AgentActivity::Working, e.clone(), Some(1), 10000);
        t.observe("p", AgentActivity::Error, e.clone(), Some(1), 12000);
        assert_eq!(
            t.observe("p", AgentActivity::Idle, e, Some(3), 14000).0,
            AgentActivity::Idle
        );
        t.retain(&HashSet::new());
        assert!(t.records.is_empty());
    }
}
