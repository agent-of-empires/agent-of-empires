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

/// One agent's resource usage. Every figure is optional because a sandboxed
/// agent's numbers come from the container runtime, which may not have a
/// sample yet (or at all, on a runtime without a stats command); reporting
/// unknown is the honest reading, a zero would not be.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct AgentMetric {
    pub id: String,
    pub title: String,
    pub cpu_fraction: Option<f64>,
    pub rss_bytes: Option<u64>,
    pub procs: Option<usize>,
    /// Measured inside a sandbox container rather than on the host process
    /// tree. The memory figure is then the container's usage, not an RSS sum.
    pub sandboxed: bool,
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
            procs: agents.iter().filter_map(|a| a.procs).sum(),
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

/// One row's figures: `(cpu_fraction, memory_bytes, procs)`.
type RowFigures = (Option<f64>, Option<u64>, Option<usize>);

/// The sandbox container backing `inst`, if it has one. Both agent populations
/// route through this, so a tmux pane and a structured worker cannot disagree
/// about whether a session is sandboxed.
fn sandbox_container_name(inst: &Instance) -> Option<&str> {
    inst.sandbox_info
        .as_ref()
        .filter(|s| s.enabled)
        .map(|s| s.container_name.as_str())
}

/// Figures as the container runtime reports them, or all-unknown when it has
/// no sample for this container: a cold cache, a stopped container, or a
/// runtime with no stats command. Unknown renders "?"; a zero would read as a
/// measured idle.
fn container_figures(stats: &crate::containers::stats::StatsMap, name: &str) -> RowFigures {
    match stats.get(name) {
        Some(stats) => (
            // `cpu_percent` is per-core (100 == one core saturated); the table
            // reads as a share of the whole host, like the host rows.
            Some(stats.cpu_percent / 100.0 / logical_cpus() as f64),
            Some(stats.mem_used_bytes),
            Some(stats.pids),
        ),
        None => (None, None, None),
    }
}

/// Figures summed over a host process tree.
fn host_figures(
    pids: &[u32],
    by_pid: &HashMap<u32, &ProcessRecord>,
    elapsed: Option<f64>,
    previous: &HashMap<(u32, u64), f64>,
) -> RowFigures {
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
    (cpu_fraction, Some(rss_bytes), Some(pids.len()))
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
    #[cfg(feature = "serve")]
    let worker_records: Vec<crate::process::worker_registry::WorkerRecord> =
        crate::process::worker_registry::list()
            .unwrap_or_default()
            .into_iter()
            .filter(crate::process::worker_registry::is_record_live)
            .collect();

    // Structured sessions are excluded from `eligible`, so their sandboxes
    // have to be counted here or a host whose only sandbox runs under a
    // structured worker would never fetch the map its row needs.
    #[cfg(feature = "serve")]
    let sandboxed_worker = worker_records.iter().any(|rec| {
        instances
            .iter()
            .any(|i| i.id == rec.session_id && sandbox_container_name(i).is_some())
    });
    #[cfg(not(feature = "serve"))]
    let sandboxed_worker = false;

    // Skip the runtime's stats pass unless a sandbox session is loaded;
    // `cached_stats` then hands back the last completed map without blocking.
    // Not gated on the health surface being open: the sampler also runs for
    // the compact strip and for the undiscovered-tip check, so a host with a
    // sandbox pays one refresh per TTL whenever the TUI is sampling at all.
    let container_stats = (eligible.iter().any(|i| sandbox_container_name(i).is_some())
        || sandboxed_worker)
        .then(crate::containers::stats::cached_stats)
        .unwrap_or_default();
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
        // The pid tree is still walked for a sandboxed session, so its pane
        // processes are claimed and cannot be double-counted onto a neighbour,
        // but its figures come from the container instead: the pane holds a
        // `docker exec` client, while the agent runs in the container, off
        // this process tree entirely.
        let sandbox_container = sandbox_container_name(inst);
        let (cpu_fraction, rss_bytes, procs) = match sandbox_container {
            Some(name) => container_figures(&container_stats, name),
            None => host_figures(&pids, &by_pid, elapsed, previous),
        };
        rows.push(AgentMetric {
            id: inst.id.clone(),
            title: inst.title.clone(),
            cpu_fraction,
            rss_bytes,
            procs,
            sandboxed: sandbox_container.is_some(),
        });
    }

    #[cfg(feature = "serve")]
    for rec in worker_records {
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
        // A sandboxed structured session wraps its agent in `docker exec` just
        // as a tmux pane does (see `spawn_runner_detached`), so the host tree
        // here is the runner shim plus that client, not the agent.
        let inst = instances.iter().find(|i| i.id == rec.session_id);
        let sandbox_container = inst.and_then(sandbox_container_name);
        let (cpu_fraction, rss_bytes, procs) = match sandbox_container {
            Some(name) => container_figures(&container_stats, name),
            None => host_figures(&pids, &by_pid, elapsed, previous),
        };
        let title = inst
            .map(|i| i.title.clone())
            .unwrap_or_else(|| rec.agent_name.clone());
        rows.push(AgentMetric {
            id: rec.session_id,
            title,
            cpu_fraction,
            rss_bytes,
            procs,
            sandboxed: sandbox_container.is_some(),
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
