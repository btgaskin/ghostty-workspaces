use crate::{
    ghostty::shell_quote,
    model::{Agent, Entry, Run, Store},
    process,
};
use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    fs::File,
    io::{BufRead, BufReader, Read},
    path::Path,
    process::Command,
};
use uuid::Uuid;
const EVENTS: [&str; 5] = [
    "SessionStart",
    "SessionEnd",
    "SubagentStart",
    "SubagentStop",
    "Stop",
];

pub fn hooks(store: &Store, entry: &Entry, token: Uuid) -> Result<Value> {
    let exe = std::env::current_exe()?;
    let command = format!(
        "{} --state-dir {} hook {} {} {}",
        shell_quote(&exe.to_string_lossy()),
        shell_quote(&store.dir.to_string_lossy()),
        entry.id,
        token,
        entry.agent
    );
    Ok(
        json!({"hooks":EVENTS.into_iter().map(|event|(event.to_owned(),json!([{ "hooks":[{"type":"command","command":command,"timeout":2}] }]))).collect::<serde_json::Map<String,Value>>()}),
    )
}
pub fn launch(store: &Store, id: Uuid, token: Uuid) -> Result<i32> {
    let entry = store.read()?.entry(id)?.clone();
    let pid = std::process::id();
    let start = process::start_time(pid)
        .context("Cannot read own process identity; process access is required")?;
    store.update(|s| {
        if let Some(run) = s.runs.get(&id)
            && !run.ended
            && process::start_time(run.pid) == Some(run.start_time)
        {
            bail!("This saved tab already has a running launcher")
        }
        s.runs.insert(
            id,
            Run {
                token,
                pid,
                start_time: start,
                terminal_id: entry.terminal_id.clone(),
                ended: false,
                hooks_seen: false,
                agents: BTreeMap::new(),
                agents_started: 0,
                last_error: None,
                exit_code: None,
            },
        );
        let saved = s
            .entries
            .iter_mut()
            .find(|e| e.id == id)
            .context("Unknown tab")?;
        saved.ever_started = true;
        saved.imported = false;
        Ok(())
    })?;
    let result = run_command(store, &entry, token);
    store.update(|s| {
        if let Some(r) = s.runs.get_mut(&id).filter(|r| r.token == token) {
            r.ended = true;
            r.agents.clear();
            r.last_error = result.as_ref().err().map(|e| format!("{e:#}"));
            r.exit_code = result.as_ref().ok().copied();
        }
        Ok(())
    })?;
    result
}
fn run_command(store: &Store, entry: &Entry, token: Uuid) -> Result<i32> {
    let hook_config = hooks(store, entry, token)?;
    let provider_args = provider_args(entry, token);
    let mut cmd = match entry.agent {
        Agent::Codex => {
            let mut c = Command::new(executable("codex")?);
            for (event, groups) in hook_config["hooks"].as_object().context("Invalid hooks")? {
                // JSON strings are valid TOML strings; object keys need TOML '=' syntax.
                let command = groups[0]["hooks"][0]["command"]
                    .as_str()
                    .context("Missing hook command")?;
                c.args([
                    "-c",
                    &format!(
                        "hooks.{event}=[{{hooks=[{{type=\"command\",command={},timeout=2}}]}}]",
                        serde_json::to_string(command)?
                    ),
                ]);
            }
            c.args(&provider_args);
            c
        }
        Agent::Claude => {
            let mut c = Command::new(executable("claude")?);
            c.args(["--settings", &serde_json::to_string(&hook_config)?]);
            c.args(&provider_args);
            c
        }
        Agent::Shell => {
            let mut c = Command::new(std::env::var_os("SHELL").unwrap_or("/bin/zsh".into()));
            c.arg("-l");
            c
        }
        Agent::Command => {
            let (program, args) = entry
                .command
                .split_first()
                .context("Command tab requires an executable")?;
            let mut c = Command::new(executable(program)?);
            c.args(args);
            c
        }
    };
    cmd.current_dir(&entry.cwd);
    cmd.env("PATH", launch_path()?);
    let status = cmd.status().with_context(|| {
        format!(
            "Could not launch {}. Check that it is on PATH.",
            entry.agent
        )
    })?;
    Ok(status.code().unwrap_or(1))
}
pub fn provider_args(entry: &Entry, token: Uuid) -> Vec<String> {
    let mut args = entry.args.clone();
    if let Some(id) = entry.session_id {
        let identity_command = match entry.agent {
            Agent::Codex => matches!(args.first().map(String::as_str), Some("resume" | "fork")),
            Agent::Claude => matches!(args.first().map(String::as_str), Some("--resume" | "-r")),
            _ => false,
        };
        if identity_command {
            args.remove(0);
            if args.first().is_some_and(|s| !s.starts_with('-')) {
                args.remove(0);
            }
        }
        if entry.agent == Agent::Codex {
            args.retain(|a| a != "--last" && a != "--all");
        }
        let flag = if entry.agent == Agent::Claude {
            "--resume"
        } else {
            "resume"
        };
        args.splice(0..0, [flag.to_owned(), id.to_string()]);
    } else if entry.agent == Agent::Claude
        && !args
            .iter()
            .any(|a| matches!(a.as_str(), "--resume" | "-r" | "--continue" | "-c"))
    {
        args.splice(0..0, ["--session-id".into(), token.to_string()]);
    }
    if entry.agent == Agent::Codex && entry.isolated && !args.iter().any(|a| a == "--no-daemon") {
        args.push("--no-daemon".into());
    }
    args
}
fn launch_path() -> Result<std::ffi::OsString> {
    let mut dirs: Vec<_> =
        std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default()).collect();
    for path in [
        crate::model::home()?.join(".local/bin"),
        "/opt/homebrew/bin".into(),
        "/usr/local/bin".into(),
        "/usr/bin".into(),
        "/bin".into(),
    ] {
        if !dirs.contains(&path) {
            dirs.push(path);
        }
    }
    std::env::join_paths(dirs).context("Invalid launch PATH")
}
fn executable(name: &str) -> Result<std::path::PathBuf> {
    if name.contains('/') {
        return Ok(name.into());
    }
    for dir in std::env::split_paths(&launch_path()?) {
        let path = dir.join(name);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if path.is_file() && path.metadata()?.permissions().mode() & 0o111 != 0 {
                return Ok(path);
            }
        }
    }
    bail!("Cannot find {name} on PATH, ~/.local/bin, or Homebrew's bin directory")
}
// The durable conversation ID comes from the transcript metadata, not a runtime hook ID.
// Codex's hook session_id can change when the same conversation is resumed.
pub fn conversation_id(agent: Agent, input: &Value) -> Option<Uuid> {
    if agent == Agent::Claude {
        return input["session_id"]
            .as_str()
            .and_then(|s| Uuid::parse_str(s).ok());
    }
    let path = input["transcript_path"].as_str()?;
    let file = File::open(Path::new(path)).ok()?;
    let mut reader = BufReader::new(file);
    let mut line = String::new();
    reader.by_ref().take(64 * 1024).read_line(&mut line).ok()?;
    let value: Value = serde_json::from_str(&line).ok()?;
    if value["type"] != "session_meta" || value["payload"]["source"].get("subagent").is_some() {
        return None;
    }
    value["payload"]["id"]
        .as_str()
        .and_then(|s| Uuid::parse_str(s).ok())
}
pub fn hook(store: &Store, id: Uuid, token: Uuid, agent: Agent) -> Result<()> {
    let input: Value = serde_json::from_reader(std::io::stdin().take(256 * 1024))?;
    apply_hook(store, id, token, agent, &input)
}
pub fn apply_hook(store: &Store, id: Uuid, token: Uuid, agent: Agent, input: &Value) -> Result<()> {
    let event = input["hook_event_name"].as_str().unwrap_or("");
    let conversation = if event == "SessionStart" {
        conversation_id(agent, input)
    } else {
        None
    };
    store.update(|s| {
        let Some(run) = s.runs.get_mut(&id).filter(|r| r.token == token && !r.ended) else {
            return Ok(());
        };
        run.hooks_seen = true;
        match event {
            "SessionStart" => {
                // A new root conversation (including /clear) resets old logical agents.
                if input["source"] != "compact" {
                    run.agents.clear();
                    run.agents_started = 0;
                }
                if let Some(conversation) = conversation
                    && let Some(e) = s.entries.iter_mut().find(|e| e.id == id)
                {
                    e.session_id = Some(conversation);
                    e.session_verified = true;
                }
            }
            "SubagentStart" => {
                if let Some(a) = input["agent_id"].as_str()
                    && run
                        .agents
                        .insert(
                            a.into(),
                            input["agent_type"].as_str().unwrap_or("agent").into(),
                        )
                        .is_none()
                {
                    run.agents_started += 1;
                }
            }
            "SubagentStop" => {
                if let Some(a) = input["agent_id"].as_str() {
                    run.agents.remove(a);
                }
            }
            "SessionEnd" => {
                run.agents.clear();
            }
            _ => {}
        }
        Ok(())
    })
}
pub fn session_from_title(title: &str) -> Option<Uuid> {
    // Import only an explicit UUID. Never guess from the cwd or the latest session.
    title
        .split_whitespace()
        .find_map(|s| Uuid::parse_str(s).ok())
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Run, State};
    use std::fs;
    fn fixture() -> (tempfile::TempDir, Store, Uuid, Uuid) {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::new(Some(dir.path().into())).unwrap();
        let id = Uuid::new_v4();
        let token = Uuid::new_v4();
        store
            .update(|s| {
                s.entries.push(Entry {
                    id,
                    workspace: "test".into(),
                    name: "test".into(),
                    cwd: dir.path().into(),
                    agent: Agent::Codex,
                    session_id: None,
                    session_verified: false,
                    args: vec![],
                    command: vec![],
                    isolated: false,
                    window: 0,
                    terminal_id: None,
                    imported: false,
                    ever_started: true,
                });
                s.runs.insert(
                    id,
                    Run {
                        token,
                        pid: 1,
                        start_time: 0,
                        terminal_id: None,
                        ended: false,
                        hooks_seen: false,
                        agents: BTreeMap::new(),
                        agents_started: 0,
                        last_error: None,
                        exit_code: None,
                    },
                );
                Ok(())
            })
            .unwrap();
        (dir, store, id, token)
    }
    #[test]
    fn durable_codex_id_comes_from_transcript_not_runtime_session() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rollout.jsonl");
        let thread = Uuid::new_v4();
        fs::write(
            &path,
            json!({"type":"session_meta","payload":{"id":thread,"source":"cli"}}).to_string()
                + "\n",
        )
        .unwrap();
        let input = json!({"session_id":Uuid::new_v4(),"transcript_path":path});
        assert_eq!(conversation_id(Agent::Codex, &input), Some(thread));
        assert_eq!(
            conversation_id(Agent::Codex, &json!({"session_id":thread})),
            None
        );
        fs::write(
            &path,
            json!({"type":"session_meta","payload":{"id":thread,"source":{"subagent":{}}}})
                .to_string(),
        )
        .unwrap();
        assert_eq!(conversation_id(Agent::Codex, &input), None);
    }
    #[test]
    fn stale_hook_cannot_change_session_or_agent_count() {
        let (_dir, s, id, _token) = fixture();
        apply_hook(
            &s,
            id,
            Uuid::new_v4(),
            Agent::Claude,
            &json!({"hook_event_name":"SessionStart","session_id":Uuid::new_v4()}),
        )
        .unwrap();
        assert_eq!(s.read().unwrap().entry(id).unwrap().session_id, None);
        assert!(!s.read().unwrap().runs[&id].hooks_seen);
    }
    #[test]
    fn logical_agents_are_idempotent_and_survive_concurrent_events() {
        let (_dir, s, id, token) = fixture();
        let mut workers = vec![];
        for n in 0..12 {
            let s = s.clone();
            workers.push(std::thread::spawn(move||{
            let start=json!({"hook_event_name":"SubagentStart","agent_id":n.to_string(),"agent_type":"worker"});
            apply_hook(&s,id,token,Agent::Codex,&start).unwrap();apply_hook(&s,id,token,Agent::Codex,&start).unwrap();
        }));
        }
        for w in workers {
            w.join().unwrap();
        }
        let state = s.read().unwrap();
        assert_eq!(state.runs[&id].agents.len(), 12);
        assert_eq!(state.runs[&id].agents_started, 12);
        apply_hook(
            &s,
            id,
            token,
            Agent::Codex,
            &json!({"hook_event_name":"SubagentStop","agent_id":"0"}),
        )
        .unwrap();
        assert_eq!(s.read().unwrap().runs[&id].agents.len(), 11);
    }
    #[test]
    fn malformed_store_is_not_overwritten() {
        let (dir, s, _id, _token) = fixture();
        let path = dir.path().join("workspaces.json");
        fs::write(&path, "broken").unwrap();
        assert!(
            s.update(|state| {
                *state = State::default();
                Ok(())
            })
            .is_err()
        );
        assert_eq!(fs::read_to_string(path).unwrap(), "broken");
    }
    #[test]
    fn provider_arguments_keep_no_daemon_opt_in_and_resume_one_exact_thread() {
        let (_dir, store, id, token) = fixture();
        let mut entry = store.read().unwrap().entry(id).unwrap().clone();
        assert!(
            !provider_args(&entry, token)
                .iter()
                .any(|a| a == "--no-daemon")
        );
        entry.args = vec![
            "resume".into(),
            "old-session-name".into(),
            "--no-daemon".into(),
        ];
        entry.isolated = true;
        let thread = Uuid::new_v4();
        entry.session_id = Some(thread);
        assert_eq!(
            provider_args(&entry, token),
            ["resume", &thread.to_string(), "--no-daemon"]
        );
        entry.args = vec!["resume".into(), "--last".into()];
        entry.isolated = false;
        assert_eq!(
            provider_args(&entry, token),
            ["resume", &thread.to_string()]
        );
    }
    #[test]
    fn claude_resume_does_not_generate_a_second_session_id() {
        let (_dir, store, id, token) = fixture();
        let mut entry = store.read().unwrap().entry(id).unwrap().clone();
        entry.agent = Agent::Claude;
        entry.args = vec!["--resume".into(), "named-conversation".into()];
        assert_eq!(provider_args(&entry, token), entry.args);
        let thread = Uuid::new_v4();
        entry.session_id = Some(thread);
        assert_eq!(
            provider_args(&entry, token),
            ["--resume", &thread.to_string()]
        );
    }
    #[test]
    fn imports_only_explicit_session_ids() {
        let id = Uuid::new_v4();
        assert_eq!(
            session_from_title(&format!("codex resume --no-daemon {id}")),
            Some(id)
        );
        assert_eq!(session_from_title("codex resume --last"), None);
    }
}
