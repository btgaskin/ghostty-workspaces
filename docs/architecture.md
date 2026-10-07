# Architecture

## Identity and integration

A workspace groups stable work items. A work item owns its saved directory, provider/profile, exact conversation binding and deliberate service relationships. Every execution gets a distinct boot-bound run token. Native Ghostty surfaces are presentation, not durable identity.

Ghostty owns tabs, rendering, keyboard behavior and selection. Public AppleScript inventory/open/focus/close avoids a fork or nested terminal. No public process-ID or split-tree contract is assumed. Existing binding requires reciprocal unique process signatures; ambiguity remains visible.

Codex's runtime hook session ID is not assumed to be its durable thread ID. A bounded root `session_meta` transcript record provides that identity. Claude lifecycle hooks provide its session ID and transcript path. Cursor uses its exact public CLI chat ID; its private database is not decoded. Generic CLIs get executable/argv/cwd restart semantics.

`bind-here` corroborates the invoking Codex thread with root metadata, its provider home and project directory. Managed work/run context must agree with a verified thread or the invoking process's birth identity. Imported items require exact or reciprocal unique evidence, with an explicit item fallback. It preserves an existing live run token and revalidates metadata and bindings before writing.

Scoped hooks include work/run identities explicitly, reject stale tokens and keep logical agent counts separate from OS descendants. Hook activity is advisory. Shared Codex servers and external helper relationships cannot be allocated to individual tabs from ancestry alone.

## Ownership before execution

A one-use launch authorization checks work binding, directory, survivors and per-item leases. Required services must still have the exact checked live run and cannot be reserved by another operation. Dependency readiness is checked before the final transactional identity validation, including direct opens and standby resumes.

The supervisor starts a gate process in its own group. The gate waits on an inherited pipe while its PID/start identity is committed. Only then is the foreground terminal handed over and execution released. Parent disappearance before release closes the pipe and aborts the gate without starting the provider. The same process subsequently execs the provider. A recorded live child blocks duplicate launch even after supervisor loss or an inconsistent ended flag.

The supervisor waits for its child, forwards termination to its owned group, records surviving members, and performs eligible run-service cleanup. It does not force-kill work. Provider control remains manual-only. Per-run private Unix sockets provide control; they are not a permanent global service or a network listener. Utility subprocesses have separate bounded/cancellable groups and are never used to supervise work.

## Operations

One policy engine serves CLI, TUI and agent callers. Preview persists exact targets, expected revisions, bindings, run IDs, controller/exclusions, boot and expiry. Acceptance persists an operation before detached execution. Claiming an accepted operation revalidates boot/expiry, including recovery after an unstarted worker.

Execution leases reserve selected items. Each step rechecks its contract and current dependant liveness. A selected dependant is not proof it stopped. Effects are saved before later terminal actions can fail. Retry reconciles an operation's exact recorded launch/stop, and refuses a different run. Completed effects are retained on cancellation or partial failure. Restore batches contain executions actually stopped rather than every planned target.

Service recipes store explicit argv, owner, lifetime, stop recipe, readiness and dependencies. Registration does not start a service. Explicit adoption requires current-user PID identity and a stop recipe; ancestry does not grant external group ownership. Run-scoped cleanup is tied to the owning run token. There is no restart scheduler.

## Storage and recovery

SQLite holds work, current runs, run history and typed durable documents for plans/receipts/audits/caches. Read transactions provide one state snapshot. Full-synchronous writes and a shared file lock serialize hooks and operations. Reads do not create state directories.

Legacy JSON is validated and backed up before import. UUIDs survive. Empty version-0 initialization can be retried. A completed import fences old binaries with a version-2 JSON marker. If legacy JSON changed after an interrupted import, both versions are preserved and use is refused until reconciliation; the database does not silently replace later legacy changes. Malformed and future schemas are refused.

History retains older executions when current runs change or work is forgotten. Provider conversation logs remain authoritative and are not rewritten or deleted by `gws`.

## Sampling, search and profiling

A worker samples processes/memory about every two seconds, Ghostty surfaces every five seconds, optional macmon every second, and conversation metadata every thirty seconds while History is visible. Unchanged bounded headers are cached by file size/mtime. The input thread uses cached liveness and redraws only on changes or input. Collector processes and reader threads are stopped/reaped/joined together.

Saved work uses the full dashboard width; technical provenance and process trees are behind `t`. Imported launch strings get display-only project/provider names, with a short item suffix for duplicates; raw titles and IDs remain searchable. `h` opens a secondary usage view with four framed, fixed 0–100% histories over the last sixty seconds. Samples use monotonic timestamps; missing intervals break lines, and monitoring pause freezes the time window. Native RAM history works without macmon. History is bounded and in memory, not written every tick.

Fuzzy search retrieves local candidates. Explicit Jev reranking classifies at most 25 candidates against descriptive relevance levels. Each result retains fuzzy position, relevance, confidence, distribution and resolved model. Remaining matches retain pagination. Low confidence keeps fuzzy order; failures preserve useful local results. No raw transcript or full absolute project path is sent to Jev.

`RelevanceClassifier` separates inference from retrieval and lifecycle policy. A future local backend can load once, atomically swap model instances, preserve revisions for in-flight responses and unload during profiling. This release implements the cloud Jev backend; no resident model service is installed.

Descriptions use bounded user/assistant excerpts and one ephemeral Luna medium run on request. Source fingerprint/conversation/model/time survive; generated prose cannot establish activity or authority. Temporary model jobs are registered, refused during pause, cancelled when pause begins, and accounted for before reporting profiling acknowledgement.

Profiling preparation saves an audit, closes eligible parked surfaces, records actual stopped runs and freezes collectors. Its result covers managed work/monitoring. Unmanaged apps, system services and shared daemons may remain active. No “whole machine quiet” claim follows from a successful managed batch.

## References

- [Ghostty AppleScript](https://ghostty.org/docs/features/applescript)
- [Codex hooks](https://learn.chatgpt.com/docs/hooks)
- [Claude Code hooks](https://code.claude.com/docs/en/hooks)
- [Cursor CLI](https://prod.cursor.com/docs/cli/overview)
- [Jev API and confidence](https://docs.typesafe.ai/api)
- [Apple memory pressure](https://support.apple.com/guide/activity-monitor/view-memory-usage-actmntr1004/mac)
- [macmon](https://github.com/vladkens/macmon)
