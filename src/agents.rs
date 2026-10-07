use crate::{
    ghostty::shell_quote,
    model::{Agent, Entry, Store},
};
use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use std::{
    fs::File,
    io::{BufRead, BufReader, Read},
    path::Path,
    process::Command,
};
use uuid::Uuid;
const EVENTS: [&str; 8] = [
    "SessionStart",
    "SessionEnd",
    "SubagentStart",
    "SubagentStop",
    "Stop",
    "UserPromptSubmit",
    "PermissionRequest",
    "PreToolUse",
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
pub fn command(store: &Store, entry: &Entry, token: Uuid) -> Result<Command> {
    if entry.ever_started && entry.session_verified && entry.resume_args.is_none() {
        checked_resume_options(entry.agent, &entry.args)?;
    }
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
        Agent::Cursor => {
            if entry.args.iter().any(|a| {
                a == "--workspace"
                    || a.starts_with("--workspace=")
                    || a == "--worktree"
                    || a.starts_with("--worktree=")
                    || a == "-w"
            }) {
                bail!(
                    "Use gws --cwd to pin the project directory; Cursor workspace/worktree overrides are not supported"
                )
            }
            let program = executable("agent").or_else(|_| executable("cursor-agent"))?;
            let mut id = entry
                .provider_session
                .clone()
                .or_else(|| entry.session_id.map(|id| id.to_string()));
            if id.is_none() {
                for (i, arg) in entry.args.iter().enumerate() {
                    if let Some(value) = arg.strip_prefix("--resume=") {
                        id = Some(value.into());
                    } else if arg == "--resume" {
                        id = entry
                            .args
                            .get(i + 1)
                            .filter(|s| !s.starts_with('-'))
                            .cloned();
                        if id.is_none() {
                            bail!(
                                "Cursor resume requires an exact chat ID; use gws cursor --conversation ID"
                            )
                        }
                    } else if arg == "--continue" {
                        bail!("Cursor --continue is ambiguous; use an exact conversation ID")
                    }
                }
            }
            if id.is_none() {
                let mut create = Command::new(&program);
                create
                    .arg("create-chat")
                    .current_dir(&entry.cwd)
                    .env("PATH", launch_path()?);
                let output = crate::util::output(create, std::time::Duration::from_secs(10))?;
                if !output.status.success() {
                    bail!("Cursor create-chat failed; no conversation was launched")
                };
                id = Some(String::from_utf8(output.stdout)?.trim().to_owned());
            }
            let id = id.context("Cursor conversation missing")?;
            validate_cursor_id(&id)?;
            store.update(|s| {
                if s.runs.get(&entry.id).is_none_or(|r| r.token != token) {
                    bail!("Cursor run changed before binding")
                };
                let e = s
                    .entries
                    .iter_mut()
                    .find(|e| e.id == entry.id)
                    .context("Item missing")?;
                e.provider_session = Some(id.clone());
                e.session_verified = true;
                e.resume_args = checked_resume_options(Agent::Cursor, &e.args).ok();
                Ok(())
            })?;
            let mut c = Command::new(program);
            c.args(["--resume", &id]);
            let args = if entry.ever_started {
                entry
                    .resume_args
                    .clone()
                    .map(Ok)
                    .unwrap_or_else(|| checked_resume_options(Agent::Cursor, &entry.args))?
            } else {
                cursor_initial_args(&entry.args)
            };
            c.args(args);
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
    cmd.env("GWS_ITEM_ID", entry.id.to_string())
        .env("GWS_RUN_ID", token.to_string())
        .env("GWS_STATE_DIR", &store.dir);
    if let Some(home) = &entry.provider_home {
        cmd.env(
            if entry.agent == Agent::Codex {
                "CODEX_HOME"
            } else {
                "CLAUDE_CONFIG_DIR"
            },
            home,
        );
    }
    Ok(cmd)
}
pub fn launch(store: &Store, id: Uuid, token: Uuid) -> Result<i32> {
    crate::supervisor::launch(store, id, token)
}

pub fn provider_args(entry: &Entry, token: Uuid) -> Vec<String> {
    let mut args = if entry.ever_started && entry.session_verified {
        entry
            .resume_args
            .clone()
            .unwrap_or_else(|| resume_options(entry.agent, &entry.args))
    } else {
        entry.args.clone()
    };
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
/// Retain options and their values, discard launch-only positional prompts/selectors.
/// Unknown options are refused on resume rather than guessing whether they consume a prompt.
pub fn checked_resume_options(agent: Agent, args: &[String]) -> Result<Vec<String>> {
    let value_flags = match agent {
        Agent::Codex => vec![
            "-c",
            "--config",
            "-m",
            "--model",
            "-p",
            "--profile",
            "-s",
            "--sandbox",
            "-a",
            "--ask-for-approval",
            "--add-dir",
            "--enable",
            "--disable",
        ],
        Agent::Claude => vec![
            "--model",
            "--permission-mode",
            "--allowedTools",
            "--disallowedTools",
            "--add-dir",
            "--mcp-config",
            "--settings",
            "--system-prompt",
            "--append-system-prompt",
            "--effort",
        ],
        Agent::Cursor => vec![
            "--model",
            "--mode",
            "--sandbox",
            "--add-dir",
            "--plugin-dir",
            "--output-format",
            "--endpoint",
            "-e",
            "-H",
            "--header",
        ],
        _ => vec![],
    };
    let switches = [
        "--no-daemon",
        "--no-alt-screen",
        "--full-auto",
        "--dangerously-bypass-approvals-and-sandbox",
        "--dangerously-skip-permissions",
        "--verbose",
        "--debug",
        "--plan",
        "--auto-review",
        "--force",
        "-f",
        "--yolo",
        "--trust",
        "--approve-mcps",
        "--print",
        "-p",
    ];
    let mut out = vec![];
    let mut i = 0;
    while i < args.len() {
        let a = &args[i];
        if matches!(
            a.as_str(),
            "resume" | "fork" | "--last" | "--all" | "--continue"
        ) {
            i += 1;
            continue;
        }
        if matches!(
            a.as_str(),
            "--resume" | "-r" | "--session-id" | "-C" | "--cd"
        ) {
            i += 1;
            if i < args.len() && !args[i].starts_with('-') {
                i += 1;
            }
            continue;
        }
        if value_flags.contains(&a.as_str()) {
            out.push(a.clone());
            i += 1;
            out.push(args.get(i).context("Missing option value")?.clone());
        } else if switches.contains(&a.as_str())
            || value_flags
                .iter()
                .any(|flag| a.starts_with(&format!("{flag}=")))
        {
            out.push(a.clone());
        } else if a.starts_with("--resume=") { /* consumed identity */
        } else if a.starts_with('-') {
            bail!("Unsupported resume option {a}; set explicit resume_args before resuming")
        };
        i += 1;
    }
    Ok(out)
}
pub fn resume_options(agent: Agent, args: &[String]) -> Vec<String> {
    checked_resume_options(agent, args).unwrap_or_default()
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
pub fn executable(name: &str) -> Result<std::path::PathBuf> {
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
    apply_hook(store, id, token, agent, &input)?;
    if input["hook_event_name"] == "SessionStart" {
        println!(
            "{}",
            json!({"hookSpecificOutput":{"hookEventName":"SessionStart","additionalContext":format!("Managed by gws. For cleanup requests use --caller {}:{}; inspect and preview before apply. Never park this controlling run.",id,token)}})
        );
    }
    Ok(())
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
        run.activity_at = Some(crate::model::now());
        run.activity_source = Some(format!("advisory_hook:{event}"));
        // Hooks may be concurrent or dropped. Stop is not an idle guarantee.
        run.activity = match event {
            "UserPromptSubmit" | "PreToolUse" => "working",
            "PermissionRequest" => "awaiting_input",
            "SubagentStart" | "SubagentStop" => run.activity.as_str(),
            _ => "unknown",
        }
        .into();
        if matches!(event, "UserPromptSubmit" | "PreToolUse") {
            run.last_turn_end = None;
        }
        if event == "Stop" {
            run.completed_turns = run.completed_turns.saturating_add(1);
            run.last_turn_end = Some(crate::model::now());
        }
        match event {
            "SessionStart" => {
                // A new root conversation (including /clear) resets old logical agents.
                if input["source"] != "compact" {
                    run.last_turn_end = None;
                    run.agents.clear();
                    run.agents_started = 0;
                }
                if let Some(conversation) = conversation
                    && let Some(e) = s.entries.iter_mut().find(|e| e.id == id)
                {
                    e.session_id = Some(conversation);
                    e.session_verified = true;
                    if let Some(path) = input["transcript_path"].as_str() {
                        let path = std::path::PathBuf::from(path);
                        if path.is_absolute() {
                            e.transcript_path = Some(path);
                        }
                    }
                    e.resume_args = checked_resume_options(e.agent, &e.args).ok();
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
pub fn validate_cursor_id(id: &str) -> Result<()> {
    if id.is_empty()
        || id.len() > 256
        || !id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "-_".contains(c))
    {
        bail!("Cursor conversation ID must contain only letters, digits, '-' or '_'")
    };
    Ok(())
}
fn cursor_initial_args(args: &[String]) -> Vec<String> {
    let mut out = vec![];
    let mut i = 0;
    while i < args.len() {
        if args[i] == "--resume" {
            i += 2;
            continue;
        }
        if args[i].starts_with("--resume=") {
            i += 1;
            continue;
        }
        out.push(args[i].clone());
        i += 1;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Run, State};
    use std::{collections::BTreeMap, fs};
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
                    ..Entry::default()
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
                        ..Run::default()
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
        apply_hook(
            &s,
            id,
            token,
            Agent::Codex,
            &json!({"hook_event_name":"UserPromptSubmit"}),
        )
        .unwrap();
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
        assert_eq!(state.runs[&id].activity, "working");
        apply_hook(
            &s,
            id,
            token,
            Agent::Codex,
            &json!({"hook_event_name":"SubagentStop","agent_id":"0"}),
        )
        .unwrap();
        assert_eq!(s.read().unwrap().runs[&id].agents.len(), 11);
        assert_eq!(s.read().unwrap().runs[&id].activity, "working");
        apply_hook(
            &s,
            id,
            token,
            Agent::Codex,
            &json!({"hook_event_name":"Stop"}),
        )
        .unwrap();
        assert_eq!(s.read().unwrap().runs[&id].completed_turns, 1);
        apply_hook(
            &s,
            id,
            token,
            Agent::Codex,
            &json!({"hook_event_name":"UserPromptSubmit"}),
        )
        .unwrap();
        let run = &s.read().unwrap().runs[&id];
        assert_eq!(run.activity, "working");
        assert_eq!(run.last_turn_end, None);
        assert_eq!(run.completed_turns, 1);
        assert_eq!(run.seen_turns, 0);
    }
    #[test]
    fn malformed_store_is_not_overwritten() {
        let dir = tempfile::tempdir().unwrap();
        let s = Store::new(Some(dir.path().into())).unwrap();
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
