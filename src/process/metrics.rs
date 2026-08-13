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
struct ProcessRecord {
    pid: u32,
    ppid: u32,
    start_id: u64,
    rss_bytes: u64,
    cpu_seconds: f64,
}

type SystemReading = (
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

#[cfg(target_os = "linux")]
fn sample_system() -> SystemReading {
    let stat = std::fs::read_to_string("/proc/stat").unwrap_or_default();
    let cpu = stat.lines().next().and_then(|line| {
        let values: Vec<u64> = line
            .split_whitespace()
            .skip(1)
            .filter_map(|v| v.parse().ok())
            .collect();
        (!values.is_empty()).then(|| {
            (
                values.iter().sum(),
                values.get(3).copied().unwrap_or(0) + values.get(4).copied().unwrap_or(0),
            )
        })
    });
    let load = std::fs::read_to_string("/proc/loadavg").ok().and_then(|s| {
        let vals: Vec<f64> = s
            .split_whitespace()
            .take(3)
            .filter_map(|v| v.parse().ok())
            .collect();
        (vals.len() == 3).then(|| [vals[0], vals[1], vals[2]])
    });
    let mem = std::fs::read_to_string("/proc/meminfo").unwrap_or_default();
    let field = |name: &str| {
        mem.lines()
            .find(|l| l.starts_with(name))
            .and_then(|l| l.split_whitespace().nth(1)?.parse::<u64>().ok())
            .unwrap_or(0)
            * 1024
    };
    let total = field("SwapTotal:");
    let free = field("SwapFree:");
    (cpu, None, load, (total, total.saturating_sub(free)))
}

#[cfg(target_os = "linux")]
fn process_snapshot() -> Vec<ProcessRecord> {
    let hz = unsafe { libc::sysconf(libc::_SC_CLK_TCK) }.max(1) as f64;
    let page = unsafe { libc::sysconf(libc::_SC_PAGESIZE) }.max(1) as u64;
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return Vec::new();
    };
    entries
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let pid = entry.file_name().to_str()?.parse::<u32>().ok()?;
            let stat = std::fs::read_to_string(entry.path().join("stat")).ok()?;
            let end = stat.rfind(')')?;
            let f: Vec<&str> = stat[end + 2..].split_whitespace().collect();
            let ppid = f.get(1)?.parse().ok()?;
            let utime: u64 = f.get(11)?.parse().ok()?;
            let stime: u64 = f.get(12)?.parse().ok()?;
            let start_id = f.get(19)?.parse().ok()?;
            let rss_pages: i64 = f.get(21)?.parse().ok()?;
            Some(ProcessRecord {
                pid,
                ppid,
                start_id,
                rss_bytes: rss_pages.max(0) as u64 * page,
                cpu_seconds: (utime + stime) as f64 / hz,
            })
        })
        .collect()
}

#[cfg(target_os = "macos")]
fn sample_system() -> SystemReading {
    let cpu = std::process::Command::new("ps")
        .args(["-A", "-o", "%cpu="])
        .output()
        .ok()
        .map(|output| {
            let total_percent: f64 = String::from_utf8_lossy(&output.stdout)
                .lines()
                .filter_map(|line| line.trim().parse::<f64>().ok())
                .sum();
            (total_percent / 100.0 / logical_cpus() as f64).clamp(0.0, 1.0)
        });
    let load = std::process::Command::new("sysctl")
        .args(["-n", "vm.loadavg"])
        .output()
        .ok()
        .and_then(|o| {
            let s = String::from_utf8_lossy(&o.stdout).replace(['{', '}'], "");
            let v: Vec<f64> = s
                .split_whitespace()
                .filter_map(|x| x.parse().ok())
                .collect();
            (v.len() >= 3).then(|| [v[0], v[1], v[2]])
        });
    let swap = std::process::Command::new("sysctl")
        .args(["-n", "vm.swapusage"])
        .output()
        .ok()
        .map(|output| parse_macos_swap(&String::from_utf8_lossy(&output.stdout)))
        .unwrap_or((0, 0));
    (None, cpu, load, swap)
}

#[cfg(target_os = "macos")]
fn parse_macos_swap(value: &str) -> (u64, u64) {
    let parse = |label: &str| {
        let raw = value
            .split_whitespace()
            .skip_while(|part| *part != label)
            .nth(2)?;
        let (number, multiplier) = if let Some(number) = raw.strip_suffix('G') {
            (number, 1u64 << 30)
        } else if let Some(number) = raw.strip_suffix('M') {
            (number, 1u64 << 20)
        } else if let Some(number) = raw.strip_suffix('K') {
            (number, 1u64 << 10)
        } else {
            (raw, 1)
        };
        Some((number.parse::<f64>().ok()? * multiplier as f64) as u64)
    };
    (parse("total").unwrap_or(0), parse("used").unwrap_or(0))
}

#[cfg(target_os = "macos")]
fn process_snapshot() -> Vec<ProcessRecord> {
    let Ok(out) = std::process::Command::new("ps")
        .args(["-axo", "pid=,ppid=,rss=,time="])
        .output()
    else {
        return Vec::new();
    };
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|line| {
            let f: Vec<&str> = line.split_whitespace().collect();
            let pid = f.first()?.parse().ok()?;
            let ppid = f.get(1)?.parse().ok()?;
            let rss_bytes = f.get(2)?.parse::<u64>().ok()?.saturating_mul(1024);
            let cpu_seconds = parse_ps_time(f.get(3)?)?;
            Some(ProcessRecord {
                pid,
                ppid,
                start_id: 0,
                rss_bytes,
                cpu_seconds,
            })
        })
        .collect()
}

#[cfg(target_os = "macos")]
fn parse_ps_time(value: &str) -> Option<f64> {
    let parts: Vec<f64> = value.split(':').filter_map(|v| v.parse().ok()).collect();
    match parts.as_slice() {
        [m, s] => Some(m * 60.0 + s),
        [h, m, s] => Some(h * 3600.0 + m * 60.0 + s),
        _ => None,
    }
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn sample_system() -> SystemReading {
    (None, None, None, (0, 0))
}
#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn process_snapshot() -> Vec<ProcessRecord> {
    Vec::new()
}
