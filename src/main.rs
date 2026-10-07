mod agents;
mod ghostty;
mod model;
mod process;
mod ui;
use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use model::{Agent, Entry, Store};
use std::{collections::BTreeMap, path::PathBuf};
use uuid::Uuid;
#[derive(Parser)]
#[command(
    version,
    about = "Native Ghostty tabs, saved agent sessions, and process monitoring"
)]
struct Cli {
    #[arg(long, global = true)]
    state_dir: Option<PathBuf>,
    #[command(subcommand)]
    command: Option<Action>,
}
#[derive(Subcommand)]
enum Action {
    /// Open the workspace dashboard (also the default command).
    Dashboard,
    /// Open the dashboard in a new native Ghostty window.
    Launch,
    /// Save the current Ghostty windows and tabs without interrupting them.
    Save {
        #[arg(default_value = "default")]
        workspace: String,
    },
    /// Restore missing tabs. An agent without an exact session ID is reported and skipped.
    Restore {
        #[arg(default_value = "default")]
        workspace: String,
        #[arg(long)]
        dry_run: bool,
    },
    /// Create a saved, managed tab.
    New {
        agent: Agent,
        #[arg(long)]
        name: Option<String>,
        #[arg(long, default_value = "default")]
        workspace: String,
        #[arg(long)]
        cwd: Option<PathBuf>,
        #[arg(long)]
        session: Option<Uuid>,
        /// Give Codex a private server so its process tree can be attributed to this tab.
        #[arg(long)]
        isolated: bool,
        #[arg(last = true)]
        args: Vec<String>,
    },
    /// Show saved tabs, IDs, session continuity, and owned process metrics.
    List {
        #[arg(long)]
        json: bool,
    },
    /// Resume one saved tab (or focus it if it is already open).
    Open { id: Uuid },
    /// Focus an existing tab.
    Focus { id: Uuid },
    /// Close a tab through Ghostty, retaining its saved conversation and directory.
    Park { id: Uuid },
    /// Attach an exact conversation ID to a saved tab whose ID could not be imported.
    Bind { id: Uuid, session: Uuid },
    /// Remove a saved tab from the workspace. Does not stop processes or delete agent history.
    Forget { id: Uuid },
    /// Print integration health and storage location.
    Doctor,
    #[command(hide = true)]
    Run { id: Uuid, token: Uuid },
    #[command(hide = true)]
    Hook { id: Uuid, token: Uuid, agent: Agent },
}
fn main() {
    let cli = Cli::parse();
    let hook = matches!(cli.command, Some(Action::Hook { .. }));
    let result = Store::new(cli.state_dir)
        .and_then(|s| dispatch(s, cli.command.unwrap_or(Action::Dashboard)));
    match result {
        Ok(code) => std::process::exit(code),
        Err(e) => {
            if hook {
                std::process::exit(0)
            } // Monitoring is advisory and never blocks the agent.
            eprintln!("gws: {e:#}");
            std::process::exit(1)
        }
    }
}
fn dispatch(store: Store, action: Action) -> Result<i32> {
    match action {
        Action::Dashboard => ui::run(&store)?,
        Action::Launch => {
            let command = format!(
                "{} --state-dir {} dashboard",
                ghostty::shell_quote(&std::env::current_exe()?.to_string_lossy()),
                ghostty::shell_quote(&store.dir.to_string_lossy())
            );
            ghostty::open(
                &std::env::current_dir()?.to_string_lossy(),
                &command,
                "Ghostty Workspaces",
                None,
            )?;
        }
        Action::Save { workspace } => save(&store, &workspace)?,
        Action::Restore { workspace, dry_run } => restore(&store, &workspace, dry_run)?,
        Action::New {
            agent,
            name,
            workspace,
            cwd,
            session,
            isolated,
            args,
        } => {
            let cwd = cwd
                .unwrap_or(std::env::current_dir()?)
                .canonicalize()
                .context("Working directory does not exist")?;
            if !cwd.is_dir() {
                bail!("Working directory must be a directory")
            }
            if matches!(agent, Agent::Shell | Agent::Command) && session.is_some() {
                bail!("Only agent tabs have conversation IDs")
            }
            if agent == Agent::Command && args.is_empty() {
                bail!("Pass the executable and arguments after --")
            }
            let name = name.unwrap_or_else(|| {
                format!(
                    "{} · {}",
                    cwd.file_name().unwrap_or_default().to_string_lossy(),
                    agent
                )
            });
            let e = Entry {
                id: Uuid::new_v4(),
                workspace,
                name,
                cwd,
                agent,
                session_id: session,
                args: if agent == Agent::Command {
                    vec![]
                } else {
                    args.clone()
                },
                command: if agent == Agent::Command {
                    args
                } else {
                    vec![]
                },
                isolated,
                window: 0,
                terminal_id: None,
                imported: false,
                ever_started: false,
            };
            store.update(|s| {
                s.entries.push(e.clone());
                Ok(())
            })?;
            open_entry(&store, e.id, None)?;
            println!("Saved {} ({})", e.name, e.id);
        }
        Action::List { json } => ui::list(&store, json)?,
        Action::Open { id } => {
            open_entry(&store, id, None)?;
        }
        Action::Focus { id } => ghostty::focus(&terminal_id(&store, id)?)?,
        Action::Park { id } => {
            park_entry(&store, id)?;
            println!("Tab closed; saved session retained.");
        }
        Action::Bind { id, session } => {
            store.update(|s| {
                if !matches!(s.entry(id)?.agent, Agent::Codex | Agent::Claude) {
                    bail!("Only agent tabs have conversation IDs")
                }
                s.entries
                    .iter_mut()
                    .find(|e| e.id == id)
                    .context("Unknown saved tab")?
                    .session_id = Some(session);
                Ok(())
            })?;
            println!("Bound {id} to {session}");
        }
        Action::Forget { id } => {
            store.update(|s| {
                s.entry(id)?;
                s.entries.retain(|e| e.id != id);
                s.runs.remove(&id);
                Ok(())
            })?;
        }
        Action::Doctor => {
            let version = ghostty::jxa("Application('Ghostty').version()")?;
            println!(
                "Ghostty {version}\nState: {}\n{} saved tabs",
                store.dir.display(),
                store.read()?.entries.len()
            );
            println!(
                "Process access: {}",
                if process::start_time(std::process::id()).is_some() {
                    "available"
                } else {
                    "unavailable"
                }
            );
            println!(
                "Managed launches add scoped hooks; Codex may require trusting them in /hooks."
            );
        }
        Action::Run { id, token } => return agents::launch(&store, id, token),
        Action::Hook { id, token, agent } => agents::hook(&store, id, token, agent)?,
    }
    Ok(0)
}
pub fn terminal_id(store: &Store, id: Uuid) -> Result<String> {
    store
        .read()?
        .entry(id)?
        .terminal_id
        .clone()
        .context("Tab has no live Ghostty terminal; use gws open")
}
pub fn park_entry(store: &Store, id: Uuid) -> Result<()> {
    let state = store.read()?;
    let entry = state.entry(id)?;
    if matches!(entry.agent, Agent::Codex | Agent::Claude) && entry.session_id.is_none() {
        bail!(
            "Capture or bind this tab's conversation ID before parking it: gws bind {id} <session-id>"
        )
    }
    ghostty::close(&terminal_id(store, id)?)
}
pub fn open_entry(store: &Store, id: Uuid, window: Option<&str>) -> Result<String> {
    let state = store.read()?;
    let entry = state.entry(id)?.clone();
    let windows = ghostty::snapshot()?;
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
    if let Some(r) = state.runs.get(&id)
        && !r.ended
        && process::start_time(r.pid) == Some(r.start_time)
    {
        bail!("The launcher is already running; refusing to start a duplicate agent")
    }
    if (entry.imported || entry.ever_started)
        && matches!(entry.agent, Agent::Codex | Agent::Claude)
        && entry.session_id.is_none()
    {
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
    let token = Uuid::new_v4();
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
    let (win, terminal) = ghostty::open(
        &entry.cwd.to_string_lossy(),
        &command,
        &entry.name,
        target.as_deref(),
    )?;
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
fn save(store: &Store, workspace: &str) -> Result<()> {
    let windows = ghostty::snapshot()?;
    let mut count = 0;
    let mut missing = 0;
    let mut splits = 0;
    store.update(|s| {
        let mut entries = vec![];
        for (group, w) in windows.iter().enumerate() {
            for tab in &w.tabs {
                if tab.name == "Ghostty Workspaces" {
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
                        e.cwd = PathBuf::from(&p.cwd);
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
                            args: vec![],
                            command: vec![],
                            isolated: tab.name.contains("--no-daemon"),
                            window: group,
                            terminal_id: Some(p.id.clone()),
                            imported: true,
                            ever_started: false,
                        }
                    };
                    if matches!(e.agent, Agent::Codex | Agent::Claude) && e.session_id.is_none() {
                        missing += 1;
                    }
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
    println!(
        "Saved {count} terminals across {} windows to {workspace}.",
        windows.len()
    );
    if missing > 0 {
        println!("{missing} agent tabs need an exact session ID before restoration. Use gws bind.");
    }
    if splits > 0 {
        println!(
            "{splits} split tabs saved as individual tabs; split geometry is not restored in v0.1."
        );
    }
    Ok(())
}
fn restore(store: &Store, workspace: &str, dry: bool) -> Result<()> {
    let state = store.read()?;
    let entries: Vec<_> = state
        .entries
        .iter()
        .filter(|e| e.workspace == workspace)
        .collect();
    if entries.is_empty() {
        bail!("No saved workspace named {workspace}")
    }
    let windows = ghostty::snapshot()?;
    let mut groups: BTreeMap<usize, String> = BTreeMap::new();
    let mut skipped = 0;
    for e in entries {
        if let Some(id) = &e.terminal_id
            && let Some(w) = windows.iter().find(|w| {
                w.tabs
                    .iter()
                    .any(|t| t.terminals.iter().any(|p| &p.id == id))
            })
        {
            groups.insert(e.window, w.id.clone());
            println!("Open: {}", e.name);
            continue;
        }
        if (e.imported || e.ever_started)
            && matches!(e.agent, Agent::Codex | Agent::Claude)
            && e.session_id.is_none()
        {
            println!("Needs session ID: {} ({})", e.name, e.id);
            skipped += 1;
            continue;
        }
        println!(
            "{}: {} [{}]",
            if dry { "Would restore" } else { "Restoring" },
            e.name,
            e.agent
        );
        if !dry {
            let win = open_entry(store, e.id, groups.get(&e.window).map(String::as_str))?;
            groups.insert(e.window, win);
        }
    }
    if skipped > 0 {
        bail!("{skipped} tabs could not be restored without their exact conversation IDs")
    }
    Ok(())
}
