use crate::model::Run;
use serde::Serialize;
use std::collections::{BTreeMap, HashMap, HashSet};
use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System};
#[derive(Debug, Clone, Serialize)]
pub struct Proc {
    pub pid: u32,
    pub parent: Option<u32>,
    pub name: String,
    pub cpu: f32,
    pub memory: u64,
    pub start_time: u64,
    pub footprint: Option<u64>,
    pub state: String,
}
#[derive(Debug, Clone, Default, Serialize)]
pub struct Metrics {
    pub cpu: f32,
    pub memory: u64,
    pub processes: Vec<Proc>,
    pub footprint: Option<u64>,
    pub footprint_coverage: usize,
}
#[derive(Debug, Clone, Serialize)]
pub struct NameGroup {
    pub name: String,
    pub metrics: Metrics,
}
/// Groups are by executable name, not inferred application ownership.
pub fn name_groups(all: &BTreeMap<u32, Proc>) -> Vec<NameGroup> {
    let mut groups: BTreeMap<String, Metrics> = BTreeMap::new();
    for p in all.values() {
        let m = groups.entry(p.name.clone()).or_default();
        m.cpu += p.cpu;
        m.memory += p.memory;
        m.processes.push(p.clone());
        if let Some(footprint) = p.footprint {
            m.footprint = Some(m.footprint.unwrap_or(0) + footprint);
            m.footprint_coverage += 1;
        }
    }
    groups
        .into_iter()
        .map(|(name, metrics)| NameGroup { name, metrics })
        .collect()
}
pub struct Monitor {
    system: System,
}
impl Default for Monitor {
    fn default() -> Self {
        Self::new()
    }
}
impl Monitor {
    pub fn new() -> Self {
        Self {
            system: System::new(),
        }
    }
    pub fn refresh(&mut self) -> BTreeMap<u32, Proc> {
        self.system.refresh_processes_specifics(
            ProcessesToUpdate::All,
            true,
            ProcessRefreshKind::nothing().with_cpu().with_memory(),
        );
        self.system
            .processes()
            .iter()
            .map(|(pid, p)| {
                (
                    pid.as_u32(),
                    Proc {
                        pid: pid.as_u32(),
                        parent: p.parent().map(|v| v.as_u32()),
                        name: p.name().to_string_lossy().into_owned(),
                        cpu: p.cpu_usage(),
                        memory: p.memory(),
                        start_time: p.start_time(),
                        footprint: footprint(pid.as_u32()),
                        state: p.status().to_string(),
                    },
                )
            })
            .collect()
    }
    pub fn total_memory(&mut self) -> (u64, u64) {
        self.system.refresh_memory();
        (self.system.used_memory(), self.system.total_memory())
    }
}
pub fn start_time(pid: u32) -> Option<u64> {
    let mut system = System::new();
    system.refresh_processes(ProcessesToUpdate::Some(&[Pid::from_u32(pid)]), true);
    system.process(Pid::from_u32(pid)).map(|p| p.start_time())
}
pub fn tree(run: &Run, all: &BTreeMap<u32, Proc>) -> Option<Metrics> {
    let root = all
        .get(&run.pid)
        .filter(|p| !run.ended && p.start_time == run.start_time)
        .or_else(|| {
            run.child_pid
                .zip(run.child_start)
                .and_then(|(pid, start)| all.get(&pid).filter(|p| p.start_time == start))
        })?;
    let mut children: HashMap<u32, Vec<u32>> = HashMap::new();
    for p in all.values() {
        if let Some(parent) = p.parent {
            children.entry(parent).or_default().push(p.pid);
        }
    }
    let mut stack = vec![root.pid];
    let mut seen = HashSet::new();
    let mut metrics = Metrics::default();
    while let Some(pid) = stack.pop() {
        if !seen.insert(pid) {
            continue;
        }
        if let Some(p) = all.get(&pid) {
            metrics.cpu += p.cpu;
            metrics.memory += p.memory;
            if let Some(f) = p.footprint {
                metrics.footprint = Some(metrics.footprint.unwrap_or(0) + f);
                metrics.footprint_coverage += 1;
            }
            metrics.processes.push(p.clone());
        }
        if let Some(kids) = children.get(&pid) {
            stack.extend(kids);
        }
    }
    Some(metrics)
}
#[cfg(test)]
mod tests {
    use super::*;
    use uuid::Uuid;
    fn run() -> Run {
        Run {
            token: Uuid::new_v4(),
            pid: 10,
            start_time: 100,
            terminal_id: None,
            ended: false,
            hooks_seen: false,
            agents: BTreeMap::new(),
            agents_started: 0,
            last_error: None,
            exit_code: None,
            ..Run::default()
        }
    }
    fn proc(pid: u32, parent: Option<u32>, start: u64) -> Proc {
        Proc {
            pid,
            parent,
            start_time: start,
            name: "test".into(),
            cpu: 2.,
            memory: 100,
            footprint: None,
            state: "unknown".into(),
        }
    }
    #[test]
    fn sums_only_owned_descendants_and_survives_cycles() {
        let all = [
            proc(10, Some(11), 100),
            proc(11, Some(10), 101),
            proc(12, Some(11), 102),
            proc(99, None, 1),
        ]
        .into_iter()
        .map(|p| (p.pid, p))
        .collect();
        let m = tree(&run(), &all).unwrap();
        assert_eq!(m.processes.len(), 3);
        assert_eq!(m.cpu, 6.);
        assert_eq!(m.memory, 300);
    }
    #[test]
    fn rejects_reused_pid_and_ended_launcher() {
        let all = [proc(10, None, 101)]
            .into_iter()
            .map(|p| (p.pid, p))
            .collect();
        assert!(tree(&run(), &all).is_none());
        let mut r = run();
        r.ended = true;
        assert!(tree(&r, &all).is_none());
    }
}

/// Charged process footprint includes compressed/swapped charges; not a RAM partition.
#[cfg(target_os = "macos")]
pub fn footprint(pid: u32) -> Option<u64> {
    #[link(name = "proc")]
    unsafe extern "C" {
        fn proc_pid_rusage(
            pid: libc::c_int,
            flavor: libc::c_int,
            buffer: *mut libc::c_void,
        ) -> libc::c_int;
    }
    let mut buffer = [0u64; 128];
    if unsafe { proc_pid_rusage(pid as i32, 1, buffer.as_mut_ptr().cast()) } == 0 {
        Some(buffer[9])
    } else {
        None
    }
}
#[cfg(not(target_os = "macos"))]
pub fn footprint(_pid: u32) -> Option<u64> {
    None
}
#[derive(Debug, Default, Clone, Serialize)]
pub struct Memory {
    pub used_bytes: u64,
    pub total_bytes: u64,
    pub swap_used_bytes: u64,
    pub compressed_bytes: Option<u64>,
    pub pressure: Option<String>,
    pub swap_in_bytes_per_second: Option<f64>,
    pub swap_out_bytes_per_second: Option<f64>,
    pub source: String,
    pub observed_at: u64,
}
pub fn memory_snapshot() -> Memory {
    let mut sys = System::new();
    sys.refresh_memory();
    let mut m = Memory {
        used_bytes: sys.used_memory(),
        total_bytes: sys.total_memory(),
        swap_used_bytes: sys.used_swap(),
        source: "sysinfo + native VM counters".into(),
        observed_at: crate::model::now(),
        ..Default::default()
    };
    #[cfg(target_os = "macos")]
    {
        m.pressure = native_pressure();
        m.compressed_bytes = native_vm().map(|v| v.0);
    }
    m
}
#[cfg(target_os = "macos")]
fn native_pressure() -> Option<String> {
    let name = c"kern.memorystatus_vm_pressure_level";
    let mut value = 0i32;
    let mut size = std::mem::size_of::<i32>();
    if unsafe {
        libc::sysctlbyname(
            name.as_ptr(),
            (&mut value as *mut i32).cast(),
            &mut size,
            std::ptr::null_mut(),
            0,
        )
    } != 0
    {
        return None;
    };
    match value {
        1 => Some("normal".into()),
        2 => Some("warning".into()),
        4 => Some("critical".into()),
        _ => None,
    }
}
#[cfg(target_os = "macos")]
fn native_vm() -> Option<(u64, u64, u64)> {
    #[repr(C)]
    #[derive(Default)]
    struct VmStats {
        pages: [u32; 4],
        counters: [u64; 9],
        purgeable: u32,
        speculative: u32,
        decompressions: u64,
        compressions: u64,
        swapins: u64,
        swapouts: u64,
        compressor: u32,
        throttled: u32,
        external: u32,
        internal: u32,
        uncompressed: u64,
        swapped: u64,
    }
    unsafe extern "C" {
        fn mach_host_self() -> u32;
        fn host_statistics64(host: u32, flavor: i32, info: *mut i32, count: *mut u32) -> i32;
        fn mach_task_self() -> u32;
        fn mach_port_deallocate(task: u32, name: u32) -> i32;
    }
    let mut stats = VmStats::default();
    let mut count = (std::mem::size_of::<VmStats>() / 4) as u32;
    let host = unsafe { mach_host_self() };
    let result =
        unsafe { host_statistics64(host, 4, (&mut stats as *mut VmStats).cast(), &mut count) };
    unsafe {
        mach_port_deallocate(mach_task_self(), host);
    }
    if result != 0 || count < 38 {
        return None;
    };
    let page = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
    if page <= 0 {
        return None;
    };
    Some((
        stats.compressor as u64 * page as u64,
        stats.swapins * page as u64,
        stats.swapouts * page as u64,
    ))
}
pub struct MemorySampler {
    previous: Option<(std::time::Instant, u64, u64)>,
}
impl Default for MemorySampler {
    fn default() -> Self {
        Self::new()
    }
}
impl MemorySampler {
    pub fn new() -> Self {
        Self { previous: None }
    }
    pub fn sample(&mut self) -> Memory {
        let mut m = memory_snapshot();
        #[cfg(target_os = "macos")]
        if let Some((_, ins, outs)) = native_vm() {
            let t = std::time::Instant::now();
            if let Some((old, a, b)) = self.previous {
                let dt = t.duration_since(old).as_secs_f64();
                if dt > 0.0 {
                    m.swap_in_bytes_per_second = Some(ins.saturating_sub(a) as f64 / dt);
                    m.swap_out_bytes_per_second = Some(outs.saturating_sub(b) as f64 / dt);
                }
            }
            self.previous = Some((t, ins, outs));
        }
        m
    }
}
