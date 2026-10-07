//! Exercise the real executable and scoped lifecycle hook transport with a local fake CLI.
//! No authentication or model/provider requests are made.
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
for event,extra in [('SessionStart',{'source':'startup'}),('SubagentStart',{'agent_id':'worker-1','agent_type':'test'}),('SubagentStart',{'agent_id':'worker-1','agent_type':'test'}),('SubagentStop',{'agent_id':'worker-1'}),('SessionEnd',{})]:
    payload={'hook_event_name':event,'session_id':session,**extra}
    command=settings['hooks'][event][0]['hooks'][0]['command']
    subprocess.run(command,shell=True,input=json.dumps(payload),text=True,check=True)
"#).unwrap();
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(&fake, fs::Permissions::from_mode(0o755)).unwrap();
    let id = Uuid::new_v4();
    let token = Uuid::new_v4();
    fs::write(dir.path().join("workspaces.json"),json!({"version":1,"entries":[{"id":id,"workspace":"test","name":"fixture","cwd":dir.path(),"agent":"claude","session_id":null}],"runs":{}}).to_string()).unwrap();
    let path = std::env::join_paths([
        bin,
        std::path::PathBuf::from("/usr/bin"),
        std::path::PathBuf::from("/bin"),
    ])
    .unwrap();
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
    let state: Value =
        serde_json::from_slice(&fs::read(dir.path().join("workspaces.json")).unwrap()).unwrap();
    assert_eq!(state["entries"][0]["session_id"], token.to_string());
    assert_eq!(state["runs"][id.to_string()]["agents_started"], 1);
    assert_eq!(state["runs"][id.to_string()]["ended"], true);
    assert_eq!(state["runs"][id.to_string()]["exit_code"], 0);
    assert_eq!(state["runs"][id.to_string()]["hooks_seen"], true);
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
    let token = Uuid::new_v4();
    fs::write(dir.path().join("workspaces.json"),json!({"version":1,"entries":[{"id":id,"workspace":"test","name":"fixture","cwd":dir.path(),"agent":"codex","session_id":null}],"runs":{}}).to_string()).unwrap();
    let path = std::env::join_paths([
        bin,
        std::path::PathBuf::from("/usr/bin"),
        std::path::PathBuf::from("/bin"),
    ])
    .unwrap();
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
    let state: Value =
        serde_json::from_slice(&fs::read(dir.path().join("workspaces.json")).unwrap()).unwrap();
    assert_eq!(
        state["entries"][0]["session_id"],
        "00000000-0000-4000-8000-000000000001"
    );
    assert_ne!(state["entries"][0]["session_id"], token.to_string());
}
