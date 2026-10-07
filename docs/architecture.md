# Architecture

## Integration boundary

Use Ghostty's native macOS AppleScript interface for surface inventory, working directories, creating windows and tabs, focus, and close. A surface runs `gws run <entry-id> <run-token>`. The launcher records its own PID and process start time, then waits for the shell or CLI child. The dashboard reads state and samples processes independently. The launcher has no sampling loop.

This keeps the terminal's renderer, native tabs, keyboard behavior, and selection owned by Ghostty. It avoids an app fork, extra renderer, nested terminal protocol, socket server, and a permanently installed agent service. macOS is the initial platform because Ghostty's native automation API currently lives there.

## Durable and live identity

A saved entry has a stable UUID, workspace, ordered window group, label, absolute working directory, provider, provider session UUID, and launch arguments. A run has a new token, PID, start time, observed logical agents, and live surface ID. Runtime identity is never treated as durable conversation identity. A title UUID is an unverified candidate until live root metadata, a lifecycle hook, or an explicit bind verifies it.

For imported Codex tabs, one-shot discovery joins native process ancestry (including root-owned login bridges), cwd, and the launch signature from the Ghostty title. Only reciprocal unique matches are adopted. Open root CLI session metadata provides exact IDs; subagent and oversized metadata are rejected. An adopted PID/start-time pair provides ownership without restarting the tab. Identical signatures, multiple root logs, and unavailable metadata remain explicit. Discovery runs on save/bind, not each dashboard frame.

Every hook command includes its entry and run token explicitly, rather than depending on environment variables forwarded by a shared agent daemon. Late hooks from an older run cannot change the current entry. File locking serializes concurrent hook writes. Sessions are written as soon as metadata is available, rather than waiting for graceful shutdown.

Codex hook `session_id` can represent runtime session identity. The adapter reads only the first bounded metadata line of `transcript_path` and persists the root `session_meta.payload.id` UUID. A child/subagent transcript cannot replace the parent conversation. Missing or changed formats leave the entry unresolved. Claude's `SessionStart.session_id` provides its resume UUID directly. Both adapters receive per-launch lifecycle hooks through normal CLI configuration. They do not edit global provider configuration, overwrite notifications, or bypass hook trust.

## Resource attribution

The process sampler uses sysinfo's native macOS process implementation. An entry owns the current descendant tree of its launcher, only if its PID and start time still match. Both are required because PIDs are reused. Memory is summed resident bytes. CPU is sampled CPU usage, summed across descendants.

Shared services are outside this tree. Codex's shared app-server can run work for many terminal clients. Accurately splitting its RSS and CPU by logical conversation is not generally possible from OS process counters. The default preserves Codex's shared-server behavior. The optional isolated launch creates a private server and more complete ownership at the cost of separate server resources.

Logical subagents are counted from lifecycle events, independently of OS children. They can share a process, or run elsewhere. Counts reset for a new launch. Unexpected failures or missed hooks can make lifecycle counts incomplete. Do not present them as independently verified running processes.

## Performance

The dashboard's worker reads tab metadata and samples process counters roughly once every two seconds. The main thread polls keyboard input and draws at most four times per second. Owned process trees are cached once per sample. Optional hardware collectors normalize macmon 0.6/0.8 pipe schemas, keep bounded histories, and mark samples stale after five seconds. Collector processes are stopped and reaped on normal dashboard exit. Optional SSH uses existing configuration with batch authentication and strict known-host checks; it has no default host or remote installation step. No conversation transcript scans, model requests, per-tab monitoring daemons, or listening network services run in the monitor. Codex metadata reads occur only for session-start events, with a bounded first-line read.

Keep performance statements measured: release binary size, idle RSS/CPU, sampling duration, and input response on a stated machine and tab/process workload. A short idle sample is not a guarantee under heavy subprocess churn.

## Resource actions

Closing a managed terminal keeps its saved entry. Later restoration invokes the provider's exact resume command. Closing a terminal can interrupt its foreground process; unsaved process state is not reconstructed. Shared or detached services may remain running. v0.1 delegates close behavior to Ghostty and does not send arbitrary process signals, purge memory, change priorities, or terminate shared servers.

See [resource optimization](resource-optimization.md) for the reviewed suspend/park/priority options. Suspension and automatic eviction are not implemented.

## Next steps

1. Broaden existing-session adapters with versioned provider APIs and explicit choices for ambiguous tabs; avoid automatic cwd/latest matching.
2. Shared-server visibility as one service row, with logical client links and independently sampled service metrics.
3. Better lifecycle health, last-hook timestamps, ended-versus-still-running subagent semantics, and provider version fixtures.
4. User-authored resource budgets with advisory warnings before any automatic action.
5. Save and restore split topology when Ghostty exposes a stable, queryable split-tree contract.
6. Optional explicit login restore and launcher shortcuts, keeping one-shot restore separate from a permanent daemon.

## Primary references

- [Ghostty AppleScript](https://ghostty.org/docs/features/applescript)
- [Ghostty window restoration](https://ghostty.org/docs/config/reference#window-save-state)
- [Codex lifecycle hooks](https://learn.chatgpt.com/docs/hooks)
- [Codex CLI](https://learn.chatgpt.com/docs/codex/cli)
- [Claude Code CLI](https://code.claude.com/docs/en/cli-reference)
- [Claude Code hooks](https://code.claude.com/docs/en/hooks)
- [sysinfo](https://docs.rs/sysinfo/0.38.4/sysinfo/)
- [Ratatui](https://docs.rs/ratatui/0.30.2/ratatui/)
