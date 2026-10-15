//! Host and AoE-agent resource sampling for the system-health views, shared by
//! the TUI strip and the web dashboard's strip.

use std::collections::{HashMap, HashSet};
use std::time::Instant;

use crate::session::{Instance, Status};

/// Fraction used at or above which the reading is Critical.
const HEADROOM_CRITICAL: f64 = 0.90;
/// Fraction used at or above which the reading is Warn.
const HEADROOM_WARN: f64 = 0.70;
/// PSI `some` avg10 percent at or above which a present PSI signal reads Critical.
const PSI_CRITICAL: f32 = 20.0;
/// PSI `some` avg10 percent at or above which a present PSI signal reads Warn.
const PSI_WARN: f32 = 5.0;

/// Memory-pressure severity, worst-of across the available signals. Ordered
/// ascending so the derived `Ord` lets callers fold inputs with `max`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum PressureBand {
    Ok,
    Warn,
    Critical,
}

impl PressureBand {
    /// The band's wire and display name. Both dashboards spell a band from
    /// this one mapping; the TUI upper-cases it for its status row.
    pub fn as_str(self) -> &'static str {
        match self {
            PressureBand::Ok => "ok",
            PressureBand::Warn => "warn",
            PressureBand::Critical => "critical",
        }
    }
}

/// Classify a memory sample into a pressure band. Headroom is always an input;
/// PSI (Linux) and the macOS pressure level contribute only when present, so a
/// platform that omits a signal never has it read as a false all-clear. The
/// worst band across the present inputs wins.
pub fn pressure_band(mem: &MemorySample) -> PressureBand {
    let mut band = band_from_headroom(mem.used_fraction());
    if let Some(v) = mem.psi_mem_some_avg10 {
        band = band.max(band_from_psi(v));
    }
    if let Some(v) = mem.psi_io_some_avg10 {
        band = band.max(band_from_psi(v));
    }
    if let Some(level) = mem.macos_pressure_level {
        band = band.max(band_from_macos(level));
    }
    band
}

fn band_from_headroom(used_fraction: f64) -> PressureBand {
    if used_fraction >= HEADROOM_CRITICAL {
        PressureBand::Critical
    } else if used_fraction >= HEADROOM_WARN {
        PressureBand::Warn
    } else {
        PressureBand::Ok
    }
}

fn band_from_psi(some_avg10: f32) -> PressureBand {
    if some_avg10 >= PSI_CRITICAL {
        PressureBand::Critical
    } else if some_avg10 >= PSI_WARN {
        PressureBand::Warn
    } else {
        PressureBand::Ok
    }
}

fn band_from_macos(level: u8) -> PressureBand {
    match level {
        4 => PressureBand::Critical,
        2 => PressureBand::Warn,
        _ => PressureBand::Ok,
    }
}

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

/// Every figure is optional: an unknown reading is honest where a zero would not be.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct AgentMetric {
    pub id: String,
    pub title: String,
    pub cpu_fraction: Option<f64>,
    pub rss_bytes: Option<u64>,
    pub procs: Option<usize>,
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
    last_pane_roots: PaneRoots,
}

/// Reused when `list-panes` fails; start identities stop a recycled pid from seeding a walk.
#[derive(Default)]
struct PaneRoots {
    panes: HashMap<String, crate::tmux::PaneMetadata>,
    start_ids: HashMap<u32, u64>,
}

impl PaneRoots {
    fn capture(
        panes: HashMap<String, crate::tmux::PaneMetadata>,
        processes: &[ProcessRecord],
    ) -> Self {
        let pids: HashSet<u32> = panes.values().filter_map(|meta| meta.pane_pid).collect();
        let start_ids = processes
            .iter()
            .filter(|p| pids.contains(&p.pid))
            .map(|p| (p.pid, p.start_id))
            .collect();
        Self { panes, start_ids }
    }

    fn root_for(&self, inst: &Instance, by_pid: &HashMap<u32, &ProcessRecord>) -> Option<u32> {
        let derived = crate::tmux::Session::generate_name(&inst.id, &inst.title);
        let name = crate::tmux::resolve_agent_session_name_in(&self.panes, &inst.id, &derived);
        let meta = self.panes.get(&name)?;
        if meta.pane_dead {
            return None;
        }
        let pid = meta.pane_pid?;
        let record = by_pid.get(&pid)?;
        (self.start_ids.get(&pid) == Some(&record.start_id)).then_some(pid)
    }
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
        // `Err` means tmux could not answer, so the previous snapshot stands.
        if let Ok(panes) = crate::tmux::batch_pane_metadata() {
            self.last_pane_roots = PaneRoots::capture(panes, &processes);
        }
        let agents = aggregate_agents(
            instances,
            &processes,
            &self.last_pane_roots,
            elapsed,
            &self.last_process_cpu,
        );
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

type RowFigures = (Option<f64>, Option<u64>, Option<usize>);

fn sandbox_container_name(inst: &Instance) -> Option<&str> {
    inst.sandbox_info
        .as_ref()
        .filter(|s| s.enabled)
        .map(|s| s.container_name.as_str())
}

fn container_figures(stats: &crate::containers::stats::StatsMap, name: &str) -> RowFigures {
    match stats.get(name) {
        Some(stats) => (
            // `cpu_percent` is per-core; the table shows a share of the whole host.
            Some(stats.cpu_percent / 100.0 / logical_cpus() as f64),
            Some(stats.mem_used_bytes),
            Some(stats.pids),
        ),
        None => (None, None, None),
    }
}

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
    roots: &PaneRoots,
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
    let worker_records: Vec<crate::process::worker_registry::WorkerRecord> =
        crate::process::worker_registry::list()
            .unwrap_or_default()
            .into_iter()
            .filter(crate::process::worker_registry::is_record_live)
            .collect();

    // Structured sessions are excluded from `eligible`, so count their sandboxes here.
    let sandboxed_worker = worker_records.iter().any(|rec| {
        instances
            .iter()
            .any(|i| i.id == rec.session_id && sandbox_container_name(i).is_some())
    });

    let container_stats = (eligible.iter().any(|i| sandbox_container_name(i).is_some())
        || sandboxed_worker)
        .then(crate::containers::stats::cached_stats)
        .unwrap_or_default();
    for inst in &eligible {
        let Some(root) = roots.root_for(inst, &by_pid) else {
            continue;
        };
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
        // Walk the tree anyway so pane processes are claimed, but take figures from the container,
        // where the agent actually runs.
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

#[cfg(test)]
mod tests {
    use super::*;

    fn record(pid: u32, start_id: u64) -> ProcessRecord {
        ProcessRecord {
            pid,
            ppid: 1,
            start_id,
            rss_bytes: 0,
            cpu_seconds: 0.0,
        }
    }

    fn pane(pid: Option<u32>, dead: bool) -> crate::tmux::PaneMetadata {
        crate::tmux::PaneMetadata {
            pane_dead: dead,
            pane_current_command: None,
            pane_start_command_is_protected: false,
            pane_pid: pid,
            pane_title: None,
            window_activity: None,
            window_size: None,
        }
    }

    fn sample_with_fraction(used_fraction: f64) -> MemorySample {
        // total 1000 so available maps cleanly to the target fraction.
        MemorySample {
            total_bytes: 1000,
            available_bytes: (1000.0 * (1.0 - used_fraction)).round() as u64,
            ..MemorySample::default()
        }
    }

    /// The band is the worst of headroom, PSI and the macOS level, so a calm
    /// input never softens a critical one.
    #[test]
    fn pressure_band_takes_the_worst_of_headroom_psi_and_macos_level() {
        use PressureBand::{Critical, Ok, Warn};
        // (used fraction, mem PSI, io PSI, macOS level) -> band; no fraction is the default sample.
        let cases = [
            (Some(0.0), None, None, None, Ok),
            (Some(0.69), None, None, None, Ok),
            (Some(0.70), None, None, None, Warn),
            (Some(0.89), None, None, None, Warn),
            (Some(0.90), None, None, None, Critical),
            (Some(0.99), None, None, None, Critical),
            (Some(0.50), Some(6.0), None, None, Warn),
            (Some(0.50), Some(25.0), None, None, Critical),
            (Some(0.50), None, Some(25.0), None, Critical),
            (Some(0.50), None, None, Some(1), Ok),
            (Some(0.50), None, None, Some(2), Warn),
            (Some(0.50), None, None, Some(4), Critical),
            (Some(0.95), Some(1.0), None, None, Critical),
            (None, None, None, None, Ok),
        ];
        for (fraction, psi_mem, psi_io, macos, expected) in cases {
            let sample = MemorySample {
                psi_mem_some_avg10: psi_mem,
                psi_io_some_avg10: psi_io,
                macos_pressure_level: macos,
                ..fraction.map(sample_with_fraction).unwrap_or_default()
            };
            assert_eq!(
                pressure_band(&sample),
                expected,
                "{fraction:?} {psi_mem:?} {psi_io:?} {macos:?}"
            );
        }
    }

    #[test]
    fn root_for_requires_a_live_pane_whose_pid_kept_its_identity() {
        let inst = Instance::new("metrics-root", "/tmp/metrics-root");
        let name = crate::tmux::Session::generate_name(&inst.id, &inst.title);
        let snapshot = [record(100, 7)];
        let cases = [
            (pane(Some(100), false), vec![record(100, 7)], Some(100)),
            (pane(Some(100), false), vec![record(100, 8)], None),
            (pane(Some(100), false), vec![], None),
            (pane(Some(100), true), vec![record(100, 7)], None),
            (pane(None, false), vec![record(100, 7)], None),
        ];
        for (meta, processes, expected) in cases {
            let roots = PaneRoots::capture(HashMap::from([(name.clone(), meta)]), &snapshot);
            let by_pid: HashMap<u32, &ProcessRecord> =
                processes.iter().map(|p| (p.pid, p)).collect();
            assert_eq!(roots.root_for(&inst, &by_pid), expected);
        }
    }
}
