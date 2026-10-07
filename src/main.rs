use anyhow::{Context, Result, bail};
use clap::{Args, Parser, Subcommand};
use engine::{open_entry, save};
use ghostty_workspaces::{
    agents, binding, catalog, descriptions, engine, ghostty, hardware, model, operations, process,
    ui,
};
use model::{Agent, Entry, Store};
use std::path::PathBuf;
use uuid::Uuid;
#[derive(Parser)]
#[command(
    version,
    about = "Native Ghostty tabs, saved agent sessions, and process monitoring"
)]
struct Cli {
    #[arg(long, global = true)]
    state_dir: Option<PathBuf>,
    /// Add a read-only macmon stream from an SSH host (repeat for additional devices).
    #[arg(long, global = true, value_name = "HOST")]
    ssh: Vec<String>,
    #[arg(long, global = true)]
    json: bool,
    #[arg(long,global=true,value_enum,default_value_t=ui::Theme::Native)]
    theme: ui::Theme,
    #[arg(long, global = true)]
    ascii: bool,
    #[arg(long, global = true)]
    caller: Option<operations::Caller>,
    /// Explicit human caller context for scripts outside a managed session.
    #[arg(long, global = true)]
    human: bool,
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
        #[arg(long, alias = "dry-run")]
        preview: bool,
        #[arg(long)]
        batch: Option<Uuid>,
    },
    /// Start a saved Codex tab in the current directory or a supplied project directory.
    Codex(LaunchOptions),
    /// Start a saved Claude Code tab in the current directory or a supplied project directory.
    Claude(LaunchOptions),
    /// Start Cursor CLI with an exact chat identity.
    Cursor(LaunchOptions),
    /// Start a saved shell tab.
    Shell(LaunchOptions),
    /// Start a saved command tab; pass its executable and arguments after --.
    Command(LaunchOptions),
    /// Create a saved, managed tab (compatibility spelling).
    New {
        agent: Agent,
        #[command(flatten)]
        options: LaunchOptions,
    },
    /// Show saved tabs, IDs, session continuity, and owned process metrics.
    List,
    /// Read durable run history without starting collectors or providers.
    History {
        #[arg(long)]
        item: Option<Uuid>,
        #[arg(long, default_value_t = 25)]
        limit: usize,
        #[arg(long, default_value_t = 0)]
        cursor: usize,
    },
    /// Resume one saved tab (or focus it if it is already open).
    Open { id: Uuid },
    /// Focus an existing tab.
    Focus { id: Uuid },
    /// Close a tab through Ghostty, retaining its saved conversation and directory.
    Park {
        id: Uuid,
        #[arg(long)]
        preview: bool,
        #[arg(long, conflicts_with = "keep_tab")]
        close_tab: bool,
        #[arg(long)]
        keep_tab: bool,
    },
    /// Inspect lifecycle, evidence, capabilities and owned processes.
    Inspect { id: Uuid },
    /// Search conversations across project directories without launching agents.
    #[command(alias = "search")]
    Sessions {
        #[arg(long, default_value = "")]
        query: String,
        #[arg(long, default_value_t = 25)]
        limit: usize,
        #[arg(long, default_value_t = 0)]
        cursor: usize,
        #[arg(long)]
        semantic: bool,
    },
    /// Configure optional Jev reranking; stores the credential file path, never its key.
    SearchConfig {
        #[arg(long)]
        env_file: Option<PathBuf>,
        #[arg(long, default_value = "jev-latest")]
        model: String,
        #[arg(long, default_value_t = 0.6)]
        min_confidence: f64,
    },
    /// Create a reviewed profiling preparation plan; does not stop anything.
    Quiet {
        #[arg(long)]
        only: Vec<Uuid>,
        #[arg(long)]
        exclude: Vec<Uuid>,
        #[arg(long)]
        preview: bool,
        #[arg(long)]
        keep_tabs: bool,
    },
    /// Apply exactly the targets and preconditions of a saved plan.
    Apply {
        plan: Uuid,
        #[arg(long)]
        wait: bool,
        #[arg(long)]
        retry: bool,
    },
    /// Read a recorded operation without starting collectors.
    Operation {
        id: Uuid,
        #[arg(long)]
        cancel: bool,
    },
    /// Inspect whole-Mac resource groups. Age and duplication do not imply safe cleanup.
    Audit {
        #[arg(long)]
        save: bool,
    },
    /// Finish work and stop its eligible run/work-scoped services.
    Finish {
        id: Uuid,
        #[arg(long)]
        preview: bool,
    },
    /// Register and control deliberately managed project services.
    Service {
        #[command(subcommand)]
        action: ServiceAction,
    },
    /// Pause or explicitly restart all registered dashboard collectors.
    Monitoring {
        #[command(subcommand)]
        action: MonitoringAction,
    },
    /// Generate concise transcript-backed descriptions on request; no background model runs.
    Describe {
        #[arg(required_unless_present = "all", conflicts_with = "all")]
        id: Option<Uuid>,
        #[arg(long)]
        all: bool,
        #[arg(long)]
        transcript: Option<PathBuf>,
        #[arg(long, default_value = "gpt-6-luna")]
        model: String,
    },
    /// On-demand files/sockets, startup or sleep assertion inspection.
    Diagnostic {
        kind: String,
        #[arg(long)]
        pid: Option<u32>,
    },
    /// Manage an existing catalog conversation and resume its exact ID/cwd.
    Conversation { id: Uuid },
    #[command(hide = true)]
    Worker {
        plan: Uuid,
        #[arg(long)]
        retry: bool,
    },
    /// Attach an exact conversation ID to a saved tab whose ID could not be imported.
    Bind {
        #[arg(
            required_unless_present_any = ["all", "auto"],
            conflicts_with_all = ["all", "auto"],
            requires = "session"
        )]
        id: Option<Uuid>,
        #[arg(requires = "id")]
        session: Option<String>,
        /// Walk through unresolved agent tabs, accepting exact IDs or skipping each one.
        #[arg(long)]
        all: bool,
        /// Bind unique live Codex matches from process ancestry and open root metadata.
        #[arg(long, conflicts_with = "all")]
        auto: bool,
        #[arg(long, conflicts_with = "id")]
        dry_run: bool,
    },
    /// Print current hardware samples as JSON; missing or disconnected sensors are explicit.
    Sensors,
    /// Remove a saved tab from the workspace. Does not stop processes or delete agent history.
    Forget { id: Uuid },
    /// Print integration health and storage location.
    Doctor,
    #[command(hide = true)]
    Run { id: Uuid, token: Uuid },
    #[command(hide = true)]
    ExecGate {
        fd: i32,
        #[arg(last = true, required = true)]
        argv: Vec<std::ffi::OsString>,
    },
    #[command(hide = true)]
    Hook { id: Uuid, token: Uuid, agent: Agent },
}
#[derive(Subcommand)]
enum ServiceAction {
    Register {
        #[arg(long, required_unless_present = "stdin")]
        file: Option<PathBuf>,
        #[arg(long)]
        stdin: bool,
    },
    Start {
        id: Uuid,
    },
    Stop {
        id: Uuid,
    },
    Adopt {
        id: Uuid,
        #[arg(long)]
        pid: u32,
        #[arg(long)]
        start_time: u64,
    },
}
#[derive(Subcommand)]
enum MonitoringAction {
    Pause,
    Resume,
}
#[derive(Args, Debug)]
#[command(subcommand_precedence_over_arg = true)]
struct LaunchOptions {
    /// Project directory; defaults to the directory where gws is invoked.
    #[arg(value_name = "DIRECTORY", conflicts_with = "cwd")]
    directory: Option<PathBuf>,
    #[arg(long, global = true)]
    name: Option<String>,
    #[arg(long, global = true, default_value = "default")]
    workspace: String,
    /// Alternative spelling for the project directory.
    #[arg(short = 'C', long, global = true, value_name = "DIRECTORY")]
    cwd: Option<PathBuf>,
    #[arg(long, global = true)]
    session: Option<Uuid>,
    /// Opaque Cursor conversation ID.
    #[arg(long, global = true, conflicts_with = "session")]
    conversation: Option<String>,
    /// Give Codex a private server so its process tree can be attributed to this tab.
    #[arg(long = "no-daemon", alias = "isolated", global = true)]
    isolated: bool,
    #[command(subcommand)]
    subcommand: Option<AgentSubcommand>,
    /// Agent options, or a custom executable and its arguments, after --.
    #[arg(last = true)]
    args: Vec<String>,
}
#[derive(Subcommand, Debug)]
enum AgentSubcommand {
    /// Resume an agent conversation by ID/name, or open its session picker.
    Resume {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// Fork a Codex conversation by ID/name, or open its session picker.
    Fork {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
}
impl Action {
    fn normalize(self) -> Self {
        match self {
            Self::Codex(options) => Self::New {
                agent: Agent::Codex,
                options,
            },
            Self::Claude(options) => Self::New {
                agent: Agent::Claude,
                options,
            },
            Self::Cursor(options) => Self::New {
                agent: Agent::Cursor,
                options,
            },
            Self::Shell(options) => Self::New {
                agent: Agent::Shell,
                options,
            },
            Self::Command(options) => Self::New {
                agent: Agent::Command,
                options,
            },
            other => other,
        }
    }
}
impl LaunchOptions {
    fn project_directory(&self) -> Result<PathBuf> {
        let directory = self
            .directory
            .clone()
            .or_else(|| self.cwd.clone())
            .unwrap_or(std::env::current_dir()?);
        let directory = directory
            .canonicalize()
            .context("Working directory does not exist")?;
        if !directory.is_dir() {
            bail!("Working directory must be a directory")
        }
        Ok(directory)
    }
}
fn main() {
    let cli = Cli::parse();
    let hook = matches!(cli.command, Some(Action::Hook { .. }));
    let json = cli.json;
    use std::io::IsTerminal;
    let human = cli.human || std::io::stdin().is_terminal();
    let result = Store::new(cli.state_dir).and_then(|s| {
        dispatch(
            s,
            cli.command.unwrap_or(Action::Dashboard),
            cli.ssh,
            json,
            cli.caller,
            cli.theme,
            cli.ascii,
            human,
        )
    });
    match result {
        Ok(code) => std::process::exit(code),
        Err(e) => {
            if hook {
                std::process::exit(0)
            } // Monitoring is advisory and never blocks the agent.
            if json {
                println!(
                    "{}",
                    serde_json::json!({"schema_version":1,"ok":false,"error":{"code":"request_failed","message":e.to_string()}})
                );
            }
            eprintln!("gws: {e:#}");
            std::process::exit(1)
        }
    }
}
#[allow(clippy::too_many_arguments)]
fn dispatch(
    store: Store,
    action: Action,
    hosts: Vec<String>,
    json: bool,
    caller: Option<operations::Caller>,
    theme: ui::Theme,
    ascii: bool,
    human: bool,
) -> Result<i32> {
    hardware::validate_hosts(&hosts)?;
    match action.normalize() {
        Action::Dashboard => ui::run(&store, &hosts, theme, ascii)?,
        Action::Launch => {
            let remote = hosts
                .iter()
                .map(|h| format!(" --ssh {}", ghostty::shell_quote(h)))
                .collect::<String>();
            let command = format!(
                "{} --state-dir {} dashboard{}",
                ghostty::shell_quote(&std::env::current_exe()?.to_string_lossy()),
                ghostty::shell_quote(&store.dir.to_string_lossy()),
                remote
            );
            ghostty::open(
                &std::env::current_dir()?.to_string_lossy(),
                &command,
                "Ghostty Workspaces",
                None,
            )?;
        }
        Action::Save { workspace } => {
            if json {
                engine::save_quiet(&store, &workspace)?;
                emit(
                    &serde_json::json!({"schema_version":1,"workspace":workspace,"items":store.read()?.entries}),
                    true,
                )?;
            } else {
                save(&store, &workspace)?;
            }
        }
        Action::Restore {
            workspace,
            preview: _,
            batch,
        } => emit(
            &operations::preview_context(
                &store,
                operations::Action::Restore,
                &[],
                &[],
                caller,
                false,
                Some(&workspace),
                batch,
                human,
            )?,
            json,
        )?,
        Action::Codex(_)
        | Action::Claude(_)
        | Action::Cursor(_)
        | Action::Shell(_)
        | Action::Command(_) => {
            unreachable!("Shorthand launches are normalized to New")
        }
        Action::New { agent, options } => {
            let cwd = options.project_directory()?;
            let LaunchOptions {
                name,
                workspace,
                session,
                conversation,
                isolated,
                mut args,
                subcommand,
                ..
            } = options;
            if let Some(command) = subcommand {
                if session.is_some() || conversation.is_some() {
                    bail!("Use either --session or an agent subcommand")
                }
                match (agent, command) {
                    (Agent::Codex, AgentSubcommand::Resume { args: command_args }) => {
                        args = std::iter::once("resume".into())
                            .chain(command_args)
                            .chain(args)
                            .collect();
                    }
                    (Agent::Codex, AgentSubcommand::Fork { args: command_args }) => {
                        args = std::iter::once("fork".into())
                            .chain(command_args)
                            .chain(args)
                            .collect();
                    }
                    (Agent::Cursor, AgentSubcommand::Resume { args: command_args }) => {
                        args = std::iter::once("--resume".into())
                            .chain(command_args)
                            .chain(args)
                            .collect();
                    }
                    (Agent::Claude, AgentSubcommand::Resume { args: command_args }) => {
                        args = std::iter::once("--resume".into())
                            .chain(command_args)
                            .chain(args)
                            .collect();
                    }
                    _ => bail!("This agent does not support that managed subcommand"),
                }
            }
            let isolated = isolated || args.iter().any(|arg| arg == "--no-daemon");
            if isolated && agent != Agent::Codex {
                bail!("--no-daemon is a Codex option")
            }
            if agent != Agent::Cursor && conversation.is_some() {
                bail!("--conversation is a Cursor option")
            };
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
                session_verified: session.is_some() || conversation.is_some(),
                provider_session: conversation,
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
                provider_home: if agent == Agent::Codex {
                    Some(
                        std::env::var_os("CODEX_HOME")
                            .map(PathBuf::from)
                            .unwrap_or(model::home()?.join(".codex")),
                    )
                } else if agent == Agent::Claude {
                    Some(
                        std::env::var_os("CLAUDE_CONFIG_DIR")
                            .map(PathBuf::from)
                            .unwrap_or(model::home()?.join(".claude")),
                    )
                } else {
                    None
                },
                ..Entry::default()
            };
            store.update(|s| {
                s.entries.push(e.clone());
                Ok(())
            })?;
            open_entry(&store, e.id, None)?;
            if json {
                emit(
                    &serde_json::json!({"schema_version":1,"item_id":e.id,"state":"launch_requested"}),
                    true,
                )?;
            } else {
                println!("Saved {} ({})", e.name, e.id);
            }
        }
        Action::List => ui::list(&store, json)?,
        Action::Open { id } => {
            open_entry(&store, id, None)?;
        }
        Action::Focus { id } => ghostty::focus(&engine::terminal_id(&store, id)?)?,
        Action::Park {
            id,
            preview: _,
            close_tab,
            keep_tab: _,
        } => emit(
            &operations::preview_context(
                &store,
                operations::Action::Park,
                &[id],
                &[],
                caller,
                close_tab,
                None,
                None,
                human,
            )?,
            json,
        )?,
        Action::Inspect { id } => emit(&operations::inspect(&store, id)?, json)?,
        Action::SearchConfig {
            env_file,
            model,
            min_confidence,
        } => {
            let config = ghostty_workspaces::search::Config {
                env_file: env_file.map(std::fs::canonicalize).transpose()?,
                model,
                min_confidence,
            };
            ghostty_workspaces::search::configure(&store, &config)?;
            emit(
                &serde_json::json!({"schema_version":1,"config":config,"credential_stored":false}),
                json,
            )?;
        }
        Action::Sessions {
            query,
            limit,
            cursor,
            semantic,
        } => emit(
            &catalog::search_with_backend(&store, &query, limit, cursor, semantic)?,
            json,
        )?,
        Action::Quiet {
            only,
            exclude,
            preview: _,
            keep_tabs,
        } => emit(
            &operations::preview_context(
                &store,
                operations::Action::Quiet,
                &only,
                &exclude,
                caller,
                !keep_tabs,
                None,
                None,
                human,
            )?,
            json,
        )?,
        Action::Finish { id, preview: _ } => emit(
            &operations::preview_context(
                &store,
                operations::Action::Finish,
                &[id],
                &[],
                caller,
                false,
                None,
                None,
                human,
            )?,
            json,
        )?,
        Action::Apply { plan, wait, retry } => {
            if json && !wait {
                let accepted = operations::accept(&store, plan, retry)?;
                if accepted.state != "accepted" {
                    emit(&operations::operation(&store, plan)?, true)?;
                    return Ok(0);
                }
                let exe = std::env::current_exe()?;
                let mut c = std::process::Command::new(exe);
                c.args([
                    "--state-dir",
                    store.dir.to_str().context("Non-UTF8 state directory")?,
                    "worker",
                    &plan.to_string(),
                ]);
                if retry {
                    c.arg("--retry");
                }
                c.stdin(std::process::Stdio::null())
                    .stdout(std::process::Stdio::null())
                    .stderr(std::process::Stdio::null());
                use std::os::unix::process::CommandExt;
                c.process_group(0);
                if let Err(e) = c.spawn() {
                    let mut op = accepted;
                    op.state = "failed".into();
                    for step in &mut op.steps {
                        step.state = "failed".into();
                        step.error = Some(format!("Worker launch failed: {e}"));
                    }
                    store.put("operations", &plan.to_string(), &op)?;
                    return Err(e.into());
                }
                emit(
                    &serde_json::json!({"schema_version":1,"state":"accepted","operation_id":plan}),
                    true,
                )?;
            } else {
                let op = operations::apply(&store, plan, retry)?;
                emit(&op, json)?;
                if op.state != "succeeded" {
                    return Ok(1);
                }
            }
        }
        Action::Worker { plan, retry } => {
            let op = operations::apply(&store, plan, retry)?;
            return Ok(if op.state == "succeeded" { 0 } else { 1 });
        }
        Action::Operation { id, cancel } => {
            if cancel {
                store.put("cancellations", &id.to_string(), &true)?;
            }
            emit(&operations::operation(&store, id)?, json)?;
        }
        Action::Audit { save } => emit(&operations::audit(&store, save)?, json)?,
        Action::Service { action } => match action {
            ServiceAction::Register { file, stdin: _ } => {
                let bytes = if let Some(path) = file {
                    std::fs::read(path)?
                } else {
                    use std::io::Read;
                    let mut bytes = vec![];
                    std::io::stdin().take(256 * 1024).read_to_end(&mut bytes)?;
                    bytes
                };
                let id = operations::register_service(&store, serde_json::from_slice(&bytes)?)?;
                emit(
                    &serde_json::json!({"schema_version":1,"item_id":id,"state":"registered"}),
                    json,
                )?;
            }
            ServiceAction::Start { id } => emit(
                &operations::preview_context(
                    &store,
                    operations::Action::Start,
                    &[id],
                    &[],
                    caller,
                    false,
                    None,
                    None,
                    human,
                )?,
                json,
            )?,
            ServiceAction::Stop { id } => emit(
                &operations::preview_context(
                    &store,
                    operations::Action::Stop,
                    &[id],
                    &[],
                    caller,
                    false,
                    None,
                    None,
                    human,
                )?,
                json,
            )?,
            ServiceAction::Adopt {
                id,
                pid,
                start_time,
            } => {
                operations::adopt_service(&store, id, pid, start_time)?;
                emit(
                    &serde_json::json!({"schema_version":1,"item_id":id,"state":"adopted"}),
                    json,
                )?;
            }
        },
        Action::Monitoring { action } => emit(
            &operations::set_monitoring(&store, matches!(action, MonitoringAction::Pause))?,
            json,
        )?,
        Action::Describe {
            id,
            all,
            transcript,
            model,
        } => {
            let ids = if all {
                store
                    .read()?
                    .entries
                    .iter()
                    .filter(|e| {
                        e.session_verified
                            && !e.needs_session()
                            && matches!(e.agent, Agent::Codex | Agent::Claude | Agent::Cursor)
                    })
                    .map(|e| e.id)
                    .collect::<Vec<_>>()
            } else {
                vec![id.context("Item ID required")?]
            };
            if all && transcript.is_some() {
                bail!("--transcript applies to one item only")
            };
            let mut results = vec![];
            for id in ids {
                eprintln!("gws: describing {id} with {model} / medium");
                match descriptions::generate(&store, id, transcript.clone(), &model) {
                    Ok(summary) => {
                        results.push(serde_json::json!({"item":id,"ok":true,"summary":summary}))
                    }
                    Err(e) => results
                        .push(serde_json::json!({"item":id,"ok":false,"error":e.to_string()})),
                }
            }
            let failed = results.iter().any(|v| v["ok"] != true);
            emit(
                &serde_json::json!({"schema_version":1,"results":results}),
                json,
            )?;
            if failed {
                return Ok(1);
            }
        }
        Action::Diagnostic { kind, pid } => emit(&operations::diagnostic(pid, &kind)?, json)?,
        Action::Conversation { id } => {
            let conversation = catalog::select(&store, id)?;
            let item = catalog::manage(&store, &conversation)?;
            open_entry(&store, item, None)?;
            emit(
                &serde_json::json!({"schema_version":1,"item_id":item}),
                json,
            )?;
        }
        Action::Bind {
            id,
            session,
            all,
            auto,
            dry_run,
        } => {
            if json && all {
                bail!(
                    "Interactive --all is unavailable with --json; use --auto or an exact item and conversation ID"
                )
            }
            if all {
                binding::automatic(&store, dry_run)?;
                if !dry_run {
                    binding::all(&store, false)?;
                }
            } else if auto {
                binding::automatic_report(&store, dry_run, !json)?;
                if json {
                    emit(
                        &serde_json::json!({"schema_version":1,"dry_run":dry_run,"items":store.read()?.entries}),
                        true,
                    )?;
                }
            } else {
                let id = id.context("Missing item ID")?;
                let session = session.context("Missing conversation ID")?;
                if store.read()?.entry(id)?.agent == Agent::Cursor {
                    agents::validate_cursor_id(&session)?;
                    store.update(|s| {
                        let e = s
                            .entries
                            .iter_mut()
                            .find(|e| e.id == id)
                            .context("Unknown item")?;
                        if e.lease.as_ref().is_some_and(|l| l.live()) {
                            bail!("Item is reserved by an operation")
                        };
                        e.provider_session = Some(session);
                        e.session_verified = true;
                        Ok(())
                    })?;
                } else {
                    binding::one_report(&store, id, Uuid::parse_str(&session)?, !json)?;
                }
                if json {
                    emit(
                        &serde_json::json!({"schema_version":1,"item_id":id,"state":"bound"}),
                        true,
                    )?;
                }
            }
        }
        Action::Sensors => return hardware::print_samples(&hosts),
        Action::Forget { id } => {
            store.update(|s| {
                let entry = s.entry(id)?;
                if entry.lease.as_ref().is_some_and(|l| l.live())
                    || s.runs.get(&id).is_some_and(|r| r.live())
                {
                    bail!("Finish or park active work before forgetting its ownership record")
                };
                if s.entries.iter().any(|e| {
                    e.dependencies.contains(&id)
                        || e.service.as_ref().is_some_and(|svc| svc.owner == id)
                }) {
                    bail!("Other work references this item; resolve dependencies first")
                };
                s.entries.retain(|e| e.id != id);
                s.runs.remove(&id);
                Ok(())
            })?;
        }
        Action::Doctor => {
            if json {
                emit(
                    &serde_json::json!({"schema_version":1,"storage":store.database(),"database_exists":store.database().exists(),"monitoring_paused":store.paused(),"ghostty":ghostty::jxa("Application('Ghostty').version()").ok(),"process_access":process::start_time(std::process::id()).is_some(),"macmon":hardware::local_binary(),"provider_control":"manual_only; no verified provider shutdown contract","no_daemon_default":false}),
                    true,
                )?;
                return Ok(0);
            }
            let version =
                ghostty::jxa("Application('Ghostty').version()").unwrap_or("unavailable".into());
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
            println!(
                "Hardware sampler: {}",
                hardware::local_binary()
                    .map(|p| p.display().to_string())
                    .unwrap_or("macmon not found (optional)".into())
            );
        }
        Action::History {
            item,
            limit,
            cursor,
        } => emit(
            &serde_json::json!({"schema_version":1,"items":store.history(item,limit,cursor)?,"cursor":cursor,"limit":limit.clamp(1,100)}),
            json,
        )?,
        Action::Run { id, token } => return agents::launch(&store, id, token),
        Action::ExecGate { fd, argv } => {
            return ghostty_workspaces::supervisor::exec_gate(fd, &argv);
        }
        Action::Hook { id, token, agent } => agents::hook(&store, id, token, agent)?,
    }
    Ok(0)
}

fn emit(value: &impl serde::Serialize, _json: bool) -> Result<()> {
    println!("{}", serde_json::to_string_pretty(value)?);
    Ok(())
}

#[cfg(test)]
mod cli_tests {
    use super::*;
    fn launch(args: &[&str]) -> (Agent, LaunchOptions) {
        let cli = Cli::try_parse_from(args).unwrap();
        match cli.command.unwrap().normalize() {
            Action::New { agent, options } => (agent, options),
            _ => panic!("Expected a launch"),
        }
    }
    #[test]
    fn shorthand_defaults_to_current_directory_without_no_daemon() {
        let (agent, options) = launch(&["gws", "codex"]);
        assert_eq!(agent, Agent::Codex);
        assert_eq!(
            options.project_directory().unwrap(),
            std::env::current_dir().unwrap().canonicalize().unwrap()
        );
        assert!(!options.isolated);
    }
    #[test]
    fn accepts_project_directory_and_compatibility_spelling() {
        for args in [
            &["gws", "codex", ".", "--name", "Project"][..],
            &["gws", "new", "codex", "--cwd", ".", "--name", "Project"][..],
        ] {
            let (agent, options) = launch(args);
            assert_eq!(agent, Agent::Codex);
            assert!(options.project_directory().unwrap().is_dir());
            assert_eq!(options.name.as_deref(), Some("Project"));
        }
        assert!(Cli::try_parse_from(["gws", "codex", ".", "--cwd", "."]).is_err());
    }
    #[test]
    fn accepts_opt_in_no_daemon_and_native_resume_subcommand() {
        let (_, options) = launch(&["gws", "codex", "--no-daemon"]);
        assert!(options.isolated);
        let (_, options) = launch(&[
            "gws",
            "codex",
            "--cwd",
            ".",
            "resume",
            "example-session",
            "--no-daemon",
        ]);
        assert!(matches!(
            options.subcommand,
            Some(AgentSubcommand::Resume { .. })
        ));
        assert!(options.project_directory().unwrap().is_dir());
        let (_, options) = launch(&["gws", "codex", "resume", "--last", "--no-daemon"]);
        assert!(matches!(
            options.subcommand,
            Some(AgentSubcommand::Resume { .. })
        ));
    }
    #[test]
    fn forwards_options_after_separator() {
        let (_, options) = launch(&["gws", "codex", "--", "--model", "example"]);
        assert_eq!(options.args, ["--model", "example"]);
    }
}
