//! Migration v066: one Custom order for every Client (#5896).
//!
//! The TUI kept its Custom order as a `sort_index` on each `sessions.json` row, and the
//! dashboard kept its own in `workspace-ordering.json` (rows) and in `state.toml`'s
//! `web_ui_state` (`aoe-repo-group-order-v1`, Repository headers). `workspace-ordering.json`
//! is now the one store. A TUI order seeds it only where the dashboard has none, so the
//! dashboard's wins. With neither, the removed sort v063 left in `legacy_sort_order` seeds
//! it, so the list keeps the order it had. The dashboard's header order is copied, not
//! removed, because the dashboard still reads it. `sort_index` and `legacy_sort_order` are
//! dropped only after the shared order is written, so an interrupted run retries from them.

use super::sessions_file;
use super::v063_shared_sidebar_view::LEGACY_SORT_KEY;
use crate::time::Timestamp;
use anyhow::{Context, Result};
use serde_json::Value;
use std::cmp::Reverse;
use std::fs;
use std::path::Path;
use tracing::info;

const WEB_REPO_ORDER_KEY: &str = "aoe-repo-group-order-v1";

pub fn run() -> Result<()> {
    run_in(&crate::session::get_app_dir()?)
}

/// A live row's place in each order v064 can seed.
struct LiveRow {
    workspace: String,
    created: Option<Timestamp>,
    accessed: Option<Timestamp>,
}

fn run_in(app_dir: &Path) -> Result<()> {
    let mut placed: Vec<(u64, String, String)> = Vec::new();
    let mut live: Vec<LiveRow> = Vec::new();
    let mut to_strip: Vec<(std::path::PathBuf, String)> = Vec::new();
    for path in sessions_file::session_files(app_dir)? {
        // Only a missing file is skipped: finishing without a profile's rows would lose
        // its order for good, so an unreadable one fails the run to retry next start.
        let Some(content) = read_if_present(&path)? else {
            continue;
        };
        let Value::Array(rows) = serde_json::from_str::<Value>(&content)
            .with_context(|| format!("Failed to parse {} during v064 migration", path.display()))?
        else {
            anyhow::bail!("{} is not a session list", path.display());
        };
        for row in rows.iter().filter_map(Value::as_object) {
            if sessions_file::is_archived(row)
                || row.get("trashed_at").is_some_and(|v| !v.is_null())
            {
                continue;
            }
            let time = |key: &str| {
                row.get(key)
                    .and_then(Value::as_str)
                    .and_then(|raw| Timestamp::parse_rfc3339(raw).ok())
            };
            let workspace = workspace_id(row);
            if let Some(index) = row.get("sort_index").and_then(Value::as_u64) {
                let created = row
                    .get("created_at")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                placed.push((index, created, workspace.clone()));
            }
            live.push(LiveRow {
                workspace,
                created: time("created_at"),
                accessed: time("last_accessed_at"),
            });
        }
        if rows.iter().any(|row| row.get("sort_index").is_some()) {
            to_strip.push((path, content));
        }
    }
    // Each group numbered its own rows from 0, so interleaving by index keeps every
    // group's order; newer rows break ties as they did on screen.
    placed.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| b.1.cmp(&a.1)));
    let tui_order = dedup(placed.into_iter().map(|(_, _, id)| id));

    let state = read_state(app_dir)?;
    let web_repositories = web_repository_order(state.as_ref());
    let legacy_sort = state
        .as_ref()
        .and_then(|state| state.get(LEGACY_SORT_KEY))
        .and_then(toml::Value::as_str)
        .map(str::to_owned);

    let ordering_path = app_dir.join("workspace-ordering.json");
    let mut ordering: crate::session::WorkspaceOrdering = read_if_present(&ordering_path)?
        .map(|content| serde_json::from_str(&content))
        .transpose()?
        .unwrap_or_default();
    let mut changed = false;
    if ordering.order.is_empty() {
        let seeded = if tui_order.is_empty() {
            legacy_sort
                .as_deref()
                .map(|sort| legacy_order(live, sort))
                .unwrap_or_default()
        } else {
            tui_order
        };
        if !seeded.is_empty() {
            ordering.order = seeded;
            changed = true;
        }
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
    for (path, content) in to_strip {
        crate::session::backup_before_migration(&path)?;
        sessions_file::heal_rows(&path, &content, |row| row.remove("sort_index").is_some())?;
    }
    if legacy_sort.is_some() {
        super::config_file::rewrite_strict(&app_dir.join("state.toml"), "v064", |state| {
            state.remove(LEGACY_SORT_KEY).is_some()
        })?;
    }
    Ok(())
}

/// Workspaces in the order the removed sort `sort` showed them; see v063's `legacy_seed`.
/// A workspace ranks by its newest session for last activity, by its oldest for oldest.
fn legacy_order(mut live: Vec<LiveRow>, sort: &str) -> Vec<String> {
    match sort {
        "oldest" => live.sort_by_key(|row| (row.created.is_none(), row.created)),
        "last_activity" => live.sort_by_key(|row| {
            (
                row.accessed.is_none(),
                Reverse(row.accessed),
                Reverse(row.created),
            )
        }),
        _ => return Vec::new(),
    }
    dedup(live.into_iter().map(|row| row.workspace))
}

/// `ids` with each one kept at its first position.
fn dedup(ids: impl Iterator<Item = String>) -> Vec<String> {
    let mut order: Vec<String> = Vec::new();
    for id in ids {
        if !order.contains(&id) {
            order.push(id);
        }
    }
    order
}

/// The file's content, or `None` when it is missing or blank. Any other read error fails.
fn read_if_present(path: &Path) -> Result<Option<String>> {
    match fs::read_to_string(path) {
        Ok(content) if content.trim().is_empty() => Ok(None),
        Ok(content) => Ok(Some(content)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error).with_context(|| format!("Failed to read {}", path.display())),
    }
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

/// `state.toml`, or `None` when it is missing. A file that does not parse fails the run
/// rather than seeding without it.
fn read_state(app_dir: &Path) -> Result<Option<toml::Table>> {
    let path = app_dir.join("state.toml");
    let content = match fs::read_to_string(&path) {
        Ok(content) => content,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    content
        .parse()
        .map(Some)
        .with_context(|| format!("Failed to parse {} during v066 migration", path.display()))
}

/// The dashboard's Repository header order.
fn web_repository_order(state: Option<&toml::Table>) -> Vec<String> {
    state
        .and_then(|state| state.get("web_ui_state"))
        .and_then(|web| web.get(WEB_REPO_ORDER_KEY))
        .and_then(toml::Value::as_str)
        .and_then(|raw| serde_json::from_str::<Vec<String>>(raw).ok())
        .unwrap_or_default()
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
        assert!(
            state.contains(WEB_REPO_ORDER_KEY),
            "the dashboard still reads it"
        );

        // A second run over migrated state changes nothing.
        run_in(dir.path()).unwrap();
        assert_eq!(ordering.order, self::ordering(dir.path()).order);
    }

    /// A run that cannot commit the shared order leaves every legacy source as it was.
    #[test]
    fn a_failed_run_keeps_the_legacy_order() {
        for (file, broken) in [
            ("workspace-ordering.json", "{not json"),
            ("state.toml", "not = [toml"),
        ] {
            let dir = tempfile::tempdir().unwrap();
            seed(
                dir.path(),
                serde_json::json!([{"id": "a", "project_path": "/r/one", "sort_index": 0}]),
            );
            fs::write(dir.path().join(file), broken).unwrap();
            assert!(run_in(dir.path()).is_err(), "{file}");
            let rows = fs::read_to_string(dir.path().join("profiles/main/sessions.json")).unwrap();
            assert!(rows.contains("sort_index"), "{file}");
            assert_eq!(fs::read_to_string(dir.path().join(file)).unwrap(), broken);
        }
    }

    /// Only a missing file counts as absent; one that exists but cannot be read or parsed
    /// fails the run so the next start retries, without seeding or stripping anything.
    #[test]
    fn an_unreadable_input_fails_the_run_untouched() {
        for broken in [
            "profiles/other/sessions.json",
            "profiles/other/sessions.json:not json",
            "profiles/other/sessions.json:{}",
            "workspace-ordering.json",
        ] {
            let dir = tempfile::tempdir().unwrap();
            seed(
                dir.path(),
                serde_json::json!([{"id": "a", "project_path": "/r/one", "sort_index": 0}]),
            );
            let (file, content) = broken.split_once(':').unwrap_or((broken, ""));
            let path = dir.path().join(file);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            if content.is_empty() {
                // A directory in the file's place cannot be read but is not missing.
                fs::create_dir(&path).unwrap();
            } else {
                fs::write(&path, content).unwrap();
            }
            assert!(run_in(dir.path()).is_err(), "{broken}");
            let rows = fs::read_to_string(dir.path().join("profiles/main/sessions.json")).unwrap();
            assert!(rows.contains("sort_index"), "{broken}");
            let ordering = dir.path().join("workspace-ordering.json");
            assert!(!ordering.is_file(), "{broken}");
        }
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

    /// With no TUI or dashboard order, the removed sort v063 kept seeds one that shows the
    /// rows as that sort did, and is then dropped.
    #[test]
    fn seeds_the_order_the_removed_sort_showed() {
        let rows = serde_json::json!([
            {"id": "a", "project_path": "/r/a", "created_at": "2026-01-01T00:00:00Z",
             "last_accessed_at": "2026-03-01T00:00:00Z"},
            {"id": "b", "project_path": "/r/b", "created_at": "2026-01-03T00:00:00Z"},
            {"id": "c", "project_path": "/r/c", "created_at": "2026-01-02T00:00:00Z",
             "last_accessed_at": "2026-03-02T00:00:00.5Z"},
            // Two sessions on one branch rank by the newest and the oldest of them.
            {"id": "d1", "project_path": "/r/d", "created_at": "2026-01-04T00:00:00Z",
             "worktree_info": {"main_repo_path": "/r/d", "branch": "x"}},
            {"id": "d2", "project_path": "/r/d", "created_at": "2025-12-31T00:00:00Z",
             "last_accessed_at": "2026-03-03T00:00:00Z",
             "worktree_info": {"main_repo_path": "/r/d", "branch": "x"}},
            {"id": "e", "project_path": "/r/e", "created_at": "2025-01-01T00:00:00Z",
             "last_accessed_at": "2027-01-01T00:00:00Z", "archived_at": "2026-01-05T00:00:00Z"},
            {"id": "f", "project_path": "/r/f", "created_at": "2025-01-01T00:00:00Z",
             "trashed_at": "2026-01-05T00:00:00Z"},
        ]);
        let (a, b, c, d) = (
            "/r/a::__session__::a",
            "/r/b::__session__::b",
            "/r/c::__session__::c",
            "/r/d::x",
        );
        for (legacy, ordering, expected) in [
            (Some("oldest"), None, vec![d, a, c, b]),
            (Some("last_activity"), None, vec![d, c, a, b]),
            // Newest, Name and the dashboard's Manual leave Custom's default.
            (None, None, vec![]),
            // A dashboard order is never overwritten.
            (
                Some("oldest"),
                Some(r#"{"order": ["/r/web::main"]}"#),
                vec!["/r/web::main"],
            ),
        ] {
            let dir = tempfile::tempdir().unwrap();
            seed(dir.path(), rows.clone());
            let state = match legacy {
                Some(sort) => format!("{LEGACY_SORT_KEY} = \"{sort}\"\nsort_order = \"custom\"\n"),
                None => "sort_order = \"custom\"\n".to_string(),
            };
            fs::write(dir.path().join("state.toml"), state).unwrap();
            if let Some(ordering) = ordering {
                fs::write(dir.path().join("workspace-ordering.json"), ordering).unwrap();
            }

            run_in(dir.path()).unwrap();

            let seeded = if expected.is_empty() {
                assert!(
                    !dir.path().join("workspace-ordering.json").exists(),
                    "{legacy:?}"
                );
                Vec::new()
            } else {
                self::ordering(dir.path()).order
            };
            assert_eq!(seeded, expected, "{legacy:?} {ordering:?}");
            let state = fs::read_to_string(dir.path().join("state.toml")).unwrap();
            assert!(!state.contains(LEGACY_SORT_KEY), "{legacy:?}");
            assert!(state.contains("sort_order"), "{legacy:?}");

            // A second run over migrated state changes nothing.
            run_in(dir.path()).unwrap();
            assert_eq!(
                fs::read_to_string(dir.path().join("state.toml")).unwrap(),
                state
            );
            if !expected.is_empty() {
                assert_eq!(self::ordering(dir.path()).order, expected);
            }
        }
    }

    /// A TUI Custom order is what the user arranged by hand, so it is kept over a removed sort.
    #[test]
    fn a_tui_order_wins_over_the_removed_sort() {
        let dir = tempfile::tempdir().unwrap();
        seed(
            dir.path(),
            serde_json::json!([
                {"id": "a", "project_path": "/r/a", "sort_index": 0, "created_at": "2026-01-02T00:00:00Z"},
                {"id": "b", "project_path": "/r/b", "created_at": "2026-01-01T00:00:00Z"},
            ]),
        );
        fs::write(
            dir.path().join("state.toml"),
            format!("{LEGACY_SORT_KEY} = \"oldest\"\n"),
        )
        .unwrap();
        run_in(dir.path()).unwrap();
        assert_eq!(ordering(dir.path()).order, ["/r/a::__session__::a"]);
    }
}
