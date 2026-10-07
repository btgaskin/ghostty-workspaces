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
}
#[derive(Debug, Clone, Default, Serialize)]
pub struct Metrics {
    pub cpu: f32,
    pub memory: u64,
    pub processes: Vec<Proc>,
}
pub struct Monitor {
    system: System,
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
    let root = all.get(&run.pid)?;
    if run.ended || root.start_time != run.start_time {
        return None;
    }
    let mut children: HashMap<u32, Vec<u32>> = HashMap::new();
    for p in all.values() {
        if let Some(parent) = p.parent {
            children.entry(parent).or_default().push(p.pid);
        }
    }
    let mut stack = vec![run.pid];
    let mut seen = HashSet::new();
    let mut metrics = Metrics::default();
    while let Some(pid) = stack.pop() {
        if !seen.insert(pid) {
            continue;
        }
        if let Some(p) = all.get(&pid) {
            metrics.cpu += p.cpu;
            metrics.memory += p.memory;
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
