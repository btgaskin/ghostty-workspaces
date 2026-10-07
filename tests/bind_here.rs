use ghostty_workspaces::model::{Agent, Entry, Run, Store};
use serde_json::{Value, json};
use std::{fs, process::Command};
use uuid::Uuid;
fn fixture() -> (tempfile::TempDir, Store, Uuid, Uuid) {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("codex");
    fs::create_dir_all(home.join("sessions")).unwrap();
    let session = Uuid::new_v4();
    fs::write(
        home.join("sessions/root.jsonl"),
        json!({"type":"session_meta","payload":{"id":session,"cwd":dir.path(),"source":"cli"}})
            .to_string(),
    )
    .unwrap();
    let store = Store::new(Some(dir.path().join("state"))).unwrap();
    let id = Uuid::new_v4();
    store
        .update(|s| {
            s.entries.push(Entry {
                id,
                name: "codex --no-daemon".into(),
                cwd: dir.path().into(),
                agent: Agent::Codex,
                imported: true,
                ..Default::default()
            });
            Ok(())
        })
        .unwrap();
    (dir, store, id, session)
}
fn command(dir: &std::path::Path, store: &Store, session: Uuid) -> Command {
    let mut c = Command::new(env!("CARGO_BIN_EXE_gws"));
    c.args([
        "--state-dir",
        store.dir.to_str().unwrap(),
        "--json",
        "bind-here",
    ])
    .env("CODEX_HOME", dir.join("codex"))
    .env("CODEX_THREAD_ID", session.to_string())
    .env_remove("CODEX_SESSION_ID")
    .env_remove("GWS_ITEM_ID")
    .env_remove("GWS_RUN_ID");
    c
}
#[test]
fn explicit_item_binds_verified_metadata_and_dry_run_does_not_mutate() {
    let (dir, store, id, session) = fixture();
    let result = command(dir.path(), &store, session)
        .args(["--item", &id.to_string(), "--dry-run"])
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert!(store.read().unwrap().entry(id).unwrap().needs_session());
    let result = command(dir.path(), &store, session)
        .args(["--item", &id.to_string()])
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let s = store.read().unwrap();
    let e = s.entry(id).unwrap();
    assert_eq!(e.session_id, Some(session));
    assert!(e.session_verified);
    assert!(e.provider_home.is_some());
    assert!(e.transcript_path.is_some());
    let receipt: Value = serde_json::from_slice(&result.stdout).unwrap();
    assert_eq!(receipt["state"], "bound");
}
#[test]
fn subagent_metadata_and_unknown_context_refuse_without_mutation() {
    let (dir, store, id, session) = fixture();
    fs::write(dir.path().join("codex/sessions/root.jsonl"),json!({"type":"session_meta","payload":{"id":session,"cwd":dir.path(),"source":{"subagent":{"thread_spawn":{}}}}}).to_string()).unwrap();
    assert!(
        !command(dir.path(), &store, session)
            .args(["--item", &id.to_string()])
            .output()
            .unwrap()
            .status
            .success()
    );
    assert!(store.read().unwrap().entry(id).unwrap().needs_session());
    assert!(
        !command(dir.path(), &store, session)
            .env_remove("CODEX_THREAD_ID")
            .args(["--item", &id.to_string()])
            .output()
            .unwrap()
            .status
            .success()
    );
}
#[test]
fn unrelated_live_context_cannot_rebind_same_project() {
    let (dir, store, id, session) = fixture();
    let mut unrelated = Command::new("/bin/sleep").arg("15").spawn().unwrap();
    let token = Uuid::new_v4();
    store
        .update(|s| {
            let e = s.entries.iter_mut().find(|e| e.id == id).unwrap();
            e.session_id = Some(Uuid::new_v4());
            e.session_verified = true;
            s.runs.insert(
                id,
                Run {
                    token,
                    pid: unrelated.id(),
                    start_time: ghostty_workspaces::process::start_time(unrelated.id()).unwrap(),
                    ..Default::default()
                },
            );
            Ok(())
        })
        .unwrap();
    let original = store.read().unwrap().entry(id).unwrap().session_id;
    let result = command(dir.path(), &store, session)
        .env("GWS_ITEM_ID", id.to_string())
        .env("GWS_RUN_ID", token.to_string())
        .output()
        .unwrap();
    unrelated.kill().unwrap();
    unrelated.wait().unwrap();
    assert!(!result.status.success());
    assert_eq!(
        store.read().unwrap().entry(id).unwrap().session_id,
        original
    );
}
#[test]
fn duplicate_unsaved_titles_are_not_chosen_by_folder() {
    let (dir, store, _, session) = fixture();
    store
        .update(|s| {
            let mut e = s.entries[0].clone();
            e.id = Uuid::new_v4();
            s.entries.push(e);
            Ok(())
        })
        .unwrap();
    let result = command(dir.path(), &store, session).output().unwrap();
    assert!(!result.status.success());
    assert!(
        store
            .read()
            .unwrap()
            .entries
            .iter()
            .all(|e| e.needs_session())
    );
}

#[test]
fn managed_provider_binds_here_with_inherited_custom_store_and_keeps_run_identity() {
    use std::os::unix::fs::PermissionsExt;
    let (dir, store, id, session) = fixture();
    store
        .update(|s| {
            s.entries[0].imported = false;
            Ok(())
        })
        .unwrap();
    let bin = dir.path().join("bin");
    fs::create_dir(&bin).unwrap();
    let fake = bin.join("codex");
    fs::write(&fake, r#"#!/usr/bin/env python3
import os, subprocess, json, pathlib
result = subprocess.run([os.environ['GWS_TEST_BIN'], '--json', 'bind-here'], capture_output=True, text=True)
pathlib.Path('bind-result.json').write_text(json.dumps({'code':result.returncode,'stdout':result.stdout,'stderr':result.stderr,'run':os.environ['GWS_RUN_ID']}))
raise SystemExit(result.returncode)
"#).unwrap();
    fs::set_permissions(&fake, fs::Permissions::from_mode(0o755)).unwrap();
    let token = ghostty_workspaces::engine::authorize_launch(&store, id).unwrap();
    let path = std::env::join_paths([bin, "/usr/bin".into(), "/bin".into()]).unwrap();
    let result = Command::new(env!("CARGO_BIN_EXE_gws"))
        .args([
            "--state-dir",
            store.dir.to_str().unwrap(),
            "run",
            &id.to_string(),
            &token.to_string(),
        ])
        .env("PATH", path)
        .env("GWS_TEST_BIN", env!("CARGO_BIN_EXE_gws"))
        .env("CODEX_HOME", dir.path().join("codex"))
        .env("CODEX_THREAD_ID", session.to_string())
        .env_remove("GWS_ITEM_ID")
        .env_remove("GWS_RUN_ID")
        .env_remove("GWS_STATE_DIR")
        .output()
        .unwrap();
    let receipt: Value =
        serde_json::from_slice(&fs::read(dir.path().join("bind-result.json")).unwrap()).unwrap();
    assert!(result.status.success(), "{receipt}");
    assert_eq!(receipt["run"], token.to_string());
    let state = store.read().unwrap();
    assert_eq!(state.runs[&id].token, token);
    assert_eq!(state.entry(id).unwrap().session_id, Some(session));
    assert!(state.entry(id).unwrap().session_verified);
}

#[test]
fn explicit_item_cannot_duplicate_another_live_invoking_owner() {
    let (dir, store, owner, session) = fixture();
    let other = Uuid::new_v4();
    store
        .update(|s| {
            let mut entry = s.entries[0].clone();
            entry.id = other;
            s.entries.push(entry);
            s.runs.insert(
                owner,
                Run {
                    token: Uuid::new_v4(),
                    pid: std::process::id(),
                    start_time: ghostty_workspaces::process::start_time(std::process::id())
                        .unwrap(),
                    ..Default::default()
                },
            );
            Ok(())
        })
        .unwrap();
    let output = command(dir.path(), &store, session)
        .args(["--item", &other.to_string()])
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("already owned"));
    let state = store.read().unwrap();
    assert!(state.entry(other).unwrap().needs_session());
    assert!(!state.runs.contains_key(&other));
}
