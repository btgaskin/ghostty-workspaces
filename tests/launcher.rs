//! Exercise the real executable and scoped lifecycle hook transport with a local fake CLI.
//! No authentication or model/provider requests are made.
use ghostty_workspaces::{engine, model::Store};
use serde_json::{Value, json};
use std::{fs, process::Command};
use uuid::Uuid;
#[test]
fn launcher_records_exact_session_and_subagents_through_scoped_hooks() {
    let dir = tempfile::tempdir().unwrap();
    let bin = dir.path().join("bin");
    fs::create_dir(&bin).unwrap();
    let fake = bin.join("claude");
    fs::write(&fake,r#"#!/usr/bin/env python3
import sys,json,subprocess,uuid
args=sys.argv[1:]
settings=json.loads(args[args.index('--settings')+1])
session=args[args.index('--session-id')+1]
for event,extra in [('SessionStart',{'source':'startup'}),('SubagentStart',{'agent_id':'worker-1','agent_type':'test'}),('SubagentStart',{'agent_id':'worker-1','agent_type':'test'}),('SubagentStop',{'agent_id':'worker-1'}),('Stop',{}),('SessionEnd',{})]:
    payload={'hook_event_name':event,'session_id':session,**extra}
    command=settings['hooks'][event][0]['hooks'][0]['command']
    subprocess.run(command,shell=True,input=json.dumps(payload),text=True,check=True)
"#).unwrap();
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(&fake, fs::Permissions::from_mode(0o755)).unwrap();
    let id = Uuid::new_v4();
    fs::write(dir.path().join("workspaces.json"),json!({"version":1,"entries":[{"id":id,"workspace":"test","name":"fixture","cwd":dir.path(),"agent":"claude","session_id":null}],"runs":{}}).to_string()).unwrap();
    let path = std::env::join_paths([
        bin,
        std::path::PathBuf::from("/usr/bin"),
        std::path::PathBuf::from("/bin"),
    ])
    .unwrap();
    let store = Store::new(Some(dir.path().into())).unwrap();
    let token = engine::authorize_launch(&store, id).unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_gws"))
        .args([
            "--state-dir",
            dir.path().to_str().unwrap(),
            "run",
            &id.to_string(),
            &token.to_string(),
        ])
        .env("PATH", path)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let state: Value = serde_json::to_value(store.read().unwrap()).unwrap();
    assert_eq!(state["entries"][0]["session_id"], token.to_string());
    assert_eq!(state["runs"][id.to_string()]["agents_started"], 1);
    assert_eq!(state["runs"][id.to_string()]["ended"], true);
    assert_eq!(state["runs"][id.to_string()]["exit_code"], 0);
    assert_eq!(state["runs"][id.to_string()]["hooks_seen"], true);
    assert_eq!(state["runs"][id.to_string()]["completed_turns"], 1);
    assert_eq!(state["runs"][id.to_string()]["seen_turns"], 0);
    assert_eq!(state["runs"][id.to_string()]["exit_seen"], false);
}

#[test]
fn codex_launch_transports_quoted_hook_command_and_exact_thread_identity() {
    let dir = tempfile::Builder::new()
        .prefix("gws ' spaced ")
        .tempdir()
        .unwrap();
    let bin = dir.path().join("bin");
    fs::create_dir(&bin).unwrap();
    let fake = bin.join("codex");
    fs::write(&fake, r#"#!/usr/bin/env python3
import sys,json,subprocess,uuid,re,pathlib
args=sys.argv[1:]
configs=[args[i+1] for i,v in enumerate(args) if v=='-c']
start_config=next(c for c in configs if c.startswith('hooks.SessionStart='))
match=re.fullmatch(r'hooks.SessionStart=\[\{hooks=\[\{type="command",command=(.*),timeout=2\}\]\}\]',start_config)
assert match is not None, start_config
start=json.loads(match.group(1))
thread='00000000-0000-4000-8000-000000000001'
transcript=pathlib.Path.cwd()/'fixture.jsonl'
transcript.write_text(json.dumps({'type':'session_meta','payload':{'id':thread,'source':'cli'}})+'\n')
payload={'hook_event_name':'SessionStart','session_id':str(uuid.uuid4()),'transcript_path':str(transcript),'source':'startup'}
subprocess.run(start,shell=True,input=json.dumps(payload),text=True,check=True)
"#).unwrap();
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(&fake, fs::Permissions::from_mode(0o755)).unwrap();
    let id = Uuid::new_v4();
    fs::write(dir.path().join("workspaces.json"),json!({"version":1,"entries":[{"id":id,"workspace":"test","name":"fixture","cwd":dir.path(),"agent":"codex","session_id":null}],"runs":{}}).to_string()).unwrap();
    let path = std::env::join_paths([
        bin,
        std::path::PathBuf::from("/usr/bin"),
        std::path::PathBuf::from("/bin"),
    ])
    .unwrap();
    let store = Store::new(Some(dir.path().into())).unwrap();
    let token = engine::authorize_launch(&store, id).unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_gws"))
        .args([
            "--state-dir",
            dir.path().to_str().unwrap(),
            "run",
            &id.to_string(),
            &token.to_string(),
        ])
        .env("PATH", path)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let state: Value = serde_json::to_value(store.read().unwrap()).unwrap();
    assert_eq!(
        state["entries"][0]["session_id"],
        "00000000-0000-4000-8000-000000000001"
    );
    assert_ne!(state["entries"][0]["session_id"], token.to_string());
}

#[test]
fn provider_gate_does_not_execute_before_release_and_aborts_on_parent_loss() {
    use std::io::Write;
    use std::os::fd::FromRawFd;
    for release in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("provider-executed");
        let mut fds = [0; 2];
        assert_eq!(unsafe { libc::pipe(fds.as_mut_ptr()) }, 0);
        let read = unsafe { std::fs::File::from_raw_fd(fds[0]) };
        let mut writer = unsafe { std::fs::File::from_raw_fd(fds[1]) };
        unsafe {
            libc::fcntl(fds[1], libc::F_SETFD, libc::FD_CLOEXEC);
        }
        let mut child = Command::new(env!("CARGO_BIN_EXE_gws"))
            .args([
                "exec-gate",
                &fds[0].to_string(),
                "--",
                "/usr/bin/touch",
                marker.to_str().unwrap(),
            ])
            .spawn()
            .unwrap();
        drop(read);
        std::thread::sleep(std::time::Duration::from_millis(100));
        assert!(!marker.exists());
        assert!(child.try_wait().unwrap().is_none());
        if release {
            writer.write_all(&[1]).unwrap();
        }
        drop(writer);
        let status = child.wait().unwrap();
        assert_eq!(marker.exists(), release);
        assert_eq!(status.code(), Some(if release { 0 } else { 125 }));
    }
}

#[test]
fn planned_restore_resumes_the_existing_standby_through_its_socket() {
    use ghostty_workspaces::{
        model::{Agent, Entry, Lease},
        operations::{self, Action},
        process, supervisor,
    };
    use std::os::{
        fd::FromRawFd,
        unix::{fs::PermissionsExt, process::CommandExt},
    };
    use std::time::{Duration, Instant};
    let dir = tempfile::tempdir().unwrap();
    let script = dir.path().join("provider");
    let count = dir.path().join("count");
    fs::write(
        &script,
        format!("#!/bin/sh\nprintf x >> '{}'\n", count.display()),
    )
    .unwrap();
    fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
    let store = Store::new(Some(dir.path().into())).unwrap();
    let id = Uuid::new_v4();
    store
        .update(|s| {
            s.entries.push(Entry {
                id,
                name: "PTY fixture".into(),
                cwd: dir.path().into(),
                agent: Agent::Command,
                command: vec![script.to_string_lossy().into_owned()],
                ..Default::default()
            });
            Ok(())
        })
        .unwrap();
    let token = engine::authorize_launch(&store, id).unwrap();
    let mut master = -1;
    let mut slave = -1;
    assert_eq!(
        unsafe {
            libc::openpty(
                &mut master,
                &mut slave,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            )
        },
        0
    );
    unsafe {
        libc::fcntl(master, libc::F_SETFD, libc::FD_CLOEXEC);
        libc::fcntl(slave, libc::F_SETFD, libc::FD_CLOEXEC);
    }
    let master_file = unsafe { fs::File::from_raw_fd(master) };
    let slave_file = unsafe { fs::File::from_raw_fd(slave) };
    let mut command = Command::new(env!("CARGO_BIN_EXE_gws"));
    command
        .args([
            "--state-dir",
            dir.path().to_str().unwrap(),
            "run",
            &id.to_string(),
            &token.to_string(),
        ])
        .stdin(slave_file.try_clone().unwrap())
        .stdout(slave_file.try_clone().unwrap())
        .stderr(slave_file);
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() < 0 {
                return Err(std::io::Error::last_os_error());
            }
            libc::ioctl(0, libc::TIOCSCTTY as _, 0);
            libc::tcsetpgrp(0, libc::getpgrp());
            Ok(())
        });
    }
    let child = command.spawn().unwrap();
    struct Owned(std::process::Child);
    impl Drop for Owned {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    let mut child = Owned(child);
    let deadline = Instant::now() + Duration::from_secs(8);
    loop {
        if store.read().unwrap().runs[&id].ended {
            break;
        }
        assert!(Instant::now() < deadline);
        assert!(child.0.try_wait().unwrap().is_none());
        std::thread::sleep(Duration::from_millis(25));
    }
    assert!(supervisor::request(token, "unsupported-action").is_err());
    let plan =
        operations::preview_human(&store, Action::Start, &[id], &[], None, false, None, None)
            .unwrap();
    operations::accept(&store, plan.id, false).unwrap();
    let pid = std::process::id();
    let start = process::start_time(pid).unwrap();
    store
        .update(|s| {
            s.entries[0].lease = Some(Lease {
                operation: plan.id,
                pid,
                start,
            });
            Ok(())
        })
        .unwrap();
    supervisor::request(token, &format!("resume:{}", plan.id)).unwrap();
    let deadline = Instant::now() + Duration::from_secs(8);
    loop {
        if fs::read(&count).is_ok_and(|v| v.len() == 2) {
            break;
        }
        assert!(Instant::now() < deadline);
        assert!(child.0.try_wait().unwrap().is_none());
        std::thread::sleep(Duration::from_millis(25));
    }
    assert_ne!(store.read().unwrap().runs[&id].token, token);
    assert!(
        operations::operation(&store, plan.id).unwrap().steps[0]
            .substeps
            .iter()
            .any(|s| s.starts_with("launched_run:"))
    );
    drop(master_file);
}
