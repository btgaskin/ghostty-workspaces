use crate::{
    ghostty,
    model::{Agent, Entry, Run, State, Store},
    process,
};
use anyhow::{Context, Result, bail};
use serde_json::Value;
use std::{
    collections::{BTreeMap, BTreeSet, HashSet},
    fs::File,
    io::{self, BufRead, BufReader, Read, Write},
    path::PathBuf,
    process::Command,
};
use sysinfo::{ProcessRefreshKind, ProcessesToUpdate, System, UpdateKind};
use uuid::Uuid;

pub fn one(store: &Store, id: Uuid, session: Uuid) -> Result<()> {
    one_report(store, id, session, true)
}
pub fn one_report(store: &Store, id: Uuid, session: Uuid, report: bool) -> Result<()> {
    store.update(|s| {
        if !matches!(s.entry(id)?.agent, Agent::Codex | Agent::Claude) {
            bail!("Only agent tabs have conversation IDs")
        }
        let entry = s.entries.iter_mut().find(|e| e.id == id).unwrap();
        if entry.lease.as_ref().is_some_and(|l| l.live()) {
            bail!("Work is reserved by an operation")
        };
        entry.session_id = Some(session);
        entry.session_verified = true;
        Ok(())
    })?;
    if report {
        println!("Bound {id} to {session}");
    }
    Ok(())
}

pub fn all(store: &Store, dry_run: bool) -> Result<()> {
    let state = store.read()?;
    let entries: Vec<_> = state
        .entries
        .iter()
        .filter(|e| {
            matches!(e.agent, Agent::Codex | Agent::Claude)
                && (e.session_id.is_none() || !e.session_verified)
        })
        .collect();
    println!(
        "{} agent tabs need an exact conversation ID.",
        entries.len()
    );
    if !dry_run && !entries.is_empty() {
        println!(
            "Find the ID in each agent's session UI. Paste a UUID, Enter to skip, or q to stop.\nThis binds saved continuity; it does not restart tabs or attach process ownership."
        );
    }
    for e in entries {
        println!(
            "\n{} · {}\n{}\nTab {}",
            e.name,
            e.agent,
            e.cwd.display(),
            e.id
        );
        if dry_run {
            continue;
        }
        loop {
            print!("Conversation UUID: ");
            io::stdout().flush()?;
            let mut input = String::new();
            if io::stdin().read_line(&mut input)? == 0 {
                return Ok(());
            }
            match input.trim() {
                "" => break,
                "q" => return Ok(()),
                value => match Uuid::parse_str(value) {
                    Ok(session) => {
                        one(store, e.id, session)?;
                        break;
                    }
                    Err(_) => println!("Expected an exact UUID; Enter skips this tab."),
                },
            }
        }
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Signature {
    isolated: bool,
    resume: bool,
    fork: bool,
    target: Option<Uuid>,
}
fn signature<'a>(words: impl Iterator<Item = &'a str>) -> Signature {
    let words: Vec<_> = words.collect();
    Signature {
        isolated: words.contains(&"--no-daemon"),
        resume: words.contains(&"resume"),
        fork: words.contains(&"fork"),
        target: words.iter().find_map(|s| Uuid::parse_str(s).ok()),
    }
}
#[derive(Debug, Clone)]
struct Root {
    pid: u32,
    start: u64,
    cwd: PathBuf,
    signature: Signature,
    sessions: BTreeSet<Uuid>,
}
type Parents = BTreeMap<u32, (u32, String)>;
fn parent_inventory() -> Result<Parents> {
    // Root-owned login processes may be omitted when sysinfo requests argv.
    // ps supplies ancestry without reading environments or command arguments.
    let output = Command::new("/bin/ps")
        .args(["-axo", "pid=,ppid=,comm="])
        .output()?;
    if !output.status.success() {
        bail!("Cannot read terminal process ancestry");
    }
    let mut parents = Parents::new();
    for line in String::from_utf8_lossy(&output.stdout).lines() {
        let mut words = line.split_whitespace();
        let (Some(pid), Some(parent)) = (words.next(), words.next()) else {
            continue;
        };
        let (Ok(pid), Ok(parent)) = (pid.parse(), parent.parse()) else {
            continue;
        };
        let command = words.collect::<Vec<_>>().join(" ");
        let name = std::path::Path::new(&command)
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .to_ascii_lowercase();
        parents.insert(pid, (parent, name));
    }
    Ok(parents)
}
fn ghostty_client(parents: &Parents, pid: u32) -> bool {
    let mut parent = parents.get(&pid).map(|p| p.0);
    let mut seen = HashSet::new();
    while let Some(pid) = parent {
        if !seen.insert(pid) {
            return false;
        }
        let Some((next, name)) = parents.get(&pid) else {
            return false;
        };
        if name == "ghostty" {
            return true;
        }
        // A nested Codex worker is not the tab's terminal client.
        if name == "codex" {
            return false;
        }
        parent = Some(*next);
    }
    false
}
fn root_metadata(path: &str) -> Option<(Uuid, PathBuf)> {
    if !path.contains("/sessions/") || !path.ends_with(".jsonl") {
        return None;
    }
    let mut line = Vec::new();
    BufReader::new(File::open(path).ok()?)
        .take(65536)
        .read_until(b'\n', &mut line)
        .ok()?;
    let value: Value = serde_json::from_slice(&line).ok()?;
    let p = &value["payload"];
    if value["type"] != "session_meta" || p["source"] != "cli" || p["parent_thread_id"].is_string()
    {
        return None;
    }
    Some((
        Uuid::parse_str(p["id"].as_str()?).ok()?,
        PathBuf::from(p["cwd"].as_str()?),
    ))
}
fn discover() -> Result<Vec<Root>> {
    let parents = parent_inventory()?;
    let mut system = System::new();
    system.refresh_processes_specifics(
        ProcessesToUpdate::All,
        true,
        ProcessRefreshKind::nothing()
            .with_cmd(UpdateKind::Always)
            .with_cwd(UpdateKind::Always),
    );
    let mut roots: Vec<_> = system
        .processes()
        .iter()
        .filter(|(pid, p)| p.name() == "codex" && ghostty_client(&parents, pid.as_u32()))
        .filter_map(|(pid, p)| {
            Some(Root {
                pid: pid.as_u32(),
                start: p.start_time(),
                cwd: p.cwd()?.to_path_buf(),
                signature: signature(p.cmd().iter().skip(1).filter_map(|s| s.to_str())),
                sessions: BTreeSet::new(),
            })
        })
        .collect();
    if roots.is_empty() {
        return Ok(roots);
    }
    let pids = roots
        .iter()
        .map(|r| r.pid.to_string())
        .collect::<Vec<_>>()
        .join(",");
    let output = Command::new("/usr/sbin/lsof")
        .args(["-nP", "-p", &pids, "-Fn"])
        .output()
        .context("Cannot inspect open Codex metadata files")?;
    // lsof may return 1 for partially inaccessible processes; process matches
    // remain useful, but no missing session ID is invented.
    let mut pid = None;
    for line in String::from_utf8_lossy(&output.stdout).lines() {
        if let Some(value) = line.strip_prefix('p') {
            pid = value.parse::<u32>().ok();
        } else if let Some(path) = line.strip_prefix('n')
            && let Some(root) = roots.iter_mut().find(|r| Some(r.pid) == pid)
            && let Some((id, cwd)) = root_metadata(path)
            && cwd == root.cwd
        {
            root.sessions.insert(id);
        }
    }
    Ok(roots)
}
fn matches(entry: &Entry, root: &Root) -> bool {
    entry.agent == Agent::Codex
        && entry.cwd == root.cwd
        && entry.name.split_whitespace().next() == Some("codex")
        && signature(entry.name.split_whitespace().skip(1)) == root.signature
}
fn unique_matches<'a>(entries: &'a [&Entry], roots: &'a [Root]) -> Vec<(&'a Entry, &'a Root)> {
    let mut linked = vec![];
    // An exact, explicitly verified ID can disambiguate identical launch titles.
    for entry in entries.iter().filter(|e| e.session_verified) {
        let Some(id) = entry.session_id else {
            continue;
        };
        let candidates: Vec<_> = roots
            .iter()
            .filter(|r| matches(entry, r) && r.sessions.len() == 1 && r.sessions.contains(&id))
            .collect();
        if candidates.len() == 1
            && entries
                .iter()
                .filter(|e| {
                    e.session_verified && e.session_id == Some(id) && matches(e, candidates[0])
                })
                .count()
                == 1
        {
            linked.push((*entry, candidates[0]));
        }
    }
    let remaining: Vec<_> = entries
        .iter()
        .copied()
        .filter(|e| !linked.iter().any(|(known, _)| known.id == e.id))
        .collect();
    let remaining_roots: Vec<_> = roots
        .iter()
        .filter(|r| !linked.iter().any(|(_, known)| known.pid == r.pid))
        .collect();
    let unique = remaining
        .iter()
        .filter_map(|e| {
            let candidates: Vec<_> = remaining_roots
                .iter()
                .copied()
                .filter(|r| matches(e, r))
                .collect();
            if candidates.len() != 1 {
                return None;
            }
            let root = candidates[0];
            if remaining
                .iter()
                .filter(|other| matches(other, root))
                .count()
                != 1
            {
                return None;
            }
            Some((*e, root))
        })
        .collect::<Vec<_>>();
    linked.extend(unique);
    linked
}
pub fn automatic(store: &Store, dry_run: bool) -> Result<()> {
    automatic_report(store, dry_run, true)
}
pub fn automatic_report(store: &Store, dry_run: bool, report: bool) -> Result<()> {
    let state = store.read()?;
    let windows = ghostty::snapshot()?;
    let entries: Vec<_> = state
        .entries
        .iter()
        .filter(|e| {
            e.imported
                && e.terminal_id
                    .as_ref()
                    .is_some_and(|t| ghostty::terminal_exists(&windows, t))
        })
        .collect();
    let roots = discover()?;
    let matches = unique_matches(&entries, &roots);
    let mut updates = vec![];
    for (e, r) in matches {
        // Existing gws launches retain their token and hook counts.
        if state.runs.get(&e.id).is_some_and(|run| {
            !run.ended && run.pid != r.pid && process::start_time(run.pid) == Some(run.start_time)
        }) {
            continue;
        }
        if process::start_time(r.pid) != Some(r.start) {
            continue;
        }
        let session = if r.sessions.len() == 1 {
            r.sessions.first().copied()
        } else {
            None
        };
        if report {
            println!(
                "{} {} → PID {}{}",
                if dry_run { "Would attach" } else { "Attached" },
                e.name,
                r.pid,
                session
                    .map(|id| format!(" · verified session {id}"))
                    .unwrap_or(" · session still unverified".into())
            );
        }
        updates.push((e.id, r.clone(), session));
    }
    if !dry_run && !updates.is_empty() {
        store.update(|s| apply_discovered(s, &updates))?;
    }
    let unresolved = if dry_run {
        let mut preview = state.clone();
        apply_discovered(&mut preview, &updates)?;
        preview.entries.iter().filter(|e| e.needs_session()).count()
    } else {
        store
            .read()?
            .entries
            .iter()
            .filter(|e| e.needs_session())
            .count()
    };
    if report {
        println!(
            "{} unique process matches; {unresolved} conversations still need verification. Identical tabs are not matched by order or latest file.",
            updates.len()
        );
    }
    Ok(())
}
fn apply_discovered(state: &mut State, updates: &[(Uuid, Root, Option<Uuid>)]) -> Result<()> {
    for (id, root, session) in updates {
        let Some(e) = state.entries.iter_mut().find(|e| e.id == *id) else {
            continue;
        };
        if let Some(session) = session {
            e.session_id = Some(*session);
            e.session_verified = true;
        }
        state.runs.entry(*id).and_modify(|run| {
            if run.pid != root.pid || run.start_time != root.start {
                run.ended = true;
            }
        });
        if state.runs.get(id).is_some_and(|run| !run.ended) {
            continue;
        }
        state.runs.insert(
            *id,
            Run {
                token: Uuid::new_v4(),
                pid: root.pid,
                start_time: root.start,
                terminal_id: e.terminal_id.clone(),
                ended: false,
                hooks_seen: false,
                agents: BTreeMap::new(),
                agents_started: 0,
                last_error: None,
                exit_code: None,
                ..Run::default()
            },
        );
    }
    Ok(())
}

/// Bind only the invoking Codex conversation; directory proximity is not identity.
pub fn here(
    store: &Store,
    item: Option<Uuid>,
    session: Option<Uuid>,
    dry_run: bool,
) -> Result<Value> {
    let env_uuid = |name: &str| -> Result<Option<Uuid>> {
        std::env::var(name)
            .ok()
            .map(|s| Uuid::parse_str(&s).with_context(|| format!("Invalid {name}")))
            .transpose()
    };
    let thread = env_uuid("CODEX_THREAD_ID")?;
    if session.zip(thread).is_some_and(|(a, b)| a != b) {
        bail!("Supplied conversation differs from the current Codex thread")
    }
    let session = thread.or(session).context("Run !gws bind-here inside Codex; if this version omits its thread ID, supply --session CONVERSATION_ID")?;
    let home = std::env::var_os("CODEX_HOME")
        .map(PathBuf::from)
        .unwrap_or(crate::model::home()?.join(".codex"));
    let home = home.canonicalize().context("Codex home is unavailable")?;
    let state = store.read()?;
    let (catalog, _) = crate::catalog::collect(store)?;
    let conversations: Vec<_> = catalog
        .iter()
        .filter(|c| {
            c.provider == Agent::Codex
                && c.id == session
                && !c.subagent
                && c.parent.is_none()
                && c.provider_home.canonicalize().ok().as_ref() == Some(&home)
        })
        .collect();
    if conversations.len() != 1 {
        bail!("Current root conversation metadata is unavailable or ambiguous; no binding changed")
    }
    let conversation = conversations[0];
    let transcript = conversation
        .transcript_path
        .clone()
        .context("Transcript metadata path missing")?;
    let context_item = env_uuid("GWS_ITEM_ID")?;
    let context_run = env_uuid("GWS_RUN_ID")?;
    if context_item.is_some() != context_run.is_some() {
        bail!("Incomplete gws run context; no binding changed")
    }
    if let Some(id) = context_item {
        if item.is_some_and(|other| other != id) {
            bail!("--item differs from the current gws work item")
        }
        let run = state.runs.get(&id).context("Current gws run is missing")?;
        if Some(run.token) != context_run || run.ended || !run.live() {
            bail!("Current gws run context is stale; no binding changed")
        }
    }
    // Ancestry is optional for explicit item/context binding, but never replaced by cwd guessing.
    let parents = parent_inventory().ok();
    let mut ancestry = vec![];
    let mut next = Some(std::process::id());
    while let (Some(pid), Some(parents)) = (next, parents.as_ref()) {
        if ancestry.contains(&pid) {
            break;
        }
        ancestry.push(pid);
        next = parents.get(&pid).map(|p| p.0);
    }
    let roots = discover().unwrap_or_default();
    let root = roots.iter().find(|r| {
        ancestry.contains(&r.pid)
            && r.cwd
                .canonicalize()
                .ok()
                .zip(conversation.cwd.canonicalize().ok())
                .is_some_and(|(a, b)| a == b)
    });
    if root.is_some_and(|r| !r.sessions.is_empty() && !r.sessions.contains(&session)) {
        bail!("Conversation does not match the invoking Codex process")
    }
    let invoking_run = |r: &Run| {
        (ancestry.contains(&r.pid) && process::start_time(r.pid) == Some(r.start_time))
            || r.child_pid.zip(r.child_start).is_some_and(|(pid, start)| {
                ancestry.contains(&pid) && process::start_time(pid) == Some(start)
            })
    };
    let id = if let Some(id) = context_item.or(item) {
        id
    } else {
        let by_process: Vec<_> = state
            .entries
            .iter()
            .filter(|e| {
                state
                    .runs
                    .get(&e.id)
                    .is_some_and(|r| !r.ended && r.live() && invoking_run(r))
            })
            .collect();
        if by_process.len() == 1 {
            by_process[0].id
        } else {
            let exact: Vec<_> = state
                .entries
                .iter()
                .filter(|e| {
                    e.agent == Agent::Codex
                        && e.session_id == Some(session)
                        && e.provider_home
                            .as_ref()
                            .is_none_or(|h| h.canonicalize().ok().as_ref() == Some(&home))
                })
                .collect();
            if exact.len() == 1 {
                exact[0].id
            } else if let Some(root) = root {
                let entries: Vec<_> = state.entries.iter().filter(|e| e.imported).collect();
                let linked = unique_matches(&entries, &roots);
                linked.iter().find(|(_,r)| r.pid == root.pid).map(|(e,_)| e.id).context("Saved tab is ambiguous; run !gws bind-here --item WORK_ID (find IDs with gws list)")?
            } else {
                bail!(
                    "No unique saved item for this conversation; run gws save first, or use !gws bind-here --item WORK_ID"
                )
            }
        }
    };
    let ensure_unclaimed = |s: &State| -> Result<()> {
        if let Some((owner, _)) = s
            .runs
            .iter()
            .find(|(owner, run)| **owner != id && run.live() && invoking_run(run))
        {
            bail!("Invoking execution is already owned by work item {owner}; no binding changed")
        }
        Ok(())
    };
    ensure_unclaimed(&state)?;
    let entry = state.entry(id)?;
    if entry.agent != Agent::Codex {
        bail!("Selected work item is not Codex")
    }
    if entry.cwd != conversation.cwd
        && entry
            .cwd
            .canonicalize()
            .ok()
            .zip(conversation.cwd.canonicalize().ok())
            .is_none_or(|(a, b)| a != b)
    {
        bail!("Selected item belongs to a different project directory")
    }
    if entry
        .provider_home
        .as_ref()
        .is_some_and(|h| h.canonicalize().ok().as_ref() != Some(&home))
    {
        bail!("Selected item belongs to a different Codex home")
    }
    if let Some(run) = state.runs.get(&id).filter(|r| r.live()) {
        let same_thread =
            context_item == Some(id) && entry.session_verified && entry.session_id == Some(session);
        if !same_thread && !invoking_run(run) {
            bail!("Selected item has another live execution; no binding changed")
        }
    }
    if entry.lease.as_ref().is_some_and(|l| l.live()) {
        bail!("Work is reserved by an operation")
    }
    let expected_run = state.runs.get(&id).map(|r| r.token);
    if !dry_run {
        store.update(|s| {
            ensure_unclaimed(s)?;
            let current = s.entry(id)?;
            if current.revision != entry.revision
                || current.binding_key() != entry.binding_key()
                || s.runs.get(&id).map(|r| r.token) != expected_run
            {
                bail!("Work changed during binding; retry !gws bind-here")
            }
            if current.lease.as_ref().is_some_and(|l| l.live()) {
                bail!("Work is reserved by an operation")
            }
            if context_item.is_some() && !s.runs.get(&id).is_some_and(|r| !r.ended && r.live()) {
                bail!("Current gws run ended during binding")
            }
            let checked = crate::catalog::metadata(&transcript, &home, false)
                .context("Conversation metadata became unavailable")?;
            if checked.id != session
                || checked.cwd != conversation.cwd
                || checked.subagent
                || checked.parent.is_some()
            {
                bail!("Conversation metadata changed during binding")
            }
            if let Some(r) = root {
                if process::start_time(r.pid) != Some(r.start) {
                    bail!("Invoking Codex process changed")
                }
                if s.runs.get(&id).is_none_or(|r| !r.live()) {
                    s.runs.insert(
                        id,
                        Run {
                            token: Uuid::new_v4(),
                            pid: r.pid,
                            start_time: r.start,
                            created: crate::model::now(),
                            boot: crate::model::boot_id(),
                            terminal_id: entry.terminal_id.clone(),
                            activity: "unknown".into(),
                            ..Default::default()
                        },
                    );
                }
            }
            let e = s
                .entries
                .iter_mut()
                .find(|e| e.id == id)
                .context("Work item disappeared")?;
            e.session_id = Some(session);
            e.session_verified = true;
            e.provider_home = Some(home.clone());
            e.transcript_path = Some(transcript.clone());
            e.ever_started = true;
            Ok(())
        })?;
    }
    Ok(
        serde_json::json!({"schema_version":1,"item_id":id,"name":entry.name,"conversation_id":session,"state":if dry_run{"would_bind"}else{"bound"},"source":if item.is_some() || thread.is_none() { "explicit_item_or_session + root_transcript_metadata" } else { "current_codex_context + root_transcript_metadata" },"hooks_attached":false}),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn ancestry_includes_login_bridge_and_excludes_nested_codex_workers() {
        let parents = [
            (1, (0, "launchd".into())),
            (10, (1, "ghostty".into())),
            (11, (10, "login".into())),
            (12, (11, "zsh".into())),
            (13, (12, "codex".into())),
            (14, (13, "codex-code-mode-host".into())),
            (15, (14, "codex".into())),
        ]
        .into_iter()
        .collect();
        assert!(ghostty_client(&parents, 13));
        assert!(!ghostty_client(&parents, 15));
        assert!(!ghostty_client(&parents, 99));
    }
    fn entry(name: &str) -> Entry {
        serde_json::from_value(serde_json::json!({"id":Uuid::new_v4(),"workspace":"test","name":name,"cwd":"/tmp/project","agent":"codex","session_id":null,"imported":true})).unwrap()
    }
    fn root(pid: u32, words: &str) -> Root {
        Root {
            pid,
            start: 100,
            cwd: "/tmp/project".into(),
            signature: signature(words.split_whitespace()),
            sessions: BTreeSet::new(),
        }
    }
    #[test]
    fn reciprocal_matching_refuses_identical_tabs_and_identical_processes() {
        let a = entry("codex --no-daemon");
        let b = entry("codex --no-daemon");
        let c = entry("codex --no-daemon resume");
        let roots = vec![
            root(1, "--no-daemon"),
            root(2, "--no-daemon"),
            root(3, "--no-daemon resume"),
        ];
        let entries = vec![&a, &b, &c];
        let matches = unique_matches(&entries, &roots);
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].0.id, c.id);
        assert_eq!(matches[0].1.pid, 3);
        assert!(unique_matches(&[&a, &b], &[root(1, "--no-daemon")]).is_empty());
        assert!(unique_matches(&[&a], &roots[..2]).is_empty());
    }
    #[test]
    fn explicit_id_disambiguates_but_duplicate_ids_are_refused() {
        let mut a = entry("codex --no-daemon");
        let mut b = entry("codex --no-daemon");
        let session = Uuid::new_v4();
        a.session_id = Some(session);
        a.session_verified = true;
        let mut r = root(1, "--no-daemon");
        r.sessions.insert(session);
        let roots = vec![r, root(2, "--no-daemon")];
        let entries = vec![&a, &b];
        let matches = unique_matches(&entries, &roots);
        assert!(matches.iter().any(|(e, r)| e.id == a.id && r.pid == 1));
        b.session_id = Some(session);
        b.session_verified = true;
        assert!(unique_matches(&[&a, &b], &roots).is_empty());
    }
    #[test]
    fn live_root_replaces_a_stale_title_candidate_and_preserves_a_live_run_token() {
        let mut e = entry("codex --no-daemon resume");
        e.session_id = Some(Uuid::new_v4());
        assert!(e.needs_session());
        let id = e.id;
        let session = Uuid::new_v4();
        let mut state = State {
            entries: vec![e],
            ..State::default()
        };
        let r = root(1, "--no-daemon resume");
        apply_discovered(&mut state, &[(id, r.clone(), Some(session))]).unwrap();
        assert_eq!(state.entry(id).unwrap().session_id, Some(session));
        assert!(!state.entry(id).unwrap().needs_session());
        let token = state.runs[&id].token;
        apply_discovered(&mut state, &[(id, r, Some(session))]).unwrap();
        assert_eq!(state.runs[&id].token, token);
    }
    #[test]
    fn metadata_reader_accepts_only_bounded_root_cli_metadata() {
        let dir = tempfile::tempdir().unwrap();
        let sessions = dir.path().join("sessions");
        std::fs::create_dir(&sessions).unwrap();
        let path = sessions.join("fixture.jsonl");
        let id = Uuid::new_v4();
        let mut v = serde_json::json!({"type":"session_meta","payload":{"id":id,"source":"cli","cwd":"/tmp/project"}});
        std::fs::write(&path, format!("{}\nnot JSON conversation content", v)).unwrap();
        assert_eq!(root_metadata(path.to_str().unwrap()).unwrap().0, id);
        v["payload"]["source"] = serde_json::json!({"subagent":{"other":"guardian"}});
        std::fs::write(&path, v.to_string()).unwrap();
        assert!(root_metadata(path.to_str().unwrap()).is_none());
        std::fs::write(&path, " ".repeat(65537)).unwrap();
        assert!(root_metadata(path.to_str().unwrap()).is_none());
    }
}
