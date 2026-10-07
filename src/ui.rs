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
use crossterm::event::{
    self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEventKind, MouseButton,
    MouseEventKind,
};
use ratatui::{
    layout::{Constraint, Layout, Rect},
    style::{Color, Modifier, Style},
    symbols::Marker,
    text::{Line, Span},
    widgets::{
        Axis, Block, Borders, Cell, Chart, Dataset, GraphType, Paragraph, Row, Table, TableState,
        Wrap,
    },
};
use serde::Serialize;
use std::{
    collections::{BTreeMap, VecDeque},
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
    memory_history: VecDeque<(Instant, f64)>,
    sampled_at: Option<Instant>,
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
                    if s.memory.total_bytes > 0 {
                        if s.memory_history.len() == 60 {
                            s.memory_history.pop_front();
                        }
                        s.memory_history.push_back((
                            Instant::now(),
                            s.memory.used_bytes as f64 / s.memory.total_bytes as f64 * 100.,
                        ));
                    }
                    owned(&mut s);
                }
                if inventory.elapsed() >= Duration::from_secs(5) {
                    match ghostty::snapshot_with_focus() {
                        Ok(snapshot) => {
                            // Only acknowledge completions already observed before this focus
                            // query. Never reuse a cached focus for a later completion.
                            if let Err(error) = record_visible_completion(
                                &store,
                                &s.state,
                                snapshot.focused_terminal.as_deref(),
                                &snapshot.windows,
                            ) {
                                s.error = Some(format!("Could not save read marker: {error}"));
                            }
                            s.windows = snapshot.windows;
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
                s.sampled_at = Some(Instant::now());
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
    binding: Option<Uuid>,
    list_area: Option<Rect>,
    seen: BTreeMap<Uuid, (Uuid, u64, bool)>,
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
            "{} {} {} {} {}",
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
            .unwrap_or_default(),
            match i {
                Item::Saved(e) => agent_model(e.agent, Some(e)),
                Item::History(c) => agent_model(
                    c.provider,
                    c.managed_item.and_then(|id| s.state.entry(id).ok())
                ),
                _ => String::new(),
            }
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
    Seen,
    ReadFailed(Uuid, (Uuid, u64, bool), String),
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
        crossterm::execute!(std::io::stdout(), EnableMouseCapture)?;
        struct MouseGuard;
        impl Drop for MouseGuard {
            fn drop(&mut self) {
                let _ = crossterm::execute!(std::io::stdout(), DisableMouseCapture);
            }
        }
        let _mouse = MouseGuard;
        let mut view = View {
            binding: None,
            list_area: None,
            seen: BTreeMap::new(),
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
                    Update::Seen => {}
                    Update::ReadFailed(id, expected, error) => {
                        if view.seen.get(&id) == Some(&expected) {
                            view.seen.remove(&id);
                        }
                        view.message = format!("Could not save read marker: {error}");
                    }
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
            let key = match input {
                Event::Key(key) => key,
                Event::Mouse(mouse) if mouse.kind == MouseEventKind::Down(MouseButton::Left) => {
                    let Some((index, bind)) =
                        clicked_row(&view, list.len(), mouse.column, mouse.row)
                    else {
                        continue;
                    };
                    view.pos_mut().table.select(Some(index));
                    view.pos_mut().identity = Some(list[index].key());
                    if let Item::Saved(e) = &list[index]
                        && bind
                        && e.needs_session()
                    {
                        view.binding = Some(e.id);
                    }
                    continue;
                }
                _ => continue,
            };
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
            if let Some(id) = view.binding {
                match key.code {
                    KeyCode::Esc => view.binding = None,
                    KeyCode::Char('c') => {
                        let entry = sample.state.entry(id)?.clone();
                        let store = store.clone();
                        view.binding = None;
                        async_task(tx.clone(), move || {
                            copy_binding(&store, &entry)?;
                            Ok(Update::Message("Bind command copied. Paste it into the corresponding Codex conversation and press Enter.".into()))
                        });
                    }
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
                KeyCode::Char('a') => {
                    if let Some(Item::Saved(e)) = chosen {
                        if attention(e, &sample, &view).0 == "◆" {
                            acknowledge(store, &sample, e, &mut view, tx.clone());
                            view.message = "Completion acknowledged".into();
                        } else {
                            view.message = "No unread completion for selected work".into();
                        }
                    }
                }
                KeyCode::Char('d') => view.detail_only = !view.detail_only,
                KeyCode::Char('B') => {
                    if let Some(Item::Saved(e)) = chosen {
                        if e.needs_session() {
                            view.binding = Some(e.id);
                        } else {
                            view.message = "Already bound · t shows its exact identity".into();
                        }
                    }
                }
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
fn overview(s: &Sample, width: u16) -> String {
    let open = s
        .state
        .entries
        .iter()
        .filter(|e| s.state.runs.get(&e.id).is_some_and(|r| cached_live(r, s)))
        .count();
    let binding = s.state.entries.iter().filter(|e| e.needs_session()).count();
    let work = format!(
        "{} saved · {open} open · {binding} need binding",
        s.state.entries.len()
    );
    if !s.ready {
        return format!("{work} · collecting usage…");
    }
    let ram = format!(
        "RAM {:.1}/{:.1} GiB",
        s.memory.used_bytes as f64 / 1073741824.,
        s.memory.total_bytes as f64 / 1073741824.
    );
    if width >= 100 {
        format!(
            "{work}   |   {ram} · pressure {} · swap {:.1} GiB",
            s.memory.pressure.as_deref().unwrap_or("unavailable"),
            s.memory.swap_used_bytes as f64 / 1073741824.
        )
    } else if width >= 70 {
        format!(
            "{} saved · {open} open · {binding} bind   |   {ram}",
            s.state.entries.len()
        )
    } else {
        format!(
            "{} saved · {open} open · {binding} bind",
            s.state.entries.len()
        )
    }
}
fn draw(f: &mut ratatui::Frame, s: &Sample, list: &[Item], v: &mut View) {
    v.list_area = None;
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
        Constraint::Length(1),
        Constraint::Min(1),
        Constraint::Length(2),
    ])
    .split(area);
    let tabs = format!(
        " gws   {}{}",
        if v.hardware_only {
            "› Usage · h returns".into()
        } else {
            format!(
                "{}Saved   {}History   {}Mac",
                if v.page == Page::Saved { "› " } else { "" },
                if v.page == Page::History { "› " } else { "" },
                if v.page == Page::Mac { "› " } else { "" }
            )
        },
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
    f.render_widget(
        Paragraph::new(overview(s, area.width)).style(v.theme.muted()),
        chunks[1],
    );
    let sidebar = area.width >= 100
        && chunks[2].height >= 24
        && !v.hardware_only
        && v.binding.is_none()
        && !v.help
        && v.plan.is_none()
        && v.receipt.is_none();
    let (left, right) = if sidebar {
        let c = Layout::horizontal([
            Constraint::Min(64),
            Constraint::Length(if area.width >= 120 { 34 } else { 32 }),
        ])
        .split(chunks[2]);
        (c[0], Some(c[1]))
    } else {
        (chunks[2], None)
    };
    if v.help {
        f.render_widget(Paragraph::new("Tab: Saved / History / Mac\n/: fuzzy search   J: Jev rerank History search\nEnter: focus/resume or details   o: open History\na: acknowledge selected completion\nb: generate description   v: Mac groups/processes\np: preview parking   r: preview workspace restore\nQ: prepare profiling   M: pause/resume monitoring\ns: save tabs   d: details   h: hardware   t: technical IDs\nB / click ?: bind selected session\nm / c: memory / CPU sort   PgUp/PgDn: scroll\nEsc: back/clear/quit   q: quit\n\nProvider parking: finish/cancel through its own UI, then exit.\nSuspension does not reclaim memory.\n● working · ◆ finished unread · ○ finished read\n! input needed · × exit error · · unknown\nFinished means an observed turn end or process exit. Read: foreground terminal (5s) or a to acknowledge. Browsing rows keeps unread markers.").block(panel(" Help · any key returns ".into(),v.theme,v.ascii)).wrap(Wrap{trim:false}),left);
    } else if let Some(id) = v.binding {
        let e = s.state.entry(id).ok();
        let codex = e.is_some_and(|e| e.agent == model::Agent::Codex);
        let lines = vec![
            Line::from("Unbound means the exact conversation identity is not verified."),
            Line::from("An open session can still be unbound; its activity marker is independent."),
            Line::from(""),
            Line::from(if codex {
                "c: copy binding command and focus its Codex tab"
            } else {
                "Use gws bind WORK_ID CONVERSATION_ID with the provider's exact ID."
            }),
            Line::from(if codex {
                "Paste into the corresponding Codex conversation, then press Enter."
            } else {
                "Use the provider exact conversation ID; automatic discovery is unavailable here."
            }),
            Line::from("Codex supplies its current thread; gws verifies root transcript metadata."),
            Line::from("Nothing is sent to the provider automatically. Esc returns."),
        ];
        f.render_widget(
            Paragraph::new(lines)
                .wrap(Wrap { trim: false })
                .block(panel(" Bind session ".into(), v.theme, v.ascii)),
            left,
        );
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
        draw_hardware(f, left, s, v, false);
    } else {
        let both = (wide || medium) && !v.detail_only;
        let (list_area, detail_area) = if both {
            let c = Layout::vertical([Constraint::Percentage(55), Constraint::Percentage(45)])
                .split(left);
            (Some(c[0]), Some(c[1]))
        } else if v.detail_only {
            (None, Some(left))
        } else {
            (Some(left), None)
        };
        if let Some(a) = list_area {
            draw_list(f, a, s, list, v);
        }
        if let Some(a) = detail_area {
            draw_details(f, a, s, list.get(v.pos().table.selected().unwrap_or(0)), v);
        }
    }
    if let Some(right) = right {
        draw_hardware(f, right, s, v, true);
    }
    let footer = if v.searching {
        format!("/{} · Enter accepts · Esc returns", v.pos().query)
    } else {
        format!(
            "{}\n● work  ◆ unread  ○ read · a acknowledge · B bind · h usage · ? help",
            v.message
        )
    };
    f.render_widget(Paragraph::new(footer).style(v.theme.muted()), chunks[3]);
}
fn short_name(e: &Entry) -> String {
    let mut words = e.name.split_whitespace();
    let first = words.next();
    let program = match first {
        Some("exec") => words.next(),
        Some("env") => words.find(|word| !word.contains('=')),
        _ => first,
    }
    .and_then(|word| std::path::Path::new(word).file_name())
    .and_then(|name| name.to_str());
    let launch_title = e.imported
        && match e.agent {
            model::Agent::Codex => program == Some("codex"),
            model::Agent::Claude => program == Some("claude"),
            model::Agent::Cursor => {
                matches!(program, Some("agent" | "cursor" | "cursor-agent"))
            }
            _ => false,
        };
    if launch_title {
        format!(
            "{} · {}",
            e.cwd.file_name().unwrap_or_default().to_string_lossy(),
            match e.agent {
                model::Agent::Codex => "Codex",
                model::Agent::Claude => "Claude Code",
                model::Agent::Cursor => "Cursor",
                _ => unreachable!(),
            }
        )
    } else {
        e.name.clone()
    }
}
fn display_name(e: &Entry, s: &Sample) -> String {
    let name = short_name(e);
    if name != e.name
        && s.state
            .entries
            .iter()
            .filter(|other| short_name(other) == name)
            .count()
            > 1
    {
        let id = e.id.simple().to_string();
        format!("{name} · {}", &id[id.len() - 6..])
    } else {
        name
    }
}
fn agent_model(agent: model::Agent, entry: Option<&Entry>) -> String {
    let label = match agent {
        model::Agent::Codex => "Codex",
        model::Agent::Claude => "Claude",
        model::Agent::Cursor => "Cursor",
        model::Agent::Shell => "Shell",
        model::Agent::Command => "Command",
    };
    let mut model = None;
    if let Some(entry) = entry {
        let args = entry.resume_args.as_ref().unwrap_or(&entry.args);
        for (i, arg) in args.iter().enumerate() {
            let value = if matches!(arg.as_str(), "--model" | "-m") {
                args.get(i + 1)
                    .map(String::as_str)
                    .filter(|v| !v.starts_with('-'))
            } else {
                arg.strip_prefix("--model=")
            };
            if let Some(value) = value.filter(|v| !v.is_empty()) {
                model = Some(value);
            }
        }
    }
    model
        .map(|model| format!("{label}/{model}"))
        .unwrap_or(label.into())
}
fn attention(e: &Entry, s: &Sample, v: &View) -> (&'static str, &'static str) {
    let Some(r) = s.state.runs.get(&e.id) else {
        return ("·", "No run observed");
    };
    if !r.ended && cached_live(r, s) && r.hooks_seen {
        match r.activity.as_str() {
            "working" => return ("●", "Working (observed hook)"),
            "awaiting_input" => return ("!", "Needs input (observed hook)"),
            _ => {}
        }
    }
    if r.ended && (r.exit_code.is_some_and(|c| c != 0) || r.last_error.is_some()) {
        return ("×", "Exited with error");
    }
    if r.ended && !r.survivors.is_empty() {
        return ("!", "Exited · survivor records need review");
    }
    let seen = v.seen.get(&e.id).filter(|(token, _, _)| *token == r.token);
    let turns = r.seen_turns.max(seen.map(|(_, seq, _)| *seq).unwrap_or(0));
    let exit = r.exit_seen || seen.is_some_and(|(_, _, ended)| *ended);
    if r.last_turn_end.is_some() || r.ended {
        if (r.last_turn_end.is_some() && r.completed_turns > turns) || (r.ended && !exit) {
            ("◆", "Finished · unread")
        } else {
            ("○", "Finished · read")
        }
    } else {
        ("·", "Open · completion unknown")
    }
}
fn marker(e: &Entry, s: &Sample, v: &View) -> Cell<'static> {
    let (symbol, _) = attention(e, s, v);
    let shown = if v.ascii {
        match symbol {
            "●" => "*",
            "◆" => "+",
            "○" => "o",
            "×" => "x",
            "·" => ".",
            _ => symbol,
        }
    } else {
        symbol
    };
    let style = match symbol {
        "●" => v.theme.accent(),
        "◆" | "!" => Style::default().fg(Color::Yellow),
        "×" => Style::default().fg(Color::Red),
        _ => v.theme.muted(),
    };
    Cell::from(Line::from(vec![
        Span::styled(shown, style),
        Span::styled(
            if e.needs_session() { "?" } else { " " },
            Style::default().fg(Color::Yellow),
        ),
    ]))
}
fn record_seen(store: &Store, id: Uuid, token: Uuid, turns: u64, ended: bool) -> Result<()> {
    let state = store.read()?;
    let Some(run) = state.runs.get(&id).filter(|r| r.token == token) else {
        return Ok(());
    };
    store
        .edit_document("attention", &token.to_string(), |old| {
            let mut attention: model::Attention = old
                .map(serde_json::from_value)
                .transpose()?
                .unwrap_or(model::Attention {
                    completed_turns: run.completed_turns,
                    seen_turns: run.seen_turns,
                    last_turn_end: run.last_turn_end,
                    exit_seen: run.exit_seen,
                });
            attention.seen_turns = attention
                .seen_turns
                .max(turns.min(attention.completed_turns));
            attention.exit_seen |= ended && run.ended;
            Ok(attention)
        })
        .map(|_| ())
}
fn record_visible_completion(
    store: &Store,
    state: &State,
    focused: Option<&str>,
    windows: &[Window],
) -> Result<()> {
    let Some(focused) = focused.filter(|id| ghostty::terminal_exists(windows, id)) else {
        return Ok(());
    };
    let mut matches = state.entries.iter().filter_map(|entry| {
        let run = state.runs.get(&entry.id)?;
        (run.terminal_id.as_deref().or(entry.terminal_id.as_deref()) == Some(focused))
            .then_some((entry, run))
    });
    let Some((entry, run)) = matches.next() else {
        return Ok(());
    };
    // An ambiguous surface association cannot establish which saved run was viewed.
    if matches.next().is_some() {
        return Ok(());
    }
    if (run.last_turn_end.is_some() && run.completed_turns > run.seen_turns)
        || (run.ended && !run.exit_seen)
    {
        record_seen(store, entry.id, run.token, run.completed_turns, run.ended)?;
    }
    Ok(())
}
fn acknowledge(store: &Store, s: &Sample, e: &Entry, v: &mut View, tx: mpsc::Sender<Update>) {
    let Some(r) = s.state.runs.get(&e.id) else {
        return;
    };
    if attention(e, s, v).0 != "◆" {
        return;
    }
    let (id, token, turns, ended) = (e.id, r.token, r.completed_turns, r.ended);
    v.seen.insert(id, (token, turns, ended));
    let store = store.clone();
    async_task(tx, move || {
        Ok(match record_seen(&store, id, token, turns, ended) {
            Ok(()) => Update::Seen,
            Err(error) => Update::ReadFailed(id, (token, turns, ended), error.to_string()),
        })
    });
}
fn clicked_row(v: &View, count: usize, x: u16, y: u16) -> Option<(usize, bool)> {
    if v.help
        || v.binding.is_some()
        || v.plan.is_some()
        || v.receipt.is_some()
        || v.hardware_only
        || v.searching
    {
        return None;
    }
    let area = v.list_area?;
    if x <= area.x
        || x >= area.right().saturating_sub(1)
        || y < area.y + 2
        || y >= area.bottom().saturating_sub(1)
    {
        return None;
    }
    let index = v.pos().table.offset() + (y - area.y - 2) as usize;
    (index < count).then_some((index, v.page == Page::Saved && x == area.x + 4))
}
fn bind_command(store: &Store, e: &Entry) -> Result<String> {
    anyhow::ensure!(
        e.agent == model::Agent::Codex,
        "This shortcut needs Codex; bind other providers with their exact conversation ID"
    );
    Ok(format!(
        "!gws --state-dir {} bind-here --item {}",
        ghostty::shell_quote(&store.dir.to_string_lossy()),
        e.id
    ))
}
fn copy_binding(store: &Store, e: &Entry) -> Result<()> {
    let command = bind_command(store, e)?;
    let mut child = std::process::Command::new("/usr/bin/pbcopy")
        .stdin(std::process::Stdio::piped())
        .spawn()?;
    let write = child
        .stdin
        .take()
        .context("Clipboard pipe unavailable")?
        .write_all(command.as_bytes());
    let status = child.wait();
    write?;
    anyhow::ensure!(status?.success(), "Could not copy binding command");
    if let Some(id) = &e.terminal_id {
        ghostty::focus(id)?;
    }
    Ok(())
}
fn location_label(path: &std::path::Path, max: usize, ascii: bool) -> String {
    let text = model::home()
        .ok()
        .and_then(|home| path.strip_prefix(home).ok())
        .map(|p| format!("~/{}", p.display()))
        .unwrap_or_else(|| path.display().to_string());
    if Line::from(text.as_str()).width() <= max {
        return text;
    }
    let prefix = if ascii { "..." } else { "…" };
    let mut width = prefix.len().min(3);
    if !ascii {
        width = 1;
    }
    let mut tail = vec![];
    for c in text.chars().rev() {
        width += Line::from(c.to_string()).width();
        if width > max {
            break;
        }
        tail.push(c);
    }
    format!("{prefix}{}", tail.into_iter().rev().collect::<String>())
}
fn draw_list(f: &mut ratatui::Frame, area: Rect, s: &Sample, list: &[Item], v: &mut View) {
    v.list_area = Some(area);
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
    let work = v.page != Page::Mac;
    let location_width = if area.width >= 96 {
        18
    } else if area.width >= 80 {
        16
    } else {
        12
    };
    let rows = list.iter().map(|item| {
        let (name, location, agent, state, ram, cpu) = match item {
            Item::Saved(e) => {
                let m = s.owned.get(&e.id);
                (
                    display_name(e, s),
                    location_label(&e.cwd, location_width, v.ascii),
                    agent_model(e.agent, Some(e)),
                    String::new(),
                    m.map(|m| format!("{:.0}", m.memory as f64 / 1048576.))
                        .unwrap_or("-".into()),
                    m.map(|m| format!("{:.1}", m.cpu)).unwrap_or("-".into()),
                )
            }
            Item::History(c) => (
                c.name.clone(),
                location_label(&c.cwd, location_width, v.ascii),
                agent_model(
                    c.provider,
                    c.managed_item.and_then(|id| s.state.entry(id).ok()),
                ),
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
                String::new(),
                String::new(),
                p.state.clone(),
                format!(
                    "{:.0} {}",
                    p.footprint.unwrap_or(p.memory) as f64 / 1048576.,
                    if p.footprint.is_some() { "FP" } else { "RSS" }
                ),
                format!("{:.1}", p.cpu),
            ),
            Item::Group(g) => {
                let complete = g.metrics.footprint_coverage == g.metrics.processes.len();
                (
                    g.name.clone(),
                    String::new(),
                    String::new(),
                    format!("{} processes", g.metrics.processes.len()),
                    format!(
                        "{:.0} {}",
                        (if complete {
                            g.metrics.footprint.unwrap_or(g.metrics.memory)
                        } else {
                            g.metrics.memory
                        }) as f64
                            / 1048576.,
                        if complete { "FP" } else { "RSS" }
                    ),
                    format!("{:.1}", g.metrics.cpu),
                )
            }
        };
        let mut cells: Vec<Cell> = if work {
            if area.width >= 80 {
                vec![name, location, agent, ram, cpu]
            } else if area.width >= 64 {
                vec![name, location, agent, ram]
            } else if area.width >= 48 {
                vec![name, location, agent]
            } else {
                vec![name, location]
            }
        } else if compact {
            vec![name, state, ram]
        } else {
            vec![name, state, ram, cpu]
        }
        .into_iter()
        .map(Cell::from)
        .collect();
        if work {
            cells.insert(
                0,
                match item {
                    Item::Saved(e) => marker(e, s, v),
                    Item::History(c) => c
                        .managed_item
                        .and_then(|id| s.state.entry(id).ok())
                        .map(|e| marker(e, s, v))
                        .unwrap_or(Cell::from("· ")),
                    _ => unreachable!(),
                },
            );
        }
        Row::new(cells)
    });
    let ram_label = if v.page == Page::History {
        "CONF %"
    } else if work {
        "RSS MiB"
    } else {
        "MiB FP/RSS"
    };
    let last_label = if v.page == Page::History {
        "REL /2"
    } else {
        "CPU %"
    };
    let (header, constraints) = if work {
        if area.width >= 96 {
            (
                vec![
                    "",
                    "WORK",
                    "LOCATION",
                    "AGENT / MODEL",
                    ram_label,
                    last_label,
                ],
                vec![
                    Constraint::Length(2),
                    Constraint::Min(14),
                    Constraint::Length(18),
                    Constraint::Length(22),
                    Constraint::Length(7),
                    Constraint::Length(6),
                ],
            )
        } else if area.width >= 80 {
            (
                vec![
                    "",
                    "WORK",
                    "LOCATION",
                    "AGENT / MODEL",
                    ram_label,
                    last_label,
                ],
                vec![
                    Constraint::Length(2),
                    Constraint::Min(14),
                    Constraint::Length(16),
                    Constraint::Length(18),
                    Constraint::Length(7),
                    Constraint::Length(6),
                ],
            )
        } else if area.width >= 64 {
            (
                vec!["", "WORK", "LOCATION", "AGENT / MODEL", ram_label],
                vec![
                    Constraint::Length(2),
                    Constraint::Min(14),
                    Constraint::Length(12),
                    Constraint::Length(14),
                    Constraint::Length(7),
                ],
            )
        } else if area.width >= 48 {
            (
                vec!["", "WORK", "LOCATION", "AGENT / MODEL"],
                vec![
                    Constraint::Length(2),
                    Constraint::Min(10),
                    Constraint::Length(12),
                    Constraint::Length(12),
                ],
            )
        } else {
            (
                vec!["", "WORK", "LOCATION"],
                vec![
                    Constraint::Length(2),
                    Constraint::Min(12),
                    Constraint::Length(12),
                ],
            )
        }
    } else if compact {
        (
            vec!["PROCESS", "STATE", ram_label],
            vec![
                Constraint::Min(12),
                Constraint::Length(10),
                Constraint::Length(10),
            ],
        )
    } else {
        (
            vec!["PROCESS", "STATE", ram_label, last_label],
            vec![
                Constraint::Min(18),
                Constraint::Length(14),
                Constraint::Length(9),
                Constraint::Length(7),
            ],
        )
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
fn related_summary(e: &Entry, s: &Sample) -> String {
    let processes = s
        .owned
        .get(&e.id)
        .map(|m| {
            format!(
                "{} process{}",
                m.processes.len(),
                if m.processes.len() == 1 { "" } else { "es" }
            )
        })
        .unwrap_or("processes unknown".into());
    let agents = s
        .state
        .runs
        .get(&e.id)
        .filter(|r| cached_live(r, s) && !r.ended && r.hooks_seen)
        .map(|r| format!("{} observed agents", r.agents.len()))
        .unwrap_or("agents unknown".into());
    format!("{processes} · {agents}")
}
fn draw_related(f: &mut ratatui::Frame, area: Rect, s: &Sample, e: &Entry, v: &View) {
    let run = s.state.runs.get(&e.id);
    let mut lines = vec![Line::from(
        run.filter(|r| cached_live(r, s) && !r.ended && r.hooks_seen)
            .map(|r| {
                format!(
                    "Subagents: {} observed{}",
                    r.agents.len(),
                    if r.agents.is_empty() {
                        String::new()
                    } else {
                        format!(
                            " · {}",
                            r.agents
                                .values()
                                .take(3)
                                .cloned()
                                .collect::<Vec<_>>()
                                .join(", ")
                        )
                    }
                )
            })
            .unwrap_or("Subagents: unknown · no current hook evidence".into()),
    )];
    if let Some(m) = s.owned.get(&e.id) {
        lines.push(Line::from(format!(
            "Processes: {} in tracked execution tree",
            m.processes.len()
        )));
        let mut processes: Vec<_> = m
            .processes
            .iter()
            .filter(|p| run.is_none_or(|r| p.pid != r.pid || p.name != "gws"))
            .collect();
        processes.sort_by(|a, b| {
            b.memory
                .cmp(&a.memory)
                .then_with(|| b.cpu.total_cmp(&a.cpu))
                .then(a.pid.cmp(&b.pid))
        });
        for p in processes
            .into_iter()
            .take(area.height.saturating_sub(5) as usize)
        {
            lines.push(Line::from(format!(
                "{} · {:.0} MiB RSS · {:.1}% CPU (core)",
                p.name,
                p.memory as f64 / 1048576.,
                p.cpu
            )));
        }
    } else {
        lines.push(Line::from("Process ownership unavailable"));
    }
    lines.push(
        Line::from("Hook agents and OS processes are separate counts.").style(v.theme.muted()),
    );
    f.render_widget(
        Paragraph::new(lines)
            .wrap(Wrap { trim: false })
            .block(panel(
                if v.paused {
                    " Related activity · frozen "
                } else {
                    " Related activity · t: full tree "
                }
                .into(),
                v.theme,
                v.ascii,
            )),
        area,
    );
}
fn draw_details(f: &mut ratatui::Frame, area: Rect, s: &Sample, item: Option<&Item>, v: &View) {
    let related = if v.technical {
        None
    } else {
        match item {
            Some(Item::Saved(e))
                if s.state.runs.get(&e.id).is_some_and(|r| cached_live(r, s))
                    || s.owned.get(&e.id).is_some_and(|m| !m.processes.is_empty()) =>
            {
                Some(*e)
            }
            _ => None,
        }
    };
    let (area, related_area) = if related.is_some() && area.height >= 12 {
        let c = Layout::vertical([
            Constraint::Min(7),
            Constraint::Length(if area.height >= 16 { 7 } else { 5 }),
        ])
        .split(area);
        (c[0], Some(c[1]))
    } else {
        (area, None)
    };
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
            lines.push(Line::from(display_name(e, s)).style(v.theme.accent()));
            lines.push(Line::from(e.cwd.display().to_string()));
            lines.push(Line::from(format!(
                "{} · Enter: {} · group {}",
                attention(e, s, v).1,
                status(e, s),
                e.workspace
            )));
            if s.state.runs.get(&e.id).is_some_and(|r| cached_live(r, s))
                && matches!(
                    e.agent,
                    model::Agent::Codex | model::Agent::Claude | model::Agent::Cursor
                )
            {
                lines.push(Line::from(
                    "Put away: finish/cancel in provider, then exit CLI.",
                ));
            }
            if let Some(d) = s.descriptions.get(&e.id) {
                lines.push(
                    Line::from(format!("Last-known summary · {}", age(d.generated_at)))
                        .style(v.theme.muted()),
                );
                lines.push(Line::from(d.description.purpose.clone()));
                lines.push(Line::from(format!(
                    "Last progress: {}",
                    d.description.progress
                )));
                lines.push(Line::from(format!(
                    "Next step: {} · blocker: {}",
                    d.description.next_step, d.description.blocker
                )));
                if v.technical {
                    lines.push(Line::from(format!(
                        "Generated {} · {} / medium · last-known description",
                        age(d.generated_at),
                        d.model
                    )));
                }
            }
            if v.technical {
                lines.push(Line::from(format!(
                    "Agent / recorded launch model: {}",
                    agent_model(e.agent, Some(e))
                )));
                lines.push(Line::from(format!(
                    "{} · {} · {:?}",
                    e.workspace, e.agent, e.intent
                )));
                lines.push(Line::from(format!("Original title: {}", e.name)));
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
                if v.technical {
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
                }
                if let Some(error) = &r.last_error {
                    lines.push(Line::from(format!("Latest outcome: {error}")));
                }
                if r.ended
                    && (v.technical
                        || r.exit_code.is_some_and(|c| c != 0)
                        || !r.survivors.is_empty())
                {
                    lines.push(Line::from(format!(
                        "Exited {} · {} survivor records",
                        r.exit_code
                            .map(|code| code.to_string())
                            .unwrap_or("without exit code".into()),
                        r.survivors.len()
                    )));
                }
            }
            if let Some(m) = s.owned.get(&e.id).filter(|_| v.technical) {
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
            .block(panel(
                if let Some(e) = related.filter(|_| related_area.is_none()) {
                    format!(" Selected · {} · d expands ", related_summary(e, s))
                } else {
                    " Selected work · d / Esc ".into()
                },
                v.theme,
                v.ascii,
            )),
        area,
    );
    if let Some((e, area)) = related.zip(related_area) {
        draw_related(f, area, s, e, v);
    }
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
// Fixed percentage and time axes; separate datasets avoid joining missing intervals.
fn history_segments(
    points: impl IntoIterator<Item = (Instant, f64)>,
    anchor: Instant,
    gap: Duration,
) -> Vec<Vec<(f64, f64)>> {
    let mut segments: Vec<Vec<(f64, f64)>> = vec![];
    let mut previous = None;
    for (time, value) in points {
        let age = anchor.saturating_duration_since(time).as_secs_f64();
        if age > 60. || !value.is_finite() {
            continue;
        }
        if previous.is_none_or(|prev| time.saturating_duration_since(prev) > gap) {
            segments.push(vec![]);
        }
        segments
            .last_mut()
            .unwrap()
            .push((-age, value.clamp(0., 100.)));
        previous = Some(time);
    }
    segments
}
fn draw_plot(
    f: &mut ratatui::Frame,
    area: Rect,
    title: String,
    segments: &[Vec<(f64, f64)>],
    v: &View,
) {
    if segments.is_empty() {
        f.render_widget(
            Paragraph::new("Unavailable · no samples")
                .wrap(Wrap { trim: false })
                .block(panel(title, v.theme, v.ascii)),
            area,
        );
        return;
    }
    let datasets = segments
        .iter()
        .map(|data| {
            Dataset::default()
                .data(data)
                .marker(if v.ascii {
                    Marker::Dot
                } else {
                    Marker::Braille
                })
                .graph_type(GraphType::Line)
                .style(v.theme.accent())
        })
        .collect();
    f.render_widget(
        Chart::new(datasets)
            .block(panel(title, v.theme, v.ascii))
            .x_axis(
                Axis::default()
                    .bounds([-60., 0.])
                    .labels(["-60s", "-30s", if v.paused { "sample" } else { "now" }])
                    .style(v.theme.muted()),
            )
            .y_axis(
                Axis::default()
                    .bounds([0., 100.])
                    .labels(["0%", "100%"])
                    .style(v.theme.muted()),
            ),
        area,
    );
    if v.ascii {
        let buffer = f.buffer_mut();
        for y in area.y..area.bottom() {
            for x in area.x..area.right() {
                let cell = &mut buffer[(x, y)];
                let symbol = match cell.symbol() {
                    "•" => ".",
                    "─" => "-",
                    "│" => "|",
                    "└" => "+",
                    _ => continue,
                };
                cell.set_symbol(symbol);
            }
        }
    }
}
fn draw_hardware(f: &mut ratatui::Frame, area: Rect, s: &Sample, v: &View, column: bool) {
    if !s.ready {
        f.render_widget(
            Paragraph::new(if v.paused {
                "Monitoring paused · no machine sample collected"
            } else {
                "Collecting machine metrics…"
            })
            .block(panel(
                if column {
                    " Usage · h expands "
                } else {
                    " Usage · h returns "
                }
                .into(),
                v.theme,
                v.ascii,
            )),
            area,
        );
        return;
    }
    let host = s.hosts.first();
    let m = &s.memory;
    let frozen = if v.paused { " · paused" } else { "" };
    let mut lines = vec![
        Line::from(format!(
            "RAM {:.1}/{:.1} GiB · pressure {}{}",
            m.used_bytes as f64 / 1073741824.,
            m.total_bytes as f64 / 1073741824.,
            m.pressure.as_deref().unwrap_or("unavailable"),
            frozen
        )),
        Line::from(format!(
            "Compressed {} · swap {:.1} GiB · swap in/out {} / {} KiB/s",
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
        )),
    ];
    if let Some(sensor) = host.and_then(|h| h.sensors.as_ref()) {
        lines.push(Line::from(format!(
            "CPU {} °C · GPU {} °C · power {} W{}",
            sensor
                .cpu_temp
                .map(|n| format!("{n:.0}"))
                .unwrap_or("-".into()),
            sensor
                .gpu_temp
                .map(|n| format!("{n:.0}"))
                .unwrap_or("-".into()),
            sensor
                .system_power
                .map(|n| format!("{n:.1}"))
                .unwrap_or("-".into()),
            if !v.paused && host.is_some_and(|h| !h.live()) {
                " · sensors stale"
            } else {
                ""
            }
        )));
    } else {
        lines.push(Line::from("E/P CPU and GPU require optional macmon"));
    }
    if let Some(error) = host.and_then(|h| h.error.as_ref()) {
        lines.push(Line::from(error.clone()));
    }
    for remote in s.hosts.iter().skip(1) {
        lines.push(Line::from(format!(
            "{} · {}",
            remote.label,
            if v.paused {
                "paused"
            } else if remote.live() {
                "live"
            } else {
                "unavailable/stale"
            }
        )));
    }
    if !column && (area.height < 18 || (area.width < 70 && area.height.saturating_sub(5) < 28)) {
        if let Some(sensor) = host.and_then(|h| h.sensors.as_ref()) {
            lines.push(Line::from(
                [
                    ("E CPU", &sensor.efficiency),
                    ("P CPU", &sensor.performance),
                    ("GPU", &sensor.gpu),
                ]
                .into_iter()
                .map(|(label, load)| {
                    format!(
                        "{label} {:.0}%{}",
                        load.ratio * 100.,
                        if load.weighted { " scaled" } else { "" }
                    )
                })
                .collect::<Vec<_>>()
                .join(" · "),
            ));
        }
        lines.push(Line::from("Enlarge terminal for history plots · h returns"));
        f.render_widget(
            Paragraph::new(lines)
                .wrap(Wrap { trim: false })
                .block(panel(" Usage ".into(), v.theme, v.ascii)),
            area,
        );
        return;
    }
    let footer_height = if column && area.height < 33 { 0 } else { 5 };
    let sections = Layout::vertical([
        Constraint::Min(if column { 24 } else { 14 }),
        Constraint::Length(footer_height),
    ])
    .split(area);
    let anchor = s.sampled_at.unwrap_or_else(Instant::now);
    let memory = history_segments(
        s.memory_history.iter().copied(),
        anchor,
        Duration::from_secs(5),
    );
    let cpu_gpu: Vec<_> = (1..4)
        .map(|i| {
            history_segments(
                host.into_iter().flat_map(|h| {
                    h.sample_times
                        .iter()
                        .copied()
                        .zip(h.history[i].iter().map(|n| *n as f64))
                }),
                anchor,
                Duration::from_millis(2500),
            )
        })
        .collect();
    let slots: Vec<_> = if !column && area.width >= 70 {
        Layout::vertical([Constraint::Percentage(50), Constraint::Percentage(50)])
            .split(sections[0])
            .iter()
            .flat_map(|row| {
                Layout::horizontal([Constraint::Percentage(50), Constraint::Percentage(50)])
                    .split(*row)
                    .iter()
                    .copied()
                    .collect::<Vec<_>>()
            })
            .collect()
    } else {
        // Four readable stacked plots require more height on narrow terminals.
        Layout::vertical([Constraint::Percentage(25); 4])
            .split(sections[0])
            .to_vec()
    };
    draw_plot(
        f,
        slots[0],
        format!(
            " RAM · {}{} ",
            if m.total_bytes > 0 {
                format!("{:.0}%", m.used_bytes as f64 / m.total_bytes as f64 * 100.)
            } else {
                "unavailable".into()
            },
            frozen
        ),
        &memory,
        v,
    );
    for (i, label) in ["E CPU", "P CPU", "GPU"].iter().enumerate() {
        let load = host.and_then(|h| h.sensors.as_ref()).map(|sensor| match i {
            0 => &sensor.efficiency,
            1 => &sensor.performance,
            _ => &sensor.gpu,
        });
        let title = format!(
            " {label}{}{}{} ",
            load.map(|l| format!(
                " · {:.0}%{}",
                l.ratio * 100.,
                if l.weighted { " scaled" } else { "" }
            ))
            .unwrap_or_default(),
            frozen,
            if !v.paused && host.is_some_and(|h| !h.live()) {
                " · stale"
            } else {
                ""
            }
        );
        draw_plot(f, slots[i + 1], title, &cpu_gpu[i], v);
    }
    if column {
        lines = vec![
            Line::from(format!(
                "RAM {:.1}/{:.1} GiB · {}",
                m.used_bytes as f64 / 1073741824.,
                m.total_bytes as f64 / 1073741824.,
                m.pressure.as_deref().unwrap_or("?")
            )),
            Line::from(format!(
                "Compressed {} · swap {:.1} GiB",
                m.compressed_bytes
                    .map(|n| format!("{:.1}", n as f64 / 1073741824.))
                    .unwrap_or("?".into()),
                m.swap_used_bytes as f64 / 1073741824.
            )),
        ];
        if let Some(sensor) = host.and_then(|h| h.sensors.as_ref()) {
            lines.push(Line::from(format!(
                "CPU {}°C · GPU {}°C · {}W",
                sensor
                    .cpu_temp
                    .map(|n| format!("{n:.0}"))
                    .unwrap_or("?".into()),
                sensor
                    .gpu_temp
                    .map(|n| format!("{n:.0}"))
                    .unwrap_or("?".into()),
                sensor
                    .system_power
                    .map(|n| format!("{n:.1}"))
                    .unwrap_or("?".into())
            )));
        } else {
            lines.push(Line::from("CPU/GPU: optional macmon"));
        }
    }
    if footer_height > 0 {
        f.render_widget(
            Paragraph::new(lines)
                .wrap(Wrap { trim: false })
                .style(v.theme.muted())
                .block(panel(
                    if column {
                        if v.paused {
                            " This Mac · frozen · h expand ".into()
                        } else {
                            " This Mac · 60s · h expand ".into()
                        }
                    } else if v.paused {
                        " This Mac · frozen 60s window · h returns ".into()
                    } else {
                        " This Mac · last 60s · h returns ".into()
                    },
                    v.theme,
                    v.ascii,
                )),
            sections[1],
        );
    }
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
            sample_times: Default::default(),
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
        let anchor = Instant::now();
        s.sampled_at = Some(anchor);
        for i in 0..60 {
            let time = anchor - Duration::from_secs(59 - i);
            s.hosts[0].sample_times.push_back(time);
            let values = [38, 12 + (i % 12), 21 + (i % 20), 6 + (i % 8)];
            for (series, value) in s.hosts[0].history.iter_mut().zip(values) {
                series.push_back(value);
            }
            if i % 2 == 0 {
                s.memory_history
                    .push_back((time, 37.5 + (i % 8) as f64 / 4.));
            }
        }
        let v = View {
            binding: None,
            list_area: None,
            seen: BTreeMap::new(),
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
        assert!(!contents(&b).contains("subagents unavailable"));
        v.technical = true;
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
    fn imported_titles_are_readable_disambiguated_and_raw_titles_still_searchable() {
        let (mut s, mut v) = fixture();
        let raw = "codex --no-daemon resume 11111111-2222-3333-4444-555555555555";
        s.state.entries[0].name = raw.into();
        s.state.entries[0].imported = true;
        let mut sibling = s.state.entries[0].clone();
        sibling.id = Uuid::from_u128(77);
        s.state.entries.push(sibling);
        assert_ne!(
            display_name(&s.state.entries[0], &s),
            display_name(&s.state.entries[2], &s)
        );
        assert!(display_name(&s.state.entries[0], &s).starts_with("workspaces · Codex"));
        for title in [
            "/opt/homebrew/bin/codex resume 123",
            "exec /bin/codex resume 123",
            "env A=b /bin/codex resume 123",
        ] {
            let mut entry = s.state.entries[0].clone();
            entry.name = title.into();
            assert_eq!(short_name(&entry), "workspaces · Codex");
        }
        assert_eq!(display_name(&s.state.entries[1], &s), "Literature notes");
        let screen = contents(&render(&s, &mut v, 80, 24));
        assert!(!screen.contains("--no-daemon"));
        v.pos_mut().query = "555555555555".into();
        assert_eq!(items(&s, &v).len(), 2);
        v.technical = true;
        assert!(contents(&render(&s, &mut v, 140, 42)).contains(raw));
    }
    #[test]
    fn percentage_history_uses_time_and_breaks_gaps_instead_of_inventing_zeroes() {
        let anchor = Instant::now();
        let points = [
            (anchor - Duration::from_secs(80), 30.),
            (anchor - Duration::from_secs(40), 35.),
            (anchor - Duration::from_secs(38), 45.),
            (anchor - Duration::from_secs(20), 55.),
        ];
        let segments = history_segments(points, anchor, Duration::from_secs(5));
        assert_eq!(
            segments,
            vec![vec![(-40., 35.), (-38., 45.)], vec![(-20., 55.)]]
        );
        let (s, mut v) = fixture();
        v.hardware_only = true;
        v.paused = true;
        let screen = contents(&render(&s, &mut v, 80, 24));
        for label in [
            "RAM", "E CPU", "P CPU", "GPU", "100%", "-60s", "paused", "sample", "frozen",
        ] {
            assert!(screen.contains(label), "missing {label}");
        }
        assert_eq!(render(&s, &mut v, 80, 24), render(&s, &mut v, 80, 24));
        let mut unavailable = s.clone();
        unavailable.hosts.clear();
        assert!(contents(&render(&unavailable, &mut v, 80, 24)).contains("Unavailable"));
        assert!(!contents(&render(&unavailable, &mut v, 80, 24)).contains("GPU · 0%"));
        let narrow = contents(&render(&s, &mut v, 40, 20));
        for label in ["E CPU", "P CPU", "GPU"] {
            assert!(narrow.contains(label));
        }
        let mut scaled = s.clone();
        scaled.hosts[0]
            .sensors
            .as_mut()
            .unwrap()
            .efficiency
            .weighted = true;
        assert!(contents(&render(&scaled, &mut v, 80, 24)).contains("scaled"));
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
    fn desktop_overview_has_stacked_usage_and_separate_location_model_columns() {
        let (mut s, mut v) = fixture();
        s.state.entries[0].args = vec!["--model=gpt-6-luna".into()];
        s.state.entries[1].imported = true;
        let b = render(&s, &mut v, 140, 42);
        let text = contents(&b);
        for label in [
            "LOCATION",
            "AGENT / MODEL",
            "Codex/gpt-6-luna",
            "E CPU",
            "P CPU",
            "GPU",
            "2 saved",
            "1 need binding",
        ] {
            assert!(text.contains(label), "missing {label}");
        }
        let sidebar = |label: &str| {
            (0..b.area.height)
                .find(|y| {
                    (106..140)
                        .map(|x| b[(x, *y)].symbol())
                        .collect::<String>()
                        .contains(label)
                })
                .unwrap()
        };
        assert!(sidebar("RAM") < sidebar("E CPU"));
        assert!(sidebar("E CPU") < sidebar("P CPU"));
        assert!(sidebar("P CPU") < sidebar("GPU"));
        let compact = contents(&render(&s, &mut v, 80, 24));
        assert!(compact.contains("LOCATION") && compact.contains("Codex/gpt-6-luna"));
        assert!(!compact.contains("-60s"));
        v.pos_mut().query = "gpt-6-luna".into();
        assert_eq!(items(&s, &v).len(), 1);
        assert_eq!(
            agent_model(model::Agent::Claude, Some(&s.state.entries[1])),
            "Claude"
        );
        assert!(
            location_label(std::path::Path::new("/long/project/path/日本語"), 12, false)
                .ends_with("日本語")
        );
    }
    #[test]
    fn related_activity_keeps_hook_agents_separate_and_excludes_other_process_trees() {
        let (mut s, mut v) = fixture();
        let id = s.state.entries[0].id;
        let root = Proc {
            pid: 900,
            start_time: s.at - 100,
            name: "codex".into(),
            memory: 100 * 1048576,
            cpu: 0.,
            footprint: None,
            state: "Running".into(),
            parent: None,
        };
        let child = Proc {
            pid: 901,
            parent: Some(root.pid),
            start_time: root.start_time + 1,
            name: "owned-preview".into(),
            memory: 80 * 1048576,
            cpu: 0.,
            footprint: None,
            state: "Running".into(),
        };
        let unrelated = Proc {
            pid: 902,
            start_time: root.start_time,
            name: "unrelated-shared-daemon".into(),
            memory: 200 * 1048576,
            cpu: 0.,
            footprint: None,
            state: "Running".into(),
            parent: None,
        };
        for p in [&root, &child, &unrelated] {
            s.processes.insert(p.pid, p.clone());
        }
        s.state.runs.insert(
            id,
            model::Run {
                pid: root.pid,
                start_time: root.start_time,
                hooks_seen: true,
                agents: BTreeMap::from([
                    ("one".into(), "review".into()),
                    ("two".into(), "tests".into()),
                ]),
                ..Default::default()
            },
        );
        s.owned.insert(
            id,
            Metrics {
                processes: vec![root, child],
                ..Default::default()
            },
        );
        let text = contents(&render(&s, &mut v, 140, 42));
        assert!(text.contains("Related activity"));
        assert!(text.contains("Subagents: 2 observed · review, tests"));
        assert!(text.contains("Processes: 2 in tracked execution tree"));
        assert!(text.contains("owned-preview"));
        assert!(!text.contains("unrelated-shared-daemon"));
        let compact = contents(&render(&s, &mut v, 80, 24));
        assert!(compact.contains("2 processes · 2 observed agents"));
        s.state.runs.get_mut(&id).unwrap().hooks_seen = false;
        let text = contents(&render(&s, &mut v, 140, 42));
        assert!(text.contains("Subagents: unknown"));
        assert!(!text.contains("Subagents: 0"));
        v.detail_only = true;
        v.pos_mut().identity = Some(s.state.entries[1].id.to_string());
        assert!(!contents(&render(&s, &mut v, 140, 42)).contains("owned-preview"));
    }
    #[test]
    fn completion_markers_persist_read_state_and_fence_old_runs_and_new_turns() {
        let (mut s, v) = fixture();
        let id = s.state.entries[0].id;
        let token = Uuid::new_v4();
        let run = s.state.runs.get_mut(&id).unwrap();
        run.token = token;
        run.completed_turns = 2;
        run.last_turn_end = Some(s.at);
        assert_eq!(attention(&s.state.entries[0], &s, &v).0, "◆");
        let temp = tempfile::tempdir().unwrap();
        let store = Store::new(Some(temp.path().into())).unwrap();
        store
            .update(|state| {
                *state = s.state.clone();
                Ok(())
            })
            .unwrap();
        assert_eq!(
            store
                .document::<model::Attention>("attention", &token.to_string())
                .unwrap()
                .unwrap()
                .completed_turns,
            2,
            "completion metadata must commit atomically with run state"
        );
        record_seen(&store, id, token, 2, true).unwrap();
        s.state = store.read().unwrap();
        assert_eq!(attention(&s.state.entries[0], &s, &v).0, "○");
        store
            .edit_document("attention", &token.to_string(), |old| {
                let mut attention: model::Attention = serde_json::from_value(old.unwrap())?;
                attention.completed_turns = 3;
                Ok(attention)
            })
            .unwrap();
        record_seen(&store, id, token, 2, true).unwrap();
        s.state = store.read().unwrap();
        assert_eq!(attention(&s.state.entries[0], &s, &v).0, "◆");
        let newer = Uuid::new_v4();
        // Simulate an older running supervisor rewriting only the fields it knows.
        let c = rusqlite::Connection::open(store.database()).unwrap();
        c.execute("UPDATE runs SET body=json_remove(body,'$.completed_turns','$.seen_turns','$.last_turn_end','$.exit_seen') WHERE item_id=?1", [id.to_string()]).unwrap();
        drop(c);
        assert_eq!(store.read().unwrap().runs[&id].completed_turns, 3);
        assert_eq!(store.read().unwrap().runs[&id].seen_turns, 2);
        store
            .update(|state| {
                let r = state.runs.get_mut(&id).unwrap();
                r.token = newer;
                r.seen_turns = 0;
                r.exit_seen = false;
                Ok(())
            })
            .unwrap();
        record_seen(&store, id, token, 3, true).unwrap();
        assert_eq!(store.read().unwrap().runs[&id].seen_turns, 0);
        assert!(!store.read().unwrap().runs[&id].exit_seen);
        let r = s.state.runs.get_mut(&id).unwrap();
        r.ended = false;
        r.pid = 988;
        r.start_time = s.at;
        r.hooks_seen = true;
        r.activity = "working".into();
        s.processes.insert(
            988,
            Proc {
                pid: 988,
                parent: None,
                name: "codex".into(),
                cpu: 0.,
                memory: 0,
                start_time: s.at,
                footprint: None,
                state: "Running".into(),
            },
        );
        assert_eq!(attention(&s.state.entries[0], &s, &v).0, "●");
        s.state.runs.get_mut(&id).unwrap().activity = "unknown".into();
        s.state.runs.get_mut(&id).unwrap().last_turn_end = None;
        assert_eq!(attention(&s.state.entries[0], &s, &v).0, "·");
    }
    #[test]
    fn native_focus_acknowledges_only_the_observed_surface_and_completion() {
        let (mut s, _) = fixture();
        let id = s.state.entries[0].id;
        let run = s.state.runs.get_mut(&id).unwrap();
        run.token = Uuid::new_v4();
        run.terminal_id = Some("selected-split".into());
        run.completed_turns = 2;
        run.seen_turns = 0;
        run.last_turn_end = Some(s.at);
        run.ended = false;
        let token = run.token;
        // A stale entry surface must not override the current run surface.
        s.state.entries[0].terminal_id = Some("other-split".into());
        let windows = vec![Window {
            id: "front-window".into(),
            tabs: vec![ghostty::Tab {
                id: "selected-tab".into(),
                name: "two splits".into(),
                terminals: ["selected-split", "other-split"]
                    .into_iter()
                    .map(|id| ghostty::Surface {
                        id: id.into(),
                        cwd: "/project".into(),
                    })
                    .collect(),
            }],
        }];
        let temp = tempfile::tempdir().unwrap();
        let store = Store::new(Some(temp.path().into())).unwrap();
        store
            .update(|state| {
                *state = s.state.clone();
                Ok(())
            })
            .unwrap();
        for focus in [None, Some("other-split"), Some("missing-surface")] {
            record_visible_completion(&store, &s.state, focus, &windows).unwrap();
            assert_eq!(store.read().unwrap().runs[&id].seen_turns, 0);
        }
        // A completion arriving during automation must remain unread.
        store
            .edit_document("attention", &token.to_string(), |_| {
                Ok(model::Attention {
                    completed_turns: 3,
                    last_turn_end: Some(s.at),
                    ..Default::default()
                })
            })
            .unwrap();
        record_visible_completion(&store, &s.state, Some("selected-split"), &windows).unwrap();
        let read = store.read().unwrap();
        assert_eq!(read.runs[&id].seen_turns, 2);
        assert_eq!(read.runs[&id].completed_turns, 3);
        assert!(!read.runs[&id].exit_seen);
        s.state = read;
        // Ambiguous ownership is not acknowledged by guessing.
        s.state
            .runs
            .insert(s.state.entries[1].id, s.state.runs[&id].clone());
        record_visible_completion(&store, &s.state, Some("selected-split"), &windows).unwrap();
        assert_eq!(store.read().unwrap().runs[&id].seen_turns, 2);
        s.state.runs.remove(&s.state.entries[1].id);
        record_visible_completion(&store, &s.state, Some("selected-split"), &windows).unwrap();
        assert_eq!(store.read().unwrap().runs[&id].seen_turns, 3);
    }
    #[test]
    fn bind_marker_hit_testing_tracks_scroll_and_never_treats_headers_as_items() {
        let (mut s, mut v) = fixture();
        s.state.entries[0].imported = true;
        s.state.entries[0].session_verified = false;
        let b = render(&s, &mut v, 140, 42);
        let area = v.list_area.unwrap();
        assert_eq!(b[(area.x + 4, area.y + 2)].symbol(), "?");
        assert_eq!(clicked_row(&v, 2, area.x + 4, area.y + 2), Some((0, true)));
        assert_eq!(clicked_row(&v, 2, area.x + 8, area.y + 2), Some((0, false)));
        assert_eq!(clicked_row(&v, 2, area.x + 4, area.y + 1), None);
        *v.pos_mut().table.offset_mut() = 1;
        assert_eq!(clicked_row(&v, 2, area.x + 4, area.y + 2), Some((1, true)));
        let temp = tempfile::tempdir().unwrap();
        let store = Store::new(Some(temp.path().join("space ' state"))).unwrap();
        let command = bind_command(&store, &s.state.entries[0]).unwrap();
        assert!(command.starts_with("!gws --state-dir '"));
        assert!(command.contains("'\\''"));
        assert!(command.ends_with(&format!("bind-here --item {}", s.state.entries[0].id)));
        v.binding = Some(s.state.entries[0].id);
        assert_eq!(clicked_row(&v, 2, area.x + 4, area.y + 2), None);
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
                args: match i {
                    0 => vec!["--model=gpt-6-luna".into()],
                    1 => vec!["--model=sonnet".into()],
                    2 => vec!["--model=gpt-6-sol".into()],
                    _ => vec![],
                },
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
                exit_seen: i == 6 || i == 8,
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
        let owner = Uuid::from_u128(1);
        for (pid, name, mib, cpu) in [(42001, "node", 160, 1.8), (42002, "python3", 64, 0.8)] {
            let p = Proc {
                pid,
                parent: Some(41000),
                name: name.into(),
                memory: mib * 1048576,
                cpu,
                start_time: s.at - 1200,
                state: "Running".into(),
                footprint: Some(mib * 1048576),
            };
            let m = s.owned.get_mut(&owner).unwrap();
            m.memory += p.memory;
            m.cpu += p.cpu;
            m.footprint = Some(m.footprint.unwrap() + p.memory);
            m.footprint_coverage += 1;
            m.processes.push(p.clone());
            s.processes.insert(pid, p);
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
        for (name, w, h) in [
            ("wide", 140, 42),
            ("desktop", 100, 30),
            ("compact", 80, 24),
            ("narrow", 40, 20),
        ] {
            let b = render(&s, &mut v, w, h);
            let cells:Vec<_>=b.content.iter().map(|c|serde_json::json!({"text":c.symbol(),"fg":format!("{:?}",c.fg),"bg":format!("{:?}",c.bg),"bold":c.modifier.contains(Modifier::BOLD),"reverse":c.modifier.contains(Modifier::REVERSED)})).collect();
            std::fs::write(
                path.join(format!("{name}.json")),
                serde_json::to_vec(&serde_json::json!({"width":w,"height":h,"cells":cells}))
                    .unwrap(),
            )
            .unwrap();
        }
        v.hardware_only = true;
        for (name, w, h) in [("hardware", 100, 30), ("hardware-compact", 80, 24)] {
            let b = render(&s, &mut v, w, h);
            let cells: Vec<_> = b.content.iter().map(|c| serde_json::json!({"text":c.symbol(),"fg":format!("{:?}",c.fg),"bg":format!("{:?}",c.bg),"bold":c.modifier.contains(Modifier::BOLD),"reverse":c.modifier.contains(Modifier::REVERSED)})).collect();
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
            binding: None,
            list_area: None,
            seen: BTreeMap::new(),
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
