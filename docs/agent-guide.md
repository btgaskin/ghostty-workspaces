# Agent commands and service ownership

Use read-only inspection first:

```sh
gws list --json
gws inspect WORK_ID --json
gws history --item WORK_ID --json
gws sessions --query 'project or task' --json
gws audit --json
gws operation OPERATION_ID --json
```

## Bind the current Codex conversation

Inside the conversation's Codex shell, run `!gws bind-here --dry-run`, then `!gws bind-here`. Managed launches inherit their state directory and work/run context. Imported tabs require one unambiguous saved match; `--item WORK_ID` resolves a duplicate explicitly. Directory proximity alone never selects a conversation.

The command checks the current thread against bounded root transcript metadata, provider home, project directory and live execution identity. If the installed Codex does not supply `CODEX_THREAD_ID`, use `--session CONVERSATION_ID`. Binding records continuity; it does not retroactively install provider hooks or grant permission to stop a shared server.

## Reviewed cleanup

Resource evidence does not grant permission to stop unrelated work. Preserve the controlling session and required services. In a managed shell, identify it explicitly:

```sh
gws --caller "$GWS_ITEM_ID:$GWS_RUN_ID" quiet --only OTHER_WORK_ID --preview --json
```

Scoped SessionStart hooks also supply the exact caller pair when a shared provider does not inherit shell environment. Unknown automation caller context blocks disruptive actions. `--human` explicitly identifies a human-issued script; it is not an agent's way to evade controller protection.

Present the concrete plan and its blockers under the user's cleanup authority. Apply its exact ID, then inspect the receipt:

```sh
gws apply PLAN_ID --json                # durable acceptance, detached worker
gws operation PLAN_ID --json
gws apply PLAN_ID --wait --json         # bounded step waits, returns final receipt
gws operation PLAN_ID --cancel --json   # prevents subsequent steps
gws apply PLAN_ID --retry --wait --json # same targets; revalidate/reconcile
```

Completed steps are not undone by cancellation. Partial stop/terminal-close failures retain effects and survivors. A fresh preview is required after changed bindings, dependencies, another run, expiry or reboot. Model confidence never broadens a plan's authority.

Running provider sessions are manual-only. Finish/cancel and exit in the provider UI, then park its standby tab if appropriate. Do not invent a graceful provider shutdown from an idle CPU sample or a `Stop` hook.

## Register a service

Every temporary preview/research server should have an owner and lifetime. Create a recipe with real paths and the saved owner ID:

```json
{
  "name": "Project preview",
  "cwd": "/absolute/path/to/project",
  "argv": ["npm", "run", "dev"],
  "owner": "00000000-0000-4000-8000-000000000001",
  "lifetime": "run",
  "stop_argv": [],
  "readiness": {"socket": "127.0.0.1:5173", "timeout_seconds": 15},
  "dependencies": []
}
```

```sh
gws service register --file service.json --json
gws service start SERVICE_ID --json     # creates a preview, does not start yet
gws apply PLAN_ID --wait --json
gws service stop SERVICE_ID --json      # creates a stop preview
```

`run` ends with the owning execution; `work` survives provider exit until deliberate work cleanup; `persistent` is deliberately retained and excluded from default quiet preparation. No background restart loop is created. Readiness accepts exactly one socket or argv probe, with a bounded timeout. Failed readiness leaves the execution visible and blocks dependants.

For an existing external service, first register the recipe with an explicit stop command, then adopt its exact PID/start identity. The PID must belong to the current user. An arbitrary OS process row is inspectable, not adopted or killable by selection alone.

## Profiling

Prepare a reviewed quiet plan, inspect actual outcomes, and check collector/model-job acknowledgements. A batch ID restores only actually stopped executions. Resume monitoring explicitly after profiling. Shared Codex servers, Chrome, editors, filesystem services and unmanaged research jobs remain outside automatic control.

`gws diagnostic files --pid PID` reports open files/sockets; `startup` and `sleep` inspect registrations/assertions. These inspections do not remove login items, diagnose watcher causation or establish sustained CPU pressure from a short sample.
