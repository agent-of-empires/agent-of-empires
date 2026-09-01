//! Migration v027: seed instance-private sandbox stores from the legacy shared store.
//!
//! Before v027, every non-Codex sandbox for an agent mounted the same
//! `~/<agent>/sandbox` directory. Instance-private mounts use a child directory,
//! so an existing conversation would otherwise disappear from the container on
//! its next recreation. Each affected instance gets a non-destructive copy of
//! the complete legacy store because agent indexes and SQLite relationships are
//! not safely separable by one session ID.

use anyhow::{Context, Result};
use serde_json::Value;
use std::collections::{BTreeMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use tracing::{info, warn};

type Planner<'a> = dyn Fn(&str, &Value) -> Result<Vec<(PathBuf, PathBuf)>> + 'a;

pub fn run() -> Result<()> {
    let app_dir = crate::session::get_app_dir()?;
    let home =
        dirs::home_dir().context("home directory unavailable for sandbox store migration")?;
    run_in(&app_dir, &|profile, row| plan_row(profile, row, &home))
}

fn plan_row(profile: &str, row: &Value, home: &Path) -> Result<Vec<(PathBuf, PathBuf)>> {
    if !row
        .get("sandbox_info")
        .and_then(|value| value.get("enabled"))
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        return Ok(Vec::new());
    }
    let Some(id) = row.get("id").and_then(Value::as_str) else {
        return Ok(Vec::new());
    };
    crate::session::validate_instance_id(id)?;
    let Some(tool) = row.get("tool").and_then(Value::as_str) else {
        return Ok(Vec::new());
    };
    let config = crate::session::profile_config::resolve_config_or_warn(profile);
    let stored_detect_as = row
        .get("detect_as")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty());
    let detect_as =
        stored_detect_as.or_else(|| config.session.agent_detect_as.get(tool).map(String::as_str));
    let Some(agent) =
        crate::agents::get_agent(tool).or_else(|| detect_as.and_then(crate::agents::get_agent))
    else {
        return Ok(Vec::new());
    };
    // Codex was already instance-private before this migration.
    if agent.name == "codex" {
        return Ok(Vec::new());
    }
    let declared = config.session.agent_config_dir_for(tool, home);
    crate::session::container_config::sandbox_store_migration_paths(
        agent.name,
        home,
        declared.as_deref(),
        id,
    )
}

pub(crate) fn run_in(app_dir: &Path, planner: &Planner<'_>) -> Result<()> {
    let mut plans = Vec::new();
    let profiles_dir = app_dir.join("profiles");
    if profiles_dir.exists() {
        for entry in fs::read_dir(&profiles_dir)? {
            let entry = entry?;
            if !entry.path().is_dir() {
                continue;
            }
            let Some(profile) = entry.file_name().to_str().map(str::to_owned) else {
                continue;
            };
            collect_plans(
                &entry.path().join("sessions.json"),
                &profile,
                planner,
                &mut plans,
            )?;
        }
    }
    collect_plans(&app_dir.join("sessions.json"), "", planner, &mut plans)?;

    let mut grouped: BTreeMap<PathBuf, Vec<PathBuf>> = BTreeMap::new();
    for (shared, private) in plans {
        if shared.is_dir() && shared != private {
            let targets = grouped.entry(shared).or_default();
            if !targets.contains(&private) {
                targets.push(private);
            }
        }
    }

    for (shared, targets) in grouped {
        let excluded: HashSet<PathBuf> = targets.iter().cloned().collect();
        for private in targets {
            let copied = merge_store(&shared, &private, &excluded)?;
            info!(
                target: "migrations",
                source = %shared.display(),
                destination = %private.display(),
                copied,
                "v027: seeded instance-private sandbox store"
            );
        }
    }
    Ok(())
}

fn collect_plans(
    path: &Path,
    profile: &str,
    planner: &Planner<'_>,
    plans: &mut Vec<(PathBuf, PathBuf)>,
) -> Result<()> {
    if !path.exists() {
        return Ok(());
    }
    let value: Value = serde_json::from_slice(&fs::read(path)?)
        .with_context(|| format!("parsing {}", path.display()))?;
    let Some(rows) = value.as_array() else {
        return Ok(());
    };
    for row in rows {
        plans.extend(planner(profile, row)?);
    }
    Ok(())
}

fn merge_store(source: &Path, destination: &Path, excluded: &HashSet<PathBuf>) -> Result<u64> {
    fs::create_dir_all(destination)?;
    let root_canonical = fs::canonicalize(source)?;
    let mut visited = HashSet::new();
    merge_store_inner(
        source,
        destination,
        excluded,
        true,
        &root_canonical,
        &mut visited,
    )
}

fn merge_store_inner(
    source: &Path,
    destination: &Path,
    excluded: &HashSet<PathBuf>,
    root: bool,
    root_canonical: &Path,
    visited: &mut HashSet<PathBuf>,
) -> Result<u64> {
    let canonical = fs::canonicalize(source)?;
    if !canonical.starts_with(root_canonical) {
        warn!(
            target: "migrations",
            path = %source.display(),
            "v027: skipped sandbox-store symlink outside the shared store"
        );
        return Ok(0);
    }
    if !visited.insert(canonical) {
        return Ok(0);
    }
    fs::create_dir_all(destination)?;
    let mut copied = 0;
    for entry in fs::read_dir(source)? {
        let entry = entry?;
        let source_path = entry.path();
        if root && excluded.contains(&source_path) {
            continue;
        }
        let file_type = entry.file_type()?;
        if file_type.is_symlink() && !fs::canonicalize(&source_path)?.starts_with(root_canonical) {
            warn!(
                target: "migrations",
                path = %source_path.display(),
                "v027: skipped sandbox-store symlink outside the shared store"
            );
            continue;
        }
        let metadata = fs::metadata(&source_path)?;
        let destination_path = destination.join(entry.file_name());
        if metadata.is_dir() {
            if destination_path.is_file() {
                warn!(
                    target: "migrations",
                    path = %destination_path.display(),
                    "v027: kept existing file instead of legacy directory"
                );
                continue;
            }
            copied += merge_store_inner(
                &source_path,
                &destination_path,
                excluded,
                false,
                root_canonical,
                visited,
            )?;
        } else if metadata.is_file() && !destination_path.exists() {
            fs::copy(&source_path, &destination_path)
                .with_context(|| format!("copying {}", source_path.display()))?;
            copied += 1;
        }
    }
    Ok(copied)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn migrates_handcrafted_shared_store_without_copying_peer_targets() {
        let temp = tempfile::tempdir().unwrap();
        let app = temp.path().join("app");
        let profile = app.join("profiles/work");
        fs::create_dir_all(&profile).unwrap();
        fs::write(
            profile.join("sessions.json"),
            br#"[
                {"id":"one","sandbox_info":{"enabled":true}},
                {"id":"two","sandbox_info":{"enabled":true}},
                {"id":"host","sandbox_info":{"enabled":false}}
            ]"#,
        )
        .unwrap();
        let shared = temp.path().join("home/.agent/sandbox");
        fs::create_dir_all(shared.join("history")).unwrap();
        fs::write(shared.join("history/conversation.jsonl"), b"legacy").unwrap();
        fs::create_dir_all(shared.join("one")).unwrap();
        fs::write(shared.join("one/config.json"), b"private-wins").unwrap();

        run_in(&app, &|profile, row| {
            assert_eq!(profile, "work");
            if !row["sandbox_info"]["enabled"].as_bool().unwrap_or(false) {
                return Ok(Vec::new());
            }
            let id = row["id"].as_str().unwrap();
            Ok(vec![(shared.clone(), shared.join(id))])
        })
        .unwrap();

        for id in ["one", "two"] {
            assert_eq!(
                fs::read(shared.join(id).join("history/conversation.jsonl")).unwrap(),
                b"legacy"
            );
            assert!(!shared.join(id).join("one").exists());
            assert!(!shared.join(id).join("two").exists());
        }
        assert_eq!(
            fs::read(shared.join("one/config.json")).unwrap(),
            b"private-wins"
        );
        assert_eq!(
            fs::read(shared.join("history/conversation.jsonl")).unwrap(),
            b"legacy"
        );

        let before = fs::read(shared.join("one/history/conversation.jsonl")).unwrap();
        run_in(&app, &|_, row| {
            let id = row["id"].as_str().unwrap();
            Ok(
                if row["sandbox_info"]["enabled"].as_bool().unwrap_or(false) {
                    vec![(shared.clone(), shared.join(id))]
                } else {
                    Vec::new()
                },
            )
        })
        .unwrap();
        assert_eq!(
            fs::read(shared.join("one/history/conversation.jsonl")).unwrap(),
            before
        );
    }
}
