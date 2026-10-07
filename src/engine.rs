use crate::{
    agents, binding, ghostty,
    model::{Agent, Entry, Store},
    process,
};
use anyhow::{Context, Result, bail};
use std::path::PathBuf;
use uuid::Uuid;
pub fn terminal_id(store: &Store, id: Uuid) -> Result<String> {
    store
        .read()?
        .entry(id)?
        .terminal_id
        .clone()
        .context("Tab has no live Ghostty terminal; use gws open")
}
pub fn open_entry(store: &Store, id: Uuid, window: Option<&str>) -> Result<String> {
    open_entry_for(store, id, window, None)
}
pub fn open_entry_for(
    store: &Store,
    id: Uuid,
    window: Option<&str>,
    operation: Option<Uuid>,
) -> Result<String> {
    let state = store.read()?;
    let entry = state.entry(id)?.clone();
    let windows = ghostty::snapshot()?;
    if let Some(r) = state.runs.get(&id)
        && r.supervised
        && r.ended
        && crate::supervisor::socket_path(r.token).exists()
    {
        crate::supervisor::request(
            r.token,
            &operation
                .map(|id| format!("resume:{id}"))
                .unwrap_or("resume".into()),
        )?;
        if let Some(t) = &entry.terminal_id {
            ghostty::focus(t)?;
        }
        return Ok(String::new());
    }
    if let Some(t) = &entry.terminal_id
        && ghostty::terminal_exists(&windows, t)
    {
        ghostty::focus(t)?;
        return windows
            .iter()
            .find(|w| {
                w.tabs
                    .iter()
                    .any(|t2| t2.terminals.iter().any(|p| &p.id == t))
            })
            .map(|w| w.id.clone())
            .context("Missing window");
    }
    if state.runs.get(&id).is_some_and(|r| r.live()) {
        bail!(
            "Execution is active or pending without a matching terminal; inspect before relaunching"
        )
    }
    if entry.needs_session() {
        bail!(
            "{} needs an exact session ID: gws bind {} <session-id>",
            entry.name,
            id
        )
    }
    if !entry.cwd.is_dir() {
        bail!("Working directory is missing: {}", entry.cwd.display())
    }
    let exe = std::env::current_exe()?;
    let token = authorize_launch_for(store, id, operation)?;
    let command = format!(
        "{} --state-dir {} run {} {}",
        ghostty::shell_quote(&exe.to_string_lossy()),
        ghostty::shell_quote(&store.dir.to_string_lossy()),
        id,
        token
    );
    let target = window.map(str::to_owned).or_else(|| {
        state
            .entries
            .iter()
            .filter(|e| e.workspace == entry.workspace && e.window == entry.window)
            .filter_map(|e| e.terminal_id.as_ref())
            .find_map(|id| {
                windows
                    .iter()
                    .find(|w| {
                        w.tabs
                            .iter()
                            .any(|t| t.terminals.iter().any(|p| &p.id == id))
                    })
                    .map(|w| w.id.clone())
            })
    });
    let opened = ghostty::open(
        &entry.cwd.to_string_lossy(),
        &command,
        &entry.name,
        target.as_deref(),
    );
    let (win, terminal) = match opened {
        Ok(v) => v,
        Err(e) => {
            store.update(|s| {
                if let Some(r) = s.runs.get_mut(&id).filter(|r| r.token == token) {
                    if !r.consumed {
                        r.ended = true;
                    }
                    r.last_error = Some(format!("Terminal creation outcome uncertain: {e}"));
                }
                Ok(())
            })?;
            return Err(e);
        }
    };
    store.update(|s| {
        s.entries
            .iter_mut()
            .find(|e| e.id == id)
            .context("Tab removed during launch")?
            .terminal_id = Some(terminal.clone());
        if let Some(r) = s.runs.get_mut(&id).filter(|r| r.token == token) {
            r.terminal_id = Some(terminal);
        }
        Ok(())
    })?;
    Ok(win)
}
pub fn save(store: &Store, workspace: &str) -> Result<()> {
    save_report(store, workspace, true)
}
pub fn save_quiet(store: &Store, workspace: &str) -> Result<()> {
    save_report(store, workspace, false)
}
fn save_report(store: &Store, workspace: &str, report: bool) -> Result<()> {
    let windows = ghostty::snapshot()?;
    let mut count = 0;
    let mut splits = 0;
    store.update(|s| {
        let mut entries = vec![];
        for (group, w) in windows.iter().enumerate() {
            for tab in &w.tabs {
                if matches!(
                    tab.name.as_str(),
                    "Ghostty Workspaces" | "gws" | "gws dashboard"
                ) {
                    continue;
                }
                if tab.terminals.len() > 1 {
                    splits += 1;
                }
                for p in &tab.terminals {
                    let existing = s
                        .entries
                        .iter()
                        .find(|e| e.terminal_id.as_deref() == Some(&p.id));
                    let e = if let Some(existing) = existing {
                        let mut e = existing.clone();
                        e.workspace = workspace.into();
                        e.window = group;
                        if e.imported {
                            e.cwd = PathBuf::from(&p.cwd);
                            let name = tab.name.replace(['\n', '\r', '\t'], " ");
                            if e.name != name {
                                e.session_verified = false;
                            }
                            e.name = name;
                            e.isolated = tab.name.split_whitespace().any(|s| s == "--no-daemon");
                        }
                        if !e.session_verified
                            && matches!(e.agent, Agent::Codex | Agent::Claude | Agent::Cursor)
                        {
                            e.session_id = agents::session_from_title(&tab.name);
                        }
                        e
                    } else {
                        let agent = if tab.name.split_whitespace().any(|s| s == "codex") {
                            Agent::Codex
                        } else if tab.name.split_whitespace().any(|s| s == "claude") {
                            Agent::Claude
                        } else {
                            Agent::Shell
                        };
                        Entry {
                            id: Uuid::new_v4(),
                            workspace: workspace.into(),
                            name: tab.name.replace(['\n', '\r', '\t'], " "),
                            cwd: PathBuf::from(&p.cwd),
                            agent,
                            session_id: if matches!(agent, Agent::Codex | Agent::Claude) {
                                agents::session_from_title(&tab.name)
                            } else {
                                None
                            },
                            session_verified: false,
                            args: vec![],
                            command: vec![],
                            isolated: tab.name.contains("--no-daemon"),
                            window: group,
                            terminal_id: Some(p.id.clone()),
                            imported: true,
                            ever_started: false,
                            ..Entry::default()
                        }
                    };
                    entries.push(e);
                    count += 1;
                }
            }
        }
        let ids: Vec<_> = entries.iter().map(|e| e.id).collect();
        // Preserve inactive saved tabs; only `forget` removes a workspace entry.
        s.entries.retain(|e| !ids.contains(&e.id));
        s.entries.extend(entries);
        let saved: Vec<_> = s.entries.iter().map(|e| e.id).collect();
        s.runs.retain(|id, _| saved.contains(id));
        Ok(())
    })?;
    binding::automatic_report(store, false, report)?;
    let missing = store
        .read()?
        .entries
        .iter()
        .filter(|e| e.workspace == workspace && e.needs_session())
        .count();
    if report {
        println!(
            "Saved {count} terminals across {} windows to {workspace}.",
            windows.len()
        );
    }
    if report && missing > 0 {
        println!("{missing} agent tabs need an exact session ID before restoration. Use gws bind.");
    }
    if report && splits > 0 {
        println!("{splits} split tabs saved as individual tabs; split geometry is not restored.");
    }
    Ok(())
}
pub fn authorize_launch(store: &Store, id: Uuid) -> Result<Uuid> {
    authorize_launch_for(store, id, None)
}
pub fn authorize_launch_for(store: &Store, id: Uuid, operation: Option<Uuid>) -> Result<Uuid> {
    let before = store.read()?;
    let dependencies = before
        .entry(id)?
        .dependencies
        .iter()
        .map(|dep| Ok((*dep, crate::operations::check_readiness(store, *dep)?)))
        .collect::<Result<Vec<_>>>()?;
    let token = Uuid::new_v4();
    store.update(|s| {
        if s.entry(id)?.dependencies != before.entry(id)?.dependencies {
            bail!("Dependencies changed during launch")
        }
        for (dep, expected) in &dependencies {
            let dependency = s.entry(*dep)?;
            if dependency
                .lease
                .as_ref()
                .is_some_and(|l| l.live() && Some(l.operation) != operation)
            {
                bail!("Required service reserved by another operation")
            }
            if !s
                .runs
                .get(dep)
                .is_some_and(|r| r.token == *expected && r.live())
            {
                bail!("Required service changed during launch")
            }
        }
        let e = s.entry(id)?;
        if e.lease
            .as_ref()
            .is_some_and(|l| l.live() && Some(l.operation) != operation)
        {
            bail!("Work is reserved by another operation")
        };
        if e.needs_session() {
            bail!("Exact conversation binding required")
        }
        if !e.cwd.is_dir() {
            bail!("Working directory is missing: {}", e.cwd.display())
        }
        if e.ever_started
            && matches!(e.agent, Agent::Codex | Agent::Claude | Agent::Cursor)
            && e.resume_args.is_none()
        {
            agents::checked_resume_options(e.agent, &e.args)?;
        }
        if let Some(r) = s.runs.get(&id) {
            if r.live() {
                bail!("This item already has an active or pending run")
            };
            if r.survivors
                .iter()
                .any(|(pid, start)| process::start_time(*pid) == Some(*start))
            {
                bail!("Owned survivors remain; inspect and resolve before relaunching")
            }
        }
        let terminal = s.entry(id)?.terminal_id.clone();
        let owner_run = s
            .entry(id)?
            .service
            .as_ref()
            .and_then(|svc| s.runs.get(&svc.owner).map(|r| r.token));
        s.runs.insert(
            id,
            crate::model::Run {
                token,
                created: crate::model::now(),
                boot: crate::model::boot_id(),
                terminal_id: terminal,
                supervised: true,
                owner_run,
                activity: "unknown".into(),
                ..Default::default()
            },
        );
        let e = s
            .entries
            .iter_mut()
            .find(|e| e.id == id)
            .context("Unknown item")?;
        e.intent = crate::model::Intent::Active;
        let services: Vec<_> = s
            .entries
            .iter()
            .filter(|e| {
                e.service.as_ref().is_some_and(|svc| {
                    svc.owner == id && svc.lifetime == crate::model::Lifetime::Run
                })
            })
            .map(|e| e.id)
            .collect();
        for service in services {
            if let Some(run) = s.runs.get_mut(&service) {
                run.owner_run = Some(token);
            }
        }
        Ok(token)
    })?;
    if let Some(operation) = operation {
        crate::operations::record_launch(store, operation, id, token)?;
    }
    Ok(token)
}
