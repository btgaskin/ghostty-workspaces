# Ghostty Workspaces

**Native Ghostty tabs. Exact agent sessions. A small resource dashboard.**

`gws` saves a set of terminal tabs and restores them after restarting your Mac. A Rust TUI shows tab ownership, local processes, observed logical subagents, and machine sensors. Codex, Claude Code, shells, and explicit commands can share a workspace.

Early release for **macOS and Ghostty 1.3+**. Uses Ghostty's public AppleScript API. No Ghostty fork, terminal multiplexer, web server, or permanent background daemon.

## Install

Requires Rust and an installed Ghostty app.

```sh
git clone https://github.com/btgaskin/ghostty-workspaces.git
cd ghostty-workspaces
cargo install --locked --path .
gws doctor
```

macOS may ask your launching terminal for permission to automate Ghostty. Agent executables are found on `PATH`, in `~/.local/bin`, and in the common Homebrew bin directories. Existing Ghostty and agent configuration files are not edited.

For Apple Silicon E/P CPU, GPU, power, and temperature panels, install the optional [macmon](https://github.com/vladkens/macmon) sampler with `brew install macmon`. Tab management and process monitoring work without it; local RAM falls back to the OS sample. An installed sampler runs only while the dashboard is open.

## Start using it

Create tabs through `gws` for reliable process ownership and automatic session capture:

```sh
gws codex               # new managed Codex tab in the current directory
gws codex ~/dev/my-project --name 'Project · Codex'
gws claude --cwd ~/dev/my-project --name 'Project · Claude'
gws shell --cwd ~/dev/my-project --name 'Project · Shell'
gws command --cwd ~/dev/my-project --name 'Project · Dev server' -- npm run dev
gws                    # dashboard in this terminal
gws launch             # dashboard in its own Ghostty window
```

A project directory can be positional (`gws codex ~/dev/project`) or passed with `--cwd` / `-C`. Relative paths are resolved from the directory where you invoke `gws`. Supplying both forms is rejected. `resume` and Codex `fork` are accepted directly, including their provider options. Other agent options go after `--`, for example `gws codex . -- --model MODEL`. The original `gws new codex` spelling remains supported.

For Codex, use `/hooks` to review and trust the scoped `gws` hooks if the CLI requests it. These hooks only record session identity and subagent lifecycle metadata. They do not read prompts or return instructions to the model. Hook trust and normal agent permissions are left to the agent's standard controls.

After a reboot:

```sh
gws restore            # default workspace
gws restore research   # a named workspace
gws restore --dry-run
gws list
```

Managed tabs are saved when created; session IDs are recorded as lifecycle hooks arrive. No save-on-shutdown process is required. Restoring also works after closing a managed tab. Already-open tabs are skipped, and live launchers are checked using both PID and process start time to avoid duplicate launches.

### Bring existing tabs into a workspace

```sh
gws save
gws save research
```

This records the current windows, tab order, and directories without interrupting agents. Inactive saved entries are retained; use `gws forget` to remove them. The dashboard itself is excluded.

For existing Codex tabs, `save` attempts a one-to-one match using Ghostty process ancestry, cwd, and the launch flags/explicit target in the tab title. A unique match gains CPU/RSS tracking immediately. A currently open root CLI metadata file supplies its exact resume UUID. Only its first bounded metadata line is read; subagent logs are rejected. Ambiguous tab/process matches and multiple root files remain unresolved. This adapter depends on the installed CLI's file/process behavior, so it can fail closed on other versions.

A UUID in a shell-generated title is an **unverified candidate**: it can describe the original `resume` target, while the CLI persists continued work under another ID. Candidates are refused when restoring or parking until verified. No latest-file/cwd heuristic assigns conversations. Preview automatic bindings, then walk through remaining tabs or bind an exact ID explicitly:

```sh
gws list
gws bind --auto --dry-run
gws bind --auto
gws bind --all          # automatic matches first; then UUID/skip prompts
gws bind <saved-tab-id> <conversation-id>
```

Imported tabs without a unique process match show unavailable ownership metrics; all local processes and machine sensors are still visible. Adoption is a snapshot, not lifecycle-hook attachment to an already-running provider. Run `gws save` again after changing an adopted conversation. Relaunching through `gws` adds ongoing session/subagent hooks. An unresolved agent tab is skipped during restoration, with a nonzero exit status. `gws` will not silently start a fresh conversation.

### Agent options and custom commands

```sh
gws codex --workspace research --cwd ~/dev/project -- --model MODEL
gws codex --cwd ~/dev/project --session CONVERSATION_UUID
gws claude --cwd ~/dev/project --session CONVERSATION_UUID
gws codex --cwd ~/dev/project --no-daemon
gws codex --cwd ~/dev/project resume CONVERSATION_UUID --no-daemon
gws codex resume --last
gws claude --cwd ~/dev/project resume CONVERSATION_UUID
```

Codex uses its normal shared server by default. `--no-daemon` (also accepted as `--isolated`) opts into a private server, which gives the tab a private process tree that can be measured more completely, at the cost of a separate server per tab. Other CLIs can be added as explicit command tabs. A command is stored as an executable plus argument list, not an inferred command from a tab title. Such commands restart; arbitrary processes and in-memory state cannot survive a reboot.

## Dashboard

Wide terminals place the tab/process list at the upper left, selected details below, and hardware monitors on the right. Narrow terminals stack the panels. Machine RAM, swap, E/P CPU frequency/load, GPU frequency/load, temperature, and power are independent of tab bindings. Larger panels include up to 60 samples of history.

| Key | Action |
| --- | --- |
| Tab | Switch between saved tabs and all local OS processes |
| `j` / `k`, arrows | Select a row |
| Page Up / Page Down | Scroll selected details |
| Enter | Focus an open tab or resume a saved tab |
| `p`, then `y` | Close the selected terminal through Ghostty; retain its saved session |
| `s` | Save the current Ghostty layout to `default` |
| `r` | Restore the selected tab's workspace |
| `m` / `c` / `w` | Sort by resident memory, CPU, or workspace |
| `q` / Escape | Quit the dashboard |

Remote sensors are completely opt-in: `gws --ssh user@host` or `gws launch --ssh user@host`. The default contacts no other machine. SSH uses existing credentials/routing, batch authentication, and strict known-host verification; `macmon` must already be installed remotely. Large screens split the monitor column between devices; `h` cycles devices when space is limited. Remote panels contain hardware sensors, not remote tab management or process ownership. Stale/disconnected streams are explicit. Nothing is installed remotely. Remote-device acceptance remains experimental.

`gws sensors` prints a bounded live hardware snapshot as JSON and exits, stopping its collectors.

`gws open`, `gws focus`, and `gws park` also accept a saved tab ID. `gws forget` removes a saved entry without closing its terminal or deleting agent history. `gws list --json` exposes the local inventory and owned metrics for other tools.

## What the numbers mean

- **CPU:** sum of observed CPU usage for the launcher and its current descendants. 100% is one CPU core; multicore work can exceed 100%.
- **RSS:** summed process resident bytes, expressed in MiB. Shared pages can be counted more than once; this is not Activity Monitor's memory footprint or system memory pressure.
- **Processes:** current descendants of a verified launcher PID. Detached, reparented, remote, and shared-daemon processes are excluded. A descendant that exits between samples may never appear.
- **Subagents:** logical agent IDs observed through `SubagentStart` / `SubagentStop`, shown separately from process count. A subagent is not necessarily an OS process. Counts describe lifecycle events observed during the current launch; missing hooks or unexpected agent shutdown can leave them incomplete. They are unavailable until hooks have run.
- **Shared Codex server:** its memory, tools, and subprocesses are not assigned to individual tabs. Counting the whole shared server once per tab would inflate totals. Use `--no-daemon` when you need attributable process trees. It is opt-in.
- **Ghostty itself:** shared renderer/app memory is not assigned to tabs.
- **Machine CPU/GPU:** macmon 0.8 active ratios are labeled `active`; older `[MHz, ratio]` samples are labeled `weighted`. Frequency-weighted utilization differs from active time. RAM is machine usage, not summed tab RSS or memory pressure.

The dashboard samples processes and tab metadata approximately every two seconds on a worker thread, caching owned trees once per sample. One optional macmon stream per device samples approximately once per second. Input remains independent of sampling. Agent transcripts and conversation content stay in their own tools; `gws` stores IDs and launch metadata only.

For resource actions, `p` parks a verified session. Suspension is a separate future action: stopping processes reduces CPU work but retains allocations. See [resource optimization](docs/resource-optimization.md) for reviewed options; no automatic suspension or eviction is enabled.

## Storage and limitations

State lives in `~/.local/share/ghostty-workspaces/workspaces.json`. Override with `GWS_STATE_DIR` or `--state-dir`. Writes use a file lock, an atomic rename, and a synced file and directory. Malformed or newer-format state is not overwritten. The directory is private to your user. Local state contains project paths, tab names, launch arguments, and conversation IDs; keep it out of public repositories and avoid secrets in command arguments.

v0.1 restores window groups and tab order when recreating a workspace from closed tabs. It does not recreate split geometry, pixel positions, terminal scrollback, remote machine state, or arbitrary live process memory. Existing split surfaces are imported as separate tabs. Shells restart at their saved directory. Agent resume relies on the agent's own persisted history and normal authentication. Conversation histories are not copied, compacted, translated, or rewritten.

Ghostty's own macOS window restoration can coexist with `gws`: live terminal IDs are used to avoid reopening the same saved surface. Automatically reopened bare shells or changed surface IDs may require a deliberate `gws restore` and closing redundant shells. v0.1 does not install a login item or alter global Ghostty window-restoration settings.

## Development

```sh
cargo fmt --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked
cargo build --release --locked
```

Tests cover exact conversation identity, stale hook rejection, concurrent subagent events, process-tree ownership, PID reuse, malformed-state preservation, and compact terminal rendering. Live macOS automation needs Ghostty running and normal Automation permission. CI cannot prove behavior across a physical reboot or live agent-provider requests.

See [architecture](docs/architecture.md) for the integration boundaries and next steps.

MIT licensed. Independent project; not affiliated with Ghostty, OpenAI, or Anthropic.
