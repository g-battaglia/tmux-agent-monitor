//! End-to-end recovery against disposable storage and strict stub executables.
//! Neither tmux nor any provider CLI from the user's machine can be executed.
use std::{fs, os::unix::fs::PermissionsExt, path::Path, process::Command};
use tmux_agent_monitor::{model::ResumeState, pi, store::Store};

#[test]
fn restore_previews_then_starts_all_marked_chats_without_touching_clients() {
    let temp = tempfile::tempdir().unwrap();
    let root = fs::canonicalize(temp.path()).unwrap();
    let bin = root.join("bin");
    let source = root.join("sessions");
    let project = root.join("project with spaces");
    for dir in [&bin, &source, &project] {
        fs::create_dir(dir).unwrap();
    }
    let python = Command::new("python3")
        .args(["-c", "import sys; print(sys.executable)"])
        .output()
        .unwrap();
    assert!(python.status.success());
    let shebang = format!("#!{}\n", String::from_utf8(python.stdout).unwrap().trim());
    let write_stub = |name: &str, body: &str| {
        let path = bin.join(name);
        fs::write(&path, format!("{shebang}{body}")).unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
    };
    write_stub(
        "ps",
        r#"import json, os
from pathlib import Path
root = Path(os.environ['TEST_ROOT'])
for pane in json.loads((root/'panes.json').read_text()) if (root/'panes.json').exists() else []:
    print(f"{pane['pid']} 1 {os.getuid()} Mon Jan 1 00:00:00 2024 sh")
    print(f"{pane['pid']+1} {pane['pid']} {os.getuid()} Mon Jan 1 00:00:00 2024 pi")
"#,
    );
    write_stub(
        "tmux",
        r#"import json, os, socket, sys
from pathlib import Path
root = Path(os.environ['TEST_ROOT'])
args = sys.argv[1:]
if args[:1] == ['-S']: args = args[2:]
with (root/'commands.jsonl').open('a') as log: log.write(json.dumps(args)+'\n')
panes_file = root/'panes.json'
panes = json.loads(panes_file.read_text()) if panes_file.exists() else []
sock = root/'tmux'/('tmux-'+str(os.getuid()))/'default'
if args[0] == 'display-message': print('1:fixture')
elif args[0] == 'list-panes':
    for p in panes:
        if args[-1] == '#{session_id}\t#{pane_current_path}': print('$1\t'+p['cwd'])
        else: print(f"{p['pane']}\t{p['pid']}\tsh\t{p['cwd']}\trecovery\t1\trecovery:1.0\tpi:c")
elif args[0] == 'capture-pane': print('>')
elif args[0] in ('new-session', 'new-window'):
    assert '-d' in args
    # Log the intended argv; deliberately NEVER execute it.
    index = len(panes)+1
    p = dict(pane='%'+str(index), pid=1000+index*10, cwd=args[args.index('-c')+1])
    panes.append(p)
    panes_file.write_text(json.dumps(panes))
    if not sock.exists():
        sock.parent.mkdir(parents=True, exist_ok=True)
        s = socket.socket(socket.AF_UNIX); s.bind(str(sock)); s.close()
    print(f"{p['pane']}\t$1\t{sock}\t1:fixture")
else: raise AssertionError('Forbidden tmux command: '+repr(args))
"#,
    );
    // Fail closed even if production accidentally tries to execute a provider.
    for provider in ["pi", "claude", "codex", "opencode"] {
        write_stub(
            provider,
            "raise AssertionError('A real agent must never be started by this test')\n",
        );
    }
    let store = Store::open(&root.join("state")).unwrap();
    store.add_source(&source).unwrap();
    let src = store.sources().unwrap().remove(0);
    for (name, state) in [
        ("one", ResumeState::Resume),
        ("two", ResumeState::Resume),
        ("history", ResumeState::History),
        ("done", ResumeState::Done),
        ("missing", ResumeState::Resume),
    ] {
        let file = source.join(format!("{name}; fixture.jsonl"));
        fs::write(
            &file,
            format!(
                "{}\n",
                serde_json::json!({
                    "type":"session", "version":3, "id":name, "cwd":project,
                    "timestamp":"2026-01-01T00:00:00Z"
                })
            ),
        )
        .unwrap();
        let (cursor, _) = pi::scan(&file, None).unwrap();
        store.ingest(&src, &cursor).unwrap();
        store.change(name, Some(state), None).unwrap();
        if name == "missing" {
            fs::remove_file(file).unwrap();
        }
    }
    let run = |args: &[&str]| {
        let result = Command::new(env!("CARGO_BIN_EXE_tmux-agent-monitor"))
            .env_clear()
            .env(
                "PATH",
                format!("{}:{}", bin.display(), std::env::var("PATH").unwrap()),
            )
            .env("HOME", root.join("home"))
            .env("PI_CODING_AGENT_DIR", root.join("profile"))
            .env("PI_CODING_AGENT_SESSION_DIR", &source)
            .env("TMUX_TMPDIR", root.join("tmux"))
            .env("TEST_ROOT", &root)
            .args(["--data-dir", root.join("state").to_str().unwrap()])
            .args(args)
            .output()
            .unwrap();
        assert!(
            result.status.success(),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
        String::from_utf8(result.stdout).unwrap()
    };
    let plan: serde_json::Value =
        serde_json::from_str(&run(&["restore", "--marked", "--json"])).unwrap();
    let entries = plan.as_array().unwrap();
    assert_eq!(entries.len(), 3);
    assert_eq!(
        entries
            .iter()
            .filter(|e| e["status"]["kind"] == "ready")
            .count(),
        2
    );
    assert!(
        !root.join("panes.json").exists(),
        "JSON preview must not create openings"
    );
    assert!(run(&["restore", "--marked"]).contains("Preview only"));
    assert!(!root.join("panes.json").exists());
    let output = run(&[
        "restore",
        "--marked",
        "--project",
        project.to_str().unwrap(),
        "--resume",
    ]);
    assert!(output.contains("2 restored, 0 failed"), "{output}");
    let commands = fs::read_to_string(root.join("commands.jsonl")).unwrap();
    let commands: Vec<Vec<String>> = commands
        .lines()
        .map(|s| serde_json::from_str(s).unwrap())
        .collect();
    let creates: Vec<_> = commands
        .iter()
        .filter(|a| a[0].starts_with("new-"))
        .collect();
    assert_eq!(
        creates.len(),
        2,
        "both chats must restore, even before their bridges report"
    );
    for args in creates {
        assert!(args.contains(&"-d".into()));
        assert!(args.windows(2).any(|w| w == ["pi", "--session"]));
        assert_eq!(
            Path::new(args.last().unwrap()).parent(),
            Some(source.as_path())
        );
    }
    for args in commands {
        assert!(!matches!(
            args[0].as_str(),
            "attach-session" | "switch-client" | "select-pane" | "send-keys"
        ));
        assert!(!args[0].starts_with("kill-"));
    }
    // A new invocation must NOT mistake existing unidentified panes for its own.
    let again = run(&["restore", "--marked", "--resume"]);
    assert!(again.contains("0 restored"), "{again}");
    assert_eq!(
        serde_json::from_str::<Vec<serde_json::Value>>(
            &fs::read_to_string(root.join("panes.json")).unwrap()
        )
        .unwrap()
        .len(),
        2
    );
    assert_eq!(store.resolve("one").unwrap().state, ResumeState::Resume);
    assert_eq!(store.resolve("two").unwrap().state, ResumeState::Resume);
}
