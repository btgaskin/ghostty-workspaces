use crate::{
    ghostty::{self, Window},
    model::{Agent, Entry, State, Store},
    process::{self, Metrics, Monitor, Proc},
};
use anyhow::Result;
use crossterm::event::{self, Event, KeyCode, KeyEventKind};
use ratatui::{
    layout::{Constraint, Layout},
    style::{Color, Modifier, Style},
    text::Line,
    widgets::{Block, Borders, Paragraph, Row, Table, TableState, Wrap},
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
    used: u64,
    total: u64,
    error: Option<String>,
    ready: bool,
}
#[derive(Serialize)]
struct Listing<'a> {
    entry: &'a Entry,
    status: &'static str,
    metrics: Option<Metrics>,
    subagents: Option<usize>,
}
fn metrics(e: &Entry, s: &Sample) -> Option<Metrics> {
    s.state
        .runs
        .get(&e.id)
        .and_then(|r| process::tree(r, &s.processes))
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
    } else if matches!(e.agent, Agent::Codex | Agent::Claude)
        && (e.imported || e.ever_started)
        && e.session_id.is_none()
    {
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
    let s = Sample {
        state,
        windows,
        processes: mon.refresh(),
        error,
        ..Default::default()
    };
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
pub fn run(store: &Store) -> Result<()> {
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
        loop {
            let sample = shared.lock().map(|s| s.clone()).unwrap_or_default();
            let mut entries: Vec<_> = sample.state.entries.iter().collect();
            match sort {
                Sort::Workspace => entries.sort_by_key(|e| (&e.workspace, e.window)),
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
            selected = selected.min(entries.len().saturating_sub(1));
            table.select(if entries.is_empty() {
                None
            } else {
                Some(selected)
            });
            terminal.draw(|f| {
                draw(
                    f,
                    &sample,
                    &entries,
                    &mut table,
                    &message,
                    confirm.is_some(),
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
                        selected = (selected + 1).min(entries.len().saturating_sub(1))
                    }
                    KeyCode::Char('k') | KeyCode::Up => selected = selected.saturating_sub(1),
                    KeyCode::Char('m') => {
                        sort = Sort::Memory;
                        selected = 0;
                    }
                    KeyCode::Char('c') => {
                        sort = Sort::Cpu;
                        selected = 0;
                    }
                    KeyCode::Char('w') => {
                        sort = Sort::Workspace;
                        selected = 0;
                    }
                    KeyCode::Enter => {
                        if let Some(e) = entries.get(selected) {
                            message = match crate::open_entry(store, e.id, None) {
                                Ok(_) => format!("Opened {}", e.name),
                                Err(e) => e.to_string(),
                            };
                        }
                    }
                    KeyCode::Char('p') => {
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
                    KeyCode::Char('r') => {
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
) {
    let area = f.area();
    let sections = Layout::vertical([
        Constraint::Length(3),
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
    let body = if area.width >= 105 {
        Layout::horizontal([Constraint::Percentage(59), Constraint::Percentage(41)])
            .split(sections[1])
    } else {
        Layout::vertical([Constraint::Percentage(55), Constraint::Percentage(45)])
            .split(sections[1])
    };
    let rows = entries.iter().map(|e| {
        let m = metrics(e, s);
        let sub = subagents(e, s);
        Row::new(vec![
            e.name.clone(),
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
    f.render_stateful_widget(widget, body[0], table);
    let details = if let Some(e) = table.selected().and_then(|i| entries.get(i)) {
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
                "Session  {}",
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
                    "Owned process tree (includes launcher)."
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
            for p in m.processes {
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
                "Existing tabs gain tracking when relaunched through gws.",
            ));
            if let Some(run) = s.state.runs.get(&e.id) {
                if let Some(error) = &run.last_error {
                    lines.push(Line::from(error.clone()).style(Style::default().fg(Color::Yellow)));
                } else if let Some(code) = run.exit_code {
                    lines.push(Line::from(format!("Last launch exited with code {code}.")));
                }
            }
        }
        if e.session_id.is_none() && matches!(e.agent, Agent::Codex | Agent::Claude) {
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
            "No saved tabs. Run gws save or gws new."
        } else {
            "Reading Ghostty…"
        })]
    };
    f.render_widget(
        Paragraph::new(details).wrap(Wrap { trim: false }).block(
            Block::default()
                .title(" Processes & continuity ")
                .borders(Borders::ALL)
                .border_style(Style::default().fg(MUTED)),
        ),
        body[1],
    );
    let footer=vec![Line::from("j/k select   enter focus/resume   p close & save   s save tabs   r restore workspace   m RAM   c CPU   w workspace   q quit").style(Style::default().fg(MUTED)),Line::from(s.error.as_deref().unwrap_or(message)).style(Style::default().fg(if confirm{Color::Yellow}else{ACCENT}))];
    f.render_widget(
        Paragraph::new(footer).wrap(Wrap { trim: false }),
        sections[2],
    );
}
#[cfg(test)]
mod tests {
    use super::*;
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
