# Resource optimization

To reclaim memory, stop an eligible execution and retain its exact conversation/cwd. Hiding a terminal or suspending a process retains allocations. `gws` keeps durable work metadata and a cheap standby surface after normal provider exit; profiling preparation can close those surfaces too.

| Action | CPU / memory | Continuity and policy |
| --- | --- | --- |
| Leave a run open | Background work and allocations remain | Same live processes |
| Finish/cancel and exit provider | Exited private processes release allocations | Exact provider history; shared helpers may remain |
| Stop a registered service | Owned group or explicit recipe stops; outcome verified | Recipe, lifetime and run outcome survive |
| Profiling preparation | Eligible managed cleanup plus collector/model-job shutdown | Reviewed plan, partial receipts, actual restore batch |
| Suspend | CPU can stop; allocations remain | Not implemented; insufficient for memory reclamation |
| Automatic idle eviction | Requires reliable provider control/activity evidence | Not enabled; advisory hooks do not provide that contract |

## Read the right evidence

Memory pressure, compression, ongoing swap rate and responsiveness are more informative than swap total alone. The dashboard displays native counters where available and labels unavailable pressure explicitly. RSS, charged footprint and physical RAM have distinct accounting boundaries.

Whole-Mac audit separates owned trees from executable-name groups of shared/unmanaged processes. Repeated MCP servers, long-lived dev servers, suspended jobs, detached helpers and costly terminal/editor/browser groups are useful review targets. Their age or name does not prove that they are unnecessary or safely stoppable.

A short CPU sample cannot diagnose a persistent `fseventsd` loop. Use an OS diagnostic sample and controlled watcher comparisons under explicit authority. `gws` supplies bounded files/sockets, startup and sleep inspections; it does not automatically stop system services or alter other applications' settings.

## Profiling scope

A successful quiet operation can establish which selected managed executions stopped and whether `gws` collectors/model jobs acknowledged shutdown. It does not establish a globally idle machine. Read the external audit groups and any refused steps before interpreting a profiling result.

The dashboard freezes cached data while paused; it performs no ongoing process/hardware sampling. New model jobs are refused and temporary in-flight jobs are cancelled/reaped. The shell or register can remain open without an active macmon stream. Restart monitoring explicitly when profiling ends.

## Earlier OS suspension probe

On 2026-10-07, a task-owned Python fixture allocated 128 MiB and ran a CPU loop. `ps` reported 139,472 KiB RSS before, during and after suspension. CPU time stayed at 1.00 seconds over a one-second stop, then advanced to 1.70 seconds after continuation. It was terminated and reaped. No user process was signalled. This earlier OS probe is not acceptance evidence for pausing a live Codex/Claude request, GPU workload or provider tool chain.

## Measure the complete monitor

Include the main dashboard, macmon and transient automation utilities when measuring overhead. Record release build, machine/workload, sampling interval and whether History is visible. A synthetic render, a short idle sample and a real profiling workload prove different things. Avoid attributing shared Codex servers or Ghostty renderer memory repeatedly to every tab.
