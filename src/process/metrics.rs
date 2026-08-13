//! Host and AoE-agent resource sampling for the TUI system-health views.

use std::collections::{HashMap, HashSet};
use std::time::Instant;

use crate::session::{Instance, Status};

#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct MemorySample {
    pub total_bytes: u64,
    pub available_bytes: u64,
    pub psi_mem_some_avg10: Option<f32>,
    pub psi_io_some_avg10: Option<f32>,
    pub macos_pressure_level: Option<u8>,
}

impl MemorySample {
    pub fn used_bytes(&self) -> u64 {
        self.total_bytes.saturating_sub(self.available_bytes)
    }

    pub fn used_fraction(&self) -> f64 {
        if self.total_bytes > 0 {
            self.used_bytes() as f64 / self.total_bytes as f64
        } else {
            0.0
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct SystemSample {
    pub cpu_fraction: Option<f64>,
    pub load_average: Option<[f64; 3]>,
    pub swap_total_bytes: u64,
    pub swap_used_bytes: u64,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct AgentMetric {
    pub id: String,
    pub title: String,
    pub cpu_fraction: Option<f64>,
    pub rss_bytes: u64,
    pub procs: usize,
}

#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct AgentCounts {
    pub agents: usize,
    pub procs: usize,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct MetricsSnapshot {
    pub memory: MemorySample,
    pub system: SystemSample,
    pub counts: AgentCounts,
    pub agents: Vec<AgentMetric>,
}

#[derive(Debug, Clone)]
pub(super) struct ProcessRecord {
    pub(super) pid: u32,
    pub(super) ppid: u32,
    pub(super) start_id: u64,
    pub(super) rss_bytes: u64,
    pub(super) cpu_seconds: f64,
}

pub(super) type SystemReading = (
    Option<(u64, u64)>,
    Option<f64>,
    Option<[f64; 3]>,
    (u64, u64),
);

#[derive(Default)]
pub(crate) struct MetricsSampler {
    last_at: Option<Instant>,
    last_host_cpu: Option<(u64, u64)>,
    last_process_cpu: HashMap<(u32, u64), f64>,
}

impl MetricsSampler {
    pub(crate) fn sample(&mut self, instances: &[Instance]) -> MetricsSnapshot {
        let now = Instant::now();
        let elapsed = self.last_at.map(|at| now.duration_since(at).as_secs_f64());
        let memory = sample_memory();
        let (host_cpu, direct_cpu, load_average, swap) = sample_system();
        let cpu_fraction = direct_cpu.or_else(|| {
            self.last_host_cpu
                .zip(host_cpu)
                .and_then(|((old_total, old_idle), (total, idle))| {
                    let total_delta = total.checked_sub(old_total)?;
                    let idle_delta = idle.checked_sub(old_idle)?;
                    (total_delta > 0).then(|| {
                        (total_delta.saturating_sub(idle_delta)) as f64 / total_delta as f64
                    })
                })
        });

        let processes = process_snapshot();
        let agents = aggregate_agents(instances, &processes, elapsed, &self.last_process_cpu);
        let counts = AgentCounts {
            agents: agents.len(),
            procs: agents.iter().map(|a| a.procs).sum(),
        };

        self.last_at = Some(now);
        self.last_host_cpu = host_cpu;
        self.last_process_cpu = processes
            .iter()
            .map(|p| ((p.pid, p.start_id), p.cpu_seconds))
            .collect();

        MetricsSnapshot {
            memory,
            system: SystemSample {
                cpu_fraction,
                load_average,
                swap_total_bytes: swap.0,
                swap_used_bytes: swap.1,
            },
            counts,
            agents,
        }
    }
}

fn eligible_instance(inst: &Instance) -> bool {
    !inst.is_structured()
        && !inst.is_archived()
        && !inst.is_trashed()
        && !inst.is_snoozed()
        && matches!(
            inst.status,
            Status::Running | Status::Waiting | Status::Idle
        )
}

fn aggregate_agents(
    instances: &[Instance],
    processes: &[ProcessRecord],
    elapsed: Option<f64>,
    previous: &HashMap<(u32, u64), f64>,
) -> Vec<AgentMetric> {
    let by_parent: HashMap<u32, Vec<u32>> = {
        let mut map: HashMap<u32, Vec<u32>> = HashMap::new();
        for p in processes {
            map.entry(p.ppid).or_default().push(p.pid);
        }
        map
    };
    let by_pid: HashMap<u32, &ProcessRecord> = processes.iter().map(|p| (p.pid, p)).collect();
    let mut claimed = HashSet::new();
    let mut rows = Vec::new();

    let eligible: Vec<Instance> = instances
        .iter()
        .filter(|i| eligible_instance(i))
        .cloned()
        .collect();
    let alive = crate::session::recovery::orphaned_agents_alive(&eligible);
    for (inst, is_alive) in eligible.iter().zip(alive) {
        if !is_alive {
            continue;
        }
        let session_name = crate::tmux::Session::resolve_name(&inst.id, &inst.title);
        let Some(root) = crate::process::get_pane_pid(&session_name) else {
            continue;
        };
        if !by_pid.contains_key(&root) {
            continue;
        }
        let mut stack = vec![root];
        let mut pids = Vec::new();
        while let Some(pid) = stack.pop() {
            if !claimed.insert(pid) {
                continue;
            }
            pids.push(pid);
            if let Some(children) = by_parent.get(&pid) {
                stack.extend(children);
            }
        }
        let rss_bytes = pids
            .iter()
            .filter_map(|pid| by_pid.get(pid))
            .map(|p| p.rss_bytes)
            .sum();
        let cpu_fraction = elapsed.filter(|v| *v > 0.0).map(|seconds| {
            pids.iter()
                .filter_map(|pid| by_pid.get(pid))
                .filter_map(|p| {
                    let old = previous.get(&(p.pid, p.start_id))?;
                    Some((p.cpu_seconds - old).max(0.0))
                })
                .sum::<f64>()
                / seconds
                / logical_cpus() as f64
        });
        rows.push(AgentMetric {
            id: inst.id.clone(),
            title: inst.title.clone(),
            cpu_fraction,
            rss_bytes,
            procs: pids.len(),
        });
    }

    #[cfg(feature = "serve")]
    for rec in crate::process::worker_registry::list()
        .unwrap_or_default()
        .into_iter()
        .filter(crate::process::worker_registry::is_record_live)
    {
        let Some(root) = by_pid.get(&rec.pid) else {
            continue;
        };
        if !claimed.insert(root.pid) {
            continue;
        }
        let mut stack = vec![root.pid];
        let mut pids = Vec::new();
        while let Some(pid) = stack.pop() {
            if pid != root.pid && !claimed.insert(pid) {
                continue;
            }
            pids.push(pid);
            if let Some(children) = by_parent.get(&pid) {
                stack.extend(children);
            }
        }
        let rss_bytes = pids
            .iter()
            .filter_map(|pid| by_pid.get(pid))
            .map(|p| p.rss_bytes)
            .sum();
        let cpu_fraction = elapsed.filter(|v| *v > 0.0).map(|seconds| {
            pids.iter()
                .filter_map(|pid| by_pid.get(pid))
                .filter_map(|p| {
                    let old = previous.get(&(p.pid, p.start_id))?;
                    Some((p.cpu_seconds - old).max(0.0))
                })
                .sum::<f64>()
                / seconds
                / logical_cpus() as f64
        });
        let title = instances
            .iter()
            .find(|i| i.id == rec.session_id)
            .map(|i| i.title.clone())
            .unwrap_or_else(|| rec.agent_name.clone());
        rows.push(AgentMetric {
            id: rec.session_id,
            title,
            cpu_fraction,
            rss_bytes,
            procs: pids.len(),
        });
    }

    rows.sort_by(|a, b| {
        b.cpu_fraction
            .partial_cmp(&a.cpu_fraction)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.id.cmp(&b.id))
    });
    rows
}

fn logical_cpus() -> usize {
    std::thread::available_parallelism()
        .map(usize::from)
        .unwrap_or(1)
}

pub(crate) fn sample_memory() -> MemorySample {
    #[cfg(target_os = "linux")]
    {
        super::linux::sample_memory()
    }
    #[cfg(target_os = "macos")]
    {
        super::macos::sample_memory()
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        MemorySample::default()
    }
}

fn sample_system() -> SystemReading {
    #[cfg(target_os = "linux")]
    {
        super::linux::sample_system()
    }
    #[cfg(target_os = "macos")]
    {
        super::macos::sample_system()
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        (None, None, None, (0, 0))
    }
}

fn process_snapshot() -> Vec<ProcessRecord> {
    #[cfg(target_os = "linux")]
    {
        super::linux::process_snapshot()
    }
    #[cfg(target_os = "macos")]
    {
        super::macos::process_snapshot()
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        Vec::new()
    }
}
