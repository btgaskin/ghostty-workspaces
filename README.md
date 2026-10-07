# Ghostty Workspaces

Keep agent work organized, resume conversations, and see what is using resources.

`gws` is a terminal dashboard for macOS and Ghostty. It supports Codex, Claude Code, Cursor CLI, shells and custom commands, using native Ghostty tabs.

![Ghostty Workspaces dashboard](docs/images/dashboard.png)

*All names, paths, processes, descriptions and readings shown are dummy data.*

## Quick start

Requires Rust, Ghostty 1.3+, and whichever agent CLI you want to use.

```sh
git clone https://github.com/btgaskin/ghostty-workspaces.git
cd ghostty-workspaces
cargo install --locked --path .
gws doctor
```

Launch work from its project directory:

```sh
gws codex                          # current directory
gws claude ~/dev/project
gws cursor -C ~/dev/project
gws command ~/dev/project -- npm run dev
gws                                # open the dashboard
```

Provider options go after `--`. Codex `--no-daemon` is opt-in. Accept scoped provider hooks through the provider's normal trust flow when prompted.

Optional [macmon](https://github.com/vladkens/macmon) adds Apple Silicon E/P CPU, GPU, temperature and power panels: `brew install macmon`. Process and memory monitoring work without it.

## Save and resume

Each saved item keeps its project directory, provider and exact conversation identity. Runs record execution history and process ownership. Opening the dashboard does not automatically start saved work.

```sh
gws save                           # import existing Ghostty tabs
gws bind --auto --dry-run           # inspect live Codex matches
gws bind --auto                     # record unambiguous matches
gws list                           # find saved work IDs
gws open WORK_ID                    # focus or explicitly resume one item
gws restore --preview              # review a workspace restore plan
gws apply PLAN_ID --wait            # apply that plan
```

**Needs binding** means a saved item lacks a verified conversation ID. In the corresponding Codex conversation, run `!gws bind-here`. If several saved items match, use `!gws bind-here --item WORK_ID`. Preview with `--dry-run`; other providers can use `gws bind WORK_ID CONVERSATION_ID`.

Shells and custom commands restart rather than recover their in-memory state. Split geometry and scrollback are not restored. Real-provider continuation after a physical reboot still needs acceptance testing.

## Search

Fuzzy search runs locally over names, paths, conversation IDs and cached descriptions. It searches Codex history across folders and registered Claude/Cursor conversations. It does not search full transcript text.

```sh
gws search --query 'memory pressure'
gws describe WORK_ID               # generate a description with Luna, on request
```

Optional Jev search reranks the top 25 fuzzy matches using relevance and confidence. Uncertain results retain their relative fuzzy order. Your query and candidate metadata are sent to Jev; raw transcripts are excluded. A reranker cannot recover conversations missed by fuzzy matching.

```sh
gws search-config --env-file /private/path/to/.env
gws search --query 'memory pressure' --semantic
```

Configuration stores the credential file path, not the key. No model request runs automatically.

## Dashboard

| Key | Action |
| --- | --- |
| Tab | Saved work / History / Mac processes |
| `/`, `J` | Fuzzy search / explicitly rerank History with Jev |
| Enter | Focus, resume or inspect the selected item |
| Click a row | Select it |
| `a` | Acknowledge the selected completion |
| `B`, click `?` at a row's edge | Binding help; `c` copies the Codex bind command and focuses its tab |
| `d`, `t`, `h` | Details / technical details / usage history |
| `M` | Pause or resume monitoring |
| `Q` | Preview profiling preparation |
| `?` | All shortcuts |

To reclaim an active agent's memory, finish or cancel in its own UI, then exit the CLI. Automatic stopping is limited to explicitly owned services. Profiling preparation pauses `gws` collectors and model jobs; other apps and shared daemons may remain active.

At desktop widths, RAM, E/P CPU and GPU history run down the right side. Press `h` to [expand usage](docs/images/usage.png). Narrow terminals keep the usage summary and collapse columns. Models show explicit recorded launch settings; otherwise the row shows the provider.

Tiny edge markers replace the action column: `●` working, `◆` finished unread, `○` finished read, `!` attention, `×` exit error and `·` unknown. A separate `?` means unbound. Finished means an observed turn end or process exit; read means viewed in the foreground Ghostty terminal or explicitly acknowledged with `a`. Browsing rows preserves unread markers. Foreground detection runs every five seconds while monitoring is active; it targets the focused split, not the whole tab. The selected panel separates observed hook subagents from tracked OS processes. Shared daemons are not assigned by guesswork.

## More

- [Architecture and storage](docs/architecture.md)
- [Agent commands and service ownership](docs/agent-guide.md)
- [Resource accounting and optimization](docs/resource-optimization.md)
- [Validation and current limits](docs/validation.md)
- [Product direction: sessions and groups](docs/product-direction.md)

Private state lives in `~/.local/share/ghostty-workspaces/`. Keep it out of public repositories. Use `gws --help` for additional commands.

MIT licensed. Independent of Ghostty and the agent providers. Hardware presentation inspired by [macmon](https://github.com/vladkens/macmon).
