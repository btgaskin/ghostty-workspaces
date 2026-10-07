//! Optional streaming collectors. No server, credentials, or remote writes.
use anyhow::{Context, Result, bail};
use serde::Serialize;
use serde_json::Value;
use std::{
    collections::VecDeque,
    io::{BufRead, BufReader, Read},
    path::PathBuf,
    process::{Child, Command, Stdio},
    sync::{Arc, Mutex},
    thread,
    time::{Duration, Instant},
};

const REMOTE_COMMAND: &str = "if command -v macmon >/dev/null 2>&1; then exec macmon pipe --interval 1000; elif [ -x /opt/homebrew/bin/macmon ]; then exec /opt/homebrew/bin/macmon pipe --interval 1000; elif [ -x /usr/local/bin/macmon ]; then exec /usr/local/bin/macmon pipe --interval 1000; else echo 'macmon is not installed on this host' >&2; exit 127; fi";

#[derive(Clone, Debug, Serialize)]
pub struct Load {
    pub mhz: f64,
    pub ratio: f64,
    pub weighted: bool,
}
#[derive(Clone, Debug, Serialize)]
pub struct Sensors {
    pub ram_used: u64,
    pub ram_total: u64,
    pub swap_used: u64,
    pub swap_total: u64,
    pub efficiency: Load,
    pub performance: Load,
    pub gpu: Load,
    pub cpu_temp: Option<f64>,
    pub gpu_temp: Option<f64>,
    pub system_power: Option<f64>,
    pub cpu_power: Option<f64>,
    pub gpu_power: Option<f64>,
    pub ane_power: Option<f64>,
}
fn number(v: &Value) -> Option<f64> {
    v.as_f64().filter(|n| n.is_finite())
}
fn load(v: &Value, prefix: &str) -> Result<Load> {
    // 0.8 exposes active and frequency-scaled ratios separately. Earlier pipe
    // schemas expose [frequency, scaled ratio]; never label this as active time.
    let old = &v[format!("{prefix}_usage")];
    let mhz = number(&v[format!("{prefix}_freq_mhz")])
        .or_else(|| number(&old[0]))
        .context("Missing frequency")?;
    let active = number(&v[format!("{prefix}_active_ratio")]);
    let ratio = active
        .or_else(|| number(&v[format!("{prefix}_scaled_ratio")]))
        .or_else(|| number(&old[1]))
        .context("Missing load ratio")?;
    if mhz < 0. || !(0. ..=1.).contains(&ratio) {
        bail!("Invalid sensor frequency or ratio");
    }
    Ok(Load {
        mhz,
        ratio,
        weighted: active.is_none(),
    })
}
pub fn parse(line: &[u8]) -> Result<Sensors> {
    let v: Value = serde_json::from_slice(line).context("Invalid macmon JSON")?;
    let m = &v["memory"];
    let ram_used = m["ram_usage"].as_u64().context("Missing RAM usage")?;
    let ram_total = m["ram_total"].as_u64().context("Missing RAM total")?;
    if ram_total == 0 || ram_used > ram_total {
        bail!("Invalid RAM counters");
    }
    Ok(Sensors {
        ram_used,
        ram_total,
        swap_used: m["swap_usage"].as_u64().unwrap_or(0),
        swap_total: m["swap_total"].as_u64().unwrap_or(0),
        efficiency: load(&v, "ecpu")?,
        performance: load(&v, "pcpu")?,
        gpu: load(&v, "gpu")?,
        cpu_temp: number(&v["temp"]["cpu_temp_avg"]),
        gpu_temp: number(&v["temp"]["gpu_temp_avg"]),
        system_power: number(&v["sys_power"]),
        cpu_power: number(&v["cpu_power"]),
        gpu_power: number(&v["gpu_power"]),
        ane_power: number(&v["ane_power"]),
    })
}
#[derive(Clone, Default)]
pub struct Host {
    pub label: String,
    pub sensors: Option<Sensors>,
    pub updated: Option<Instant>,
    pub error: Option<String>,
    pub history: [VecDeque<u64>; 4],
    pub sample_times: VecDeque<Instant>,
}
impl Host {
    pub fn live(&self) -> bool {
        self.error.is_none()
            && self
                .updated
                .is_some_and(|t| t.elapsed() < Duration::from_secs(5))
    }
    fn receive(&mut self, sensors: Sensors) {
        let now = Instant::now();
        if self.sample_times.len() == 60 {
            self.sample_times.pop_front();
        }
        self.sample_times.push_back(now);
        let values = [
            sensors.ram_used as f64 / sensors.ram_total as f64,
            sensors.efficiency.ratio,
            sensors.performance.ratio,
            sensors.gpu.ratio,
        ];
        for (history, value) in self.history.iter_mut().zip(values) {
            if history.len() == 60 {
                history.pop_front();
            }
            history.push_back((value * 100.) as u64);
        }
        self.sensors = Some(sensors);
        self.updated = Some(now);
        self.error = None;
    }
}
pub fn validate_hosts(hosts: &[String]) -> Result<()> {
    for h in hosts {
        if h.is_empty()
            || h.starts_with('-')
            || !h
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || "@._-:[]".contains(c))
        {
            bail!("SSH host must be a host alias or user@host, without options or whitespace");
        }
    }
    if hosts.len() > 3 {
        bail!("At most three remote monitor streams are supported");
    }
    Ok(())
}
pub fn local_binary() -> Option<PathBuf> {
    let mut paths: Vec<_> = std::env::var_os("PATH")
        .map(|p| {
            std::env::split_paths(&p)
                .map(|p| p.join("macmon"))
                .collect()
        })
        .unwrap_or_default();
    paths.extend([
        PathBuf::from("/opt/homebrew/bin/macmon"),
        PathBuf::from("/usr/local/bin/macmon"),
    ]);
    paths.into_iter().find(|p| p.is_file())
}
pub fn remote_command(host: &str) -> Command {
    let mut command = Command::new("/usr/bin/ssh");
    command.args([
        "-T",
        "-oBatchMode=yes",
        "-oConnectTimeout=8",
        "-oStrictHostKeyChecking=yes",
        "-oServerAliveInterval=5",
        "-oServerAliveCountMax=1",
        host,
        REMOTE_COMMAND,
    ]);
    command
}
struct Collector {
    state: Arc<Mutex<Host>>,
    child: Option<Arc<Mutex<Child>>>,
    readers: Vec<thread::JoinHandle<()>>,
}
impl Collector {
    fn spawn(label: String, command: Result<Command>) -> Self {
        let state = Arc::new(Mutex::new(Host {
            label,
            ..Host::default()
        }));
        let result = command.and_then(|mut c| {
            c.stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .context("Cannot start sampler")
        });
        let mut child = match result {
            Ok(c) => c,
            Err(e) => {
                state.lock().unwrap().error = Some(e.to_string());
                return Self {
                    state,
                    child: None,
                    readers: vec![],
                };
            }
        };
        let stdout = child.stdout.take().unwrap();
        let stderr = child.stderr.take().unwrap();
        let errors = Arc::new(Mutex::new(String::new()));
        let stderr_errors = errors.clone();
        let stderr_thread = thread::spawn(move || {
            let mut reader = BufReader::new(stderr);
            let mut bytes = [0u8; 512];
            while let Ok(n) = reader.read(&mut bytes) {
                if n == 0 {
                    break;
                }
                let mut text = stderr_errors.lock().unwrap();
                if text.len() < 4096 {
                    text.extend(
                        String::from_utf8_lossy(&bytes[..n])
                            .chars()
                            .filter(|c| !c.is_control() || *c == '\n'),
                    );
                }
            }
        });
        let stream_state = state.clone();
        let stdout_thread = thread::spawn(move || {
            let mut reader = BufReader::new(stdout);
            loop {
                let mut line = Vec::new();
                let result = reader.by_ref().take(65536).read_until(b'\n', &mut line);
                match result {
                    Ok(0) => break,
                    Ok(_) if line.len() >= 65536 => {
                        stream_state.lock().unwrap().error =
                            Some("Sampler line exceeds 64 KiB".into());
                        return;
                    }
                    Ok(_) => match parse(&line) {
                        Ok(s) => stream_state.lock().unwrap().receive(s),
                        Err(e) => stream_state.lock().unwrap().error = Some(e.to_string()),
                    },
                    Err(e) => {
                        stream_state.lock().unwrap().error = Some(e.to_string());
                        return;
                    }
                }
            }
            // stderr drains independently; errors are advisory, never a blocking wait.
            let detail = errors.lock().unwrap().trim().to_owned();
            stream_state.lock().unwrap().error = Some(if detail.is_empty() {
                "Sampler disconnected".into()
            } else {
                detail
            });
        });
        Self {
            state,
            child: Some(Arc::new(Mutex::new(child))),
            readers: vec![stderr_thread, stdout_thread],
        }
    }
}
impl Drop for Collector {
    fn drop(&mut self) {
        if let Some(child) = &self.child {
            let mut child = child.lock().unwrap();
            let _ = child.kill();
            let _ = child.wait();
        }
        for reader in self.readers.drain(..) {
            let _ = reader.join();
        }
    }
}
pub struct Monitors {
    collectors: Vec<Collector>,
}
impl Monitors {
    pub fn start(hosts: &[String]) -> Result<Self> {
        validate_hosts(hosts)?;
        let local = local_binary()
            .context("Install macmon for E/P CPU, GPU, power, and temperature sensors")
            .map(|p| {
                let mut c = Command::new(p);
                c.args(["pipe", "--interval", "1000"]);
                c
            });
        let mut collectors = vec![Collector::spawn("This Mac".into(), local)];
        for host in hosts {
            collectors.push(Collector::spawn(host.clone(), Ok(remote_command(host))));
        }
        Ok(Self { collectors })
    }
    pub fn snapshot(&self) -> Vec<Host> {
        self.collectors
            .iter()
            .map(|c| c.state.lock().unwrap().clone())
            .collect()
    }
}
pub fn print_samples(hosts: &[String]) -> Result<i32> {
    let monitors = Monitors::start(hosts)?;
    let start = Instant::now();
    loop {
        let samples = monitors.snapshot();
        if samples
            .iter()
            .all(|s| s.sensors.is_some() || s.error.is_some())
            || start.elapsed() > Duration::from_secs(12)
        {
            let values: Vec<_> = samples.iter().map(|s| serde_json::json!({"host": s.label, "live": s.live(), "age_seconds": s.updated.map(|t| t.elapsed().as_secs_f64()), "error": s.error, "sensors": s.sensors})).collect();
            println!("{}", serde_json::to_string_pretty(&values)?);
            return Ok(if samples.iter().all(Host::live) { 0 } else { 1 });
        }
        thread::sleep(Duration::from_millis(100));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn fixture() -> Value {
        serde_json::json!({"memory":{"ram_total":100,"ram_usage":60,"swap_usage":2,"swap_total":10},"ecpu_usage":[3000,0.2],"pcpu_usage":[2500,0.1],"gpu_usage":[300,0.05]})
    }
    #[test]
    fn supports_both_schemas_without_confusing_scaled_and_active_load() {
        let mut v = fixture();
        let old = parse(&serde_json::to_vec(&v).unwrap()).unwrap();
        assert!(old.efficiency.weighted);
        v["ecpu_freq_mhz"] = serde_json::json!(3100);
        v["ecpu_active_ratio"] = serde_json::json!(0.8);
        v["ecpu_scaled_ratio"] = serde_json::json!(0.4);
        let new = parse(&serde_json::to_vec(&v).unwrap()).unwrap();
        assert!(!new.efficiency.weighted);
        assert_eq!(new.efficiency.ratio, 0.8);
        v["ecpu_active_ratio"] = serde_json::json!(8);
        assert!(parse(&serde_json::to_vec(&v).unwrap()).is_err());
    }
    #[test]
    fn refuses_ssh_options_and_keeps_host_out_of_remote_shell_text() {
        for host in ["-oProxyCommand=bad", "host;bad", "host\n", "user host"] {
            assert!(validate_hosts(&[host.into()]).is_err());
        }
        validate_hosts(&["bird@mini.tailnet.ts.net".into()]).unwrap();
        let command = remote_command("mini");
        let args: Vec<_> = command.get_args().map(|a| a.to_string_lossy()).collect();
        assert_eq!(args[args.len() - 2], "mini");
        assert_eq!(args.last().unwrap(), REMOTE_COMMAND);
    }
    #[test]
    fn stale_samples_are_not_live_and_history_is_bounded() {
        let mut host = Host::default();
        for _ in 0..100 {
            host.receive(parse(&serde_json::to_vec(&fixture()).unwrap()).unwrap());
        }
        assert_eq!(host.history[0].len(), 60);
        assert_eq!(host.sample_times.len(), 60);
        assert!(host.live());
        host.updated = Some(Instant::now() - Duration::from_secs(6));
        assert!(!host.live());
    }
    #[test]
    fn collector_is_terminated_and_reaped_on_drop() {
        let mut c = Command::new("/bin/sh");
        c.args(["-c", "exec sleep 30"]);
        let collector = Collector::spawn("fixture".into(), Ok(c));
        let child = collector.child.as_ref().unwrap().clone();
        drop(collector);
        assert!(child.lock().unwrap().try_wait().unwrap().is_some());
    }
}
