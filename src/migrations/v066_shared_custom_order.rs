//! Migration v066: one Custom order for every Client (#5896).
//!
//! The TUI kept its Custom order as a `sort_index` on each `sessions.json` row, and the
//! dashboard kept its own in `workspace-ordering.json` (rows) and in `state.toml`'s
//! `web_ui_state` (`aoe-repo-group-order-v1`, Repository headers). `workspace-ordering.json`
//! is now the one store. A TUI order seeds it only where the dashboard has none, so the
//! dashboard's wins; `sort_index` is then dropped from the rows.

use super::sessions_file;
use anyhow::Result;
use serde_json::Value;
use std::fs;
use std::path::Path;
use tracing::info;

const WEB_REPO_ORDER_KEY: &str = "aoe-repo-group-order-v1";

pub fn run() -> Result<()> {
    run_in(&crate::session::get_app_dir()?)
}

fn run_in(app_dir: &Path) -> Result<()> {
    let mut placed: Vec<(u64, String, String)> = Vec::new();
    for path in sessions_file::session_files(app_dir)? {
        let Ok(content) = fs::read_to_string(&path) else {
            continue;
        };
        let Ok(Value::Array(rows)) = serde_json::from_str::<Value>(&content) else {
            continue;
        };
        if !rows.iter().any(|row| row.get("sort_index").is_some()) {
            continue;
        }
        for row in rows.iter().filter_map(Value::as_object) {
            let live = !sessions_file::is_archived(row)
                && row.get("trashed_at").is_none_or(Value::is_null);
            if let (true, Some(index)) = (live, row.get("sort_index").and_then(Value::as_u64)) {
                let created = row
                    .get("created_at")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                placed.push((index, created, workspace_id(row)));
            }
        }
        crate::session::backup_before_migration(&path)?;
        sessions_file::heal_rows(&path, &content, |row| row.remove("sort_index").is_some())?;
    }
    // Each group numbered its own rows from 0, so interleaving by index keeps every
    // group's order; newer rows break ties as they did on screen.
    placed.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| b.1.cmp(&a.1)));
    let mut tui_order: Vec<String> = Vec::new();
    for (_, _, id) in placed {
        if !tui_order.contains(&id) {
            tui_order.push(id);
        }
    }

    let web_repositories = take_web_repository_order(app_dir)?;

    let ordering_path = app_dir.join("workspace-ordering.json");
    let mut ordering: crate::session::WorkspaceOrdering = fs::read_to_string(&ordering_path)
        .ok()
        .filter(|content| !content.trim().is_empty())
        .map(|content| serde_json::from_str(&content))
        .transpose()?
        .unwrap_or_default();
    let mut changed = false;
    if ordering.order.is_empty() && !tui_order.is_empty() {
        ordering.order = tui_order;
        changed = true;
    }
    if ordering.repositories.is_empty() && !web_repositories.is_empty() {
        ordering.repositories = web_repositories;
        changed = true;
    }
    if changed {
        crate::session::atomic_write(
            &ordering_path,
            serde_json::to_string_pretty(&ordering)?.as_bytes(),
        )?;
        info!(
            target: "migrations.v066",
            rows = ordering.order.len(),
            repositories = ordering.repositories.len(),
            "seeded the shared Custom order"
        );
    }
    Ok(())
}

/// `repoPath::branch`, or `repoPath::__session__::id` without a worktree, as
/// `Instance::workspace_id` computes it.
fn workspace_id(row: &serde_json::Map<String, Value>) -> String {
    let worktree = row.get("worktree_info").filter(|w| !w.is_null());
    fn text(value: Option<&Value>) -> &str {
        value.and_then(Value::as_str).unwrap_or_default()
    }
    let repo = match worktree {
        Some(worktree) => text(worktree.get("main_repo_path")),
        None => text(row.get("project_path")),
    }
    .trim_end_matches('/');
    match worktree {
        Some(worktree) => format!("{repo}::{}", text(worktree.get("branch"))),
        None => format!("{repo}::__session__::{}", text(row.get("id"))),
    }
}

/// Remove the dashboard's Repository header order from `state.toml`, returning it.
fn take_web_repository_order(app_dir: &Path) -> Result<Vec<String>> {
    let mut taken = Vec::new();
    super::config_file::rewrite_strict(&app_dir.join("state.toml"), "v066", |state| {
        let Some(web) = state
            .get_mut("web_ui_state")
            .and_then(toml::Value::as_table_mut)
        else {
            return false;
        };
        let Some(raw) = web.remove(WEB_REPO_ORDER_KEY) else {
            return false;
        };
        taken = raw
            .as_str()
            .and_then(|raw| serde_json::from_str::<Vec<String>>(raw).ok())
            .unwrap_or_default();
        if web.is_empty() {
            state.remove("web_ui_state");
        }
        true
    })?;
    Ok(taken)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ordering(dir: &Path) -> crate::session::WorkspaceOrdering {
        serde_json::from_str(&fs::read_to_string(dir.join("workspace-ordering.json")).unwrap())
            .unwrap()
    }

    fn seed(dir: &Path, rows: Value) {
        let profile = dir.join("profiles/main");
        fs::create_dir_all(&profile).unwrap();
        fs::write(profile.join("sessions.json"), rows.to_string()).unwrap();
    }

    #[test]
    fn seeds_the_shared_order_from_the_tui_and_drops_sort_index() {
        let dir = tempfile::tempdir().unwrap();
        seed(
            dir.path(),
            serde_json::json!([
                {"id": "a", "project_path": "/r/one", "sort_index": 1, "created_at": "2026-01-02T00:00:00Z"},
                {"id": "b", "project_path": "/r/two/", "sort_index": 0, "created_at": "2026-01-01T00:00:00Z",
                 "worktree_info": {"main_repo_path": "/r/two/", "branch": "feat"}},
                {"id": "c", "project_path": "/r/three", "created_at": "2026-01-03T00:00:00Z"},
                {"id": "d", "project_path": "/r/four", "sort_index": 0, "archived_at": "2026-01-04T00:00:00Z"},
            ]),
        );
        fs::write(
            dir.path().join("state.toml"),
            "[web_ui_state]\n\"aoe-repo-group-order-v1\" = '[\"/r/two\",\"/r/one\"]'\n",
        )
        .unwrap();

        run_in(dir.path()).unwrap();

        let ordering = ordering(dir.path());
        assert_eq!(ordering.order, ["/r/two::feat", "/r/one::__session__::a"]);
        assert_eq!(ordering.repositories, ["/r/two", "/r/one"]);
        let rows = fs::read_to_string(dir.path().join("profiles/main/sessions.json")).unwrap();
        assert!(!rows.contains("sort_index"));
        let state = fs::read_to_string(dir.path().join("state.toml")).unwrap();
        assert!(!state.contains(WEB_REPO_ORDER_KEY));

        // A second run over migrated state changes nothing.
        run_in(dir.path()).unwrap();
        assert_eq!(ordering.order, self::ordering(dir.path()).order);
    }

    #[test]
    fn an_existing_dashboard_order_wins() {
        let dir = tempfile::tempdir().unwrap();
        seed(
            dir.path(),
            serde_json::json!([{"id": "a", "project_path": "/r/one", "sort_index": 0}]),
        );
        fs::write(
            dir.path().join("workspace-ordering.json"),
            r#"{"order": ["/r/web::main"]}"#,
        )
        .unwrap();
        run_in(dir.path()).unwrap();
        assert_eq!(ordering(dir.path()).order, ["/r/web::main"]);
    }
}
