//! Background worker: blocking I/O lives here, never in render/input.
//!
//! Two lanes with different semantics: detail reads are latest-wins (only
//! the newest query is answered, stale ones are dropped), while mutations
//! are an ordered queue where every write gets an explicit ack. The status
//! line reports "Saved" only when that ack arrives.
use crate::{
    model::*,
    presence,
    service::{ResumeSpec, Service},
};
use std::{
    path::PathBuf,
    sync::{Arc, Mutex, mpsc},
    time::{Duration, Instant},
};

#[derive(Debug, Clone)]
pub struct DetailQuery {
    pub id: String,
    pub generation: u64,
    pub leaf: Option<String>,
    pub tools: bool,
    pub page: usize,
}
pub enum Action {
    Change {
        id: String,
        state: Option<ResumeState>,
        note: Option<String>,
    },
    Undo(String),
    Focus(Box<Binding>),
    FocusPane(Box<crate::tmux::PaneIdentity>),
    Prepare(String),
    Resume {
        id: String,
        allow_uncertain: bool,
    },
    Refresh,
}
pub enum Reply {
    Catalog(Catalog),
    Detail {
        query: DetailQuery,
        result: Result<Conversation, String>,
    },
    Ack(Result<String, String>),
    Confirm {
        id: String,
        spec: ResumeSpec,
    },
    Launch(ResumeSpec),
}
/// Handles into the worker thread: ordered action queue, latest detail
/// query slot, and the reply channel the event loop drains each frame.
pub struct Worker {
    pub actions: mpsc::SyncSender<Action>,
    pub queries: Arc<Mutex<Option<DetailQuery>>>,
    pub replies: mpsc::Receiver<Reply>,
}
impl Worker {
    #[cfg(test)]
    pub(super) fn idle() -> Self {
        let (actions, _) = mpsc::sync_channel(16);
        let (_, replies) = mpsc::sync_channel(16);
        Self {
            actions,
            queries: Arc::new(Mutex::new(None)),
            replies,
        }
    }
    pub fn start(root: PathBuf) -> Self {
        let (actions, rx) = mpsc::sync_channel(16);
        let (tx, replies) = mpsc::sync_channel(16);
        let queries = Arc::new(Mutex::new(None));
        let queries_worker = queries.clone();
        std::thread::spawn(move || run(root, rx, queries_worker, tx));
        Self {
            actions,
            queries,
            replies,
        }
    }
    pub fn query(&self, query: DetailQuery) {
        *self.queries.lock().unwrap() = Some(query);
    }
}
fn error(e: anyhow::Error) -> String {
    crate::paths::line(&format!("{e:#}"))
}
fn run(
    root: PathBuf,
    actions: mpsc::Receiver<Action>,
    queries: Arc<Mutex<Option<DetailQuery>>>,
    replies: mpsc::SyncSender<Reply>,
) {
    let service = match Service::open(Some(root)) {
        Ok(s) => s,
        Err(e) => {
            let _ = replies.send(Reply::Ack(Err(error(e))));
            return;
        }
    };
    // Startup scope/view belong to the UI defaults, not saved preferences.
    let mut catalog = Catalog {
        sessions: service.store.list().unwrap_or_default(),
        scanning: true,
        ..Default::default()
    };
    if replies.send(Reply::Catalog(catalog.clone())).is_err() {
        return;
    }
    let mut indexer = match service.begin_index() {
        Ok(i) => Some(i),
        Err(e) => {
            let _ = replies.send(Reply::Ack(Err(error(e))));
            None
        }
    };
    let mut last_presence = Instant::now() - Duration::from_secs(3);
    let mut last_catalog = Instant::now();
    let mut last_index = Instant::now();
    loop {
        // One mutation per loop turn; reads can never consume a pending write.
        match actions.try_recv() {
            Ok(action) => {
                let reply = match action {
                    Action::Change { id, state, note } => Reply::Ack(
                        service
                            .store
                            .change(&id, state, note.as_deref())
                            .map(|()| "Saved".into())
                            .map_err(error),
                    ),
                    Action::Undo(id) => Reply::Ack(
                        service
                            .store
                            .undo(&id)
                            .map(|()| "Change undone".into())
                            .map_err(error),
                    ),
                    Action::Focus(binding) => Reply::Ack(
                        presence::focus(&binding)
                            .map(|()| "Opening selected".into())
                            .map_err(error),
                    ),
                    Action::FocusPane(pane) => Reply::Ack(
                        crate::tmux::focus_probable(&pane)
                            .map(|()| "Pane selected (association unverified)".into())
                            .map_err(error),
                    ),
                    Action::Prepare(id) => match service.resume_spec(&id) {
                        Ok(spec) => Reply::Confirm { id, spec },
                        Err(e) => Reply::Ack(Err(error(e))),
                    },
                    Action::Resume {
                        id,
                        allow_uncertain,
                    } => match service.resume_spec(&id) {
                        Ok(spec) if !spec.warning.is_empty() && !allow_uncertain => {
                            Reply::Confirm { id, spec }
                        }
                        Ok(spec) => Reply::Launch(spec),
                        Err(e) => Reply::Ack(Err(error(e))),
                    },
                    Action::Refresh => {
                        last_presence = Instant::now() - Duration::from_secs(3);
                        last_index = Instant::now() - Duration::from_secs(6);
                        Reply::Ack(Ok("Refresh requested".into()))
                    }
                };
                if replies.send(reply).is_err() {
                    break;
                }
                last_catalog = Instant::now() - Duration::from_secs(1);
            }
            Err(mpsc::TryRecvError::Disconnected) => break,
            Err(mpsc::TryRecvError::Empty) => {}
        }
        let query = queries.lock().unwrap().take();
        if let Some(query) = query {
            let result = service
                .detail_page(&query.id, query.leaf.as_deref(), query.tools, query.page)
                .map_err(error);
            if replies.send(Reply::Detail { query, result }).is_err() {
                break;
            }
        }
        if last_presence.elapsed() >= Duration::from_secs(2) {
            match service.catalog() {
                Ok(new) => catalog = new,
                Err(e) => {
                    catalog.warnings = vec![error(e)];
                    for session in &mut catalog.sessions {
                        session.presence_verified = false;
                        session.probable.clear();
                        session
                            .bindings
                            .retain(|b| (0..8000).contains(&(crate::paths::now() - b.bridge.seen)));
                        for binding in &mut session.bindings {
                            if let Some(pane) = &mut binding.pane {
                                pane.activity = Some(crate::activity::AgentActivity::Unknown);
                                pane.activity_evidence = Some(crate::activity::MatchEvidence {
                                    fallback_reason: "presence-refresh-unavailable".into(),
                                    ..Default::default()
                                });
                            }
                        }
                    }
                }
            }
            last_presence = Instant::now();
            last_catalog = Instant::now() - Duration::from_secs(1);
        }
        if let Some(index) = &mut indexer {
            if let Err(e) = service.index_step(index) {
                catalog.warnings.push(error(e));
                indexer = None;
            } else if !index.pending() {
                for warning in &index.warnings {
                    if !catalog.warnings.contains(warning) {
                        catalog.warnings.push(warning.clone());
                    }
                }
                indexer = None;
                last_catalog = Instant::now() - Duration::from_secs(1);
            }
        }
        if indexer.is_none() && last_index.elapsed() >= Duration::from_secs(5) {
            indexer = service.begin_index().ok();
            last_index = Instant::now();
        }
        if last_catalog.elapsed() >= Duration::from_millis(200) {
            if let Ok(mut sessions) = service.store.list() {
                let live = crate::service::reattach_presence(&mut sessions, &catalog.sessions);
                sessions.extend(live);
                // Preserve screen activity across store refreshes, keyed by
                // socket+pane (store rows never hold live state).
                let activity: std::collections::HashMap<_, _> = catalog
                    .unbound
                    .iter()
                    .map(|p| {
                        (
                            (crate::tmux::socket_key(&p.socket), p.pane.clone()),
                            (p.activity, p.activity_evidence.clone()),
                        )
                    })
                    .collect();
                // Full presence (with fresh captures) refreshes on the 2 s
                // tick via catalog(); between ticks, keep showing the last
                // sampled activity instead of flickering to unsampled.
                for pane in &mut catalog.unbound {
                    if pane.activity.is_none()
                        && let Some((state, evidence)) = activity
                            .get(&(crate::tmux::socket_key(&pane.socket), pane.pane.clone()))
                    {
                        pane.activity = *state;
                        pane.activity_evidence = evidence.clone();
                    }
                }
                crate::service::associate_probable(&mut sessions, &catalog.unbound);
                crate::service::append_live_panes(&mut sessions, &catalog.unbound);
                catalog.sessions = sessions;
            }
            catalog.scanning = indexer.as_ref().is_some_and(|i| i.initial && i.pending());
            if let Some(index) = &indexer {
                for warning in &index.warnings {
                    if !catalog.warnings.contains(warning) {
                        catalog.warnings.push(warning.clone());
                    }
                }
            }
            if replies.send(Reply::Catalog(catalog.clone())).is_err() {
                break;
            }
            last_catalog = Instant::now();
        }
        if indexer.is_none() {
            std::thread::sleep(Duration::from_millis(15));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn detail_slot_is_latest_wins_without_consuming_actions() {
        let slot = Arc::new(Mutex::new(None));
        let (tx, rx) = mpsc::sync_channel(16);
        tx.send(Action::Undo("first".into())).unwrap();
        tx.send(Action::Undo("second".into())).unwrap();
        for generation in 0..100 {
            *slot.lock().unwrap() = Some(DetailQuery {
                id: generation.to_string(),
                generation,
                leaf: None,
                tools: false,
                page: 0,
            });
        }
        assert_eq!(slot.lock().unwrap().take().unwrap().generation, 99);
        assert!(matches!(rx.recv().unwrap(),Action::Undo(id) if id=="first"));
        assert!(matches!(rx.recv().unwrap(),Action::Undo(id) if id=="second"));
    }
}
