//! Shared policy and durable preview/apply protocol for humans and agents.
use crate::{
    engine, ghostty,
    model::{self, Agent, Entry, Intent, Lifetime, Service, Store},
    process, supervisor,
};
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet, HashSet},
    path::PathBuf,
    time::{Duration, Instant},
};
use uuid::Uuid;
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, clap::ValueEnum)]
#[serde(rename_all = "snake_case")]
pub enum Action {
    Park,
    Quiet,
    Restore,
    Finish,
    Start,
    Stop,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Caller {
    pub item: Uuid,
    pub run: Uuid,
}
impl std::str::FromStr for Caller {
    type Err = anyhow::Error;
    fn from_str(s: &str) -> Result<Self> {
        let (a, b) = s.split_once(':').context("Caller must be ITEM:RUN")?;
        Ok(Self {
            item: Uuid::parse_str(a)?,
            run: Uuid::parse_str(b)?,
        })
    }
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Blocker {
    pub item: Option<Uuid>,
    pub code: String,
    pub reason: String,
    pub remedy: String,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Target {
    pub item: Uuid,
    pub revision: u64,
    pub run: Option<Uuid>,
    pub name: String,
    pub action: Action,
    pub close_tab: bool,
    pub blocked: Option<Blocker>,
    pub binding: Value,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Plan {
    pub schema_version: u32,
    pub id: Uuid,
    pub action: Action,
    pub targets: Vec<Target>,
    pub excluded: Vec<Uuid>,
    pub caller: Option<Caller>,
    pub created_at: u64,
    pub expires_at: u64,
    pub boot: String,
    pub blockers: Vec<Blocker>,
    #[serde(default)]
    pub human: bool,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Step {
    pub item: Uuid,
    pub state: String,
    pub substeps: Vec<String>,
    pub error: Option<String>,
    pub survivors: Vec<(u32, u64)>,
    pub revision_after: Option<u64>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Operation {
    pub schema_version: u32,
    pub id: Uuid,
    pub plan: Uuid,
    pub state: String,
    pub worker_pid: u32,
    pub worker_start: u64,
    pub created_at: u64,
    pub updated_at: u64,
    pub steps: Vec<Step>,
    pub monitoring: Option<Value>,
    pub batch: Option<Uuid>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BatchItem {
    pub item: Uuid,
    pub stopped_run: Uuid,
    pub revision_after: u64,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Batch {
    pub id: Uuid,
    pub operation: Uuid,
    pub created_at: u64,
    pub stopped: Vec<BatchItem>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Dashboard {
    pub token: Uuid,
    pub pid: u32,
    pub start_time: u64,
    pub paused: bool,
}
fn blocker(id: Uuid, code: &str, reason: &str, remedy: &str) -> Blocker {
    Blocker {
        item: Some(id),
        code: code.into(),
        reason: reason.into(),
        remedy: remedy.into(),
    }
}
fn ancestors() -> HashSet<u32> {
    let mut mon = process::Monitor::new();
    let all = mon.refresh();
    let mut out = HashSet::new();
    let mut pid = Some(std::process::id());
    while let Some(p) = pid {
        if !out.insert(p) {
            break;
        }
        pid = all.get(&p).and_then(|p| p.parent);
    }
    out
}
fn dependencies(state: &model::State, id: Uuid, out: &mut BTreeSet<Uuid>) {
    if !out.insert(id) {
        return;
    }
    if let Ok(e) = state.entry(id) {
        for d in &e.dependencies {
            dependencies(state, *d, out)
        }
    }
}
fn protected(
    state: &model::State,
    caller: &Option<Caller>,
    exclusions: &[Uuid],
) -> Result<BTreeSet<Uuid>> {
    let mut out = BTreeSet::new();
    for id in exclusions {
        state.entry(*id)?;
        dependencies(state, *id, &mut out)
    }
    if let Some(c) = caller {
        let r = state
            .runs
            .get(&c.item)
            .filter(|r| r.token == c.run && r.live())
            .context("Caller context is stale or unresolved; supply the current ITEM:RUN")?;
        if r.ended {
            bail!("Caller run has ended")
        };
        dependencies(state, c.item, &mut out);
    }
    let ancestors = ancestors();
    for (id, r) in &state.runs {
        if r.live()
            && (ancestors.contains(&r.pid)
                || r.child_pid.is_some_and(|pid| ancestors.contains(&pid)))
        {
            dependencies(state, *id, &mut out)
        }
    }
    Ok(out)
}
fn stop_blocker(
    state: &model::State,
    e: &Entry,
    selected: &BTreeSet<Uuid>,
    protected: &BTreeSet<Uuid>,
) -> Option<Blocker> {
    if protected.contains(&e.id) {
        return Some(blocker(
            e.id,
            "protected",
            "Controller, exclusion, or required dependency",
            "Exclude this item from cleanup",
        ));
    }
    let live = state.runs.get(&e.id).is_some_and(|r| r.live());
    if live && matches!(e.agent, Agent::Codex | Agent::Claude | Agent::Cursor) {
        return Some(blocker(
            e.id,
            if state.runs[&e.id].activity == "working" {
                "busy"
            } else {
                "manual_only"
            },
            "Provider activity/shutdown contract is not verified",
            "Focus this tab, finish or cancel through the provider, then exit its CLI",
        ));
    }
    if live
        && (e.service.is_none() || (!state.runs[&e.id].supervised && !state.runs[&e.id].adopted))
    {
        return Some(blocker(
            e.id,
            "manual_only",
            "Execution lacks a managed service shutdown contract",
            "Exit the existing process manually; relaunch through gws for supervision",
        ));
    }
    if !live && e.needs_session() {
        return Some(blocker(
            e.id,
            "continuity_unresolved",
            "Exact conversation is unresolved",
            "Bind the conversation before parking",
        ));
    }
    for dependant in &state.entries {
        if dependant.dependencies.contains(&e.id)
            && state.runs.get(&dependant.id).is_some_and(|r| r.live())
            && (!selected.contains(&dependant.id) || protected.contains(&dependant.id))
        {
            return Some(blocker(
                e.id,
                "shared_dependency",
                "An unselected or protected run requires this service",
                "Include eligible dependants or leave the shared service running",
            ));
        }
    }
    if state.runs.get(&e.id).is_some_and(|r| {
        r.survivors
            .iter()
            .any(|(pid, start)| process::start_time(*pid) == Some(*start))
    }) {
        return Some(blocker(
            e.id,
            "survivors",
            "Previously owned processes remain",
            "Inspect surviving processes and resolve them explicitly",
        ));
    }
    None
}
pub fn inspect(store: &Store, id: Uuid) -> Result<Value> {
    let state = store.read()?;
    let e = state.entry(id)?;
    let r = state.runs.get(&id);
    let active = r.is_some_and(|r| r.live());
    let selected = BTreeSet::from([id]);
    let blocked = stop_blocker(&state, e, &selected, &BTreeSet::new());
    let mut monitor = process::Monitor::new();
    let processes = monitor.refresh();
    let metrics = r.and_then(|r| process::tree(r, &processes));
    let inventory = ghostty::snapshot();
    let focus = inventory.as_ref().ok().is_some_and(|windows| {
        e.terminal_id
            .as_ref()
            .is_some_and(|t| ghostty::terminal_exists(windows, t))
    });
    let resume_blocker = if active {
        Some("Execution is active or pending")
    } else if e.intent == Intent::Finished {
        Some("Work is finished; explicitly reopen to start again")
    } else if e.needs_session() {
        Some("Exact conversation binding required")
    } else if !e.cwd.is_dir() {
        Some("Working directory is missing")
    } else if e.lease.as_ref().is_some_and(|l| l.live()) {
        Some("Work is reserved by an operation")
    } else if r.is_some_and(|r| {
        r.survivors
            .iter()
            .any(|(pid, start)| process::start_time(*pid) == Some(*start))
    }) {
        Some("Owned survivors remain")
    } else if e.ever_started
        && matches!(e.agent, Agent::Codex | Agent::Claude | Agent::Cursor)
        && e.resume_args.is_none()
        && crate::agents::checked_resume_options(e.agent, &e.args).is_err()
    {
        Some("Saved provider options require review")
    } else if !e.dependencies.is_empty() {
        Some("Required services need a start preview and readiness checks")
    } else {
        None
    };
    Ok(
        json!({"schema_version":1,"item":{"id":e.id,"revision":e.revision,"name":e.name,"workspace":e.workspace,"cwd":e.cwd,"provider":e.agent,"provider_home":e.provider_home,"intent":e.intent,"service":e.service,"dependencies":e.dependencies},"continuity":{"state":if e.needs_session(){"unresolved"}else{"verified"},"conversation_id":e.session_id},"run":r,"runtime":if active{"active"}else{"stopped"},"activity":{"state":r.map(|r|r.activity.as_str()).unwrap_or("unknown"),"source":r.and_then(|r|r.activity_source.as_ref()),"observed_at":r.and_then(|r|r.activity_at),"reliable_for_shutdown":false},"capabilities":{"focus":focus,"focus_inventory_available":inventory.is_ok(),"resume":resume_blocker.is_none(),"resume_blocker":resume_blocker,"park":blocked.is_none(),"park_blocker":blocked,"control":if active&&e.service.is_some()&&r.is_some_and(|r|r.supervised){"owned_process_stop"}else{"manual_only"}},"metrics":{"observed_at":model::now(),"source":"sysinfo + proc_pid_rusage","cpu_unit":"percent_of_one_core","memory_unit":"bytes","accounting":"RSS plus charged footprint; not additive physical RAM","owned":metrics},"description":crate::descriptions::saved(store,id)?,"shared_services":"declared dependencies; undeclared relationships remain unresolved"}),
    )
}
#[allow(clippy::too_many_arguments)] // CLI and UI share one explicit policy path.
pub fn preview(
    store: &Store,
    action: Action,
    only: &[Uuid],
    exclude: &[Uuid],
    caller: Option<Caller>,
    close_tab: bool,
    workspace: Option<&str>,
    batch: Option<Uuid>,
) -> Result<Plan> {
    preview_context(
        store, action, only, exclude, caller, close_tab, workspace, batch, false,
    )
}
#[allow(clippy::too_many_arguments)]
pub fn preview_human(
    store: &Store,
    action: Action,
    only: &[Uuid],
    exclude: &[Uuid],
    caller: Option<Caller>,
    close_tab: bool,
    workspace: Option<&str>,
    batch: Option<Uuid>,
) -> Result<Plan> {
    preview_context(
        store, action, only, exclude, caller, close_tab, workspace, batch, true,
    )
}
#[allow(clippy::too_many_arguments)]
pub fn preview_context(
    store: &Store,
    action: Action,
    only: &[Uuid],
    exclude: &[Uuid],
    caller: Option<Caller>,
    close_tab: bool,
    workspace: Option<&str>,
    batch: Option<Uuid>,
    human: bool,
) -> Result<Plan> {
    let state = store.read()?;
    let protected = protected(&state, &caller, exclude)?;
    let mut selected: BTreeSet<Uuid> = if only.is_empty() {
        match action {
            Action::Quiet => state
                .entries
                .iter()
                .filter(|e| {
                    (state.runs.get(&e.id).is_some_and(|r| r.live())
                        || (close_tab && e.terminal_id.is_some()))
                        && e.service
                            .as_ref()
                            .is_none_or(|s| s.lifetime != Lifetime::Persistent)
                })
                .map(|e| e.id)
                .collect(),
            Action::Restore => state
                .entries
                .iter()
                .filter(|e| {
                    workspace.is_none_or(|w| w == e.workspace) && e.intent != Intent::Finished
                })
                .map(|e| e.id)
                .collect(),
            _ => bail!("This action requires an explicit work item"),
        }
    } else {
        only.iter().copied().collect()
    };
    let mut batch_items = None;
    if let Some(id) = batch {
        let b: Batch = store
            .document("batches", &id.to_string())?
            .context("No restore batch with that ID")?;
        selected = b.stopped.iter().map(|i| i.item).collect();
        batch_items = Some(b.stopped);
    }
    for id in &selected {
        state.entry(*id)?;
    }
    if matches!(action, Action::Park | Action::Quiet | Action::Finish) {
        let owners = selected.clone();
        for e in &state.entries {
            if let Some(s) = &e.service
                && owners.contains(&s.owner)
                && ((s.lifetime == Lifetime::Run)
                    || ((action == Action::Quiet || action == Action::Finish)
                        && s.lifetime == Lifetime::Work))
            {
                selected.insert(e.id);
            }
        }
    }
    if matches!(action, Action::Restore | Action::Start) {
        let current = selected.clone();
        let mut closure = BTreeSet::new();
        for id in current {
            dependencies(&state, id, &mut closure);
        }
        selected.extend(closure);
    }
    let ordered = order(&state, &selected)?;
    let mut targets = vec![];
    let mut blockers = vec![];
    let order: Vec<_> = if matches!(action, Action::Restore | Action::Start) {
        ordered
    } else {
        ordered.into_iter().rev().collect()
    };
    for id in order {
        let e = state.entry(id)?;
        let r = state.runs.get(&id);
        let start = matches!(action, Action::Restore | Action::Start);
        let blocked = if !start && caller.is_none() && !human {
            Some(blocker(
                id,
                "caller_unresolved",
                "Controlling session is unknown",
                "Supply --caller ITEM:RUN, or --human for a human-issued request",
            ))
        } else if start {
            if protected.contains(&id) && !r.is_some_and(|r| r.live()) {
                Some(blocker(
                    id,
                    "protected",
                    "Protected work cannot be changed by this batch",
                    "Remove it from this restore request",
                ))
            } else if let Some(items) = &batch_items
                && let Some(item) = items.iter().find(|i| i.item == id)
                && (e.revision != item.revision_after
                    || r.is_none_or(|r| r.token != item.stopped_run))
            {
                Some(blocker(
                    id,
                    "changed_since_batch",
                    "Work changed after profiling preparation",
                    "Resume it separately or create a fresh restore plan",
                ))
            } else if e.needs_session() {
                Some(blocker(
                    id,
                    "continuity_unresolved",
                    "Exact conversation is unresolved",
                    "Bind an exact conversation ID",
                ))
            } else if !e.cwd.is_dir() {
                Some(blocker(
                    id,
                    "directory_missing",
                    "Project directory is missing",
                    "Repair the saved directory before resuming",
                ))
            } else {
                None
            }
        } else {
            stop_blocker(&state, e, &selected, &protected)
        };
        if let Some(b) = &blocked {
            blockers.push(b.clone())
        };
        targets.push(Target {
            item: id,
            revision: e.revision,
            run: r.map(|r| r.token),
            name: e.name.clone(),
            action: if start { Action::Start } else { Action::Stop },
            close_tab,
            blocked,
            binding: e.binding_key(),
        });
    }
    if matches!(action, Action::Quiet | Action::Park | Action::Finish) {
        let refused: BTreeSet<_> = targets
            .iter()
            .filter(|t| t.blocked.is_some())
            .map(|t| t.item)
            .collect();
        for t in &mut targets {
            let e = state.entry(t.item)?;
            if e.service
                .as_ref()
                .is_some_and(|svc| refused.contains(&svc.owner))
                || state.entries.iter().any(|d| {
                    refused.contains(&d.id)
                        && d.dependencies.contains(&t.item)
                        && state.runs.get(&d.id).is_some_and(|r| r.live())
                })
            {
                let b = blocker(
                    t.item,
                    "dependant_refused",
                    "An active dependant cannot be parked",
                    "Leave this service running until its dependant exits",
                );
                t.blocked = Some(b.clone());
                blockers.push(b);
            }
        }
    }
    if action == Action::Quiet {
        blockers.push(Blocker {
            item: None,
            code: "external_scope".into(),
            reason:
                "Unmanaged apps, shared provider servers and undeclared services are not stopped"
                    .into(),
            remedy: "Review gws audit and native app controls before benchmarking".into(),
        });
    }
    let plan = Plan {
        schema_version: 1,
        id: Uuid::new_v4(),
        action,
        targets,
        excluded: protected.into_iter().collect(),
        caller,
        created_at: model::now(),
        expires_at: model::now() + 600,
        boot: model::boot_id(),
        blockers,
        human,
    };
    store.put("plans", &plan.id.to_string(), &plan)?;
    Ok(plan)
}
fn order(state: &model::State, selected: &BTreeSet<Uuid>) -> Result<Vec<Uuid>> {
    fn visit(
        state: &model::State,
        id: Uuid,
        selected: &BTreeSet<Uuid>,
        visiting: &mut BTreeSet<Uuid>,
        done: &mut BTreeSet<Uuid>,
        out: &mut Vec<Uuid>,
    ) -> Result<()> {
        if done.contains(&id) {
            return Ok(());
        }
        if !visiting.insert(id) {
            bail!("Service dependency cycle")
        };
        for dep in &state.entry(id)?.dependencies {
            state.entry(*dep)?;
            if selected.contains(dep) {
                visit(state, *dep, selected, visiting, done, out)?
            }
        }
        visiting.remove(&id);
        done.insert(id);
        out.push(id);
        Ok(())
    }
    let mut out = vec![];
    let mut visiting = BTreeSet::new();
    let mut done = BTreeSet::new();
    for id in selected {
        visit(state, *id, selected, &mut visiting, &mut done, &mut out)?
    }
    Ok(out)
}
pub fn operation(store: &Store, id: Uuid) -> Result<Operation> {
    let mut op: Operation = store
        .document("operations", &id.to_string())?
        .context("Operation not found")?;
    if op.state == "running" && process::start_time(op.worker_pid) != Some(op.worker_start) {
        op.state = "outcome_unknown".into();
        for step in &mut op.steps {
            if step.state == "running" {
                step.state = "outcome_unknown".into();
            }
        }
    }
    Ok(op)
}
fn fresh_operation(plan: &Plan) -> Operation {
    Operation {
        schema_version: 1,
        id: plan.id,
        plan: plan.id,
        state: "accepted".into(),
        worker_pid: 0,
        worker_start: 0,
        created_at: model::now(),
        updated_at: model::now(),
        steps: plan
            .targets
            .iter()
            .map(|t| Step {
                item: t.item,
                state: "pending".into(),
                substeps: vec![],
                error: None,
                survivors: vec![],
                revision_after: None,
            })
            .collect(),
        monitoring: None,
        batch: None,
    }
}
pub fn accept(store: &Store, id: Uuid, retry: bool) -> Result<Operation> {
    if !retry && let Some(op) = store.document::<Operation>("operations", &id.to_string())? {
        return Ok(op);
    }
    let plan: Plan = store
        .document("plans", &id.to_string())?
        .context("Plan not found")?;
    if plan.schema_version != 1 || plan.boot != model::boot_id() || plan.expires_at < model::now() {
        bail!("Plan expired or unsupported; create a new preview")
    }
    store.edit_document("operations", &id.to_string(), |old| {
        if let Some(old) = old {
            let mut op: Operation = serde_json::from_value(old)?;
            if op.state == "running" && process::start_time(op.worker_pid) == Some(op.worker_start)
            {
                return Ok(op);
            }
            if !retry {
                return Ok(op);
            }
            for step in &mut op.steps {
                if matches!(
                    step.state.as_str(),
                    "running" | "failed" | "outcome_unknown"
                ) {
                    step.state = "pending".into();
                }
            }
            op.state = "accepted".into();
            op.worker_pid = 0;
            op.worker_start = 0;
            Ok(op)
        } else {
            Ok(fresh_operation(&plan))
        }
    })
}
struct Reservations {
    store: Store,
    operation: Uuid,
}
impl Drop for Reservations {
    fn drop(&mut self) {
        let _ = self.store.update(|s| {
            for e in &mut s.entries {
                if e.lease
                    .as_ref()
                    .is_some_and(|l| l.operation == self.operation)
                {
                    e.lease = None;
                }
            }
            Ok(())
        });
    }
}
fn stopped_effect(step: &Step) -> bool {
    step.substeps.iter().any(|s| s == "execution_stopped")
}
fn launched_run(step: &Step) -> Option<Uuid> {
    step.substeps.iter().find_map(|effect| {
        effect
            .strip_prefix("launched_run:")
            .and_then(|id| Uuid::parse_str(id).ok())
    })
}
pub(crate) fn record_launch(store: &Store, operation: Uuid, item: Uuid, token: Uuid) -> Result<()> {
    mark_effect(store, operation, item, &format!("launched_run:{token}"))
}
fn mark_effect(store: &Store, operation: Uuid, item: Uuid, effect: &str) -> Result<()> {
    store.edit_document("operations", &operation.to_string(), |old| {
        let mut op: Operation = serde_json::from_value(old.context("Operation disappeared")?)?;
        let step = op
            .steps
            .iter_mut()
            .find(|s| s.item == item)
            .context("Step disappeared")?;
        if !step.substeps.iter().any(|s| s == effect) {
            step.substeps.push(effect.into());
        }
        op.updated_at = model::now();
        Ok(op)
    })?;
    Ok(())
}
pub fn apply(store: &Store, id: Uuid, retry: bool) -> Result<Operation> {
    let result = apply_inner(store, id, retry);
    if let Err(error) = &result
        && store
            .document::<Operation>("operations", &id.to_string())?
            .is_some()
    {
        let _ = store.edit_document("operations", &id.to_string(), |old| {
            let mut op: Operation = serde_json::from_value(old.context("Operation disappeared")?)?;
            if matches!(op.state.as_str(), "running" | "accepted") {
                op.state = "outcome_unknown".into();
                op.updated_at = model::now();
                for step in &mut op.steps {
                    if matches!(step.state.as_str(), "pending" | "running") {
                        step.state = "outcome_unknown".into();
                        step.error = Some(error.to_string());
                    }
                }
                if let Some(step) = op.steps.last_mut()
                    && step.error.is_none()
                {
                    step.error = Some(format!("Finalization: {error}"));
                }
            }
            Ok(op)
        });
    }
    result
}
fn apply_inner(store: &Store, id: Uuid, retry: bool) -> Result<Operation> {
    let accepted = accept(store, id, retry)?;
    if accepted.state != "accepted" {
        return operation(store, id);
    };
    let plan: Plan = store
        .document("plans", &id.to_string())?
        .context("Plan not found")?;
    if plan.schema_version != 1 || plan.boot != model::boot_id() || model::now() > plan.expires_at {
        let mut refused = accepted;
        refused.state = "refused".into();
        refused.updated_at = model::now();
        for step in &mut refused.steps {
            if step.state == "pending" {
                step.state = "refused".into();
                step.error = Some(
                    "Plan expired or belongs to a different boot; create a new preview".into(),
                );
            }
        }
        store.put("operations", &id.to_string(), &refused)?;
        return Ok(refused);
    }
    let pid = std::process::id();
    let start = process::start_time(pid).context("Worker process identity unavailable")?;
    let mut op: Operation = store.edit_document("operations", &id.to_string(), |old| {
        let mut op: Operation = serde_json::from_value(old.context("Operation disappeared")?)?;
        if op.state == "accepted" {
            op.state = "running".into();
            op.worker_pid = pid;
            op.worker_start = start;
        }
        Ok(op)
    })?;
    if op.worker_pid != pid || op.worker_start != start || op.state != "running" {
        return Ok(op);
    };
    let _reservations = Reservations {
        store: store.clone(),
        operation: id,
    };
    store.update(|s| {
        for (t, step) in plan.targets.iter().zip(&mut op.steps) {
            if step.state != "pending" {
                continue;
            };
            let current = s.runs.get(&t.item);
            let same_run = current.map(|r| r.token) == t.run;
            let own_launch =
                launched_run(step).is_some_and(|token| current.is_some_and(|r| r.token == token));
            let e = s
                .entries
                .iter_mut()
                .find(|e| e.id == t.item)
                .context("Work disappeared")?;
            let reconciled =
                ((stopped_effect(step) && same_run) || own_launch) && e.binding_key() == t.binding;
            let reason = if e
                .lease
                .as_ref()
                .is_some_and(|l| l.operation != id && l.live())
            {
                Some("Reserved by another operation")
            } else if (!same_run && !own_launch) || (e.revision != t.revision && !reconciled) {
                Some("Changed since preview; create a new plan")
            } else {
                None
            };
            if let Some(reason) = reason {
                step.state = "refused".into();
                step.error = Some(reason.into());
            } else {
                e.lease = Some(model::Lease {
                    operation: id,
                    pid,
                    start,
                });
            }
        }
        Ok(())
    })?;
    store.put("operations", &id.to_string(), &op)?;
    let state = store.read()?;
    let protected = match protected(&state, &plan.caller, &plan.excluded) {
        Ok(p) => p,
        Err(e) => {
            for step in &mut op.steps {
                if step.state == "pending" {
                    step.state = "refused".into();
                    step.error = Some(e.to_string());
                }
            }
            BTreeSet::new()
        }
    };
    for (index, target) in plan.targets.iter().enumerate() {
        if op.steps[index].state != "pending" {
            continue;
        };
        if store
            .document::<bool>("cancellations", &id.to_string())?
            .unwrap_or(false)
        {
            op.steps[index].state = "refused".into();
            op.steps[index].error =
                Some("Cancelled before this step; completed effects retained".into());
            continue;
        }
        op.steps[index].state = "running".into();
        op.updated_at = model::now();
        store.put("operations", &id.to_string(), &op)?;
        let result = execute_target(store, target, &plan, &protected, &op.steps);
        // Irreversible effects were recorded before later terminal actions could fail.
        if let Some(saved) = store.document::<Operation>("operations", &id.to_string())? {
            op.steps[index].substeps = saved.steps[index].substeps.clone();
        }
        match result {
            Ok((state, effects)) => {
                op.steps[index].state = state;
                for effect in effects {
                    if !op.steps[index].substeps.contains(&effect) {
                        op.steps[index].substeps.push(effect);
                    }
                }
                op.steps[index].error = None;
            }
            Err(e) => {
                op.steps[index].state = if e.to_string().starts_with("REFUSED:") {
                    "refused"
                } else {
                    "failed"
                }
                .into();
                op.steps[index].error = Some(e.to_string());
            }
        }
        let state = store.read()?;
        op.steps[index].revision_after = state.entry(target.item).ok().map(|e| e.revision);
        op.steps[index].survivors = state
            .runs
            .get(&target.item)
            .map(|r| r.survivors.clone())
            .unwrap_or_default();
        op.updated_at = model::now();
        store.put("operations", &id.to_string(), &op)?;
    }
    if plan.action == Action::Finish
        && op
            .steps
            .iter()
            .all(|s| matches!(s.state.as_str(), "succeeded" | "already_satisfied"))
    {
        store.update(|s| {
            for t in &plan.targets {
                let step = op
                    .steps
                    .iter()
                    .find(|step| step.item == t.item)
                    .context("Finish step missing")?;
                let entry = s.entry(t.item)?;
                if s.runs.get(&t.item).map(|r| r.token) != t.run
                    || entry.binding_key() != t.binding
                    || entry.revision != step.revision_after.unwrap_or(t.revision)
                    || entry
                        .lease
                        .as_ref()
                        .is_some_and(|l| l.operation != id && l.live())
                {
                    bail!("Finish finalization refused: work changed after its stop step")
                }
                let e = s
                    .entries
                    .iter_mut()
                    .find(|e| e.id == t.item)
                    .context("Item disappeared")?;
                if e.service.is_none() {
                    e.intent = Intent::Finished;
                }
            }
            Ok(())
        })?;
    }
    if plan.action == Action::Quiet {
        let stopped = plan
            .targets
            .iter()
            .zip(&op.steps)
            .filter_map(|(t, s)| {
                if stopped_effect(s) {
                    t.run.map(|run| BatchItem {
                        item: t.item,
                        stopped_run: run,
                        revision_after: s.revision_after.unwrap_or(t.revision),
                    })
                } else {
                    None
                }
            })
            .collect();
        store.put(
            "batches",
            &id.to_string(),
            &Batch {
                id,
                operation: id,
                created_at: model::now(),
                stopped,
            },
        )?;
        op.batch = Some(id);
        let snapshot = audit(store, false)?;
        store.put("audits", &id.to_string(), &snapshot)?;
        op.monitoring = Some(set_monitoring(store, true)?);
    }
    op.state = if op
        .steps
        .iter()
        .all(|s| matches!(s.state.as_str(), "succeeded" | "already_satisfied"))
    {
        "succeeded"
    } else {
        "partial"
    }
    .into();
    if op
        .monitoring
        .as_ref()
        .is_some_and(|m| m["all_acknowledged"] != true)
    {
        op.state = "partial".into();
    }
    op.updated_at = model::now();
    store.put("operations", &id.to_string(), &op)?;
    Ok(op)
}
fn validate_reserved(state: &model::State, t: &Target, plan: &Plan, step: &Step) -> Result<()> {
    let e = state.entry(t.item)?;
    if e.lease.as_ref().is_none_or(|l| l.operation != plan.id) {
        bail!("REFUSED: operation no longer owns this reservation")
    };
    let current = state.runs.get(&t.item).map(|r| r.token);
    let same_run = current == t.run;
    let own_launch = launched_run(step).is_some_and(|token| current == Some(token));
    if (!same_run && !own_launch)
        || e.binding_key() != t.binding
        || (e.revision != t.revision && !stopped_effect(step) && !own_launch)
    {
        bail!("REFUSED: work changed since preview; create a new plan")
    };
    Ok(())
}
fn execute_target(
    store: &Store,
    t: &Target,
    plan: &Plan,
    protected: &BTreeSet<Uuid>,
    steps: &[Step],
) -> Result<(String, Vec<String>)> {
    let state = store.read()?;
    let step = steps
        .iter()
        .find(|s| s.item == t.item)
        .context("Step missing")?;
    validate_reserved(&state, t, plan, step)?;
    let e = state.entry(t.item)?;
    let r = state.runs.get(&t.item);
    if let Some(b) = &t.blocked {
        bail!("REFUSED: {} ({})", b.reason, b.remedy)
    };
    if t.action == Action::Start {
        if r.is_some_and(|r| r.live()) {
            if let Some(run) = r.filter(|r| r.supervised && r.child_pid.is_none()) {
                wait_started(store, e.id, run.token, Duration::from_secs(10))?;
            }
            check_readiness(store, e.id)?;
            return Ok((
                "already_satisfied".into(),
                vec!["existing_run_retained".into()],
            ));
        }
        if launched_run(step).is_some() {
            bail!("REFUSED: this operation's launch has ended; create a new start preview")
        }
        for dep in &e.dependencies {
            if let Some(step) = steps.iter().find(|s| s.item == *dep)
                && !matches!(step.state.as_str(), "succeeded" | "already_satisfied")
            {
                bail!("REFUSED: prerequisite did not become ready")
            };
            if !store.read()?.runs.get(dep).is_some_and(|r| r.live()) {
                bail!("REFUSED: prerequisite is not running")
            };
            check_readiness(store, *dep)?;
        }
        mark_effect(store, plan.id, e.id, "launch_requested")?;
        engine::open_entry_for(store, e.id, None, Some(plan.id))?;
        let token = store
            .read()?
            .runs
            .get(&e.id)
            .context("Launch record missing")?
            .token;
        mark_effect(store, plan.id, e.id, &format!("launched_run:{token}"))?;
        wait_started(store, e.id, token, Duration::from_secs(10))?;
        mark_effect(store, plan.id, e.id, "execution_started")?;
        check_readiness(store, e.id)?;
        return Ok((
            "succeeded".into(),
            vec![
                if e.service
                    .as_ref()
                    .and_then(|s| s.readiness.as_ref())
                    .is_some()
                {
                    "readiness_checked"
                } else {
                    "readiness_not_configured"
                }
                .into(),
            ],
        ));
    }
    if !plan.human && plan.caller.is_none() {
        bail!("REFUSED: caller unresolved; use --caller ITEM:RUN or a human-issued preview")
    }
    // Selection is not evidence a dependant stopped. Every remaining live dependant blocks its service.
    let empty = BTreeSet::new();
    if let Some(b) = stop_blocker(&state, e, &empty, protected) {
        bail!("REFUSED: {} ({})", b.reason, b.remedy)
    }
    if let Some(svc) = &e.service
        && matches!(plan.action, Action::Quiet | Action::Park | Action::Finish)
        && plan.targets.iter().any(|t| t.item == svc.owner)
        && state.runs.get(&svc.owner).is_some_and(|r| r.live())
    {
        bail!("REFUSED: owning agent remains active")
    }
    let active = r.is_some_and(|r| r.live());
    let mut effects = vec![];
    if active {
        let run = r.context("Run missing")?;
        let service = e.service.as_ref().context("No supported stop method")?;
        if !service.stop_argv.is_empty() {
            let mut command = std::process::Command::new(&service.stop_argv[0]);
            command.args(&service.stop_argv[1..]).current_dir(&e.cwd);
            let output = crate::util::output(command, Duration::from_secs(10))?;
            if !output.status.success() {
                bail!("Stop command failed")
            }
        } else {
            supervisor::request(run.token, "stop")?;
        }
        let started = Instant::now();
        loop {
            let s = store.read()?;
            let current = s.runs.get(&e.id).context("Run disappeared")?;
            if current.token != run.token {
                bail!("REFUSED: run changed during stop")
            };
            if !current.live() {
                if current
                    .survivors
                    .iter()
                    .any(|(pid, start)| process::start_time(*pid) == Some(*start))
                {
                    bail!("Owned survivors remain")
                };
                if current.adopted && !current.ended {
                    store.update(|s| {
                        if let Some(r) = s.runs.get_mut(&e.id).filter(|r| r.token == run.token) {
                            r.ended = true;
                            r.ended_at = Some(model::now());
                        }
                        Ok(())
                    })?;
                }
                break;
            }
            if started.elapsed() > Duration::from_secs(10) {
                bail!("Stop timed out; execution remains visible")
            };
            std::thread::sleep(Duration::from_millis(100));
        }
        mark_effect(store, plan.id, e.id, "execution_stopped")?;
        mark_effect(store, plan.id, e.id, "owned_child_cleanup_verified")?;
    }
    let current = store.read()?;
    let recorded = operation(store, plan.id)?;
    let current_step = recorded
        .steps
        .iter()
        .find(|s| s.item == t.item)
        .context("Step disappeared")?;
    validate_reserved(&current, t, plan, current_step)?;
    if t.close_tab {
        if let Some(terminal) = &e.terminal_id {
            let windows = ghostty::snapshot()?;
            if ghostty::terminal_exists(&windows, terminal) {
                ghostty::close(terminal)?;
                mark_effect(store, plan.id, e.id, "terminal_closed")?;
            }
        }
    } else {
        effects.push("placeholder_retained_when_available".into());
    }
    store.update(|s| {
        validate_reserved(s, t, plan, current_step)?;
        let e = s
            .entries
            .iter_mut()
            .find(|e| e.id == t.item)
            .context("Item disappeared")?;
        e.intent = Intent::Parked;
        if t.close_tab {
            e.terminal_id = None;
        }
        Ok(())
    })?;
    Ok((
        if active || stopped_effect(step) {
            "succeeded"
        } else {
            "already_satisfied"
        }
        .into(),
        effects,
    ))
}
fn wait_started(store: &Store, id: Uuid, token: Uuid, timeout: Duration) -> Result<()> {
    let t = Instant::now();
    loop {
        let s = store.read()?;
        let r = s.runs.get(&id).context("Run missing")?;
        if r.token != token {
            bail!("Execution changed while waiting for launch")
        };
        if r.consumed && r.child_pid.is_some() && !r.ended {
            return Ok(());
        };
        if r.ended {
            bail!("Execution exited before readiness")
        };
        if t.elapsed() > timeout {
            bail!("Launch acknowledgement timed out; inspect before retrying")
        };
        std::thread::sleep(Duration::from_millis(100));
    }
}
pub(crate) fn check_readiness(store: &Store, id: Uuid) -> Result<Uuid> {
    let s = store.read()?;
    let e = s.entry(id)?;
    let token = s
        .runs
        .get(&id)
        .filter(|r| r.live())
        .context("Required service is not active; create a start preview")?
        .token;
    if e.service.is_some()
        && s.runs.get(&id).is_some_and(|r| {
            r.supervised
                && r.child_pid
                    .zip(r.child_start)
                    .is_none_or(|(pid, start)| process::start_time(pid) != Some(start))
        })
    {
        bail!("Required service child is not established or has exited")
    }
    let Some(check) = e.service.as_ref().and_then(|s| s.readiness.as_ref()) else {
        return Ok(token);
    };
    let timeout = Duration::from_secs(check.timeout_seconds.clamp(1, 30));
    let t = Instant::now();
    loop {
        let ready = if let Some(socket) = &check.socket {
            if let Ok(addr) = socket.parse() {
                std::net::TcpStream::connect_timeout(&addr, Duration::from_millis(200)).is_ok()
            } else {
                false
            }
        } else if let Some(program) = check.argv.first() {
            let mut c = std::process::Command::new(program);
            c.args(&check.argv[1..]).current_dir(&e.cwd);
            crate::util::output(c, Duration::from_secs(1)).is_ok_and(|o| o.status.success())
        } else {
            false
        };
        let current = store.read()?;
        if !current
            .runs
            .get(&id)
            .is_some_and(|r| r.token == token && r.live())
        {
            bail!("Execution changed or exited during readiness")
        };
        if ready {
            return Ok(token);
        };
        if t.elapsed() > timeout {
            bail!("Readiness failed; execution remains visible and dependants will not start")
        };
        std::thread::sleep(Duration::from_millis(200));
    }
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServiceRecipe {
    pub name: String,
    pub cwd: PathBuf,
    pub argv: Vec<String>,
    pub owner: Uuid,
    #[serde(default)]
    pub lifetime: Lifetime,
    #[serde(default)]
    pub stop_argv: Vec<String>,
    pub readiness: Option<model::Readiness>,
    #[serde(default)]
    pub dependencies: Vec<Uuid>,
}
pub fn register_service(store: &Store, recipe: ServiceRecipe) -> Result<Uuid> {
    if recipe.argv.is_empty() {
        bail!("Service argv cannot be empty")
    };
    let cwd = recipe.cwd.canonicalize()?;
    if !cwd.is_dir() {
        bail!("Service cwd must be a directory")
    };
    if let Some(check) = &recipe.readiness {
        if check.argv.is_empty() == check.socket.is_none() {
            bail!("Readiness requires exactly one argv or socket check")
        };
        if let Some(addr) = &check.socket {
            let _: std::net::SocketAddr =
                addr.parse().context("Readiness socket must be IP:PORT")?;
        }
    }
    let id = Uuid::new_v4();
    store.update(|s| {
        let owner = s.entry(recipe.owner)?;
        let workspace = owner.workspace.clone();
        s.entries.push(Entry {
            id,
            name: recipe.name,
            cwd,
            agent: Agent::Command,
            workspace,
            command: recipe.argv,
            service: Some(Service {
                owner: recipe.owner,
                lifetime: recipe.lifetime,
                stop_argv: recipe.stop_argv,
                readiness: recipe.readiness,
            }),
            dependencies: recipe.dependencies,
            ..Default::default()
        });
        order(s, &s.entries.iter().map(|e| e.id).collect())?;
        Ok(())
    })?;
    Ok(id)
}
pub fn adopt_service(store: &Store, id: Uuid, pid: u32, start: u64) -> Result<()> {
    if process::start_time(pid) != Some(start) {
        bail!("PID identity does not match")
    };
    let mut monitor = process::Monitor::new();
    let all = monitor.refresh();
    let p = all.get(&pid).context("Process not visible")?;
    #[cfg(unix)]
    {
        let mut sys = sysinfo::System::new();
        sys.refresh_processes(
            sysinfo::ProcessesToUpdate::Some(&[sysinfo::Pid::from_u32(pid)]),
            true,
        );
        if sys
            .process(sysinfo::Pid::from_u32(pid))
            .and_then(|p| p.user_id())
            .is_none_or(|uid| **uid != unsafe { libc::getuid() })
        {
            bail!("Cannot adopt a process without verified current-user ownership")
        }
    }
    store.update(|s|{let e=s.entry(id)?;let service=e.service.as_ref().context("Only registered services can be adopted")?;if service.stop_argv.is_empty(){bail!("External adoption requires an explicit stop_argv; process ancestry does not grant group ownership")};if s.runs.get(&id).is_some_and(|r|r.live()){bail!("Service already has a live run")};s.runs.insert(id,model::Run{token:Uuid::new_v4(),pid,start_time:p.start_time,boot:model::boot_id(),consumed:true,adopted:true,created:model::now(),activity:"unknown".into(),..Default::default()});Ok(())})
}
pub fn cleanup_run_services(store: &Store, owner: Uuid, owner_run: Uuid) -> Result<()> {
    let state = store.read()?;
    let ids: Vec<_> = state
        .entries
        .iter()
        .filter(|e| {
            e.service
                .as_ref()
                .is_some_and(|svc| svc.owner == owner && svc.lifetime == Lifetime::Run)
                && state
                    .runs
                    .get(&e.id)
                    .is_some_and(|r| r.owner_run == Some(owner_run) && r.live())
        })
        .map(|e| e.id)
        .collect();
    if ids.is_empty() {
        return Ok(());
    };
    let plan = preview_human(store, Action::Stop, &ids, &[], None, false, None, None)?;
    let op = apply(store, plan.id, false)?;
    if op.state != "succeeded" {
        store.update(|s| {
            if let Some(r) = s.runs.get_mut(&owner).filter(|r| r.token == owner_run) {
                r.last_error = Some(format!(
                    "Run service cleanup incomplete; inspect operation {}",
                    op.id
                ));
            }
            Ok(())
        })?;
    }
    Ok(())
}
pub fn set_monitoring(store: &Store, paused: bool) -> Result<Value> {
    store.pause(paused)?;
    let dashboards: Vec<Dashboard> = store.documents("dashboards")?;
    let mut results = vec![];
    for d in dashboards {
        if process::start_time(d.pid) != Some(d.start_time) {
            continue;
        };
        let result =
            supervisor::request(d.token, if paused { "pause" } else { "resume_monitoring" });
        results.push(json!({"pid":d.pid,"acknowledged":result.is_ok(),"error":result.err().map(|e|e.to_string())}));
    }
    if paused {
        let deadline = Instant::now() + Duration::from_secs(3);
        while !crate::util::active_model_jobs(store)?.is_empty() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(50));
        }
    }
    let jobs = crate::util::active_model_jobs(store)?;
    Ok(
        json!({"paused":paused,"at":model::now(),"all_acknowledged":results.iter().all(|v|v["acknowledged"]==true)&&jobs.is_empty(),"active_model_jobs":jobs,"dashboards":results,"scope":"gws collectors and temporary model jobs; other applications remain independently active"}),
    )
}
pub fn audit(store: &Store, save: bool) -> Result<Value> {
    let mut monitor = process::Monitor::new();
    monitor.refresh();
    let mut memory = process::MemorySampler::new();
    memory.sample();
    std::thread::sleep(Duration::from_millis(250));
    let processes = monitor.refresh();
    let memory = memory.sample();
    let state = store.read()?;
    let mut owned = BTreeMap::new();
    let mut members = HashSet::new();
    for (id, r) in &state.runs {
        if let Some(m) = process::tree(r, &processes) {
            members.extend(m.processes.iter().map(|p| p.pid));
            owned.insert(*id, m);
        }
    }
    let mut groups: BTreeMap<String, Vec<&process::Proc>> = BTreeMap::new();
    for p in processes.values().filter(|p| !members.contains(&p.pid)) {
        groups.entry(p.name.clone()).or_default().push(p);
    }
    let mut external:Vec<_>=groups.into_iter().map(|(name,ps)|{let rss:u64=ps.iter().map(|p|p.memory).sum();let fp:Vec<_>=ps.iter().filter_map(|p|p.footprint).collect();json!({"name":name,"instance_count":ps.len(),"rss_bytes":rss,"charged_footprint_bytes":if fp.len()==ps.len(){Some(fp.iter().sum::<u64>())}else{None},"footprint_coverage":fp.len(),"cpu_percent_of_one_core":ps.iter().map(|p|p.cpu).sum::<f32>(),"processes":ps,"ownership":"unmanaged_or_shared","cues":{"multiple_instances":ps.len()>1,"long_lived":ps.iter().any(|p|model::now().saturating_sub(p.start_time)>86400),"suspended":ps.iter().any(|p|p.state.to_lowercase().contains("stop"))},"safe_to_stop":false})}).collect();
    external.sort_by(|a, b| {
        b["charged_footprint_bytes"]
            .as_u64()
            .unwrap_or(b["rss_bytes"].as_u64().unwrap_or(0))
            .cmp(
                &a["charged_footprint_bytes"]
                    .as_u64()
                    .unwrap_or(a["rss_bytes"].as_u64().unwrap_or(0)),
            )
    });
    let value = json!({"schema_version":1,"id":Uuid::new_v4(),"observed_at":model::now(),"boot":model::boot_id(),"memory":memory,"managed":owned,"external_groups":external,"monitoring_paused":store.paused(),"limits":["Footprint is charged memory, not additive physical RAM","Shared ownership is not inferred from names","Age, suspension and duplication do not authorize cleanup","A short sample does not establish sustained activity or filesystem causation"]});
    if save {
        store.put(
            "audits",
            value["id"].as_str().context("Audit ID missing")?,
            &value,
        )?;
    }
    Ok(value)
}
pub fn diagnostic(pid: Option<u32>, kind: &str) -> Result<Value> {
    let mut c = std::process::Command::new(match kind {
        "files" => "/usr/sbin/lsof",
        "startup" => "/usr/bin/sfltool",
        "sleep" => "/usr/bin/pmset",
        _ => bail!("Diagnostic must be files, startup or sleep"),
    });
    match kind {
        "files" => {
            let pid = pid.context("Files diagnostic requires a PID")?;
            let start = process::start_time(pid).context("PID unavailable")?;
            c.args(["-nP", "-p", &pid.to_string()]);
            let o = crate::util::output(c, Duration::from_secs(5))?;
            if process::start_time(pid) != Some(start) {
                bail!("PID changed during diagnostic")
            };
            return Ok(
                json!({"schema_version":1,"pid":pid,"start_time":start,"kind":kind,"output":String::from_utf8_lossy(&o.stdout),"error":String::from_utf8_lossy(&o.stderr)}),
            );
        }
        "startup" => {
            c.arg("dumpbtm");
        }
        "sleep" => {
            c.args(["-g", "assertions"]);
        }
        _ => {}
    }
    let o = crate::util::output(c, Duration::from_secs(5))?;
    Ok(
        json!({"schema_version":1,"kind":kind,"output":String::from_utf8_lossy(&o.stdout),"error":String::from_utf8_lossy(&o.stderr)}),
    )
}
