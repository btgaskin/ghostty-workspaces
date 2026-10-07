//! A per-run supervisor, not a global daemon. Provider control is capability gated.
use crate::{
    agents,
    model::{self, Intent, Store},
    process,
};
use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use std::{
    fs,
    io::{BufRead, BufReader, Write},
    os::unix::{
        net::{UnixListener, UnixStream},
        process::CommandExt,
    },
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
use uuid::Uuid;

pub fn socket_path(token: Uuid) -> PathBuf {
    std::env::temp_dir()
        .join(format!("gws-{}", unsafe { libc::getuid() }))
        .join(format!("{}.sock", token.simple()))
}
pub(crate) fn listener(token: Uuid) -> Result<UnixListener> {
    let path = socket_path(token);
    let dir = path.parent().context("No socket parent")?;
    if dir.exists() {
        let m = fs::symlink_metadata(dir)?;
        use std::os::unix::fs::MetadataExt;
        if !m.is_dir() || m.uid() != unsafe { libc::getuid() } {
            bail!("Unsafe runtime directory")
        }
    } else {
        match fs::create_dir(dir) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(e.into()),
        }
        let m = fs::symlink_metadata(dir)?;
        use std::os::unix::fs::MetadataExt;
        if !m.is_dir() || m.uid() != unsafe { libc::getuid() } {
            bail!("Unsafe runtime directory")
        }
    }
    model::private_dir(dir)?;
    let l = UnixListener::bind(&path)?;
    model::private_file(&path)?;
    l.set_nonblocking(true)?;
    Ok(l)
}
pub fn request(token: Uuid, action: &str) -> Result<Value> {
    let mut s = UnixStream::connect(socket_path(token))
        .context("Supervisor is unavailable; inspect the run")?;
    s.set_read_timeout(Some(Duration::from_secs(12)))?;
    s.set_write_timeout(Some(Duration::from_secs(2)))?;
    writeln!(s, "{}", json!({"token":token,"action":action}))?;
    let mut line = String::new();
    BufReader::new(s).take_line(&mut line)?;
    let v: Value = serde_json::from_str(&line)?;
    if v["ok"] != true {
        bail!(
            "{}",
            v["error"].as_str().unwrap_or("Control request refused")
        )
    };
    Ok(v)
}
trait ReadLine {
    fn take_line(&mut self, line: &mut String) -> std::io::Result<usize>;
}
impl<R: std::io::Read> ReadLine for BufReader<R> {
    fn take_line(&mut self, line: &mut String) -> std::io::Result<usize> {
        use std::io::Read;
        self.take(8192).read_line(line)
    }
}
fn accept(l: &UnixListener, token: Uuid) -> Option<(UnixStream, String)> {
    let (mut s, _) = l.accept().ok()?;
    let _ = s.set_read_timeout(Some(Duration::from_millis(200)));
    let _ = s.set_write_timeout(Some(Duration::from_millis(200)));
    let mut line = String::new();
    let v: Value = if BufReader::new(&mut s).take_line(&mut line).is_ok() {
        serde_json::from_str(&line).ok()?
    } else {
        return None;
    };
    if v["token"] != token.to_string() {
        let _ = writeln!(s, "{}", json!({"ok":false,"error":"Stale run token"}));
        return None;
    };
    Some((s, v["action"].as_str()?.to_owned()))
}
fn respond(s: &mut UnixStream, result: Result<()>) {
    let v = match result {
        Ok(()) => json!({"ok":true}),
        Err(e) => json!({"ok":false,"error":e.to_string()}),
    };
    let _ = writeln!(s, "{v}");
}
struct TerminalGuard {
    original_group: i32,
    termios: Option<libc::termios>,
    sigttou: libc::sighandler_t,
}
impl TerminalGuard {
    fn new() -> Self {
        unsafe {
            let group = libc::tcgetpgrp(0);
            let mut t = std::mem::zeroed();
            let termios = if libc::tcgetattr(0, &mut t) == 0 {
                Some(t)
            } else {
                None
            };
            Self {
                original_group: group,
                termios,
                sigttou: libc::signal(libc::SIGTTOU, libc::SIG_IGN),
            }
        }
    }
    fn foreground(&self, pgid: i32) {
        if self.original_group >= 0 {
            unsafe {
                libc::tcsetpgrp(0, pgid);
            }
        }
    }
    fn restore(&self) {
        self.foreground(self.original_group);
        if let Some(t) = &self.termios {
            unsafe {
                libc::tcsetattr(0, libc::TCSANOW, t);
            }
        }
    }
}
impl Drop for TerminalGuard {
    fn drop(&mut self) {
        self.restore();
        unsafe {
            libc::signal(libc::SIGTTOU, self.sigttou);
        }
    }
}
struct SocketGuard(Uuid);
impl Drop for SocketGuard {
    fn drop(&mut self) {
        let _ = fs::remove_file(socket_path(self.0));
    }
}
fn members(pgid: i32) -> Vec<(u32, u64)> {
    let mut m = process::Monitor::new();
    m.refresh()
        .values()
        .filter(|p| unsafe { libc::getpgid(p.pid as i32) } == pgid)
        .map(|p| (p.pid, p.start_time))
        .collect()
}
/// The gate process cannot execute the provider until its durable PID identity exists.
/// Parent death closes the pipe, so an unreleased gate exits without starting work.
pub fn exec_gate(fd: i32, argv: &[std::ffi::OsString]) -> Result<i32> {
    use std::io::Read;
    use std::os::fd::FromRawFd;
    if fd < 3 || argv.is_empty() {
        bail!("Invalid execution gate")
    }
    let mut gate = unsafe { std::fs::File::from_raw_fd(fd) };
    let mut byte = [0];
    if gate.read_exact(&mut byte).is_err() || byte[0] != 1 {
        return Ok(125);
    }
    drop(gate);
    let error = std::process::Command::new(&argv[0]).args(&argv[1..]).exec();
    Err(error).context("Provider exec failed")
}
fn gated_command(
    original: &std::process::Command,
) -> Result<(std::process::Command, std::fs::File, std::os::fd::OwnedFd)> {
    use std::os::fd::FromRawFd;
    let mut fds = [0; 2];
    if unsafe { libc::pipe(fds.as_mut_ptr()) } != 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    let read = unsafe { std::os::fd::OwnedFd::from_raw_fd(fds[0]) };
    let write = unsafe { std::fs::File::from_raw_fd(fds[1]) };
    if unsafe { libc::fcntl(fds[1], libc::F_SETFD, libc::FD_CLOEXEC) } < 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    let mut command = std::process::Command::new(std::env::current_exe()?);
    command
        .args([
            std::ffi::OsString::from("exec-gate"),
            fds[0].to_string().into(),
            "--".into(),
        ])
        .arg(original.get_program())
        .args(original.get_args());
    if let Some(cwd) = original.get_current_dir() {
        command.current_dir(cwd);
    }
    for (key, value) in original.get_envs() {
        if let Some(value) = value {
            command.env(key, value);
        } else {
            command.env_remove(key);
        }
    }
    command.process_group(0);
    unsafe {
        command.pre_exec(|| {
            libc::signal(libc::SIGTTOU, libc::SIG_DFL);
            Ok(())
        });
    }
    Ok((command, write, read))
}
pub fn launch(store: &Store, id: Uuid, initial: Uuid) -> Result<i32> {
    let mut token = initial;
    loop {
        let state = store.read()?;
        let entry = state.entry(id)?.clone();
        let allowed = state.runs.get(&id).is_some_and(|r| {
            r.token == token
                && !r.consumed
                && !r.ended
                && r.boot == model::boot_id()
                && model::now().saturating_sub(r.created) < 30
        });
        if !allowed {
            return standby(store, id, None, 0);
        }
        let l = listener(token)?;
        let _socket = SocketGuard(token);
        let pid = std::process::id();
        let start = process::start_time(pid).context("Own process identity unavailable")?;
        store.update(|s| {
            let r = s
                .runs
                .get_mut(&id)
                .filter(|r| r.token == token && !r.consumed && !r.ended)
                .context("Launch authorization already consumed")?;
            r.consumed = true;
            r.pid = pid;
            r.start_time = start;
            let e = s
                .entries
                .iter_mut()
                .find(|e| e.id == id)
                .context("Unknown item")?;
            e.ever_started = true;
            e.imported = false;
            Ok(())
        })?;
        let terminal = TerminalGuard::new();
        let terminated = Arc::new(AtomicBool::new(false));
        let mut registrations = vec![];
        for sig in [libc::SIGTERM, libc::SIGHUP] {
            registrations.push(signal_hook::flag::register(sig, terminated.clone())?);
        }
        let result = (|| -> Result<i32> {
            let command = agents::command(store, &entry, token)?;
            let (mut command, mut gate, read) = gated_command(&command)?;
            let mut child = command.spawn().context("Provider gate launch failed")?;
            drop(read);
            let pgid = child.id() as i32;
            let establish = (|| -> Result<()> {
                let start = process::start_time(child.id())
                    .context("Child identity unavailable; provider gate remains closed")?;
                store.update(|s| {
                    let r = s
                        .runs
                        .get_mut(&id)
                        .filter(|r| r.token == token && !r.ended)
                        .context("Run changed before provider release")?;
                    r.child_pid = Some(child.id());
                    r.child_start = Some(start);
                    r.pgid = Some(pgid);
                    Ok(())
                })?;
                terminal.foreground(pgid);
                gate.write_all(&[1])?;
                Ok(())
            })();
            drop(gate);
            if let Err(error) = establish {
                let _ = child.wait();
                return Err(error);
            }
            unsafe {
                libc::kill(-pgid, libc::SIGCONT);
            }
            let mut stop_at = None;
            let status = loop {
                if let Some(status) = child.try_wait()? {
                    break status;
                }
                if let Some((mut stream, action)) = accept(&l, token) {
                    let current = store.read()?;
                    let response = if !current.runs.get(&id).is_some_and(|r| {
                        r.token == token && !r.ended && r.child_pid == Some(child.id())
                    }) {
                        Err(anyhow::anyhow!("Run identity changed; stop refused"))
                    } else if action == "stop" && entry.service.is_some() {
                        if stop_at.is_none() {
                            unsafe {
                                libc::kill(-pgid, libc::SIGTERM);
                            }
                            stop_at = Some(std::time::Instant::now());
                        }
                        Ok(())
                    } else {
                        Err(anyhow::anyhow!(
                            "Provider requires manual exit; use its own finish/cancel then quit"
                        ))
                    };
                    respond(&mut stream, response);
                }
                if terminated.load(Ordering::Relaxed) && stop_at.is_none() {
                    unsafe {
                        libc::kill(-pgid, libc::SIGTERM);
                    }
                    stop_at = Some(std::time::Instant::now());
                }
                std::thread::sleep(Duration::from_millis(100));
            };
            let remaining = members(pgid);
            if !remaining.is_empty() {
                unsafe {
                    libc::kill(-pgid, libc::SIGTERM);
                }
                for _ in 0..20 {
                    if members(pgid).is_empty() {
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(100));
                }
            }
            let survivors = members(pgid);
            store.update(|s| {
                if let Some(r) = s.runs.get_mut(&id).filter(|r| r.token == token) {
                    r.survivors = survivors;
                }
                Ok(())
            })?;
            Ok(status.code().unwrap_or(1))
        })();
        terminal.restore();
        for registration in registrations {
            signal_hook::low_level::unregister(registration);
        }
        // A timeout leaves a live child. Never call that parked or permit a duplicate.
        let live_child = store.read()?.runs.get(&id).is_some_and(|r| {
            r.child_pid
                .zip(r.child_start)
                .is_some_and(|(pid, start)| process::start_time(pid) == Some(start))
        });
        store.update(|s| {
            if let Some(r) = s.runs.get_mut(&id).filter(|r| r.token == token) {
                r.ended = !live_child;
                r.ended_at = (!live_child).then(model::now);
                r.agents.clear();
                r.last_error = result.as_ref().err().map(|e| format!("{e:#}"));
                r.exit_code = result.as_ref().ok().copied();
            }
            if !live_child {
                let e = s
                    .entries
                    .iter_mut()
                    .find(|e| e.id == id)
                    .context("Unknown item")?;
                e.intent = Intent::Parked;
            }
            Ok(())
        })?;
        if !live_child {
            crate::operations::cleanup_run_services(store, id, token)?;
        }
        let code = result.as_ref().ok().copied().unwrap_or(1);
        if !unsafe { libc::isatty(0) }.eq(&1) {
            return result;
        }
        match standby_loop(store, id, Some((&l, token)), code)? {
            Some(next) => token = next,
            None => return Ok(code),
        }
    }
}
fn standby(
    store: &Store,
    id: Uuid,
    listener: Option<(&UnixListener, Uuid)>,
    code: i32,
) -> Result<i32> {
    if unsafe { libc::isatty(0) } != 1 {
        return Ok(code);
    };
    if let Some(next) = standby_loop(store, id, listener, code)? {
        launch(store, id, next)
    } else {
        Ok(code)
    }
}
fn standby_loop(
    store: &Store,
    id: Uuid,
    listener: Option<(&UnixListener, Uuid)>,
    _code: i32,
) -> Result<Option<Uuid>> {
    use crossterm::event::{self, Event, KeyCode, KeyEventKind};
    let s = store.read()?;
    let e = s.entry(id)?;
    println!(
        "\r\n{} · {}\r\n{}\r\nConversation: {}\r\n{}\r\n[r] Resume  [d] Details  [w] Work register  [q] Close",
        e.name,
        e.agent,
        e.cwd.display(),
        e.session_id
            .map(|v| v.to_string())
            .unwrap_or("needs conversation binding".into()),
        if s.runs.get(&id).is_some_and(|r| r.live()) {
            "Execution still active · inspect before resuming"
        } else if s.runs.get(&id).is_some_and(|r| !r.survivors.is_empty()) {
            "Survivors recorded · inspect before resuming"
        } else if e.needs_session() {
            "Stopped · needs exact conversation binding"
        } else {
            "Stopped · resume checks saved directory, binding and dependencies"
        }
    );
    crossterm::terminal::enable_raw_mode()?;
    struct Raw;
    impl Drop for Raw {
        fn drop(&mut self) {
            let _ = crossterm::terminal::disable_raw_mode();
        }
    }
    let _raw = Raw;
    loop {
        if let Some((l, t)) = listener
            && let Some((mut stream, action)) = accept(l, t)
        {
            if action == "resume" || action.starts_with("resume:") {
                let result = (|| {
                    let operation = action
                        .strip_prefix("resume:")
                        .map(Uuid::parse_str)
                        .transpose()?;
                    crate::engine::authorize_launch_for(store, id, operation)
                })();
                match result {
                    Ok(next) => {
                        respond(&mut stream, Ok(()));
                        return Ok(Some(next));
                    }
                    Err(e) => respond(&mut stream, Err(e)),
                }
            } else {
                respond(
                    &mut stream,
                    Err(anyhow::anyhow!("Unsupported standby action")),
                );
            }
        }
        if event::poll(Duration::from_millis(250))?
            && let Event::Key(k) = event::read()?
            && k.kind == KeyEventKind::Press
        {
            match k.code {
                KeyCode::Char('q') => return Ok(None),
                KeyCode::Char('r') => match crate::engine::authorize_launch(store, id) {
                    Ok(next) => return Ok(Some(next)),
                    Err(e) => println!("\r\n{e}"),
                },
                KeyCode::Char('d') => println!("\r\n{}", crate::operations::inspect(store, id)?),
                KeyCode::Char('w') => {
                    let exe = std::env::current_exe()?;
                    let command = format!(
                        "{} --state-dir {} dashboard",
                        crate::ghostty::shell_quote(&exe.to_string_lossy()),
                        crate::ghostty::shell_quote(&store.dir.to_string_lossy())
                    );
                    let result = crate::ghostty::open(
                        &e.cwd.to_string_lossy(),
                        &command,
                        "Ghostty Workspaces",
                        None,
                    );
                    if let Err(err) = result {
                        println!("\r\n{err}");
                    }
                }
                _ => {}
            }
        }
    }
}
