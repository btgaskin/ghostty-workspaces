use crate::{
    ghostty::{self, Window},
    hardware::{Host, Load, Monitors},
    model::{Agent, Entry, State, Store},
    process::{self, Metrics, Monitor, Proc},
};
use anyhow::Result;
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
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::Duration,
};
const ACCENT: Color = Color::Rgb(143, 200, 181);
const MUTED: Color = Color::Rgb(139, 148, 158);
#[derive(Default, Clone)]
struct Sample {
    state: State,
    windows: Vec<Window>,
    processes: BTreeMap<u32, Proc>,
    owned: BTreeMap<uuid::Uuid, Metrics>,
    used: u64,
    total: u64,
    error: Option<String>,
    ready: bool,
}
#[derive(Serialize)]
struct Listing<'a> {
    entry: &'a Entry,
    status: &'static str,
    metrics: Option<&'a Metrics>,
    subagents: Option<usize>,
}
fn metrics<'a>(e: &Entry, s: &'a Sample) -> Option<&'a Metrics> {
    s.owned.get(&e.id)
}
fn calculate_owned(s: &mut Sample) {
    s.owned = s
        .state
        .runs
        .iter()
        .filter_map(|(id, run)| process::tree(run, &s.processes).map(|m| (*id, m)))
        .collect();
}
fn status(e: &Entry, s: &Sample) -> &'static str {
    if s.error.is_some() {
        return "unknown";
    }
    if e.terminal_id
        .as_ref()
        .is_some_and(|id| ghostty::terminal_exists(&s.windows, id))
    {
        if metrics(e, s).is_some() {
            "running"
        } else {
            "open"
        }
    } else if e.needs_session() {
        "needs ID"
    } else {
        "saved"
    }
}
fn subagents(e: &Entry, s: &Sample) -> Option<usize> {
    metrics(e, s)?;
    s.state
        .runs
        .get(&e.id)
        .filter(|r| r.hooks_seen)
        .map(|r| r.agents.len())
}
pub fn list(store: &Store, json: bool) -> Result<()> {
    let mut mon = Monitor::new();
    mon.refresh();
    thread::sleep(Duration::from_millis(250));
    let state = store.read()?;
    let (windows, error) = match ghostty::snapshot() {
        Ok(w) => (w, None),
        Err(e) => (vec![], Some(e.to_string())),
    };
    let mut s = Sample {
        state,
        windows,
        processes: mon.refresh(),
        error,
        ..Default::default()
    };
    calculate_owned(&mut s);
    let rows: Vec<_> = s
        .state
        .entries
        .iter()
        .map(|e| Listing {
            entry: e,
            status: status(e, &s),
            metrics: metrics(e, &s),
            subagents: subagents(e, &s),
        })
        .collect();
    if json {
        println!("{}", serde_json::to_string_pretty(&rows)?)
    } else {
        println!(
            "{:<36}  {:<10}  {:<9}  {:>7}  {:>10}  NAME",
            "ID", "AGENT", "STATE", "CPU %", "RSS MiB"
        );
        for r in rows {
            println!(
                "{}  {:<10}  {:<9}  {:>7}  {:>10}  {}",
                r.entry.id,
                r.entry.agent.to_string(),
                r.status,
                r.metrics
                    .as_ref()
                    .map(|m| format!("{:.1}", m.cpu))
                    .unwrap_or("—".into()),
                r.metrics
                    .as_ref()
                    .map(|m| format!("{:.1}", m.memory as f64 / 1048576.))
                    .unwrap_or("—".into()),
                r.entry.name
            );
        }
        if let Some(e) = s.error {
            eprintln!("{e}");
        }
    }
    Ok(())
}
#[derive(Clone, Copy)]
enum Sort {
    Workspace,
    Memory,
    Cpu,
}
struct View<'a> {
    processes: Vec<&'a Proc>,
    hosts: Vec<Host>,
    process_mode: bool,
    host_index: usize,
    detail_offset: u16,
}
pub fn run(store: &Store, hosts: &[String]) -> Result<()> {
    crossterm::execute!(
        std::io::stdout(),
        crossterm::terminal::SetTitle("Ghostty Workspaces")
    )?;
    let monitors = Monitors::start(hosts)?;
    let shared = Arc::new(Mutex::new(Sample::default()));
    let stop = Arc::new(AtomicBool::new(false));
    let worker_shared = shared.clone();
    let worker_stop = stop.clone();
    let worker_store = store.clone();
    thread::spawn(move || {
        let mut mon = Monitor::new();
        while !worker_stop.load(Ordering::Relaxed) {
            let mut next = Sample::default();
            match worker_store.read() {
                Ok(s) => next.state = s,
                Err(e) => next.error = Some(e.to_string()),
            }
            match ghostty::snapshot() {
                Ok(w) => next.windows = w,
                Err(e) => next.error = Some(e.to_string()),
            }
            next.processes = mon.refresh();
            calculate_owned(&mut next);
            (next.used, next.total) = mon.total_memory();
            next.ready = true;
            if let Ok(mut s) = worker_shared.lock() {
                *s = next;
            }
            // Fast shutdown checks; sampling remains once every two seconds.
            for _ in 0..20 {
                if worker_stop.load(Ordering::Relaxed) {
                    break;
                }
                thread::sleep(Duration::from_millis(100));
            }
        }
    });
    let result = ratatui::run(|terminal| -> Result<()> {
        let mut selected = 0usize;
        let mut table = TableState::default();
        let mut message = String::from("Collecting Ghostty tabs and process metrics…");
        let mut sort = Sort::Workspace;
        let mut confirm = None;
        let mut process_mode = false;
        let mut host_index = 0;
        let mut identity = None;
        let mut detail_offset = 0;
        loop {
            let sample = shared.lock().map(|s| s.clone()).unwrap_or_default();
            if sample.ready && message == "Collecting Ghostty tabs and process metrics…" {
                message = "Ready · PgUp/PgDn scroll details".into();
            }
            let mut entries: Vec<_> = sample.state.entries.iter().collect();
            match sort {
                Sort::Workspace => entries.sort_by_key(|e| {
                    (
                        !e.terminal_id
                            .as_ref()
                            .is_some_and(|t| ghostty::terminal_exists(&sample.windows, t)),
                        &e.workspace,
                        e.window,
                    )
                }),
                Sort::Memory => entries.sort_by(|a, b| {
                    metrics(b, &sample)
                        .map(|m| m.memory)
                        .cmp(&metrics(a, &sample).map(|m| m.memory))
                }),
                Sort::Cpu => entries.sort_by(|a, b| {
                    metrics(b, &sample)
                        .map(|m| m.cpu)
                        .unwrap_or(-1.)
                        .total_cmp(&metrics(a, &sample).map(|m| m.cpu).unwrap_or(-1.))
                }),
            }
            let mut processes: Vec<_> = sample.processes.values().collect();
            match sort {
                Sort::Cpu => {
                    processes.sort_by(|a, b| b.cpu.total_cmp(&a.cpu).then(a.pid.cmp(&b.pid)))
                }
                _ => processes.sort_by(|a, b| b.memory.cmp(&a.memory).then(a.pid.cmp(&b.pid))),
            }
            let keys: Vec<_> = if process_mode {
                processes
                    .iter()
                    .map(|p| format!("{}:{}", p.pid, p.start_time))
                    .collect()
            } else {
                entries.iter().map(|e| e.id.to_string()).collect()
            };
            if let Some(index) = identity
                .as_ref()
                .and_then(|id| keys.iter().position(|k| k == id))
            {
                selected = index;
            }
            selected = selected.min(keys.len().saturating_sub(1));
            table.select(if keys.is_empty() {
                None
            } else {
                Some(selected)
            });
            let view = View {
                processes,
                hosts: monitors.snapshot(),
                process_mode,
                host_index,
                detail_offset,
            };
            terminal.draw(|f| {
                draw(
                    f,
                    &sample,
                    &entries,
                    &mut table,
                    &message,
                    confirm.is_some(),
                    &view,
                )
            })?;
            if event::poll(Duration::from_millis(250))? {
                let Event::Key(key) = event::read()? else {
                    continue;
                };
                if key.kind != KeyEventKind::Press {
                    continue;
                }
                if let Some(id) = confirm.take() {
                    if key.code == KeyCode::Char('y') {
                        message = match crate::park_entry(store, id) {
                            Ok(()) => "Closed tab. Conversation and workspace retained.".into(),
                            Err(e) => e.to_string(),
                        };
                    } else {
                        message = "Cancelled.".into()
                    }
                    continue;
                }
                match key.code {
                    KeyCode::Char('q') | KeyCode::Esc => break,
                    KeyCode::Char('j') | KeyCode::Down => {
                        selected = (selected + 1).min(keys.len().saturating_sub(1));
                        identity = keys.get(selected).cloned();
                        detail_offset = 0;
                    }
                    KeyCode::Char('k') | KeyCode::Up => {
                        selected = selected.saturating_sub(1);
                        identity = keys.get(selected).cloned();
                        detail_offset = 0;
                    }
                    KeyCode::Tab => {
                        process_mode = !process_mode;
                        selected = 0;
                        identity = None;
                        detail_offset = 0;
                    }
                    KeyCode::PageDown => detail_offset = detail_offset.saturating_add(5),
                    KeyCode::PageUp => detail_offset = detail_offset.saturating_sub(5),
                    KeyCode::Char('h') => {
                        host_index = (host_index + 1) % view.hosts.len();
                    }
                    KeyCode::Char('m') => {
                        sort = Sort::Memory;
                        selected = 0;
                        identity = None;
                    }
                    KeyCode::Char('c') => {
                        sort = Sort::Cpu;
                        selected = 0;
                        identity = None;
                    }
                    KeyCode::Char('w') => {
                        sort = Sort::Workspace;
                        selected = 0;
                        identity = None;
                    }
                    KeyCode::Enter if !process_mode => {
                        if let Some(e) = entries.get(selected) {
                            message = match crate::open_entry(store, e.id, None) {
                                Ok(_) => format!("Opened {}", e.name),
                                Err(e) => e.to_string(),
                            };
                        }
                    }
                    KeyCode::Char('p') if !process_mode => {
                        if let Some(e) = entries.get(selected) {
                            confirm = Some(e.id);
                            message = format!(
                                "Close {} and keep its saved conversation? y / any other key cancels",
                                e.name
                            );
                        }
                    }
                    KeyCode::Char('s') => {
                        message = match crate::save(store, "default") {
                            Ok(()) => "Saved current Ghostty tabs to default.".into(),
                            Err(e) => e.to_string(),
                        };
                    }
                    KeyCode::Char('r') if !process_mode => {
                        if let Some(e) = entries.get(selected) {
                            message = match crate::restore(store, &e.workspace, false) {
                                Ok(()) => format!("Restored {}", e.workspace),
                                Err(e) => e.to_string(),
                            };
                        }
                    }
                    _ => {}
                }
            }
        }
        Ok(())
    });
    stop.store(true, Ordering::Relaxed);
    result
}
fn draw(
    f: &mut ratatui::Frame,
    s: &Sample,
    entries: &[&Entry],
    table: &mut TableState,
    message: &str,
    confirm: bool,
    view: &View<'_>,
) {
    let area = f.area();
    let sections = Layout::vertical([
        Constraint::Length(2),
        Constraint::Min(4),
        Constraint::Length(3),
    ])
    .split(area);
    let tracked = entries
        .iter()
        .filter_map(|e| metrics(e, s))
        .fold((0f32, 0u64), |(c, m), v| (c + v.cpu, m + v.memory));
    let title = format!(
        " GHOSTTY WORKSPACES   {} tabs   owned {:.1}% CPU · {:.1} MiB RSS   system {:.1}/{:.1} GiB RAM",
        entries.len(),
        tracked.0,
        tracked.1 as f64 / 1048576.,
        s.used as f64 / 1073741824.,
        s.total as f64 / 1073741824.
    );
    f.render_widget(
        Paragraph::new(title)
            .style(Style::default().fg(ACCENT))
            .block(Block::default().borders(Borders::BOTTOM)),
        sections[0],
    );
    let body = if area.width >= 100 {
        Layout::horizontal([Constraint::Percentage(57), Constraint::Percentage(43)])
            .split(sections[1])
    } else {
        Layout::vertical([
            Constraint::Min(5),
            Constraint::Length((sections[1].height / 2).clamp(6, 14)),
        ])
        .split(sections[1])
    };
    let left =
        Layout::vertical([Constraint::Percentage(60), Constraint::Percentage(40)]).split(body[0]);
    let rows = entries.iter().map(|e| {
        let m = metrics(e, s);
        let sub = subagents(e, s);
        Row::new(vec![
            if e.imported {
                format!(
                    "{} · {}",
                    e.cwd.file_name().unwrap_or_default().to_string_lossy(),
                    e.agent
                )
            } else {
                e.name.clone()
            },
            status(e, s).into(),
            m.as_ref()
                .map(|v| format!("{:.1}", v.cpu))
                .unwrap_or("—".into()),
            m.as_ref()
                .map(|v| format!("{:.0}", v.memory as f64 / 1048576.))
                .unwrap_or("—".into()),
            sub.map(|n| n.to_string()).unwrap_or("—".into()),
        ])
    });
    let widget = Table::new(
        rows,
        [
            Constraint::Min(12),
            Constraint::Length(9),
            Constraint::Length(7),
            Constraint::Length(8),
            Constraint::Length(5),
        ],
    )
    .header(
        Row::new(["TAB", "STATE", "CPU %", "RSS MiB", "AGENT"]).style(Style::default().fg(MUTED)),
    )
    .row_highlight_style(
        Style::default()
            .bg(Color::Rgb(31, 42, 45))
            .fg(ACCENT)
            .add_modifier(Modifier::BOLD),
    )
    .highlight_symbol("› ")
    .block(
        Block::default()
            .title(" Tabs ")
            .borders(Borders::ALL)
            .border_style(Style::default().fg(MUTED)),
    );
    if view.process_mode {
        let rows = view.processes.iter().map(|p| {
            Row::new(vec![
                p.name.clone(),
                p.pid.to_string(),
                format!("{:.1}", p.cpu),
                format!("{:.1}", p.memory as f64 / 1048576.),
            ])
        });
        let widget = Table::new(
            rows,
            [
                Constraint::Min(10),
                Constraint::Length(7),
                Constraint::Length(7),
                Constraint::Length(9),
            ],
        )
        .header(Row::new(["PROCESS", "PID", "CPU %", "RSS MiB"]).style(Style::default().fg(MUTED)))
        .row_highlight_style(Style::default().bg(Color::Rgb(31, 42, 45)).fg(ACCENT))
        .highlight_symbol("› ")
        .block(panel(" All local processes · Tab: workspaces "));
        f.render_stateful_widget(widget, left[0], table);
    } else {
        f.render_stateful_widget(widget, left[0], table);
    }
    let details = if view.process_mode {
        table
            .selected()
            .and_then(|i| view.processes.get(i))
            .map(|p| {
                let children = s
                    .processes
                    .values()
                    .filter(|c| c.parent == Some(p.pid))
                    .count();
                vec![
                    Line::from(p.name.clone()).style(Style::default().fg(ACCENT)),
                    Line::from(format!(
                        "PID {} · parent {} · {} direct children",
                        p.pid,
                        p.parent.map(|p| p.to_string()).unwrap_or("—".into()),
                        children
                    )),
                    Line::from(format!(
                        "{:.1}% CPU · {:.1} MiB RSS",
                        p.cpu,
                        p.memory as f64 / 1048576.
                    )),
                    Line::from(format!(
                        "Process started at {} (Unix seconds)",
                        p.start_time
                    )),
                    Line::from("Local OS process; conversation ownership is separate.")
                        .style(Style::default().fg(MUTED)),
                ]
            })
            .unwrap_or_else(|| vec![Line::from("Collecting local processes…")])
    } else if let Some(e) = table.selected().and_then(|i| entries.get(i)) {
        let mut lines = vec![
            Line::from(e.name.clone()).style(Style::default().fg(ACCENT)),
            Line::from(format!(
                "{} · {} · window {}",
                e.workspace,
                e.agent,
                e.window + 1
            )),
            Line::from(e.cwd.display().to_string()),
            Line::from(""),
            Line::from(format!(
                "Session ({})  {}",
                if e.session_verified {
                    "verified"
                } else {
                    "unverified"
                },
                e.session_id
                    .map(|s| s.to_string())
                    .unwrap_or("not captured yet".into())
            )),
            Line::from(format!("Tab ID   {}", e.id)),
            Line::from(""),
        ];
        if let Some(m) = metrics(e, s) {
            let run = s.state.runs.get(&e.id);
            lines.push(Line::from(format!(
                "{} owned processes · {:.1}% CPU · {:.1} MiB RSS",
                m.processes.len(),
                m.cpu,
                m.memory as f64 / 1048576.
            )));
            lines.push(
                Line::from(if e.agent == Agent::Codex && !e.isolated {
                    "Shared Codex server excluded from owned totals."
                } else {
                    "Owned process tree (verified root and descendants)."
                })
                .style(Style::default().fg(MUTED)),
            );
            lines.push(Line::from(match run {
                Some(r) if r.hooks_seen => format!(
                    "Subagents: {} active / {} observed starts",
                    r.agents.len(),
                    r.agents_started
                ),
                _ => "Subagents: unavailable until lifecycle hooks run.".into(),
            }));
            lines.push(Line::from(""));
            for p in &m.processes {
                lines.push(Line::from(format!(
                    "{:>6}  {:>5.1}%  {:>7.1} MiB  {}",
                    p.pid,
                    p.cpu,
                    p.memory as f64 / 1048576.,
                    p.name
                )));
            }
        } else {
            lines.push(Line::from("No owned live process tree."));
            lines.push(Line::from(
                "Run gws save to attach unique matches, or relaunch through gws.",
            ));
            if let Some(run) = s.state.runs.get(&e.id) {
                if let Some(error) = &run.last_error {
                    lines.push(Line::from(error.clone()).style(Style::default().fg(Color::Yellow)));
                } else if let Some(code) = run.exit_code {
                    lines.push(Line::from(format!("Last launch exited with code {code}.")));
                }
            }
        }
        if (e.session_id.is_none() || !e.session_verified)
            && matches!(e.agent, Agent::Codex | Agent::Claude)
        {
            lines.push(Line::from(""));
            lines.push(
                Line::from(if e.imported {
                    "Use gws bind <tab-id> <exact-session-id>."
                } else {
                    "Codex: trust scoped lifecycle hooks in /hooks."
                })
                .style(Style::default().fg(Color::Yellow)),
            );
        }
        lines
    } else {
        vec![Line::from(if s.ready {
            "No saved tabs. Run gws save or gws codex."
        } else {
            "Reading Ghostty…"
        })]
    };
    f.render_widget(
        Paragraph::new(details)
            .scroll((view.detail_offset, 0))
            .wrap(Wrap { trim: false })
            .block(
                Block::default()
                    .title(" Processes & continuity ")
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(MUTED)),
            ),
        left[1],
    );
    draw_hosts(f, body[1], &view.hosts, view.host_index, (s.used, s.total));
    let footer=vec![Line::from("Tab tabs/processes · j/k select · m RAM · c CPU · h device · enter focus/resume · p park · s save · r restore · q quit").style(Style::default().fg(MUTED)),Line::from(s.error.as_deref().unwrap_or(message)).style(Style::default().fg(if confirm{Color::Yellow}else{ACCENT}))];
    f.render_widget(
        Paragraph::new(footer).wrap(Wrap { trim: false }),
        sections[2],
    );
}
fn panel(title: &str) -> Block<'_> {
    Block::default()
        .title(title)
        .borders(Borders::ALL)
        .border_style(Style::default().fg(MUTED))
}
fn draw_hosts(
    f: &mut ratatui::Frame,
    area: Rect,
    hosts: &[Host],
    index: usize,
    fallback: (u64, u64),
) {
    if hosts.is_empty() {
        return;
    }
    // Split only when each device retains enough room for all four sensors.
    let split = area.height as usize / hosts.len() >= 16;
    let visible: Vec<_> = if split {
        hosts.iter().collect()
    } else {
        vec![&hosts[index % hosts.len()]]
    };
    let rows = Layout::vertical(vec![
        Constraint::Ratio(1, visible.len() as u32);
        visible.len()
    ])
    .split(area);
    for (host, area) in visible.into_iter().zip(rows.iter()) {
        let title = format!(
            " {} · {}{} ",
            host.label,
            if host.live() { "live" } else { "offline" },
            if !split && hosts.len() > 1 {
                " · h: next"
            } else {
                ""
            }
        );
        let block = panel(&title);
        let inner = block.inner(*area);
        f.render_widget(block, *area);
        if !host.live() {
            let mut lines = vec![
                Line::from(host.error.as_deref().unwrap_or(if host.updated.is_some() {
                    "Sensor stream stale (>5 seconds)"
                } else {
                    "Connecting to macmon…"
                }))
                .style(Style::default().fg(Color::Yellow)),
            ];
            if host.label == "This Mac" && fallback.1 > 0 {
                lines.push(Line::from(format!(
                    "RAM {:.1}/{:.1} GiB (OS sample)",
                    fallback.0 as f64 / 1073741824.,
                    fallback.1 as f64 / 1073741824.
                )));
            }
            f.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), inner);
            continue;
        }
        let Some(s) = &host.sensors else {
            continue;
        };
        let labels = [
            format!(
                "RAM  {:.1}/{:.1} GiB",
                s.ram_used as f64 / 1073741824.,
                s.ram_total as f64 / 1073741824.
            ),
            load_label("E CPU", &s.efficiency),
            load_label("P CPU", &s.performance),
            load_label("GPU", &s.gpu),
        ];
        let ratios = [
            s.ram_used as f64 / s.ram_total as f64,
            s.efficiency.ratio,
            s.performance.ratio,
            s.gpu.ratio,
        ];
        if inner.height < 12 {
            let lines: Vec<_> = labels
                .iter()
                .zip(ratios)
                .map(|(label, ratio)| Line::from(format!("{label} · {:.0}%", ratio * 100.)))
                .chain(std::iter::once(Line::from(format!(
                    "Swap {:.1}/{:.1} GiB",
                    s.swap_used as f64 / 1073741824.,
                    s.swap_total as f64 / 1073741824.
                ))))
                .collect();
            f.render_widget(
                Paragraph::new(lines).style(Style::default().fg(ACCENT)),
                inner,
            );
            continue;
        }
        let charts = inner.height >= 18;
        let height = if charts { 4 } else { 2 };
        let rows = Layout::vertical([
            Constraint::Length(height),
            Constraint::Length(height),
            Constraint::Length(height),
            Constraint::Length(height),
            Constraint::Min(2),
        ])
        .split(inner);
        for (i, (label, ratio)) in labels.iter().zip(ratios).enumerate() {
            let parts = Layout::vertical([
                Constraint::Length(1),
                Constraint::Length(1),
                Constraint::Min(0),
            ])
            .split(rows[i]);
            f.render_widget(
                Paragraph::new(label.clone()).style(Style::default().fg(MUTED)),
                parts[0],
            );
            f.render_widget(
                Gauge::default()
                    .ratio(ratio.clamp(0., 1.))
                    .label(format!("{:.0}%", ratio * 100.))
                    .gauge_style(Style::default().fg(ACCENT).bg(Color::Rgb(31, 42, 45))),
                parts[1],
            );
            if charts {
                let values: Vec<_> = host.history[i].iter().copied().collect();
                f.render_widget(
                    Sparkline::default()
                        .data(&values)
                        .max(100)
                        .style(Style::default().fg(ACCENT)),
                    parts[2],
                );
            }
        }
        let value =
            |v: Option<f64>, unit: &str| v.map(|v| format!("{v:.1}{unit}")).unwrap_or("—".into());
        let lines = vec![
            Line::from(format!(
                "Swap {:.1}/{:.1} GiB",
                s.swap_used as f64 / 1073741824.,
                s.swap_total as f64 / 1073741824.
            )),
            Line::from(format!(
                "CPU {} · GPU {}",
                value(s.cpu_temp, "°C"),
                value(s.gpu_temp, "°C")
            )),
            Line::from(format!(
                "Power {} · CPU {} · GPU {}",
                value(s.system_power, "W"),
                value(s.cpu_power, "W"),
                value(s.gpu_power, "W")
            )),
            Line::from(format!(
                "ANE {} · 60-sample history",
                value(s.ane_power, "W")
            )),
        ];
        f.render_widget(
            Paragraph::new(lines)
                .style(Style::default().fg(MUTED))
                .wrap(Wrap { trim: false }),
            rows[4],
        );
    }
}
fn load_label(label: &str, load: &Load) -> String {
    format!(
        "{label}  {:.0} MHz · {}",
        load.mhz,
        if load.weighted { "weighted" } else { "active" }
    )
}
#[cfg(test)]
mod tests {
    use super::*;
    fn text(terminal: &ratatui::Terminal<ratatui::backend::TestBackend>) -> String {
        let buffer = terminal.backend().buffer();
        buffer
            .content
            .chunks(buffer.area.width as usize)
            .map(|row| row.iter().map(|c| c.symbol()).collect::<String>())
            .collect::<Vec<_>>()
            .join("\n")
    }
    fn sensors() -> crate::hardware::Sensors {
        crate::hardware::parse(br#"{"memory":{"ram_total":34359738368,"ram_usage":21474836480,"swap_total":2147483648,"swap_usage":536870912},"ecpu_usage":[3000,0.25],"pcpu_usage":[2700,0.5],"gpu_usage":[500,0.12],"cpu_power":3.2,"gpu_power":1.1,"ane_power":0.0,"sys_power":16.3,"temp":{"cpu_temp_avg":54.3,"gpu_temp_avg":48.1}}"#).unwrap()
    }
    #[test]
    fn responsive_layout_keeps_sensors_visible_and_splits_devices_when_space_allows() {
        let s = Sample {
            ready: true,
            ..Sample::default()
        };
        let host = Host {
            label: "This Mac".into(),
            sensors: Some(sensors()),
            updated: Some(std::time::Instant::now()),
            ..Host::default()
        };
        let mut remote = host.clone();
        remote.label = "mini-example".into();
        for (width, height) in [(60, 18), (80, 24), (120, 40), (160, 50)] {
            let mut terminal =
                ratatui::Terminal::new(ratatui::backend::TestBackend::new(width, height)).unwrap();
            let view = View {
                processes: vec![],
                hosts: vec![host.clone(), remote.clone()],
                process_mode: false,
                host_index: 0,
                detail_offset: 0,
            };
            terminal
                .draw(|f| draw(f, &s, &[], &mut TableState::default(), "", false, &view))
                .unwrap();
            let output = text(&terminal);
            for label in ["This Mac", "RAM", "E CPU", "P CPU", "GPU"] {
                assert!(
                    output.contains(label),
                    "{width}x{height} missing {label}\n{output}"
                );
            }
            if width >= 100 {
                assert!(output.contains("mini-example"));
            }
        }
    }
    #[test]
    fn process_mode_and_selected_details_render_without_workspace_actions() {
        let p = Proc {
            pid: 123,
            parent: Some(1),
            name: "example-worker".into(),
            cpu: 12.5,
            memory: 104857600,
            start_time: 12345,
        };
        let view = View {
            processes: vec![&p],
            hosts: vec![],
            process_mode: true,
            host_index: 0,
            detail_offset: 0,
        };
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(120, 40)).unwrap();
        terminal
            .draw(|f| {
                draw(
                    f,
                    &Sample::default(),
                    &[],
                    &mut TableState::default().with_selected(Some(0)),
                    "",
                    false,
                    &view,
                )
            })
            .unwrap();
        let output = text(&terminal);
        assert!(output.contains("All local processes"));
        assert!(output.contains("PID 123 · parent 1"));
        assert!(output.contains("100.0 MiB RSS"));
    }
    #[test]
    fn renders_small_terminal_and_empty_workspace() {
        let backend = ratatui::backend::TestBackend::new(60, 18);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal
            .draw(|f| {
                draw(
                    f,
                    &Sample::default(),
                    &[],
                    &mut TableState::default(),
                    "",
                    false,
                    &View {
                        processes: vec![],
                        hosts: vec![Host::default()],
                        process_mode: false,
                        host_index: 0,
                        detail_offset: 0,
                    },
                )
            })
            .unwrap();
        assert!(
            terminal
                .backend()
                .buffer()
                .content
                .iter()
                .any(|c| c.symbol() == "G")
        );
    }
}
