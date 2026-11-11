//! Topology recovery tests: every tmux command goes to a strict Python stub.
use super::*;
use std::os::unix::fs::PermissionsExt;

fn layout(body: &str) -> String {
    format!("{:04x},{body}", layout_checksum(body))
}
fn pane(id: u32, index: u32, root: &Path) -> SavedPane {
    SavedPane {
        id: format!("%{id}"),
        index,
        active: index == 3,
        cwd: root.to_string_lossy().into(),
        title: format!("pane {id}; title"),
        current_command: "sh".into(),
        start_command: String::new(),
        stamp: None,
        content: PaneContent::Shell,
    }
}
fn fixture(root: &Path) -> Workspace {
    let mut first = SavedWindow {
        id: "@1".into(),
        index: 2,
        name: "API ; $(literal) 界".into(),
        layout: layout("100x30,0,0{49x30,0,0,11,50x30,50,0[50x14,50,0,12,50x15,50,15,13]}"),
        width: 100,
        height: 30,
        active: true,
        zoomed: true,
        linked: false,
        pane_base_index: 3,
        automatic_rename: false,
        allow_rename: false,
        size_policy: "manual".into(),
        panes: vec![pane(11, 3, root), pane(12, 4, root), pane(13, 5, root)],
    };
    // One resumed chat per original pane, never merely the executable basename.
    let transcript = root.join("chat; $(literal).jsonl");
    fs::write(&transcript, format!("{}\n", serde_json::json!({"type":"session","version":3,"id":"native-chat","cwd":root,"timestamp":"2026-01-01T00:00:00Z"}))).unwrap();
    first.panes[1].content = PaneContent::Chat {
        spec: ResumeSpec {
            file: transcript,
            provider: Provider::Pi,
            cwd: root.to_string_lossy().into(),
            title: "saved conversation".into(),
            native_id: "native-chat".into(),
            warning: String::new(),
        },
        confidence: Confidence::User,
    };
    let second = SavedWindow {
        id: "@2".into(),
        index: 7,
        name: "Shell window".into(),
        layout: layout("80x24,0,0,21"),
        width: 80,
        height: 24,
        active: false,
        zoomed: false,
        panes: vec![pane(21, 3, root)],
        ..first.clone()
    };
    let session = SavedSession {
        id: "$1".into(),
        name: "project with spaces".into(),
        base_index: 2,
        renumber: false,
        group: String::new(),
        default_shell: "/bin/sh".into(),
        windows: vec![first, second],
    };
    let other = SavedSession {
        id: "$2".into(),
        name: "other-project".into(),
        windows: vec![SavedWindow {
            id: "@3".into(),
            index: 0,
            active: true,
            layout: layout("80x24,0,0,31"),
            panes: vec![pane(31, 3, root)],
            ..session.windows[1].clone()
        }],
        ..session.clone()
    };
    Workspace {
        version: VERSION,
        captured_at: 1_800_000_000_000,
        servers: vec![Server {
            socket: root.join("socket/server").to_string_lossy().into(),
            identity: "77:old".into(),
            sessions: vec![session, other],
        }],
    }
}
fn stub(root: &Path, snapshot: &Workspace) -> Client {
    let config = root.join("config.json");
    let sessions = snapshot.servers[0]
        .sessions
        .iter()
        .map(|s| {
            format!(
                "{}\t{}\t{}\t{}\t{}\t{}\n",
                s.id,
                s.name,
                s.base_index,
                u8::from(s.renumber),
                s.group,
                s.default_shell
            )
        })
        .collect::<String>();
    let windows = snapshot.servers[0]
        .sessions
        .iter()
        .flat_map(|s| {
            s.windows.iter().map(move |w| {
                format!(
                    "{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\n",
                    s.id,
                    w.id,
                    w.index,
                    w.name,
                    w.layout,
                    w.width,
                    w.height,
                    u8::from(w.active),
                    u8::from(w.zoomed),
                    u8::from(w.linked),
                    w.pane_base_index,
                    u8::from(w.automatic_rename),
                    u8::from(w.allow_rename),
                    w.size_policy
                )
            })
        })
        .collect::<String>();
    let panes = snapshot.servers[0]
        .sessions
        .iter()
        .flat_map(|s| {
            s.windows.iter().flat_map(move |w| {
                w.panes.iter().map(move |p| {
                    format!(
                        "{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t111\n",
                        s.id,
                        w.id,
                        p.id,
                        p.index,
                        u8::from(p.active),
                        p.cwd,
                        p.title,
                        p.current_command,
                        p.start_command
                    )
                })
            })
        })
        .collect::<String>();
    fs::write(&config, serde_json::to_vec(&serde_json::json!({"sessions":sessions,"windows":windows,"panes":panes,"live_names":[]})).unwrap()).unwrap();
    let python = Command::new("python3")
        .args(["-c", "import sys; print(sys.executable)"])
        .output()
        .unwrap();
    assert!(python.status.success());
    let path = root.join("fake-tmux");
    fs::write(&path, format!("#!{}\n{}",String::from_utf8(python.stdout).unwrap().trim(), r#"import json, sys
from pathlib import Path
root = Path(__file__).parent
cfg = json.loads((root/'config.json').read_text())
a = sys.argv[1:]
assert a[:1] == ['-S']
socket = a[1]; a = a[2:]
with (root/'commands.jsonl').open('a') as f: f.write(json.dumps(a)+'\n')
state_file=root/'state.json'
state=json.loads(state_file.read_text()) if state_file.exists() else dict(next=100,panes={},windows={},sessions={})
def create(session,index):
    n=state['next']; state['next']+=1
    pane='%'+str(n); window='@'+str(n)
    state['panes'][pane]=dict(pid=1000+n,command='sleep',session=session,window=window)
    state['windows'][window]=dict(session=session,pane=pane,zoomed=0)
    return pane,window,1000+n
if cfg.get('fail_on') == a[0]: sys.exit(1)
if a[0]=='list-sessions':
    print('\n'.join(cfg['live_names']) if a[-1]=='#{session_name}' else cfg['sessions'],end='')
elif a[0]=='list-windows': print(cfg['windows'],end='')
elif a[0]=='list-panes': print(cfg['panes'],end='')
elif a[0]=='display-message':
    if a[-1]=='#{pid}:#{start_time}': print('77:old')
    elif a[-1]=='#{session_id}': print(state['windows'][a[a.index('-t')+1]]['session'])
    elif a[-1].startswith('#{window_layout}'):
        w=state['windows'][a[a.index('-t')+1]]
        print(f"{w['layout']}\t{int(w['width'])+int(bool(cfg.get('wrong_dimensions')))}\t{w['height']}\t{w['zoomed']}")
    else:
        target=a[a.index('-t')+1]; p=state['panes'][target]
        pid=p['pid']+1 if cfg.get('poison_guard') else p['pid']
        print(f"88:new\t{target}\t{pid}\t{p['command']}")
elif a[0]=='new-session':
    assert '-d' in a and a[-2:]==['sleep','86400']
    session='$'+str(len(state['sessions'])+100)
    state['sessions'][session]=a[a.index('-s')+1]
    pane,window,pid=create(session,1)
    print(f'{session}\t{window}\t{pane}\t1\t88:new\t{pid}')
elif a[0]=='new-window':
    assert '-d' in a and a[-2:]==['sleep','86400']
    session,index=a[a.index('-t')+1].split(':')
    pane,window,pid=create(session,index)
    print(f'{session}\t{window}\t{pane}\t{index}\t88:new\t{pid}')
elif a[0]=='split-window':
    assert '-d' in a and a[-2:]==['sleep','86400']
    old=state['panes'][a[a.index('-t')+1]]
    pane,unused,pid=create(old['session'],0)
    state['panes'][pane]['window']=old['window']; del state['windows'][unused]
    print(f'{pane}\t{pid}')
elif a[0]=='respawn-pane':
    assert '-k' in a
    p=state['panes'][a[a.index('-t')+1]]
    assert p['command']=='sleep', 'must never replace a non-placeholder'
    p['command']=a[a.index('-c')+2]; p['pid']+=5000
elif a[0]=='resize-window':
    w=state['windows'][a[a.index('-t')+1]]; w['width']=a[a.index('-x')+1]; w['height']=a[a.index('-y')+1]
elif a[0]=='select-layout': state['windows'][a[a.index('-t')+1]]['layout']=a[-1]
elif a[0]=='resize-pane':
    p=state['panes'][a[a.index('-t')+1]]; state['windows'][p['window']]['zoomed']=1
elif a[0] in ('set-option','set-window-option','move-window','select-pane','rename-window','select-window'): pass
else: raise AssertionError('Forbidden tmux command: '+repr(a))
state_file.write_text(json.dumps(state))
"#)).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
    Client {
        program: path.into_os_string(),
    }
}
fn commands(root: &Path) -> Vec<Vec<String>> {
    fs::read_to_string(root.join("commands.jsonl"))
        .unwrap_or_default()
        .lines()
        .map(|s| serde_json::from_str(s).unwrap())
        .collect()
}
fn configure(root: &Path, key: &str, value: serde_json::Value) {
    let file = root.join("config.json");
    let mut cfg: serde_json::Value = serde_json::from_slice(&fs::read(&file).unwrap()).unwrap();
    cfg[key] = value;
    fs::write(file, serde_json::to_vec(&cfg).unwrap()).unwrap();
}

#[test]
fn mixed_layout_parser_preserves_geometry_and_rewrites_only_pane_ids() {
    let body = "100x30,0,0{49x30,0,0,11,50x30,50,0[50x14,50,0,12,50x15,50,15,13]}";
    let parsed = Layout::parse(&layout(body)).unwrap();
    let mapped = parsed
        .remap(&HashMap::from([(11, 201), (12, 202), (13, 203)]))
        .unwrap();
    assert!(mapped.contains("49x30,0,0,201"));
    assert!(mapped.contains("50x15,50,15,203"));
    Layout::parse(&mapped).unwrap();
    for invalid in [
        "ffff,100x30,0,0,1".to_owned(),
        layout("100x30,0,0{49x30,0,0,1,49x30,50,0,2}"),
        layout("100x30,0,0,1trailing"),
    ] {
        assert!(Layout::parse(&invalid).is_err());
    }
    assert!(parsed.remap(&HashMap::new()).is_err());
}
#[test]
fn recovery_rebuilds_names_indexes_nested_splits_active_and_zoom_without_client_switches() {
    let dir = tempfile::tempdir().unwrap();
    let root = paths::canonical(dir.path());
    let snapshot = fixture(&root);
    let client = stub(&root, &snapshot);
    assert!(
        snapshot
            .blockers(&Catalog::default(), RestoreOptions::default())
            .is_empty()
    );
    let receipt = restore_with(
        &client,
        &root,
        &snapshot,
        &Catalog::default(),
        RestoreOptions::default(),
    )
    .unwrap();
    assert!(
        fs::read_to_string(receipt)
            .unwrap()
            .contains("\"event\":\"complete\"")
    );
    let calls = commands(&root);
    assert_eq!(calls.iter().filter(|a| a[0] == "new-session").count(), 2);
    assert_eq!(calls.iter().filter(|a| a[0] == "new-window").count(), 1);
    assert_eq!(calls.iter().filter(|a| a[0] == "split-window").count(), 2);
    assert_eq!(calls.iter().filter(|a| a[0] == "respawn-pane").count(), 5);
    let splits = calls
        .iter()
        .filter(|a| a[0] == "split-window")
        .collect::<Vec<_>>();
    assert!(splits[0].contains(&"-h".into()));
    assert!(splits[1].contains(&"-v".into()));
    let mapped = calls
        .iter()
        .find(|a| a[0] == "select-layout")
        .unwrap()
        .last()
        .unwrap();
    let remapped = Layout::parse(mapped).unwrap();
    let mut ids = vec![];
    remapped.leaves(&mut ids);
    assert_eq!(ids, vec![100, 101, 102]);
    let resume = calls
        .iter()
        .find(|a| a[0] == "respawn-pane" && a.contains(&"--session".into()))
        .unwrap();
    assert_eq!(
        &resume[resume.len() - 3..],
        &[
            "pi".to_owned(),
            "--session".into(),
            root.join("chat; $(literal).jsonl")
                .to_string_lossy()
                .into_owned()
        ]
    );
    assert!(
        calls
            .iter()
            .any(|a| a[0] == "new-window" && a.contains(&"$100:7".into()))
    );
    assert!(
        calls
            .iter()
            .any(|a| a[0] == "move-window" && a.contains(&"$100:2".into()))
    );
    assert!(
        calls
            .iter()
            .any(|a| a[0] == "rename-window" && a.last() == Some(&"API ; $(literal) 界".into()))
    );
    assert!(
        calls
            .iter()
            .any(|a| a[0] == "resize-pane" && a.contains(&"-Z".into()))
    );
    for call in calls {
        assert!(
            !call[0].starts_with("kill-")
                && !matches!(
                    call[0].as_str(),
                    "send-keys" | "switch-client" | "attach-session"
                )
        );
    }
}
#[test]
fn collision_preflight_and_unknown_chats_never_create_or_overwrite_anything() {
    let dir = tempfile::tempdir().unwrap();
    let root = paths::canonical(dir.path());
    let mut snapshot = fixture(&root);
    let client = stub(&root, &snapshot);
    fs::create_dir(root.join("socket")).unwrap();
    fs::write(&snapshot.servers[0].socket, "").unwrap();
    configure(
        &root,
        "live_names",
        serde_json::json!(["project with spaces"]),
    );
    assert!(
        restore_with(
            &client,
            &root,
            &snapshot,
            &Catalog::default(),
            RestoreOptions::default()
        )
        .unwrap_err()
        .to_string()
        .contains("already exists")
    );
    assert!(!commands(&root).iter().any(|a| a[0].starts_with("new-")));
    snapshot.servers[0].sessions[0].windows[0].panes[0].content = PaneContent::Unresolved {
        reason: "unsaved chat".into(),
    };
    assert!(
        !snapshot
            .blockers(&Catalog::default(), RestoreOptions::default())
            .is_empty()
    );
    assert!(
        snapshot
            .blockers(
                &Catalog::default(),
                RestoreOptions {
                    layout_only: true,
                    ..Default::default()
                }
            )
            .is_empty()
    );
}
#[test]
fn changed_placeholder_and_mid_restore_failures_never_respawn_other_processes() {
    for (key, value) in [
        ("poison_guard", serde_json::json!(true)),
        ("fail_on", serde_json::json!("select-layout")),
    ] {
        let dir = tempfile::tempdir().unwrap();
        let root = paths::canonical(dir.path());
        let snapshot = fixture(&root);
        let client = stub(&root, &snapshot);
        configure(&root, key, value);
        let error = restore_with(
            &client,
            &root,
            &snapshot,
            &Catalog::default(),
            RestoreOptions::default(),
        )
        .unwrap_err();
        assert!(format!("{error:#}").contains("journal"));
        assert!(!commands(&root).iter().any(|a| a[0] == "respawn-pane"));
    }
}
#[test]
fn final_geometry_must_be_verified_not_merely_requested() {
    let dir = tempfile::tempdir().unwrap();
    let root = paths::canonical(dir.path());
    let snapshot = fixture(&root);
    let client = stub(&root, &snapshot);
    configure(&root, "wrong_dimensions", serde_json::json!(true));
    let error = restore_with(
        &client,
        &root,
        &snapshot,
        &Catalog::default(),
        RestoreOptions::default(),
    )
    .unwrap_err();
    assert!(format!("{error:#}").contains("exact recovery could not be verified"));
}

#[test]
fn command_replay_and_title_only_identity_need_separate_authorization() {
    let dir = tempfile::tempdir().unwrap();
    let root = paths::canonical(dir.path());
    let mut snapshot = fixture(&root);
    snapshot.servers[0].sessions[0].windows[0].panes[0].content = PaneContent::Command {
        command: "python3 -m http.server 8768".into(),
    };
    if let PaneContent::Chat { confidence, .. } =
        &mut snapshot.servers[0].sessions[0].windows[0].panes[1].content
    {
        *confidence = Confidence::Title;
    }
    let issues = snapshot.blockers(&Catalog::default(), RestoreOptions::default());
    assert_eq!(issues.len(), 2);
    assert!(
        snapshot
            .blockers(
                &Catalog::default(),
                RestoreOptions {
                    commands: true,
                    allow_probable: true,
                    layout_only: false
                }
            )
            .is_empty()
    );
    let client = stub(&root, &snapshot);
    restore_with(
        &client,
        &root,
        &snapshot,
        &Catalog::default(),
        RestoreOptions {
            layout_only: true,
            ..Default::default()
        },
    )
    .unwrap();
    assert!(
        commands(&root)
            .iter()
            .filter(|a| a[0] == "respawn-pane")
            .all(|a| a.last().is_some_and(|v| v == "-l"))
    );
}
#[test]
fn capture_is_read_only_and_rejects_ambiguous_bindings_and_inconsistent_geometry() {
    let dir = tempfile::tempdir().unwrap();
    let root = paths::canonical(dir.path());
    let snapshot = fixture(&root);
    let client = stub(&root, &snapshot);
    let saved = capture_with(
        &client,
        &[snapshot.servers[0].socket.clone()],
        &Catalog::default(),
        None,
        &[],
    )
    .unwrap();
    assert_eq!(saved.counts(), (2, 3, 5));
    assert_eq!(
        saved.servers[0].sessions[0].windows[0].name,
        "API ; $(literal) 界"
    );
    assert!(commands(&root).iter().all(|a| matches!(
        a[0].as_str(),
        "display-message" | "list-sessions" | "list-windows" | "list-panes"
    )));
    assert!(
        capture_with(
            &client,
            &[snapshot.servers[0].socket.clone()],
            &Catalog::default(),
            None,
            &["%999=missing".into()]
        )
        .is_err()
    );
    let mut broken = snapshot.clone();
    broken.servers[0].sessions[0].windows[0].panes.pop();
    assert!(broken.validate().is_err());
}
#[test]
fn explicit_chat_binding_survives_checkpoints_but_not_process_reuse() {
    let dir = tempfile::tempdir().unwrap();
    let root = paths::canonical(dir.path());
    let snapshot = fixture(&root);
    let client = stub(&root, &snapshot);
    let PaneContent::Chat { spec, .. } =
        &snapshot.servers[0].sessions[0].windows[0].panes[1].content
    else {
        panic!("missing fixture chat")
    };
    let opening = tmux::PaneIdentity {
        socket: snapshot.servers[0].socket.clone(),
        server: "77:old".into(),
        pane: "%11".into(),
        pane_pid: 111,
        pane_start: "start".into(),
        client_pid: 222,
        client_start: "start".into(),
        target: "project:2.3".into(),
        session: "project with spaces".into(),
        window: "2".into(),
        cwd: root.to_string_lossy().into(),
        command: "pi".into(),
        title: "pane 11; title".into(),
        provider: Some(Provider::Pi),
        activity: None,
        activity_evidence: None,
    };
    let saved = Session {
        id: "chosen".into(),
        metadata: Metadata {
            native_id: spec.native_id.clone(),
            file: spec.file.clone(),
            cwd: spec.cwd.clone(),
            ..Default::default()
        },
        state: ResumeState::Resume,
        note: String::new(),
        done_revision: 0,
        done_at: 0,
        available: true,
        presence_verified: false,
        bindings: vec![],
        probable: vec![],
    };
    let mut catalog = Catalog {
        sessions: vec![saved],
        unbound: vec![opening],
        ..Default::default()
    };
    let sockets = vec![snapshot.servers[0].socket.clone()];
    let unknown = capture_with(&client, &sockets, &catalog, None, &[]).unwrap();
    assert!(matches!(
        unknown.servers[0].sessions[0].windows[0].panes[0].content,
        PaneContent::Unresolved { .. }
    ));
    let assigned = capture_with(&client, &sockets, &catalog, None, &["%11=chosen".into()]).unwrap();
    assert!(matches!(
        assigned.servers[0].sessions[0].windows[0].panes[0].content,
        PaneContent::Chat {
            confidence: Confidence::User,
            ..
        }
    ));
    // Even if the next catalog also finds a title-only hint, preserve the
    // stronger explicit user assignment instead of downgrading its confidence.
    catalog.sessions[0]
        .probable
        .push(catalog.unbound[0].clone());
    let next = capture_with(&client, &sockets, &catalog, Some(&assigned), &[]).unwrap();
    assert!(matches!(
        next.servers[0].sessions[0].windows[0].panes[0].content,
        PaneContent::Chat {
            confidence: Confidence::User,
            ..
        }
    ));
    catalog.unbound[0].client_start = "reused process".into();
    catalog.sessions[0].probable.clear();
    let changed = capture_with(&client, &sockets, &catalog, Some(&assigned), &[]).unwrap();
    assert!(matches!(
        changed.servers[0].sessions[0].windows[0].panes[0].content,
        PaneContent::Unresolved { .. }
    ));
}

#[test]
fn unobservable_cwd_keeps_the_geometry_but_blocks_launch_without_guessing() {
    let dir = tempfile::tempdir().unwrap();
    let root = paths::canonical(dir.path());
    let mut snapshot = fixture(&root);
    snapshot.servers[0].sessions[0].windows[0].panes[0]
        .cwd
        .clear();
    let client = stub(&root, &snapshot);
    let captured = capture_with(
        &client,
        &[snapshot.servers[0].socket.clone()],
        &Catalog::default(),
        None,
        &[],
    )
    .unwrap();
    assert_eq!(captured.counts(), (2, 3, 5));
    save(&root, &captured).unwrap();
    assert_eq!(load(&root, None).unwrap().counts(), (2, 3, 5));
    assert!(
        captured
            .blockers(&Catalog::default(), RestoreOptions::default())
            .iter()
            .any(|s| s.contains("directory is unavailable"))
    );
    assert!(
        restore_with(
            &client,
            &root,
            &captured,
            &Catalog::default(),
            RestoreOptions::default()
        )
        .is_err()
    );
    assert!(!commands(&root).iter().any(|a| a[0].starts_with("new-")));
}

#[test]
fn workspace_lock_serializes_recovery_and_checkpoint_publication() {
    let dir = tempfile::tempdir().unwrap();
    let root = paths::canonical(dir.path());
    let snapshot = fixture(&root);
    let client = stub(&root, &snapshot);
    let held = workspace_lock(&root).unwrap();
    assert!(save(&root, &snapshot).is_err());
    assert!(
        restore_with(
            &client,
            &root,
            &snapshot,
            &Catalog::default(),
            RestoreOptions::default()
        )
        .is_err()
    );
    assert!(commands(&root).is_empty());
    drop(held);
    assert!(save(&root, &snapshot).is_ok());
}

#[test]
fn checkpoints_are_private_atomic_archived_and_empty_discovery_keeps_the_last_one() {
    let dir = tempfile::tempdir().unwrap();
    let root = paths::canonical(dir.path());
    let mut snapshot = fixture(&root);
    save(&root, &snapshot).unwrap();
    assert_eq!(load(&root, None).unwrap(), snapshot);
    let latest = root.join("workspace/latest.json");
    assert_eq!(
        fs::metadata(&latest).unwrap().permissions().mode() & 0o777,
        0o600
    );
    let empty = Workspace {
        servers: vec![],
        ..snapshot.clone()
    };
    assert!(save(&root, &empty).unwrap().is_none());
    assert_eq!(load(&root, None).unwrap(), snapshot);
    let foreign = root.join("workspace/user-file.json");
    fs::write(&foreign, "must be preserved").unwrap();
    for i in 0..25 {
        snapshot.captured_at += 1;
        snapshot.servers[0].sessions[0].windows[0].name = format!("changed {i}");
        save(&root, &snapshot).unwrap();
    }
    assert_eq!(checkpoints(&root).unwrap().len(), KEEP_CHECKPOINTS);
    assert_eq!(fs::read_to_string(foreign).unwrap(), "must be preserved");
    fs::remove_file(&latest).unwrap();
    std::os::unix::fs::symlink(root.join("do-not-touch"), &latest).unwrap();
    assert!(save(&root, &snapshot).is_err());
    assert!(load(&root, None).is_err());
}
