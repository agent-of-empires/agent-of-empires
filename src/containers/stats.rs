//! Container resource sampling. A sandbox pane holds a `docker exec` client, not the agent,
//! so sandbox rows use the runtime's own accounting.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct ContainerStats {
    /// Percent of one core, like `docker stats`: four busy cores read 400.
    pub cpu_percent: f64,
    pub mem_used_bytes: u64,
    pub pids: usize,
}

pub type StatsMap = HashMap<String, ContainerStats>;

/// `docker stats` takes ~2s with many sandboxes, so refreshes run off-thread on this TTL.
const STATS_TTL: Duration = Duration::from_secs(15);

struct Cache {
    map: Arc<StatsMap>,
    fetched_at: Option<Instant>,
}

/// Recover from poisoning: the map has no invariant a panic could break.
fn cache() -> std::sync::MutexGuard<'static, Cache> {
    static CACHE: OnceLock<Mutex<Cache>> = OnceLock::new();
    CACHE
        .get_or_init(|| {
            Mutex::new(Cache {
                map: Arc::default(),
                fetched_at: None,
            })
        })
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

static REFRESHING: AtomicBool = AtomicBool::new(false);

struct RefreshGuard;

impl Drop for RefreshGuard {
    fn drop(&mut self) {
        REFRESHING.store(false, Ordering::SeqCst);
    }
}

/// Never blocks; returns the last completed map.
pub fn cached_stats() -> Arc<StatsMap> {
    let (map, stale) = {
        let cache = cache();
        (
            cache.map.clone(),
            cache.fetched_at.is_none_or(|at| at.elapsed() >= STATS_TTL),
        )
    };
    if stale && !REFRESHING.swap(true, Ordering::SeqCst) {
        let spawned = std::thread::Builder::new()
            .name("aoe-container-stats".to_string())
            .spawn(|| {
                let _guard = RefreshGuard;
                let fresh = super::batch_container_stats();
                let mut cache = cache();
                cache.map = Arc::new(fresh);
                cache.fetched_at = Some(Instant::now());
            });
        if spawned.is_err() {
            REFRESHING.store(false, Ordering::SeqCst);
        }
    }
    map
}

pub(crate) fn parse_stats_output(stdout: &str, prefix: &str) -> StatsMap {
    stdout
        .lines()
        .filter_map(parse_stats_line)
        .filter(|(name, _)| name.starts_with(prefix))
        .collect()
}

/// A restarting container reports `--`; drop the row rather than read a zero.
fn parse_stats_line(line: &str) -> Option<(String, ContainerStats)> {
    let mut parts = line.split('\t');
    let name = parts.next()?.trim();
    let cpu = parts.next()?.trim();
    let mem = parts.next()?.trim();
    let pids = parts.next()?.trim();
    if name.is_empty() {
        return None;
    }
    let cpu_percent = cpu.strip_suffix('%')?.trim().parse().ok()?;
    let mem_used_bytes = parse_size(mem.split('/').next()?)?;
    Some((
        name.to_string(),
        ContainerStats {
            cpu_percent,
            mem_used_bytes,
            pids: pids.parse().ok()?,
        },
    ))
}

/// Docker prints binary units, podman decimal ones.
fn parse_size(text: &str) -> Option<u64> {
    let text = text.trim();
    let unit_at = text.find(|c: char| c.is_ascii_alphabetic())?;
    let (value, unit) = text.split_at(unit_at);
    let value: f64 = value.trim().parse().ok()?;
    let multiplier: f64 = match unit.trim() {
        "B" => 1.0,
        "kB" | "KB" => 1e3,
        "MB" => 1e6,
        "GB" => 1e9,
        "TB" => 1e12,
        "KiB" | "kiB" => 1024.0,
        "MiB" => 1024f64.powi(2),
        "GiB" => 1024f64.powi(3),
        "TiB" => 1024f64.powi(4),
        _ => return None,
    };
    (value >= 0.0).then(|| (value * multiplier).round() as u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_size_accepts_both_unit_families() {
        let cases = [
            ("0B", Some(0)),
            ("512B", Some(512)),
            ("1022MiB", Some(1022 * (1 << 20))),
            ("450.4MiB", Some(472_278_630)),
            ("3.085GiB", Some(3_312_493_527)),
            ("20GiB", Some(20 * (1 << 30))),
            ("1.045MB", Some(1_045_000)),
            ("16.62GB", Some(16_620_000_000)),
            ("--", None),
            ("12", None),
            ("MiB", None),
            ("1.2Xb", None),
        ];
        for (text, expected) in cases {
            assert_eq!(parse_size(text), expected, "size {text:?}");
        }
    }

    #[test]
    fn parse_stats_line_reads_a_docker_row() {
        let (name, stats) =
            parse_stats_line("aoe-sandbox-32f0940b\t82.30%\t3.085GiB / 20GiB\t36").expect("row");
        assert_eq!(name, "aoe-sandbox-32f0940b");
        assert_eq!(stats.cpu_percent, 82.30);
        assert_eq!(stats.mem_used_bytes, 3_312_493_527);
        assert_eq!(stats.pids, 36);
    }

    #[test]
    fn parse_stats_line_rejects_unusable_rows() {
        let cases = [
            "aoe-sandbox-a\t--\t-- / --\t0",
            "aoe-sandbox-a\t12.0\t1MiB / 2MiB\t3", // CPU missing its percent
            "aoe-sandbox-a\t12.0%\t1MiB / 2MiB",   // truncated row
            "\t12.0%\t1MiB / 2MiB\t3",             // no name
            "",
        ];
        for line in cases {
            assert!(parse_stats_line(line).is_none(), "line {line:?}");
        }
    }

    #[test]
    fn parse_stats_output_keeps_only_prefixed_containers() {
        let stdout = "aoe-sandbox-a1\t1.00%\t100MiB / 20GiB\t7\n\
                      clawbolt-pg\t0.00%\t75.01MiB / 30.56GiB\t6\n\
                      not-aoe-sandbox-a2\t2.00%\t200MiB / 20GiB\t8\n";
        let map = parse_stats_output(stdout, "aoe-sandbox-");
        assert_eq!(map.keys().collect::<Vec<_>>(), ["aoe-sandbox-a1"]);
        assert_eq!(map["aoe-sandbox-a1"].pids, 7);
    }
}
