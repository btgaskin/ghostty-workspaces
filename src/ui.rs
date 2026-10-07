//! Terminal-native dashboard. Sampling and actions never run on the input thread.
use crate::{
    catalog::{self, Conversation},
    engine,
    ghostty::{self, Window},
    hardware::{Host, Monitors},
    model::{self, Entry, State, Store},
    operations::{self, Action, Dashboard, Plan},
    process::{self, Metrics, Monitor, Proc},
};
use anyhow::{Context, Result};
use crossterm::event::{self, Event, KeyCode, KeyEventKind};
use ratatui::{
    layout::{Constraint, Layout, Rect},
    style::{Color, Modifier, Style},
    text::Line,
    widgets::{Block, Borders, Gauge, Paragraph, Row, Sparkline, Table, TableState, Wrap},
};
use serde::Serialize;
use std::{
    collections::BTreeMap,
    io::{BufRead, BufReader, Write},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread,
    time::{Duration, Instant},
};
use uuid::Uuid;
#[derive(Debug, Clone, Copy, Default, clap::ValueEnum)]
pub enum Theme {
    #[default]
    Native,
    Dark,
    Mono,
}
impl Theme {
    fn base(self) -> Style {
        match self {
            Self::Dark => Style::default()
                .bg(Color::Rgb(17, 21, 24))
                .fg(Color::Rgb(226, 232, 229)),
            _ => Style::default(),
        }
    }
    fn accent(self) -> Style {
        match self {
            Self::Native => Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
            Self::Dark => Style::default().fg(Color::Rgb(143, 200, 181)),
            Self::Mono => Style::default().add_modifier(Modifier::BOLD),
        }
    }
    fn muted(self) -> Style {
        if matches!(self, Self::Dark) {
            Style::default().fg(Color::Rgb(143, 154, 151))
        } else {
            Style::default().add_modifier(Modifier::DIM)
        }
    }
}
#[derive(Default, Clone)]
struct Sample {
    state: State,
    windows: Vec<Window>,
    processes: BTreeMap<u32, Proc>,
    groups: Vec<process::NameGroup>,
    owned: BTreeMap<Uuid, Metrics>,
    memory: process::Memory,
    hosts: Vec<Host>,
    catalog: Vec<Conversation>,
    catalog_error: Vec<String>,
    inventory_error: Option<String>,
    error: Option<String>,
    ready: bool,
    sequence: u64,
    at: u64,
    inventory_at: u64,
    catalog_at: u64,
    descriptions: BTreeMap<Uuid, crate::descriptions::SavedDescription>,
    search_revision: u64,
}
fn cached_live(r: &model::Run, s: &Sample) -> bool {
    (r.boot.is_empty() || r.boot == model::boot_id())
        && (((!r.ended)
            && ((r.pid == 0 && s.at.saturating_sub(r.created) < 30)
                || s.processes
                    .get(&r.pid)
                    .is_some_and(|p| p.start_time == r.start_time)))
            || r.child_pid.zip(r.child_start).is_some_and(|(pid, start)| {
                s.processes.get(&pid).is_some_and(|p| p.start_time == start)
            }))
}
fn status<'a>(e: &Entry, s: &'a Sample) -> &'a str {
    if e.intent == model::Intent::Finished {
        return "Finished";
    }
    if let Some(r) = s.state.runs.get(&e.id) {
        if r.ended {
            return if e.needs_session() {
                "Needs binding"
            } else if r.survivors.is_empty() {
                "Resume"
            } else {
                "Resolve"
            };
        }
        if cached_live(r, s) {
            if s.inventory_error.is_some() {
                return "Inventory stale";
            }
            return if e
                .terminal_id
                .as_ref()
                .is_some_and(|t| ghostty::terminal_exists(&s.windows, t))
            {
                "Focus"
            } else {
                "Detached"
            };
        }
    }
    if e.needs_session() {
        "Needs binding"
    } else {
        "Resume"
    }
}
fn owned(s: &mut Sample) {
    s.owned = s
        .state
        .runs
        .iter()
        .filter_map(|(id, r)| process::tree(r, &s.processes).map(|m| (*id, m)))
        .collect()
}
fn search_signature(s: &Sample) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hash = std::hash::DefaultHasher::new();
    for c in &s.catalog {
        c.id.hash(&mut hash);
        c.provider_home.hash(&mut hash);
        c.name.hash(&mut hash);
        c.cwd.hash(&mut hash);
        c.updated.hash(&mut hash);
        c.managed_item.hash(&mut hash);
    }
    for (id, d) in &s.descriptions {
        id.hash(&mut hash);
        d.generated_at.hash(&mut hash);
        d.source_fingerprint.hash(&mut hash);
        d.description.purpose.hash(&mut hash);
        d.description.progress.hash(&mut hash);
        d.description.blocker.hash(&mut hash);
        d.description.next_step.hash(&mut hash);
    }
    hash.finish()
}
pub fn list(store: &Store, json: bool) -> Result<()> {
    #[derive(Serialize)]
    struct Listing<'a> {
        entry: &'a Entry,
        status: &'a str,
        metrics: Option<&'a Metrics>,
        subagents: Option<usize>,
    }
    let mut monitor = Monitor::new();
    monitor.refresh();
    thread::sleep(Duration::from_millis(250));
    let mut s = Sample {
        state: store.read()?,
        processes: monitor.refresh(),
        ..Default::default()
    };
    match ghostty::snapshot() {
        Ok(w) => s.windows = w,
        Err(e) => s.inventory_error = Some(e.to_string()),
    };
    owned(&mut s);
    let rows: Vec<_> = s
        .state
        .entries
        .iter()
        .map(|e| Listing {
            entry: e,
            status: status(e, &s),
            metrics: s.owned.get(&e.id),
            subagents: s
                .state
                .runs
                .get(&e.id)
                .filter(|r| r.hooks_seen && !r.ended)
                .map(|r| r.agents.len()),
        })
        .collect();
    if json {
        println!("{}", serde_json::to_string_pretty(&rows)?)
    } else {
        println!("{:<36}  {:<14}  {:>9}  NAME", "ID", "ACTION", "RSS MiB");
        for r in rows {
            println!(
                "{}  {:<14}  {:>9}  {}",
                r.entry.id,
                r.status,
                r.metrics
                    .map(|m| format!("{:.1}", m.memory as f64 / 1048576.))
                    .unwrap_or("-".into()),
                r.entry.name
            )
        }
    }
    Ok(())
}
struct Worker {
    stop: Arc<AtomicBool>,
    handle: Option<thread::JoinHandle<()>>,
}
impl Worker {
    fn start(
        store: Store,
        hosts: Vec<String>,
        shared: Arc<Mutex<Arc<Sample>>>,
        history: Arc<AtomicBool>,
    ) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let signal = stop.clone();
        let handle = thread::spawn(move || {
            let monitors = Monitors::start(&hosts).ok();
            let mut process = Monitor::new();
            let mut memory = process::MemorySampler::new();
            let mut inventory = Instant::now() - Duration::from_secs(10);
            let mut last_history = Instant::now() - Duration::from_secs(60);
            let mut tick = 0;
            while !signal.load(Ordering::Relaxed) {
                let mut s = shared.lock().unwrap().as_ref().clone();
                if tick % 2 == 0 {
                    match store.read() {
                        Ok(state) => {
                            s.state = state;
                            s.error = None;
                            s.descriptions = store
                                .documents::<crate::descriptions::SavedDescription>("descriptions")
                                .unwrap_or_default()
                                .into_iter()
                                .map(|d| (d.item, d))
                                .collect();
                        }
                        Err(e) => s.error = Some(e.to_string()),
                    };
                    s.processes = process.refresh();
                    s.groups = process::name_groups(&s.processes);
                    s.memory = memory.sample();
                    owned(&mut s);
                }
                if inventory.elapsed() >= Duration::from_secs(5) {
                    match ghostty::snapshot() {
                        Ok(w) => {
                            s.windows = w;
                            s.inventory_error = None
                        }
                        Err(e) => s.inventory_error = Some(e.to_string()),
                    };
                    s.inventory_at = model::now();
                    inventory = Instant::now();
                }
                if history.load(Ordering::Relaxed)
                    && last_history.elapsed() >= Duration::from_secs(30)
                {
                    match catalog::collect(&store) {
                        Ok((list, errors)) => {
                            s.catalog = list;
                            s.catalog_error = errors
                        }
                        Err(e) => s.catalog_error = vec![e.to_string()],
                    };
                    s.catalog_at = model::now();
                    last_history = Instant::now();
                }
                s.hosts = monitors.as_ref().map(|m| m.snapshot()).unwrap_or_default();
                s.at = model::now();
                s.sequence += 1;
                s.ready = true;
                s.search_revision = search_signature(&s);
                *shared.lock().unwrap() = Arc::new(s);
                tick += 1;
                for _ in 0..10 {
                    if signal.load(Ordering::Relaxed) {
                        break;
                    }
                    thread::sleep(Duration::from_millis(100));
                }
            }
        });
        Self {
            stop,
            handle: Some(handle),
        }
    }
}
impl Drop for Worker {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}
#[derive(Clone, Copy, Default, PartialEq, Eq)]
enum Page {
    #[default]
    Saved,
    History,
    Mac,
}
impl Page {
    fn index(self) -> usize {
        match self {
            Self::Saved => 0,
            Self::History => 1,
            Self::Mac => 2,
        }
    }
    fn next(self) -> Self {
        match self {
            Self::Saved => Self::History,
            Self::History => Self::Mac,
            Self::Mac => Self::Saved,
        }
    }
}
#[derive(Default)]
struct Position {
    identity: Option<String>,
    query: String,
    detail_offset: u16,
    table: TableState,
}
struct View {
    page: Page,
    positions: [Position; 3],
    searching: bool,
    help: bool,
    detail_only: bool,
    technical: bool,
    paused: bool,
    pausing: bool,
    receipt: Option<operations::Operation>,
    message: String,
    plan: Option<Plan>,
    theme: Theme,
    ascii: bool,
    sort_cpu: bool,
    grouped: bool,
    hardware_only: bool,
    semantic: Option<(Page, String, serde_json::Value)>,
    search_request: Option<Uuid>,
}
impl View {
    fn pos(&self) -> &Position {
        &self.positions[self.page.index()]
    }
    fn pos_mut(&mut self) -> &mut Position {
        &mut self.positions[self.page.index()]
    }
}
#[derive(Clone)]
enum Item<'a> {
    Saved(&'a Entry),
    History(&'a Conversation),
    Process(&'a Proc),
    Group(&'a process::NameGroup),
}
impl Item<'_> {
    fn key(&self) -> String {
        match self {
            Self::Saved(e) => e.id.to_string(),
            Self::History(c) => format!("{}:{}", c.provider_home.display(), c.id),
            Self::Process(p) => format!("{}:{}", p.pid, p.start_time),
            Self::Group(g) => format!("group:{}", g.name),
        }
    }
    fn name(&self) -> String {
        match self {
            Self::Saved(e) => e.name.clone(),
            Self::History(c) => c.name.clone(),
            Self::Process(p) => p.name.clone(),
            Self::Group(g) => g.name.clone(),
        }
    }
}
fn items<'a>(s: &'a Sample, v: &View) -> Vec<Item<'a>> {
    let query = v.pos().query.to_lowercase();
    let mut out: Vec<_> = match v.page {
        Page::Saved => s.state.entries.iter().map(Item::Saved).collect(),
        Page::History => s
            .catalog
            .iter()
            .filter(|c| !c.subagent)
            .map(Item::History)
            .collect(),
        Page::Mac => {
            if v.grouped {
                s.groups.iter().map(Item::Group).collect()
            } else {
                s.processes.values().map(Item::Process).collect()
            }
        }
    };
    let document = |i: &Item| {
        format!(
            "{} {} {} {}",
            i.name(),
            i.key(),
            match i {
                Item::Saved(e) => e.cwd.display().to_string(),
                Item::History(c) => c.cwd.display().to_string(),
                _ => String::new(),
            },
            match i {
                Item::Saved(e) => s.descriptions.get(&e.id),
                Item::History(c) => c.managed_item.and_then(|id| s.descriptions.get(&id)),
                _ => None,
            }
            .map(|d| format!(
                "{} {} {} {}",
                d.description.purpose,
                d.description.progress,
                d.description.blocker,
                d.description.next_step
            ))
            .unwrap_or_default()
        )
    };
    let mut scored: Vec<_> = out
        .into_iter()
        .filter_map(|i| crate::search::fuzzy(&query, &document(&i)).map(|score| (score, i)))
        .collect();
    if !query.is_empty() {
        scored.sort_by_key(|(score, _)| *score)
    }
    out = scored.into_iter().map(|(_, i)| i).collect();
    if let Some((page, searched, result)) = &v.semantic
        && *page == v.page
        && searched == &v.pos().query
        && result["ui_snapshot"] == s.search_revision
    {
        let ranks = result["semantic"]["ranked"].as_array();
        out.sort_by_key(|i| {
            ranks
                .and_then(|rows| {
                    rows.iter().position(|r| match i {
                        Item::History(c) => {
                            r["conversation"]["id"] == c.id.to_string()
                                && r["conversation"]["provider_home"]
                                    == c.provider_home.to_string_lossy().as_ref()
                        }
                        _ => false,
                    })
                })
                .unwrap_or(usize::MAX)
        });
    }
    if v.page == Page::Mac {
        out.sort_by(|a, b| {
            let value = |i: &Item| match i {
                Item::Process(p) => {
                    if v.sort_cpu {
                        p.cpu as f64
                    } else {
                        p.footprint.unwrap_or(p.memory) as f64
                    }
                }
                Item::Group(g) => {
                    if v.sort_cpu {
                        g.metrics.cpu as f64
                    } else {
                        (if g.metrics.footprint_coverage == g.metrics.processes.len() {
                            g.metrics.footprint.unwrap_or(g.metrics.memory)
                        } else {
                            g.metrics.memory
                        }) as f64
                    }
                }
                _ => 0.,
            };
            value(b).total_cmp(&value(a)).then(a.key().cmp(&b.key()))
        });
    }
    out
}
fn select(v: &mut View, items: &[Item]) {
    let pos = v.pos_mut();
    let index = pos
        .identity
        .as_ref()
        .and_then(|id| items.iter().position(|i| &i.key() == id))
        .unwrap_or(pos.table.selected().unwrap_or(0))
        .min(items.len().saturating_sub(1));
    pos.table.select((!items.is_empty()).then_some(index));
    if let Some(i) = items.get(index) {
        pos.identity = Some(i.key());
    }
}
enum Update {
    Message(String),
    Plan(Plan),
    Stopped(Option<std::os::unix::net::UnixStream>),
    Receipt(operations::Operation),
    Ranked(Uuid, Page, String, serde_json::Value),
    RankFailed(Uuid, String),
}
fn ranked_update(
    request: Uuid,
    page: Page,
    query: String,
    signature: u64,
    result: Result<serde_json::Value>,
) -> Update {
    match result {
        Ok(mut result) => {
            result["ui_snapshot"] = serde_json::json!(signature);
            Update::Ranked(request, page, query, result)
        }
        Err(error) => Update::RankFailed(request, error.to_string()),
    }
}
fn failed_search(view: &mut View, request: Uuid, error: String) {
    if view.search_request == Some(request) {
        view.search_request = None;
        view.message = format!("Fuzzy results retained · {error} · J retries");
    }
}
fn async_task(tx: mpsc::Sender<Update>, task: impl FnOnce() -> Result<Update> + Send + 'static) {
    thread::spawn(move || {
        let result = task().unwrap_or_else(|e| Update::Message(e.to_string()));
        let _ = tx.send(result);
    });
}
pub fn run(store: &Store, hosts: &[String], theme: Theme, ascii: bool) -> Result<()> {
    let shared = Arc::new(Mutex::new(Arc::new(Sample {
        state: store.read()?,
        ..Default::default()
    })));
    let history = Arc::new(AtomicBool::new(false));
    let mut worker = if store.paused() {
        None
    } else {
        Some(Worker::start(
            store.clone(),
            hosts.to_vec(),
            shared.clone(),
            history.clone(),
        ))
    };
    let token = Uuid::new_v4();
    let listener = crate::supervisor::listener(token)?;
    let registration = Dashboard {
        token,
        pid: std::process::id(),
        start_time: process::start_time(std::process::id()).unwrap_or(0),
        paused: worker.is_none(),
    };
    store.put("dashboards", &token.to_string(), &registration)?;
    struct Socket(Uuid);
    impl Drop for Socket {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(crate::supervisor::socket_path(self.0));
        }
    }
    let _socket = Socket(token);
    let (tx, rx) = mpsc::channel();
    let result = ratatui::run(|terminal| -> Result<()> {
        let mut view = View {
            page: Page::Saved,
            positions: Default::default(),
            searching: false,
            help: false,
            detail_only: false,
            technical: false,
            paused: worker.is_none(),
            pausing: false,
            receipt: None,
            message: "Enter: focus/resume · p: preview park · Q: prepare profiling".into(),
            plan: None,
            theme,
            ascii,
            sort_cpu: false,
            grouped: true,
            hardware_only: false,
            semantic: None,
            search_request: None,
        };
        let mut dirty = true;
        let mut sequence = u64::MAX;
        loop {
            let sample = shared.lock().unwrap().clone();
            if sample.sequence != sequence {
                sequence = sample.sequence;
                dirty = true;
            }
            while let Ok(update) = rx.try_recv() {
                match update {
                    Update::Stopped(stream) => {
                        view.paused = true;
                        view.pausing = false;
                        view.message = "Monitoring paused; collectors stopped and joined".into();
                        store.put(
                            "dashboards",
                            &token.to_string(),
                            &Dashboard {
                                paused: true,
                                ..registration.clone()
                            },
                        )?;
                        if let Some(mut stream) = stream {
                            writeln!(stream, "{}", serde_json::json!({"ok":true,"paused":true}))?;
                        }
                    }
                    Update::Receipt(op) => {
                        view.message = format!("Operation {} · {}", op.id, op.state);
                        view.receipt = Some(op);
                    }
                    Update::Message(m) => view.message = m,
                    Update::Ranked(request, page, query, result) => {
                        if view.search_request == Some(request)
                            && view.page == page
                            && view.pos().query == query
                        {
                            if result["ui_snapshot"] != sample.search_revision {
                                view.search_request = None;
                                view.message="Search metadata changed; fuzzy order retained. J reranks again.".into();
                                dirty = true;
                                continue;
                            }
                            view.message = if result["backend"] == "jev" {
                                "Jev ranked fuzzy candidates · confidence describes model uncertainty".into()
                            } else {
                                format!("Fuzzy results retained · {}", result["errors"])
                            };
                            view.semantic = Some((page, query, result));
                            view.search_request = None;
                        }
                    }
                    Update::RankFailed(request, error) => failed_search(&mut view, request, error),
                    Update::Plan(plan) => {
                        view.plan = Some(plan);
                        view.message = "Review exact targets · y: apply · Esc: cancel".into();
                        view.pos_mut().detail_offset = 0;
                    }
                }
                dirty = true;
            }
            if let Ok((mut stream, _)) = listener.accept() {
                stream.set_read_timeout(Some(Duration::from_millis(200)))?;
                let mut line = String::new();
                use std::io::Read;
                let _ = BufReader::new(&mut stream).take(8192).read_line(&mut line);
                let request: serde_json::Value = serde_json::from_str(&line).unwrap_or_default();
                let action = request["action"].as_str().unwrap_or("");
                if request["token"] != token.to_string()
                    || !matches!(action, "pause" | "resume_monitoring")
                {
                    writeln!(
                        stream,
                        "{}",
                        serde_json::json!({"ok":false,"error":"Invalid dashboard request"})
                    )?;
                } else if action == "pause" && !view.paused && !view.pausing {
                    view.pausing = true;
                    view.message = "Pausing monitoring…".into();
                    let stopping = worker.take();
                    async_task(tx.clone(), move || {
                        drop(stopping);
                        Ok(Update::Stopped(Some(stream)))
                    });
                    dirty = true;
                } else if view.pausing {
                    writeln!(
                        stream,
                        "{}",
                        serde_json::json!({"ok":false,"error":"Pause in progress; retry after acknowledgement"})
                    )?;
                } else {
                    if action == "resume_monitoring" && worker.is_none() {
                        worker = Some(Worker::start(
                            store.clone(),
                            hosts.to_vec(),
                            shared.clone(),
                            history.clone(),
                        ));
                        view.paused = false;
                    }
                    store.put(
                        "dashboards",
                        &token.to_string(),
                        &Dashboard {
                            paused: view.paused,
                            ..registration.clone()
                        },
                    )?;
                    writeln!(
                        stream,
                        "{}",
                        serde_json::json!({"ok":true,"paused":view.paused})
                    )?;
                    dirty = true;
                }
            }
            if !dirty && !event::poll(Duration::from_millis(100))? {
                continue;
            }
            let list = items(&sample, &view);
            select(&mut view, &list);
            if dirty {
                terminal.draw(|f| draw(f, &sample, &list, &mut view))?;
                dirty = false;
            }
            if !event::poll(Duration::from_millis(100))? {
                continue;
            };
            let input = event::read()?;
            dirty = true;
            let Event::Key(key) = input else { continue };
            if key.kind != KeyEventKind::Press {
                continue;
            };
            if view.searching {
                view.search_request = None;
                match key.code {
                    KeyCode::Esc | KeyCode::Enter => view.searching = false,
                    KeyCode::Backspace => {
                        view.pos_mut().query.pop();
                    }
                    KeyCode::Char(c) => view.pos_mut().query.push(c),
                    _ => {}
                }
                continue;
            }
            if view.receipt.is_some() {
                match key.code {
                    KeyCode::Esc => view.receipt = None,
                    KeyCode::PageDown => {
                        view.pos_mut().detail_offset = view.pos().detail_offset.saturating_add(5)
                    }
                    KeyCode::PageUp => {
                        view.pos_mut().detail_offset = view.pos().detail_offset.saturating_sub(5)
                    }
                    _ => {}
                }
                continue;
            }
            if view.help {
                view.help = false;
                continue;
            }
            if let Some(plan) = view.plan.clone() {
                match key.code {
                    KeyCode::Esc => {
                        view.plan = None;
                        view.message = "Preview cancelled".into();
                    }
                    KeyCode::Char('y') => {
                        view.plan = None;
                        view.message = format!("Applying operation {}", plan.id);
                        let store = store.clone();
                        async_task(tx.clone(), move || {
                            let mut cmd = std::process::Command::new(std::env::current_exe()?);
                            use std::os::unix::process::CommandExt;
                            cmd.process_group(0)
                                .args([
                                    "--state-dir",
                                    store.dir.to_str().context("State directory encoding")?,
                                    "worker",
                                    &plan.id.to_string(),
                                ])
                                .stdin(std::process::Stdio::null());
                            let output = cmd.output()?;
                            let op = operations::operation(&store, plan.id)?;
                            if !output.status.success() && op.state == "running" {
                                return Ok(Update::Message(
                                    String::from_utf8_lossy(&output.stderr).into_owned(),
                                ));
                            };
                            Ok(Update::Receipt(op))
                        });
                    }
                    KeyCode::PageDown => {
                        view.pos_mut().detail_offset = view.pos().detail_offset.saturating_add(5)
                    }
                    KeyCode::PageUp => {
                        view.pos_mut().detail_offset = view.pos().detail_offset.saturating_sub(5)
                    }
                    _ => {}
                }
                continue;
            }
            let chosen = list.get(view.pos().table.selected().unwrap_or(0));
            match key.code {
                KeyCode::Char('q') => break,
                KeyCode::Esc => {
                    view.search_request = None;
                    if view.detail_only {
                        view.detail_only = false
                    } else if !view.pos().query.is_empty() {
                        view.pos_mut().query.clear()
                    } else {
                        break;
                    }
                }
                KeyCode::Tab => {
                    view.search_request = None;
                    view.page = view.page.next();
                    view.detail_only = false;
                    history.store(view.page == Page::History, Ordering::Relaxed)
                }
                KeyCode::Char('/') => view.searching = true,
                KeyCode::Char('J') => {
                    if view.search_request.is_some() {
                        view.message =
                            "Jev request already pending; fuzzy results remain available".into();
                    } else if view.page != Page::History || view.pos().query.trim().is_empty() {
                        view.message =
                            "Search History with /, then J to rerank fuzzy matches".into();
                    } else if view.paused || view.pausing {
                        view.message = "Jev search disabled while profiling is paused".into();
                    } else {
                        let query = view.pos().query.clone();
                        let request = Uuid::new_v4();
                        view.search_request = Some(request);
                        let store = store.clone();
                        let page = view.page;
                        let signature = sample.search_revision;
                        view.message = "Jev reranking… fuzzy results remain available".into();
                        async_task(tx.clone(), move || {
                            let result = catalog::search_with_backend(&store, &query, 100, 0, true);
                            Ok(ranked_update(request, page, query, signature, result))
                        });
                    }
                }
                KeyCode::Char('?') => view.help = true,
                KeyCode::Char('d') => view.detail_only = !view.detail_only,
                KeyCode::Char('h') => view.hardware_only = !view.hardware_only,
                KeyCode::Char('t') => view.technical = !view.technical,
                KeyCode::Down | KeyCode::Char('j') => {
                    let index = (view.pos().table.selected().unwrap_or(0) + 1)
                        .min(list.len().saturating_sub(1));
                    view.pos_mut().identity = list.get(index).map(|i| i.key());
                    view.pos_mut().detail_offset = 0;
                }
                KeyCode::Up | KeyCode::Char('k') => {
                    let index = view.pos().table.selected().unwrap_or(0).saturating_sub(1);
                    view.pos_mut().identity = list.get(index).map(|i| i.key());
                    view.pos_mut().detail_offset = 0;
                }
                KeyCode::PageDown => {
                    view.pos_mut().detail_offset = view.pos().detail_offset.saturating_add(5)
                }
                KeyCode::PageUp => {
                    view.pos_mut().detail_offset = view.pos().detail_offset.saturating_sub(5)
                }
                KeyCode::Char('c') => view.sort_cpu = true,
                KeyCode::Char('m') => view.sort_cpu = false,
                KeyCode::Char('v') if view.page == Page::Mac => {
                    view.grouped = !view.grouped;
                    view.pos_mut().identity = None;
                }
                KeyCode::Enter => {
                    if let Some(chosen) = chosen {
                        match chosen {
                            Item::Saved(e) if e.intent == model::Intent::Finished => {
                                view.detail_only = true;
                            }
                            Item::Saved(e) => {
                                let id = e.id;
                                let deps = !e.dependencies.is_empty();
                                let store = store.clone();
                                async_task(tx.clone(), move || {
                                    if deps {
                                        return Ok(Update::Plan(operations::preview_human(
                                            &store,
                                            Action::Start,
                                            &[id],
                                            &[],
                                            None,
                                            false,
                                            None,
                                            None,
                                        )?));
                                    };
                                    engine::open_entry(&store, id, None)?;
                                    Ok(Update::Message("Focused/resumed selected work".into()))
                                });
                            }
                            _ => view.detail_only = true,
                        }
                    }
                }
                KeyCode::Char('o') => {
                    if let Some(Item::History(c)) = chosen {
                        let conversation = (*c).clone();
                        let store = store.clone();
                        async_task(tx.clone(), move || {
                            let id = catalog::manage(&store, &conversation)?;
                            engine::open_entry(&store, id, None)?;
                            Ok(Update::Message("Opened exact conversation".into()))
                        });
                    }
                }
                KeyCode::Char('p') | KeyCode::Char('f') => {
                    if let Some(Item::Saved(e)) = chosen {
                        let id = e.id;
                        let action = if key.code == KeyCode::Char('f') {
                            Action::Finish
                        } else {
                            Action::Park
                        };
                        let store = store.clone();
                        async_task(tx.clone(), move || {
                            Ok(Update::Plan(operations::preview_human(
                                &store,
                                action,
                                &[id],
                                &[],
                                None,
                                false,
                                None,
                                None,
                            )?))
                        });
                    }
                }
                KeyCode::Char('r') => {
                    if let Some(Item::Saved(e)) = chosen {
                        let workspace = e.workspace.clone();
                        let store = store.clone();
                        async_task(tx.clone(), move || {
                            Ok(Update::Plan(operations::preview_human(
                                &store,
                                Action::Restore,
                                &[],
                                &[],
                                None,
                                false,
                                Some(&workspace),
                                None,
                            )?))
                        });
                    }
                }
                KeyCode::Char('Q') => {
                    let store = store.clone();
                    async_task(tx.clone(), move || {
                        Ok(Update::Plan(operations::preview_human(
                            &store,
                            Action::Quiet,
                            &[],
                            &[],
                            None,
                            true,
                            None,
                            None,
                        )?))
                    });
                }
                KeyCode::Char('b') => {
                    if let Some(Item::Saved(e)) = chosen {
                        let id = e.id;
                        let store = store.clone();
                        view.message = "Generating description · Luna / medium".into();
                        async_task(tx.clone(), move || {
                            crate::descriptions::generate(&store, id, None, "gpt-6-luna")?;
                            Ok(Update::Message(
                                "Description saved; generated prose is not activity evidence"
                                    .into(),
                            ))
                        });
                    }
                }
                KeyCode::Char('s') => {
                    let store = store.clone();
                    async_task(tx.clone(), move || {
                        engine::save_quiet(&store, "default")?;
                        Ok(Update::Message(
                            "Saved current surfaces; ambiguous bindings retained".into(),
                        ))
                    });
                }
                KeyCode::Char('M') if !view.pausing => {
                    let pause = !view.paused;
                    store.pause(pause)?;
                    if pause {
                        view.pausing = true;
                        view.message = "Pausing monitoring…".into();
                        let stopping = worker.take();
                        async_task(tx.clone(), move || {
                            drop(stopping);
                            Ok(Update::Stopped(None))
                        });
                    } else {
                        worker = Some(Worker::start(
                            store.clone(),
                            hosts.to_vec(),
                            shared.clone(),
                            history.clone(),
                        ));
                        view.paused = false;
                        store.put(
                            "dashboards",
                            &token.to_string(),
                            &Dashboard {
                                paused: false,
                                ..registration.clone()
                            },
                        )?;
                    }
                }
                _ => {}
            }
        }
        Ok(())
    });
    drop(worker);
    result
}
fn panel(title: String, theme: Theme, ascii: bool) -> Block<'static> {
    Block::default()
        .title(title)
        .borders(Borders::ALL)
        .border_type(if ascii {
            ratatui::widgets::BorderType::Plain
        } else {
            ratatui::widgets::BorderType::Rounded
        })
        .border_style(theme.muted())
}
fn age(at: u64) -> String {
    if at == 0 {
        return "not observed".into();
    }
    let seconds = model::now().saturating_sub(at);
    if seconds < 60 {
        format!("{seconds}s ago")
    } else if seconds < 3600 {
        format!("{}m ago", seconds / 60)
    } else {
        format!("{}h ago", seconds / 3600)
    }
}
fn draw(f: &mut ratatui::Frame, s: &Sample, list: &[Item], v: &mut View) {
    let area = f.area();
    f.render_widget(Block::default().style(v.theme.base()), area);
    if area.width < 40 || area.height < 10 {
        f.render_widget(
            Paragraph::new("gws · enlarge terminal to at least 40×10; q quits"),
            area,
        );
        return;
    }
    let chunks = Layout::vertical([
        Constraint::Length(2),
        Constraint::Min(1),
        Constraint::Length(2),
    ])
    .split(area);
    let tabs = format!(
        " gws   {}Saved   {}History   {}Mac{}",
        if v.page == Page::Saved { "› " } else { "" },
        if v.page == Page::History { "› " } else { "" },
        if v.page == Page::Mac { "› " } else { "" },
        if v.paused {
            if s.ready {
                format!("   PAUSED · frozen sample {}", age(s.memory.observed_at))
            } else {
                "   PAUSED · no sample collected".into()
            }
        } else {
            String::new()
        }
    );
    f.render_widget(
        Paragraph::new(tabs)
            .style(v.theme.accent())
            .block(Block::default().borders(Borders::BOTTOM)),
        chunks[0],
    );
    let wide = area.width >= 100 && area.height >= 30;
    let medium = area.width >= 80 && area.height >= 24;
    let (left, right) = if wide && !v.hardware_only {
        let c = Layout::horizontal([Constraint::Percentage(60), Constraint::Percentage(40)])
            .split(chunks[1]);
        (c[0], Some(c[1]))
    } else {
        (chunks[1], None)
    };
    if v.help {
        f.render_widget(Paragraph::new("Tab: Saved / History / Mac\n/: fuzzy search   J: Jev rerank History search\nEnter: focus/resume or details   o: open History\nb: generate description   v: Mac groups/processes\np: preview parking   r: preview workspace restore\nQ: prepare profiling   M: pause/resume monitoring\ns: save tabs   d: details   h: hardware   t: technical IDs\nm / c: memory / CPU sort   PgUp/PgDn: scroll\nEsc: back/clear/quit   q: quit\n\nProvider parking: finish/cancel through its own UI, then exit.\nSuspension does not reclaim memory.").block(panel(" Help · any key returns ".into(),v.theme,v.ascii)).wrap(Wrap{trim:false}),left);
    } else if let Some(plan) = &v.plan {
        let mut lines = vec![Line::from(format!(
            "{:?} · {} targets · y applies this exact plan",
            plan.action,
            plan.targets.len()
        ))];
        for t in &plan.targets {
            lines.push(Line::from(format!(
                "{} {} · {:?} · {}",
                if t.blocked.is_some() {
                    "REFUSED"
                } else {
                    "READY"
                },
                t.name,
                t.action,
                if t.close_tab { "close tab" } else { "keep tab" }
            )));
            if let Some(b) = &t.blocked {
                lines.push(Line::from(format!("  {} · {}", b.reason, b.remedy)));
            }
        }
        for b in &plan.blockers {
            if b.item.is_none() {
                lines.push(Line::from(format!("{} · {}", b.reason, b.remedy)))
            }
        }
        f.render_widget(
            Paragraph::new(lines)
                .block(panel(" Review plan ".into(), v.theme, v.ascii))
                .wrap(Wrap { trim: false })
                .scroll((v.pos().detail_offset, 0)),
            left,
        );
    } else if let Some(op) = &v.receipt {
        let mut lines = vec![Line::from(format!("{} · {}", op.id, op.state))];
        for step in &op.steps {
            lines.push(Line::from(format!("{} · {}", step.item, step.state)));
            for effect in &step.substeps {
                lines.push(Line::from(format!("  {effect}")));
            }
            if let Some(error) = &step.error {
                lines.push(Line::from(format!("  {error}")));
            }
        }
        if let Some(batch) = op.batch {
            lines.push(Line::from(format!(
                "Restore: gws restore --batch {batch} --preview"
            )));
        }
        if let Some(m) = &op.monitoring {
            lines.push(Line::from(format!(
                "Monitoring paused: {} · collectors acknowledged: {}",
                m["paused"], m["all_acknowledged"]
            )));
        }
        f.render_widget(
            Paragraph::new(lines)
                .wrap(Wrap { trim: false })
                .scroll((v.pos().detail_offset, 0))
                .block(panel(
                    " Operation receipt · Esc returns ".into(),
                    v.theme,
                    v.ascii,
                )),
            left,
        );
    } else if v.hardware_only {
        draw_hardware(f, left, s, v);
    } else {
        let summary = if !wide { if medium { 2 } else { 1 } } else { 0 };
        let sub = Layout::vertical([Constraint::Length(summary), Constraint::Min(1)]).split(left);
        if summary > 0 {
            f.render_widget(
                Paragraph::new(if !s.ready {
                    "Machine metrics unavailable · monitoring has not collected a sample".into()
                } else {
                    format!(
                        "RAM {:.1}/{:.1} GiB · pressure {} · swap {:.1} GiB",
                        s.memory.used_bytes as f64 / 1073741824.,
                        s.memory.total_bytes as f64 / 1073741824.,
                        s.memory.pressure.as_deref().unwrap_or("unavailable"),
                        s.memory.swap_used_bytes as f64 / 1073741824.
                    )
                })
                .style(v.theme.muted()),
                sub[0],
            );
        }
        let both = (wide || medium) && !v.detail_only;
        let (list_area, detail_area) = if both {
            let c = Layout::vertical([Constraint::Percentage(55), Constraint::Percentage(45)])
                .split(sub[1]);
            (Some(c[0]), Some(c[1]))
        } else if v.detail_only {
            (None, Some(sub[1]))
        } else {
            (Some(sub[1]), None)
        };
        if let Some(a) = list_area {
            draw_list(f, a, s, list, v);
        }
        if let Some(a) = detail_area {
            draw_details(f, a, s, list.get(v.pos().table.selected().unwrap_or(0)), v);
        }
    }
    if let Some(right) = right {
        draw_hardware(f, right, s, v);
    }
    let footer = if v.searching {
        format!("/{} · Enter accepts · Esc returns", v.pos().query)
    } else {
        format!(
            "{}\nTab views · / search · d details · ? help · q quit",
            v.message
        )
    };
    f.render_widget(Paragraph::new(footer).style(v.theme.muted()), chunks[2]);
}
fn draw_list(f: &mut ratatui::Frame, area: Rect, s: &Sample, list: &[Item], v: &mut View) {
    if list.is_empty() {
        let text = if !v.pos().query.is_empty() {
            "No matching items · Esc clears search"
        } else {
            match v.page {
                Page::Saved => "No saved work · launch with gws codex or save existing tabs",
                Page::History => {
                    if !s.catalog_error.is_empty() {
                        "Conversation metadata unavailable; inspect source access"
                    } else if v.paused {
                        "Monitoring paused · M resumes conversation discovery"
                    } else if !s.ready {
                        "Loading conversation metadata…"
                    } else {
                        "No root conversations found"
                    }
                }
                Page::Mac => "Collecting visible processes…",
            }
        };
        f.render_widget(
            Paragraph::new(text)
                .block(panel(" Work register ".into(), v.theme, v.ascii))
                .wrap(Wrap { trim: false }),
            area,
        );
        return;
    }
    let compact = area.width < 60;
    let rows = list.iter().map(|item| {
        let (name, state, ram, cpu) = match item {
            Item::Saved(e) => {
                let m = s.owned.get(&e.id);
                (
                    format!(
                        "{} · {}",
                        e.name,
                        e.cwd.file_name().unwrap_or_default().to_string_lossy()
                    ),
                    status(e, s).to_owned(),
                    m.map(|m| format!("{:.0}", m.memory as f64 / 1048576.))
                        .unwrap_or("-".into()),
                    m.map(|m| format!("{:.1}", m.cpu)).unwrap_or("-".into()),
                )
            }
            Item::History(c) => (
                format!("{} · {}", c.name, c.cwd.display()),
                if c.archived {
                    "Archived"
                } else if c.managed_item.is_some() {
                    "Registered"
                } else {
                    "Details"
                }
                .into(),
                classification(s, v, c)
                    .and_then(|r| r["confidence"].as_f64())
                    .map(|n| format!("{:.0}%", n * 100.))
                    .unwrap_or("-".into()),
                classification(s, v, c)
                    .and_then(|r| r["relevance_score"].as_f64())
                    .map(|n| format!("{n:.2}/2"))
                    .unwrap_or("-".into()),
            ),
            Item::Process(p) => (
                format!("{} · {}", p.name, p.pid),
                p.state.clone(),
                format!(
                    "{:.0} {}",
                    p.footprint.unwrap_or(p.memory) as f64 / 1048576.,
                    if p.footprint.is_some() { "FP" } else { "RSS" }
                ),
                format!("{:.1}", p.cpu),
            ),
            Item::Group(g) => (
                g.name.clone(),
                format!("{} processes", g.metrics.processes.len()),
                {
                    let complete = g.metrics.footprint_coverage == g.metrics.processes.len();
                    format!(
                        "{:.0} {}",
                        (if complete {
                            g.metrics.footprint.unwrap_or(g.metrics.memory)
                        } else {
                            g.metrics.memory
                        }) as f64
                            / 1048576.,
                        if complete { "FP" } else { "RSS" }
                    )
                },
                format!("{:.1}", g.metrics.cpu),
            ),
        };
        Row::new(if compact {
            vec![name, state, ram]
        } else {
            vec![name, state, ram, cpu]
        })
    });
    let constraints = if compact {
        vec![
            Constraint::Min(12),
            Constraint::Length(10),
            Constraint::Length(10),
        ]
    } else {
        vec![
            Constraint::Min(18),
            Constraint::Length(14),
            Constraint::Length(9),
            Constraint::Length(7),
        ]
    };
    let state_label = if v.page == Page::Mac {
        "STATE"
    } else {
        "ACTION"
    };
    let ram_label = if v.page == Page::History {
        "CONF %"
    } else if v.page == Page::Mac {
        "MiB FP/RSS"
    } else {
        "RSS MiB"
    };
    let header = if compact {
        vec!["WORK", state_label, ram_label]
    } else {
        vec![
            "WORK / PROJECT",
            state_label,
            ram_label,
            if v.page == Page::History {
                "REL /2"
            } else {
                "CPU %"
            },
        ]
    };
    let title = match v.page {
        Page::Saved => " Saved · owned RSS ",
        Page::History => " History · metadata only ",
        Page::Mac => {
            if v.grouped {
                " Mac · executable groups · v: processes "
            } else {
                " Mac · processes · v: groups "
            }
        }
    };
    let widget = Table::new(rows, constraints)
        .column_spacing(1)
        .header(Row::new(header).style(v.theme.muted()))
        .row_highlight_style(Style::default().add_modifier(Modifier::REVERSED | Modifier::BOLD))
        .highlight_symbol(if v.ascii { "> " } else { "› " })
        .block(panel(
            format!(
                "{}{}",
                title,
                if v.pos().query.is_empty() {
                    String::new()
                } else {
                    format!("/{}", v.pos().query)
                }
            ),
            v.theme,
            v.ascii,
        ));
    f.render_stateful_widget(widget, area, &mut v.pos_mut().table);
}
fn classification<'a>(s: &Sample, v: &'a View, c: &Conversation) -> Option<&'a serde_json::Value> {
    let (page, query, result) = v.semantic.as_ref()?;
    if *page != v.page || query != &v.pos().query || result["ui_snapshot"] != s.search_revision {
        return None;
    }
    result["semantic"]["ranked"].as_array()?.iter().find(|r| {
        r["conversation"]["id"] == c.id.to_string()
            && r["conversation"]["provider_home"] == c.provider_home.to_string_lossy().as_ref()
    })
}
fn draw_details(f: &mut ratatui::Frame, area: Rect, s: &Sample, item: Option<&Item>, v: &View) {
    let mut lines = vec![];
    match item {
        Some(Item::Group(g)) => {
            lines.push(Line::from(g.name.clone()).style(v.theme.accent()));
            lines.push(Line::from(format!(
                "{} processes · {:.1}% CPU · {:.1} MiB summed RSS",
                g.metrics.processes.len(),
                g.metrics.cpu,
                g.metrics.memory as f64 / 1048576.
            )));
            lines.push(Line::from(
                "Grouped by executable name; shared pages can be counted repeatedly.",
            ));
            lines.push(Line::from("Ownership is verified per process. Shared/unmanaged work requires separate review."));
            for p in &g.metrics.processes {
                let owners: Vec<_> = s
                    .owned
                    .iter()
                    .filter(|(_, m)| {
                        m.processes
                            .iter()
                            .any(|member| member.pid == p.pid && member.start_time == p.start_time)
                    })
                    .filter_map(|(id, _)| s.state.entry(*id).ok().map(|e| e.name.as_str()))
                    .collect();
                lines.push(Line::from(format!(
                    "PID {} · {:.0} MiB RSS · {} · {}",
                    p.pid,
                    p.memory as f64 / 1048576.,
                    p.state,
                    if owners.is_empty() {
                        "unmanaged/shared".into()
                    } else {
                        owners.join(", ")
                    }
                )));
            }
        }
        Some(Item::Saved(e)) => {
            lines.push(Line::from(e.name.clone()).style(v.theme.accent()));
            lines.push(Line::from(e.cwd.display().to_string()));
            if let Some(d) = s.descriptions.get(&e.id) {
                lines.push(Line::from(d.description.purpose.clone()));
                lines.push(Line::from(format!(
                    "Last progress: {}",
                    d.description.progress
                )));
                lines.push(Line::from(format!(
                    "Next: {} · blocker: {}",
                    d.description.next_step, d.description.blocker
                )));
                lines.push(Line::from(format!(
                    "Generated {} · {} / medium · last-known description",
                    age(d.generated_at),
                    d.model
                )));
            }
            lines.push(Line::from(format!(
                "{} · {} · {:?}",
                e.workspace, e.agent, e.intent
            )));
            lines.push(Line::from(format!("Next: {}", status(e, s))));
            if s.state.runs.get(&e.id).is_some_and(|r| cached_live(r, s))
                && matches!(
                    e.agent,
                    model::Agent::Codex | model::Agent::Claude | model::Agent::Cursor
                )
            {
                lines.push(Line::from(
                    "Park: finish/cancel in provider, then exit CLI. Activity is advisory.",
                ));
            }
            if v.technical {
                lines.push(Line::from(format!(
                    "Conversation: {}",
                    if e.session_verified {
                        e.provider_session
                            .clone()
                            .or_else(|| e.session_id.map(|i| i.to_string()))
                            .unwrap_or("not required".into())
                    } else {
                        "needs binding".into()
                    }
                )));
            }
            if let Some(r) = s.state.runs.get(&e.id) {
                lines.push(Line::from(if r.hooks_seen {
                    format!(
                        "Activity {} · subagents {} active / {} started (observed hooks)",
                        r.activity,
                        r.agents.len(),
                        r.agents_started,
                    )
                } else {
                    format!(
                        "Activity {} · subagents unavailable (no observed hooks)",
                        r.activity
                    )
                }));
                if let Some(error) = &r.last_error {
                    lines.push(Line::from(format!("Latest outcome: {error}")));
                }
                if r.ended {
                    lines.push(Line::from(format!(
                        "Exited {} · {} survivor records",
                        r.exit_code
                            .map(|code| code.to_string())
                            .unwrap_or("without exit code".into()),
                        r.survivors.len()
                    )));
                }
            }
            if let Some(m) = s.owned.get(&e.id) {
                lines.push(Line::from(format!(
                    "{:.1}% CPU (one core) · {:.1} MiB RSS · footprint {}/{} processes",
                    m.cpu,
                    m.memory as f64 / 1048576.,
                    m.footprint_coverage,
                    m.processes.len()
                )));
                let root = s.state.runs.get(&e.id).map(|r| r.pid);
                for p in &m.processes {
                    let depth = depth(p.pid, root, &s.processes);
                    lines.push(Line::from(format!(
                        "{}{} · PID {} · {:.0} MiB RSS",
                        "  ".repeat(depth.min(8)),
                        p.name,
                        p.pid,
                        p.memory as f64 / 1048576.
                    )));
                }
            }
            if !e.dependencies.is_empty() {
                lines.push(Line::from(format!(
                    "Registered dependencies: {}",
                    e.dependencies.len()
                )));
            }
            if v.technical {
                lines.push(Line::from(format!(
                    "Item {} · revision {}",
                    e.id, e.revision
                )));
                if let Some(r) = s.state.runs.get(&e.id) {
                    lines.push(Line::from(format!("Run {} · PID {}", r.token, r.pid)));
                }
            }
        }
        Some(Item::History(c)) => {
            if let Some(r) = classification(s, v, c) {
                lines.push(Line::from(format!(
                    "{} · relevance {} /2 · confidence {} · {}",
                    r["model"], r["relevance_score"], r["confidence"], r["assessment"]
                )));
            }

            lines.push(Line::from(c.name.clone()).style(v.theme.accent()));
            lines.push(Line::from(c.cwd.display().to_string()));
            lines.push(Line::from(format!(
                "{} · {}",
                c.provider,
                if c.archived {
                    "archived; unarchive through provider"
                } else {
                    "exact resume available"
                }
            )));
            lines.push(Line::from(if c.managed_item.is_some() {
                "o: open existing managed item"
            } else {
                "o: add to work register and resume"
            }));
            lines.push(Line::from(format!("Conversation {}", c.id)));
            if v.technical {
                lines.push(Line::from(format!(
                    "Provider home {} · {}",
                    c.provider_home.display(),
                    c.source
                )));
            }
        }
        Some(Item::Process(p)) => {
            lines.push(Line::from(format!("{} · PID {}", p.name, p.pid)).style(v.theme.accent()));
            lines.push(Line::from(format!(
                "{} · age {:.1} hours",
                p.state,
                model::now().saturating_sub(p.start_time) as f64 / 3600.
            )));
            lines.push(Line::from(format!(
                "{:.1}% CPU · {:.1} MiB RSS",
                p.cpu,
                p.memory as f64 / 1048576.
            )));
            lines.push(Line::from(format!(
                "Charged footprint: {}",
                p.footprint
                    .map(|n| format!("{:.1} MiB", n as f64 / 1048576.))
                    .unwrap_or("unavailable".into())
            )));
            lines.push(Line::from(
                "External/shared attribution needs evidence; age is not permission to stop.",
            ));
            lines.push(Line::from(format!(
                "Inspect files: gws diagnostic files --pid {}",
                p.pid
            )));
            for child in s.processes.values().filter(|c| c.parent == Some(p.pid)) {
                lines.push(Line::from(format!("  {} · {}", child.pid, child.name)));
            }
        }
        None => lines.push(Line::from(
            "Select work to see its next action and resource ownership",
        )),
    }
    if let Some(error) = &s.inventory_error {
        lines.push(Line::from(format!(
            "Ghostty inventory unavailable: {error}"
        )));
    }
    if let Some(error) = &s.error {
        lines.push(Line::from(format!("Registry unavailable: {error}")));
    }
    f.render_widget(
        Paragraph::new(lines)
            .wrap(Wrap { trim: false })
            .scroll((v.pos().detail_offset, 0))
            .block(panel(" Selected work · d / Esc ".into(), v.theme, v.ascii)),
        area,
    );
}
fn depth(mut pid: u32, root: Option<u32>, all: &BTreeMap<u32, Proc>) -> usize {
    let mut depth = 0;
    let mut seen = std::collections::HashSet::new();
    while Some(pid) != root && seen.insert(pid) {
        let Some(parent) = all.get(&pid).and_then(|p| p.parent) else {
            break;
        };
        pid = parent;
        depth += 1;
    }
    depth
}
fn draw_hardware(f: &mut ratatui::Frame, area: Rect, s: &Sample, v: &View) {
    if !s.ready {
        f.render_widget(
            Paragraph::new(if v.paused {
                "Monitoring paused · no sample collected"
            } else {
                "Collecting machine metrics…"
            })
            .block(panel(" Memory / sensors ".into(), v.theme, v.ascii)),
            area,
        );
        return;
    }
    let host = s.hosts.first();
    if area.height < 27 {
        let m = &s.memory;
        let mut lines = vec![
            Line::from(format!(
                "RAM {:.1}/{:.1} GiB",
                m.used_bytes as f64 / 1073741824.,
                m.total_bytes as f64 / 1073741824.
            )),
            Line::from(format!(
                "Pressure {}",
                m.pressure.as_deref().unwrap_or("unavailable")
            )),
            Line::from(format!(
                "Compressed {} · swap {:.1} GiB",
                m.compressed_bytes
                    .map(|n| format!("{:.1} GiB", n as f64 / 1073741824.))
                    .unwrap_or("unavailable".into()),
                m.swap_used_bytes as f64 / 1073741824.
            )),
        ];
        if let Some(sensor) = host.filter(|h| h.live()).and_then(|h| h.sensors.as_ref()) {
            for (label, load) in [
                ("E CPU", &sensor.efficiency),
                ("P CPU", &sensor.performance),
                ("GPU", &sensor.gpu),
            ] {
                lines.push(Line::from(format!(
                    "{label} {:.0}% · {:.0} MHz {}",
                    load.ratio * 100.,
                    load.mhz,
                    if load.weighted { "scaled" } else { "active" }
                )));
            }
            lines.push(Line::from(format!(
                "CPU {} C · GPU {} C",
                sensor
                    .cpu_temp
                    .map(|n| format!("{n:.0}"))
                    .unwrap_or("-".into()),
                sensor
                    .gpu_temp
                    .map(|n| format!("{n:.0}"))
                    .unwrap_or("-".into())
            )));
            lines.push(Line::from(format!(
                "System power {} W",
                sensor
                    .system_power
                    .map(|n| format!("{n:.1}"))
                    .unwrap_or("-".into())
            )));
        } else {
            lines.push(Line::from("E/P CPU, GPU and power unavailable"));
        }
        lines.push(Line::from(format!(
            "Sample {} · h returns",
            age(m.observed_at)
        )));
        f.render_widget(
            Paragraph::new(lines)
                .block(panel(" Memory / sensors ".into(), v.theme, v.ascii))
                .wrap(Wrap { trim: false }),
            area,
        );
        return;
    }
    let slots = Layout::vertical([
        Constraint::Length(7),
        Constraint::Length(5),
        Constraint::Length(5),
        Constraint::Length(5),
        Constraint::Min(1),
    ])
    .split(area);
    let m = &s.memory;
    let ratio = if m.total_bytes > 0 {
        (m.used_bytes as f64 / m.total_bytes as f64).clamp(0., 1.)
    } else {
        0.
    };
    f.render_widget(
        Gauge::default()
            .ratio(ratio)
            .label(format!(
                "{:.1}/{:.1} GiB",
                m.used_bytes as f64 / 1073741824.,
                m.total_bytes as f64 / 1073741824.
            ))
            .gauge_style(v.theme.accent())
            .block(panel(
                format!(
                    " Memory · pressure {} ",
                    m.pressure.as_deref().unwrap_or("unavailable")
                ),
                v.theme,
                v.ascii,
            )),
        Rect {
            height: 3,
            ..slots[0]
        },
    );
    let foot = Rect {
        y: slots[0].y + 3,
        height: slots[0].height.saturating_sub(3),
        ..slots[0]
    };
    f.render_widget(
        Paragraph::new(format!(
            "Compressed {} · swap {:.1} GiB\nSwap in/out {} / {} KiB/s",
            m.compressed_bytes
                .map(|n| format!("{:.1} GiB", n as f64 / 1073741824.))
                .unwrap_or("unavailable".into()),
            m.swap_used_bytes as f64 / 1073741824.,
            m.swap_in_bytes_per_second
                .map(|n| format!("{:.1}", n / 1024.))
                .unwrap_or("-".into()),
            m.swap_out_bytes_per_second
                .map(|n| format!("{:.1}", n / 1024.))
                .unwrap_or("-".into())
        ))
        .style(v.theme.muted())
        .wrap(Wrap { trim: false }),
        foot,
    );
    for (index, label) in ["E CPU", "P CPU", "GPU"].iter().enumerate() {
        let a = slots[index + 1];
        if let Some(host) = host
            && let Some(sensor) = &host.sensors
        {
            let load = match index {
                0 => &sensor.efficiency,
                1 => &sensor.performance,
                _ => &sensor.gpu,
            };
            f.render_widget(
                Gauge::default()
                    .ratio(load.ratio)
                    .label(format!(
                        "{:.0}% · {:.0} MHz{}",
                        load.ratio * 100.,
                        load.mhz,
                        if load.weighted { " scaled" } else { " active" }
                    ))
                    .gauge_style(v.theme.accent())
                    .block(panel(
                        format!(" {label}{} ", if host.live() { "" } else { " · stale" }),
                        v.theme,
                        v.ascii,
                    )),
                Rect { height: 3, ..a },
            );
            let data: Vec<_> = host.history[index + 1].iter().copied().collect();
            f.render_widget(
                Sparkline::default()
                    .data(&data)
                    .max(100)
                    .style(v.theme.accent()),
                Rect {
                    y: a.y + 3,
                    height: a.height.saturating_sub(3),
                    ..a
                },
            );
        } else {
            f.render_widget(
                Paragraph::new("Unavailable · optional macmon").block(panel(
                    format!(" {label} "),
                    v.theme,
                    v.ascii,
                )),
                a,
            );
        }
    }
    let mut lines = vec![Line::from(if v.paused {
        format!("Monitoring paused · frozen {}", s.at)
    } else {
        format!(
            "This Mac · metrics {} · inventory {}",
            age(s.memory.observed_at),
            age(s.inventory_at)
        )
    })];
    if let Some(host) = host {
        if let Some(sensor) = &host.sensors {
            lines.push(Line::from(format!(
                "CPU {} °C · GPU {} °C",
                sensor
                    .cpu_temp
                    .map(|n| format!("{n:.0}"))
                    .unwrap_or("-".into()),
                sensor
                    .gpu_temp
                    .map(|n| format!("{n:.0}"))
                    .unwrap_or("-".into())
            )));
            lines.push(Line::from(format!(
                "System power {} W",
                sensor
                    .system_power
                    .map(|n| format!("{n:.1}"))
                    .unwrap_or("-".into())
            )));
        }
        if let Some(error) = &host.error {
            lines.push(Line::from(error.clone()));
        }
    }
    for remote in s.hosts.iter().skip(1) {
        lines.push(Line::from(format!(
            "{} · {}",
            remote.label,
            if remote.live() {
                "live"
            } else {
                "unavailable/stale"
            }
        )));
    }
    lines.push(Line::from(
        "Footprint/RSS are not an additive RAM partition.",
    ));
    f.render_widget(
        Paragraph::new(lines)
            .wrap(Wrap { trim: false })
            .block(panel(" Sensors / accounting ".into(), v.theme, v.ascii)),
        slots[4],
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    fn fixture() -> (Sample, View) {
        let id = Uuid::from_u128(1);
        let mut s = Sample {
            ready: true,
            at: model::now(),
            inventory_at: model::now(),
            memory: process::Memory {
                used_bytes: 12 * 1073741824,
                total_bytes: 32 * 1073741824,
                compressed_bytes: Some(2 * 1073741824),
                swap_used_bytes: 1073741824,
                pressure: Some("normal".into()),
                swap_in_bytes_per_second: Some(0.),
                swap_out_bytes_per_second: Some(0.),
                observed_at: model::now(),
                ..Default::default()
            },
            ..Default::default()
        };
        s.state.entries = vec![
            Entry {
                id,
                name: "Dashboard profiling".into(),
                workspace: "research".into(),
                cwd: "/projects/workspaces".into(),
                agent: model::Agent::Codex,
                session_id: Some(Uuid::from_u128(2)),
                session_verified: true,
                ever_started: true,
                intent: model::Intent::Parked,
                ..Default::default()
            },
            Entry {
                id: Uuid::from_u128(3),
                name: "Literature notes".into(),
                cwd: "/projects/notes".into(),
                agent: model::Agent::Claude,
                ..Default::default()
            },
        ];
        s.state.runs.insert(
            id,
            model::Run {
                token: Uuid::from_u128(4),
                ended: true,
                exit_code: Some(0),
                ..Default::default()
            },
        );
        let load = |ratio| crate::hardware::Load {
            mhz: 1800.,
            ratio,
            weighted: false,
        };
        s.hosts.push(Host {
            label: "Local".into(),
            updated: Some(Instant::now()),
            error: None,
            history: Default::default(),
            sensors: Some(crate::hardware::Sensors {
                ram_used: s.memory.used_bytes,
                ram_total: s.memory.total_bytes,
                swap_used: s.memory.swap_used_bytes,
                swap_total: 4 * 1073741824,
                efficiency: load(0.12),
                performance: load(0.21),
                gpu: load(0.06),
                cpu_temp: Some(45.),
                gpu_temp: Some(40.),
                system_power: Some(14.),
                cpu_power: Some(4.),
                gpu_power: Some(1.),
                ane_power: None,
            }),
        });
        let v = View {
            page: Page::Saved,
            positions: Default::default(),
            searching: false,
            help: false,
            detail_only: false,
            technical: false,
            paused: false,
            pausing: false,
            receipt: None,
            message: "Enter: focus/resume · /: search · Q: prepare profiling".into(),
            plan: None,
            theme: Theme::Dark,
            ascii: false,
            sort_cpu: false,
            grouped: true,
            hardware_only: false,
            semantic: None,
            search_request: None,
        };
        (s, v)
    }
    fn render(s: &Sample, v: &mut View, w: u16, h: u16) -> ratatui::buffer::Buffer {
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(w, h)).unwrap();
        let list = items(s, v);
        select(v, &list);
        terminal.draw(|f| draw(f, s, &list, v)).unwrap();
        terminal.backend().buffer().clone()
    }
    fn contents(b: &ratatui::buffer::Buffer) -> String {
        b.content.iter().map(|c| c.symbol()).collect()
    }
    #[test]
    fn responsive_layout_and_paused_unsampled_state_are_truthful() {
        let (mut s, mut v) = fixture();
        for (w, h) in [(140, 42), (100, 30), (80, 24), (40, 20), (22, 12)] {
            let b = render(&s, &mut v, w, h);
            assert_eq!(b.content.len(), w as usize * h as usize);
            assert!(contents(&b).contains("gws"));
            if w >= 100 {
                assert!(contents(&b).contains("swap 1.0 GiB"));
            }
        }
        s.ready = false;
        v.paused = true;
        let b = render(&s, &mut v, 140, 42);
        assert!(contents(&b).contains("no sample collected"));
        assert!(!contents(&b).contains("0.0/0.0"));
    }
    #[test]
    fn finished_items_and_unknown_subagents_do_not_offer_false_status() {
        let (mut s, mut v) = fixture();
        s.state.entries[0].intent = model::Intent::Finished;
        assert_eq!(status(&s.state.entries[0], &s), "Finished");
        let b = render(&s, &mut v, 140, 42);
        assert!(contents(&b).contains("subagents unavailable"));
        assert!(!contents(&b).contains("subagents 0 active"));
    }
    #[test]
    fn fuzzy_filter_orders_and_searches_all_summary_fields() {
        let (mut s, mut v) = fixture();
        s.descriptions.insert(
            Uuid::from_u128(1),
            crate::descriptions::SavedDescription {
                schema_version: 1,
                item: Uuid::from_u128(1),
                conversation: Uuid::from_u128(2).to_string(),
                description: crate::descriptions::Description {
                    purpose: "Build work manager".into(),
                    progress: "UI complete".into(),
                    blocker: "watcher loop".into(),
                    next_step: "measure swap rate".into(),
                },
                generated_at: 1234,
                model: "fixture".into(),
                reasoning_effort: "medium".into(),
                source_path: "/fixture.jsonl".into(),
                source_fingerprint: "fixture".into(),
                excerpt_limited: false,
            },
        );
        v.pos_mut().query = "watcher".into();
        assert_eq!(items(&s, &v).len(), 1);
        v.pos_mut().query = "swap".into();
        assert_eq!(items(&s, &v).len(), 1);
    }
    #[test]
    fn failed_rerank_releases_only_matching_pending_request_and_allows_retry() {
        let (_, mut view) = fixture();
        let temp = tempfile::tempdir().unwrap();
        let store = Store::new(Some(temp.path().into())).unwrap();
        std::fs::write(temp.path().join("workspaces.json"), b"malformed").unwrap();
        let request = Uuid::new_v4();
        view.search_request = Some(request);
        let update = ranked_update(
            request,
            Page::History,
            "memory".into(),
            7,
            catalog::search_with_backend(&store, "memory", 100, 0, true),
        );
        let Update::RankFailed(id, error) = update else {
            panic!("expected scoped storage error")
        };
        failed_search(&mut view, Uuid::new_v4(), error.clone());
        assert_eq!(view.search_request, Some(request));
        failed_search(&mut view, id, error);
        assert!(view.search_request.is_none());
        let retry = Uuid::new_v4();
        view.search_request = Some(retry);
        let update = ranked_update(
            retry,
            Page::History,
            "memory".into(),
            7,
            Ok(serde_json::json!({"backend":"fuzzy","items":[]})),
        );
        assert!(matches!(update, Update::Ranked(id, _, _, _) if id == retry));
    }
    #[test]
    #[ignore = "requires GWS_RENDER_DIR for synthetic visual review artifacts"]
    fn render_review_artifacts() {
        let Ok(root) = std::env::var("GWS_RENDER_DIR") else {
            return;
        };
        let path = std::path::Path::new(&root);
        std::fs::create_dir_all(path).unwrap();
        let (mut s, mut v) = fixture();
        // Entirely synthetic: never discover provider files, tabs or live processes.
        s.state.entries.clear();
        s.state.runs.clear();
        let examples = [
            ("Memory dashboard", "atlas", model::Agent::Codex, 768, 4.2),
            ("API migration", "orchard", model::Agent::Claude, 512, 1.4),
            ("UI polish", "solstice", model::Agent::Cursor, 384, 0.8),
            ("Preview server", "atlas", model::Agent::Command, 192, 0.3),
            ("Test runner", "orchard", model::Agent::Command, 96, 0.1),
            ("Release notes", "solstice", model::Agent::Codex, 0, 0.0),
            ("Benchmark plan", "atlas", model::Agent::Claude, 0, 0.0),
            ("Imported tab", "sandbox", model::Agent::Codex, 0, 0.0),
            ("Docs cleanup", "orchard", model::Agent::Codex, 0, 0.0),
        ];
        let mut tabs = vec![];
        for (i, (name, project, agent, mib, cpu)) in examples.into_iter().enumerate() {
            let id = Uuid::from_u128(i as u128 + 1);
            let cwd = format!("/demo/projects/{project}");
            let active = mib > 0;
            let terminal_id = format!("demo-terminal-{i}");
            s.state.entries.push(Entry {
                id,
                name: name.into(),
                workspace: "demo".into(),
                cwd: cwd.clone().into(),
                agent,
                ever_started: true,
                session_verified: i != 7,
                session_id: (i != 7).then_some(Uuid::from_u128(i as u128 + 100)),
                imported: i == 7,
                intent: if active {
                    model::Intent::Active
                } else if i == 8 {
                    model::Intent::Finished
                } else {
                    model::Intent::Parked
                },
                terminal_id: active.then_some(terminal_id.clone()),
                ..Default::default()
            });
            let mut run = model::Run {
                token: Uuid::from_u128(i as u128 + 200),
                ended: !active,
                exit_code: (!active).then_some(0),
                hooks_seen: i < 2,
                activity: if active { "working" } else { "ended" }.into(),
                ..Default::default()
            };
            if active {
                let pid = 41000 + i as u32;
                let p = Proc {
                    pid,
                    parent: None,
                    name: agent.to_string(),
                    cpu,
                    memory: mib * 1048576,
                    start_time: s.at - 2400,
                    footprint: Some(mib * 1048576),
                    state: "Running".into(),
                };
                run.pid = pid;
                run.start_time = p.start_time;
                s.processes.insert(pid, p.clone());
                s.owned.insert(
                    id,
                    Metrics {
                        cpu,
                        memory: p.memory,
                        processes: vec![p],
                        footprint: Some(mib * 1048576),
                        footprint_coverage: 1,
                    },
                );
                tabs.push(ghostty::Tab {
                    id: format!("demo-tab-{i}"),
                    name: name.into(),
                    terminals: vec![ghostty::Surface {
                        id: terminal_id,
                        cwd,
                    }],
                });
            }
            if i == 0 {
                run.agents = BTreeMap::from([
                    ("demo-agent-a".into(), "review".into()),
                    ("demo-agent-b".into(), "tests".into()),
                ]);
                run.agents_started = 3;
            }
            s.state.runs.insert(id, run);
        }
        s.windows = vec![Window {
            id: "demo-window".into(),
            tabs,
        }];
        s.descriptions.insert(
            Uuid::from_u128(1),
            crate::descriptions::SavedDescription {
                schema_version: 1,
                item: Uuid::from_u128(1),
                conversation: Uuid::from_u128(100).to_string(),
                description: crate::descriptions::Description {
                    purpose: "Track memory pressure and resumable development work.".into(),
                    progress: "Resource panels complete; recovery checks in progress.".into(),
                    blocker: "None recorded".into(),
                    next_step: "Review restart behavior".into(),
                },
                generated_at: s.at - 120,
                model: "demo summary".into(),
                reasoning_effort: "medium".into(),
                source_path: "/demo/transcript.jsonl".into(),
                source_fingerprint: "synthetic".into(),
                excerpt_limited: true,
            },
        );
        v.message = "Demo · all data synthetic · Enter: focus/resume · /: search".into();
        for (name, w, h) in [("wide", 140, 42), ("compact", 80, 24), ("narrow", 40, 20)] {
            let b = render(&s, &mut v, w, h);
            let cells:Vec<_>=b.content.iter().map(|c|serde_json::json!({"text":c.symbol(),"fg":format!("{:?}",c.fg),"bg":format!("{:?}",c.bg),"bold":c.modifier.contains(Modifier::BOLD),"reverse":c.modifier.contains(Modifier::REVERSED)})).collect();
            std::fs::write(
                path.join(format!("{name}.json")),
                serde_json::to_vec(&serde_json::json!({"width":w,"height":h,"cells":cells}))
                    .unwrap(),
            )
            .unwrap();
        }
    }
}

#[cfg(test)]
mod performance_tests {
    use super::*;
    #[test]
    #[ignore = "requires GWS_BENCHMARK_FILE and an optimized build"]
    fn benchmark_large_history_when_requested() {
        let Ok(destination) = std::env::var("GWS_BENCHMARK_FILE") else {
            return;
        };
        let mut s = Sample::default();
        for i in 0..10000 {
            s.catalog.push(Conversation {
                id: Uuid::from_u128(i + 1),
                provider: model::Agent::Codex,
                provider_home: "/private/fixture".into(),
                cwd: format!("/projects/research-{i}").into(),
                name: format!("Research memory monitor {i}"),
                updated: i as u64,
                parent: None,
                subagent: false,
                archived: false,
                source: "fixture".into(),
                managed_item: None,
                transcript_path: None,
                provider_session: None,
            });
        }
        s.search_revision = search_signature(&s);
        let mut v = View {
            page: Page::History,
            positions: Default::default(),
            searching: false,
            help: false,
            detail_only: false,
            technical: false,
            paused: false,
            pausing: false,
            receipt: None,
            message: String::new(),
            plan: None,
            theme: Theme::Native,
            ascii: false,
            sort_cpu: false,
            grouped: true,
            hardware_only: false,
            semantic: None,
            search_request: None,
        };
        v.pos_mut().query = "mem mon".into();
        let mut timings = vec![];
        for _ in 0..40 {
            let t = Instant::now();
            let list = items(&s, &v);
            std::hint::black_box(&list);
            timings.push(t.elapsed().as_secs_f64() * 1000.);
            assert_eq!(list.len(), 10000);
        }
        timings.sort_by(f64::total_cmp);
        std::fs::write(destination,serde_json::to_vec(&serde_json::json!({"fixture_count":10000,"iterations":40,"query":"mem mon","fuzzy_p50_ms":timings[20],"fuzzy_p95_ms":timings[38],"includes":"fuzzy documents, filtering and sorting; excludes file discovery and model requests"})).unwrap()).unwrap();
    }
}
