//! Deterministic desktop tests: every backend below is in-memory, never native.
use super::*;
use crate::{activity::MatchEvidence, model::Provider, pi::Cursor};
use std::{os::unix::fs::PermissionsExt, time::Duration};

struct RecordingBackend(mpsc::Sender<DesktopNotification>);
impl DesktopBackend for RecordingBackend {
    fn label(&self) -> String {
        "test desktop".into()
    }
    fn send(&mut self, notification: &DesktopNotification) -> Result<()> {
        self.0.send(notification.clone())?;
        Ok(())
    }
}
fn private_root() -> tempfile::TempDir {
    let root = tempfile::tempdir().unwrap();
    std::fs::set_permissions(root.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    root
}
fn engine() -> (Notifications, mpsc::Receiver<DesktopNotification>) {
    let (tx, rx) = mpsc::channel();
    (Notifications::new(RecordingBackend(tx)), rx)
}
fn pane(key: &str) -> PaneIdentity {
    PaneIdentity {
        socket: "/fixture/socket".into(),
        server: "server-1".into(),
        pane: "%3".into(),
        pane_pid: 10,
        pane_start: "start-1".into(),
        client_pid: 11,
        client_start: "start-2".into(),
        target: "dev:2.1".into(),
        session: "dev".into(),
        window: "2".into(),
        cwd: "/repo/project".into(),
        command: "pi".into(),
        title: "private prompt must not be sent".into(),
        provider: Some(Provider::Pi),
        activity: Some(crate::activity::AgentActivity::Finished),
        activity_evidence: Some(MatchEvidence {
            observation_key: key.into(),
            ..Default::default()
        }),
    }
}
fn catalog(panes: &[PaneIdentity]) -> Catalog {
    Catalog {
        unbound: panes.to_vec(),
        ..Default::default()
    }
}
fn saved_session(store: &Store) -> Session {
    store.add_source(Path::new("/fixtures")).unwrap();
    let source = store.sources().unwrap().remove(0);
    let mut cursor = Cursor::default();
    cursor.metadata.native_id = "native-id".into();
    cursor.metadata.file = "/fixtures/one.jsonl".into();
    store.ingest(&source, &cursor).unwrap();
    store.resolve("native-id").unwrap()
}
fn received(rx: &mpsc::Receiver<DesktopNotification>) -> DesktopNotification {
    rx.recv_timeout(Duration::from_secs(2))
        .expect("fake desktop should receive")
}
fn quiet(rx: &mpsc::Receiver<DesktopNotification>) {
    assert!(rx.recv_timeout(Duration::from_millis(30)).is_err());
}

#[test]
fn master_is_opt_in_batch_is_deduplicated_and_disabled_events_are_not_replayed() {
    let root = private_root();
    let store = Store::open(root.path()).unwrap();
    let p = pane("first-generation");
    let c = catalog(std::slice::from_ref(&p));
    let (mut notifications, rx) = engine();
    notifications.refresh(&store, root.path(), &c);
    assert!(!notifications.snapshot().preferences.enabled);
    notifications.consume(vec![p.clone()], &c);
    quiet(&rx);
    notifications.set_global(&store, true).unwrap();
    notifications.consume(vec![], &c);
    quiet(&rx);
    notifications.consume(vec![p.clone(), p.clone()], &c);
    let notice = received(&rx);
    assert_eq!(notice.title, "pi: turn finished");
    assert!(notice.body.contains("dev:2.1 (%3)"));
    assert!(!notice.body.contains("private prompt"));
    quiet(&rx);
    let target = Target {
        session: None,
        openings: vec!["first-generation".into()],
    };
    notifications.set_target(&store, &target, false).unwrap();
    notifications.consume(vec![p], &c);
    quiet(&rx);
    notifications.set_global(&store, false).unwrap();
    notifications.set_global(&store, true).unwrap();
    assert!(!notifications.snapshot().target_enabled(&target));
}

#[test]
fn saved_mute_survives_restart_closed_history_and_reimport_without_changing_decisions() {
    let root = private_root();
    let store = Store::open(root.path()).unwrap();
    let mut session = saved_session(&store);
    store
        .change(
            &session.id,
            Some(crate::model::ResumeState::Done),
            Some("private next step"),
        )
        .unwrap();
    let (mut notifications, _) = engine();
    let target = Target::from_session(&session);
    notifications.set_target(&store, &target, false).unwrap();
    assert!(!notifications.snapshot().session_enabled(&session));
    drop(notifications);
    drop(store);
    let store = Store::open(root.path()).unwrap();
    session = store.resolve(&session.id).unwrap();
    let (mut notifications, rx) = engine();
    let p = pane("generation");
    session.probable.push(p.clone());
    let c = Catalog {
        sessions: vec![session.clone()],
        ..Default::default()
    };
    notifications.refresh(&store, root.path(), &c);
    notifications.set_global(&store, true).unwrap();
    assert!(!notifications.snapshot().session_enabled(&session));
    notifications.consume(vec![p], &c);
    quiet(&rx);
    assert_eq!(session.note, "private next step");
    assert_eq!(session.state, crate::model::ResumeState::Done);
    store.undo(&session.id).unwrap();
    assert_eq!(store.resolve(&session.id).unwrap().note, "");
    assert_eq!(
        Preferences::load(&store)
            .unwrap()
            .0
            .sessions
            .get(&session.id),
        Some(&false)
    );
}

#[test]
fn ephemeral_override_survives_association_but_not_reused_process_identity() {
    let root = private_root();
    let store = Store::open(root.path()).unwrap();
    let p = pane("old-process");
    let (mut notifications, rx) = engine();
    notifications.refresh(&store, root.path(), &catalog(std::slice::from_ref(&p)));
    notifications.set_global(&store, true).unwrap();
    let target = Target {
        session: None,
        openings: vec!["old-process".into()],
    };
    notifications.set_target(&store, &target, false).unwrap();
    let mut session = saved_session(&store);
    session.probable.push(p.clone());
    let c = Catalog {
        sessions: vec![session.clone()],
        ..Default::default()
    };
    notifications.refresh(&store, root.path(), &c);
    assert!(!notifications.snapshot().session_enabled(&session));
    notifications.consume(vec![p], &c);
    quiet(&rx);
    notifications
        .set_target(&store, &Target::from_session(&session), true)
        .unwrap();
    assert!(notifications.snapshot().temporary.is_empty());
    // Muting old identity again must not mute a replacement in the same pane.
    notifications.set_target(&store, &target, false).unwrap();
    let replacement = pane("new-process-or-pi-generation");
    let c = catalog(std::slice::from_ref(&replacement));
    notifications.refresh(&store, root.path(), &c);
    assert!(notifications.snapshot().temporary.is_empty());
    assert!(notifications.set_target(&store, &target, false).is_err());
    notifications.consume(vec![replacement], &c);
    received(&rx);
}

#[test]
fn ambiguous_session_association_uses_opening_policy_not_arbitrary_saved_mute() {
    let root = private_root();
    let store = Store::open(root.path()).unwrap();
    let p = pane("identity");
    let mut one = saved_session(&store);
    let mut two = one.clone();
    two.id = "other-session".into();
    one.probable.push(p.clone());
    two.probable.push(p.clone());
    let (mut notifications, rx) = engine();
    notifications
        .set_target(&store, &Target::from_session(&one), false)
        .unwrap();
    notifications.set_global(&store, true).unwrap();
    let c = Catalog {
        sessions: vec![one, two],
        ..Default::default()
    };
    notifications.refresh(&store, root.path(), &c);
    notifications.consume(vec![p], &c);
    received(&rx);
}

#[test]
fn lock_elects_one_sender_and_takeover_does_not_replay_discarded_events() {
    let root = private_root();
    let store = Store::open(root.path()).unwrap();
    let p = pane("identity");
    let c = catalog(std::slice::from_ref(&p));
    let (mut first, first_rx) = engine();
    let (mut second, second_rx) = engine();
    first.set_global(&store, true).unwrap();
    first.refresh(&store, root.path(), &c);
    second.refresh(&store, root.path(), &c);
    assert!(first.snapshot().owner);
    assert!(!second.snapshot().owner);
    let temporary = Target {
        session: None,
        openings: vec!["identity".into()],
    };
    assert!(!second.snapshot().can_change_target(&temporary));
    assert!(second.set_target(&store, &temporary, false).is_err());
    second.consume(vec![p.clone()], &c);
    first.consume(vec![p.clone()], &c);
    received(&first_rx);
    quiet(&second_rx);
    drop(first);
    second.refresh(&store, root.path(), &c);
    assert!(second.snapshot().owner);
    second.consume(vec![], &c);
    quiet(&second_rx);
    second.consume(vec![p], &c);
    received(&second_rx);
}

#[test]
fn separate_openings_notify_independently_even_when_session_activity_is_mixed() {
    let root = private_root();
    let store = Store::open(root.path()).unwrap();
    let first = pane("first-opening");
    let mut second = pane("second-opening");
    second.pane = "%4".into();
    let mut session = saved_session(&store);
    session.probable = vec![first.clone(), second.clone()];
    session.probable[1].activity = Some(crate::activity::AgentActivity::Working);
    assert_eq!(
        session.live_activity(),
        Some(crate::activity::AgentActivity::Mixed)
    );
    let mut c = Catalog {
        sessions: vec![session],
        ..Default::default()
    };
    let (mut notifications, rx) = engine();
    notifications.set_global(&store, true).unwrap();
    notifications.refresh(&store, root.path(), &c);
    // Events are per opening; neither UI filters nor a mixed session aggregate
    // participates in the delivery policy. Duplicated attachment is ignored.
    notifications.consume(vec![first.clone(), first], &c);
    assert!(received(&rx).body.contains("%3"));
    quiet(&rx);
    c.sessions[0].probable[1] = second.clone();
    notifications.refresh(&store, root.path(), &c);
    notifications.consume(vec![second], &c);
    assert!(received(&rx).body.contains("%4"));
    quiet(&rx);
}

struct BlockingBackend {
    delivered: mpsc::Sender<DesktopNotification>,
    release: mpsc::Receiver<()>,
    first: bool,
}
impl DesktopBackend for BlockingBackend {
    fn label(&self) -> String {
        "blocked test desktop".into()
    }
    fn send(&mut self, notification: &DesktopNotification) -> Result<()> {
        self.delivered.send(notification.clone())?;
        if self.first {
            self.first = false;
            self.release.recv_timeout(Duration::from_secs(3))?;
        }
        Ok(())
    }
}
#[test]
fn bounded_queue_never_blocks_and_toggle_cancels_pending_even_after_reenable() {
    let root = private_root();
    let store = Store::open(root.path()).unwrap();
    let p = pane("identity");
    let c = catalog(std::slice::from_ref(&p));
    let (tx, rx) = mpsc::channel();
    let (release, wait) = mpsc::channel();
    let mut notifications = Notifications::new(BlockingBackend {
        delivered: tx,
        release: wait,
        first: true,
    });
    notifications.set_global(&store, true).unwrap();
    notifications.refresh(&store, root.path(), &c);
    notifications.consume(vec![p.clone()], &c);
    received(&rx); // Sender now blocks while worker operations must remain usable.
    for _ in 0..QUEUE_SIZE + 2 {
        notifications.consume(vec![p.clone()], &c);
    }
    assert!(notifications.snapshot().diagnostic.contains("queue busy"));
    notifications.set_global(&store, false).unwrap();
    notifications.set_global(&store, true).unwrap();
    release.send(()).unwrap();
    quiet(&rx); // Old revision cannot become eligible again.
    notifications.consume(vec![p], &c);
    received(&rx);
    drop(notifications);
    quiet(&rx);
}

struct FailingBackend(mpsc::Sender<()>);
impl DesktopBackend for FailingBackend {
    fn label(&self) -> String {
        "missing desktop".into()
    }
    fn send(&mut self, _: &DesktopNotification) -> Result<()> {
        self.0.send(()).unwrap();
        anyhow::bail!("synthetic permission denial")
    }
}
#[test]
fn backend_errors_are_bounded_diagnostics_without_retry_and_failed_refresh_cancels() {
    let root = private_root();
    let store = Store::open(root.path()).unwrap();
    let p = pane("identity");
    let c = catalog(std::slice::from_ref(&p));
    let (tx, rx) = mpsc::channel();
    let mut notifications = Notifications::new(FailingBackend(tx));
    notifications.set_global(&store, true).unwrap();
    notifications.refresh(&store, root.path(), &c);
    notifications.consume(vec![p.clone()], &c);
    rx.recv_timeout(Duration::from_secs(2)).unwrap();
    notifications.unavailable();
    notifications.consume(vec![p], &c);
    let feedback = notifications.feedback.clone();
    drop(notifications); // Join ensures the failure has been recorded.
    assert!(
        feedback
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .contains("synthetic permission denial")
    );
    assert!(rx.try_recv().is_err());
}

#[test]
fn unsafe_lock_files_and_invalid_preferences_fail_closed_without_touching_provider_data() {
    let root = private_root();
    let store = Store::open(root.path()).unwrap();
    let victim = root.path().join("victim");
    std::fs::write(&victim, "untouched").unwrap();
    let lock = root.path().join("notifications.lock");
    std::os::unix::fs::symlink(&victim, &lock).unwrap();
    assert!(SenderLock::acquire(root.path()).is_err());
    std::fs::remove_file(&lock).unwrap();
    std::fs::write(&lock, "").unwrap();
    std::fs::set_permissions(&lock, std::fs::Permissions::from_mode(0o644)).unwrap();
    assert!(SenderLock::acquire(root.path()).is_err());
    assert_eq!(std::fs::read_to_string(victim).unwrap(), "untouched");
    store.set_preference(MASTER, "invalid").unwrap();
    store
        .set_preference(&format!("{SESSION_PREFIX}session"), "invalid")
        .unwrap();
    let (preferences, diagnostic) = Preferences::load(&store).unwrap();
    assert!(!preferences.enabled);
    assert_eq!(preferences.sessions.get("session"), Some(&false));
    assert!(!diagnostic.is_empty());
}

#[test]
fn payload_strips_controls_and_caps_metadata_without_capturing_transcripts() {
    let mut p = pane("identity");
    p.cwd = format!("/repo/{}\u{202e}\x1b[31m", "x".repeat(1000));
    p.target = "\x1b]0;spoof\x07\"\n <b>& unsafe".into();
    let notice = DesktopNotification::from_pane(&p);
    assert!(!notice.body.contains('\x1b'));
    assert!(!notice.body.contains('\u{202e}'));
    assert!(!notice.body.contains("private prompt"));
    assert!(notice.body.len() < 300);
}
