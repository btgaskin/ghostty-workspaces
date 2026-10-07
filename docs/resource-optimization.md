# Resource optimization

Start with explicit parking of verified sessions. Add suspension separately after process ownership and recovery have been verified. Hiding a Ghostty tab does not stop its CLI or its tools.

| Action | CPU effect | Memory effect | Continuity | Status |
| --- | --- | --- | --- | --- |
| Leave an idle tab open | Provider-dependent background work continues | Allocations remain | Live process state | Available |
| Park a verified session | Terminal-owned work normally exits | Exited processes release allocations; shared/detached services can remain | Exact provider session ID and cwd; reopen with `gws open` | Available through `gws park`, or `p` then `y` |
| Suspend a private process group | Stopped processes cease execution | Allocations remain; RAM savings are not guaranteed | Same live processes on `SIGCONT`; connections and deadlines can expire meanwhile | Reviewed, not implemented |
| Reduce background priority | Reduces competition for CPU under contention | Allocations remain | Processes keep running | Future option |
| Automatically park idle sessions | Depends on correct idle/busy detection | Same boundary as manual parking | Requires verified durable identity | Future, opt-in only |

Apple's [signal documentation](https://developer.apple.com/library/archive/documentation/System/Conceptual/ManPages_iPhoneOS/man2/sigaction.2.html) describes `SIGSTOP` and `SIGCONT`. Its [kill documentation](https://developer.apple.com/library/archive/documentation/System/Conceptual/ManPages_iPhoneOS/man2/kill.2.html) distinguishes a process from a process group. Sending a stop signal to the TUI alone does not necessarily stop its server or children. Suspending the shared Codex server would affect other clients, so shared-server tabs must not be treated as independently suspendable workloads.

## Local probe

On 2026-10-07, a task-owned Python worker allocated 128 MiB and ran a CPU loop. `ps` reported 139,472 KiB RSS before, during, and after suspension. CPU time stayed at 1.00 seconds over a one-second stop, then advanced to 1.70 seconds after continuation. The worker was terminated and reaped. No existing agent or user process was signalled. This verifies basic OS behavior on this machine; it does not validate pausing an active Codex/Claude request, GPU workload, or tool chain.

## Suspension requirements

A future `suspend`/`continue` action should use an owned process group established by the launcher, with PID/start-time checks. It should show exactly which processes are affected, refuse shared-server/ambiguous ownership, and never infer a group from an arbitrary process row. Detached processes require separate accounting. The dashboard must remain outside the stopped group.

Suspension state and a recovery command must survive a dashboard crash. Continuation needs to work without launching a duplicate CLI. A parked or restored session must not inherit an obsolete suspended PID. Tests should cover nested children, group membership changes, stale PIDs, partial failures, dashboard exit, and foreground terminal behavior. Provider acceptance should include an idle session, an active network request, a long-running local tool, and normal shutdown after continuation.

OS suspension does not establish that an agent is safely idle. For later automatic parking, use provider lifecycle state to distinguish idle, working, awaiting approval, and awaiting input. Prefer advisory memory-pressure/budget indicators first. CPU usage alone is insufficient: an idle-looking process can be waiting for a network request or a tool.

## Monitor overhead

Owned process trees are calculated once per two-second process sample and reused during drawing and sorting. Hardware uses one optional macmon stream per explicitly selected device. No per-tab sampler or permanent `gws` daemon is installed. SSH is absent from the default path. Collectors are killed and reaped when the dashboard or one-shot sensor command exits normally.

Measure the complete monitor workload, including macmon and transient AppleScript processes, when comparing performance. Do not describe a main-process RSS measurement as total monitoring overhead. Memory pressure, swap growth, user-visible latency, and active tool workloads are more useful than a single utilization reading. Machine RAM, summed process RSS, and GPU load have different accounting boundaries; the dashboard labels these separately.
