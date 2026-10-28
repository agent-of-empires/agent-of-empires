//! Migration v052: rename `cockpit` targets in `runtime_filter` to `acp`.
//! v012 missed this file, so the daemon and every runner kept applying a
//! filter with no `acp` directive and dropped all ACP logs.

use anyhow::{Context, Result};
use std::collections::HashSet;
use std::path::Path;
use tracing::info;

pub fn run() -> Result<()> {
    let app_dir = crate::session::get_app_dir()?;
    rename_targets(&crate::logging::runtime_filter_path(&app_dir))
}

fn rename_targets(path: &Path) -> Result<()> {
    let old = match std::fs::read_to_string(path) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
    };
    let directives: Vec<&str> = old.trim().split(',').map(str::trim).collect();
    fn target(d: &str) -> &str {
        d[..d.find(['=', '[']).unwrap_or(d.len())].trim()
    }
    let renamed = |t: &str| match t {
        "cockpit" => Some("acp".to_string()),
        _ => t.strip_prefix("cockpit.").map(|rest| format!("acp.{rest}")),
    };
    if directives.iter().all(|d| renamed(target(d)).is_none()) {
        return Ok(());
    }
    // The level `=` follows the span; field values inside it may contain `=`.
    fn selector(d: &str) -> &str {
        let from = d.rfind(']').map_or(0, |i| i + 1);
        d[..d[from..].find('=').map_or(d.len(), |i| from + i)].trim()
    }
    let existing: HashSet<&str> = directives.iter().map(|d| selector(d)).collect();
    let mut out = Vec::new();
    for d in &directives {
        let t = target(d);
        match renamed(t) {
            None => out.push(d.to_string()),
            Some(new) => {
                let renamed = format!("{new}{}", &d[t.len()..]);
                // An explicit `acp` directive with the same selector wins.
                if !existing.contains(selector(&renamed)) {
                    out.push(renamed);
                }
            }
        }
    }
    // Replace atomically so a failed write leaves the old filter for the retry.
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, out.join(",")).with_context(|| format!("writing {}", tmp.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600))?;
    }
    std::fs::rename(&tmp, path).with_context(|| format!("replacing {}", path.display()))?;
    info!(target: "migrations.v052", "Renamed cockpit log targets to acp in {}", path.display());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::rename_targets;

    #[test]
    fn renames_cockpit_targets_once() {
        let cases = [
            (
                Some("agent_of_empires=trace,cockpit=trace,cockpit.event_store=debug,http=info"),
                Some("agent_of_empires=trace,acp=trace,acp.event_store=debug,http=info"),
            ),
            (Some("acp=info,cockpit=trace"), Some("acp=info")),
            (
                Some("acp[turn]=info,cockpit=trace"),
                Some("acp[turn]=info,acp=trace"),
            ),
            (
                Some("acp[turn{status=1}]=info,cockpit[turn{status=2}]=trace"),
                Some("acp[turn{status=1}]=info,acp[turn{status=2}]=trace"),
            ),
            (Some("cockpit[turn]=debug"), Some("acp[turn]=debug")),
            (
                Some("agent_of_empires=info,acp=info\n"),
                Some("agent_of_empires=info,acp=info\n"),
            ),
            (None, None),
        ];
        for (before, after) in cases {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("runtime_filter");
            if let Some(before) = before {
                std::fs::write(&path, before).unwrap();
            }
            for run in 0..2 {
                rename_targets(&path).unwrap();
                assert_eq!(
                    std::fs::read_to_string(&path).ok().as_deref(),
                    after,
                    "{before:?} run {run}"
                );
            }
        }
    }
}
