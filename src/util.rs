use anyhow::{Result, bail};
use std::{
    io::{Read, Write},
    os::fd::AsRawFd,
    os::unix::process::CommandExt,
    process::{Command, Output, Stdio},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};
pub fn output(command: Command, timeout: Duration) -> Result<Output> {
    output_input(command, timeout, None)
}
/// Bound utility execution, pipes and output. Never use this as a work supervisor.
pub fn output_input(command: Command, timeout: Duration, input: Option<Vec<u8>>) -> Result<Output> {
    output_input_cancel(command, timeout, input, || false)
}
pub fn output_input_cancel(
    mut command: Command,
    timeout: Duration,
    input: Option<Vec<u8>>,
    cancel: impl Fn() -> bool,
) -> Result<Output> {
    command
        .stdin(if input.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0);
    let mut child = command.spawn()?;
    let done = Arc::new(AtomicBool::new(false));
    let stdout = child.stdout.take().unwrap();
    let stderr = child.stderr.take().unwrap();
    let out_done = done.clone();
    let err_done = done.clone();
    let out = std::thread::spawn(move || drain(stdout, out_done));
    let err = std::thread::spawn(move || drain(stderr, err_done));
    let writer = if let Some(data) = input {
        let mut stdin = child.stdin.take().unwrap();
        unsafe {
            let flags = libc::fcntl(stdin.as_raw_fd(), libc::F_GETFL);
            libc::fcntl(stdin.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK);
        }
        let writer_done = done.clone();
        Some(std::thread::spawn(move || {
            let mut offset = 0;
            while offset < data.len() && !writer_done.load(Ordering::Relaxed) {
                match stdin.write(&data[offset..]) {
                    Ok(0) => break,
                    Ok(n) => offset += n,
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(10))
                    }
                    Err(_) => break,
                }
            }
        }))
    } else {
        None
    };
    let t = Instant::now();
    let mut cancelled = false;
    let mut last_cancel = Instant::now() - Duration::from_secs(1);
    let status = loop {
        if let Some(s) = child.try_wait()? {
            break Some(s);
        }
        if last_cancel.elapsed() >= Duration::from_millis(250) {
            cancelled = cancel();
            last_cancel = Instant::now();
        }
        if t.elapsed() > timeout || cancelled {
            unsafe {
                libc::kill(-(child.id() as i32), libc::SIGKILL);
            }
            let _ = child.wait();
            break None;
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    done.store(true, Ordering::Relaxed);
    if let Some(writer) = writer {
        let _ = writer.join();
    }
    let stdout = out.join().unwrap_or_default();
    let stderr = err.join().unwrap_or_default();
    let Some(status) = status else {
        if cancelled {
            bail!("Temporary model utility cancelled for profiling pause")
        }
        bail!("Utility timed out after {} seconds", timeout.as_secs())
    };
    Ok(Output {
        status,
        stdout,
        stderr,
    })
}
fn drain(mut pipe: impl Read + AsRawFd, done: Arc<AtomicBool>) -> Vec<u8> {
    unsafe {
        let flags = libc::fcntl(pipe.as_raw_fd(), libc::F_GETFL);
        libc::fcntl(pipe.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK);
    }
    let mut data = vec![];
    let mut buffer = [0u8; 8192];
    loop {
        match pipe.read(&mut buffer) {
            Ok(0) => break,
            Ok(n) => {
                let remaining = (256 * 1024usize).saturating_sub(data.len());
                data.extend_from_slice(&buffer[..n.min(remaining)]);
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                if done.load(Ordering::Relaxed) {
                    break;
                };
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(_) => break,
        }
    }
    data
}

#[derive(Clone, serde::Serialize, serde::Deserialize)]
pub struct ModelJob {
    pub id: uuid::Uuid,
    pub pid: u32,
    pub start: u64,
    pub kind: String,
    pub ended: bool,
}
pub struct ModelJobGuard {
    store: crate::model::Store,
    job: ModelJob,
}
impl ModelJobGuard {
    pub fn start(store: &crate::model::Store, kind: &str) -> Result<Self> {
        let job = ModelJob {
            id: uuid::Uuid::new_v4(),
            pid: std::process::id(),
            start: crate::process::start_time(std::process::id())
                .ok_or_else(|| anyhow::anyhow!("Model job identity unavailable"))?,
            kind: kind.into(),
            ended: false,
        };
        store.edit_document("model_jobs", &job.id.to_string(), |_| {
            if store.paused() {
                bail!("Model work disabled during profiling pause")
            }
            Ok(job.clone())
        })?;
        Ok(Self {
            store: store.clone(),
            job,
        })
    }
}
impl Drop for ModelJobGuard {
    fn drop(&mut self) {
        self.job.ended = true;
        let _ = self
            .store
            .put("model_jobs", &self.job.id.to_string(), &self.job);
    }
}
pub fn active_model_jobs(store: &crate::model::Store) -> Result<Vec<ModelJob>> {
    Ok(store
        .documents::<ModelJob>("model_jobs")?
        .into_iter()
        .filter(|j| !j.ended && crate::process::start_time(j.pid) == Some(j.start))
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn bounded_pipes_do_not_wait_for_detached_descendants() {
        let mut c = Command::new("/bin/sh");
        c.args(["-c", "sleep 1 & printf done"]);
        let t = Instant::now();
        let out =
            output_input(c, Duration::from_millis(500), Some(vec![b'x'; 1024 * 1024])).unwrap();
        assert!(out.status.success());
        assert_eq!(out.stdout, b"done");
        assert!(t.elapsed() < Duration::from_millis(800));
    }
    #[test]
    fn temporary_model_utility_can_be_cancelled_and_reaped() {
        let mut c = Command::new("/bin/sleep");
        c.arg("20");
        let t = Instant::now();
        assert!(
            output_input_cancel(c, Duration::from_secs(30), None, || true)
                .unwrap_err()
                .to_string()
                .contains("cancelled")
        );
        assert!(t.elapsed() < Duration::from_secs(1));
    }
}
