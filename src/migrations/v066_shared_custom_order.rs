//! Migration v066: one Custom order for every Client (#5896).
//!
//! The TUI kept its Custom order as a `sort_index` on each `sessions.json` row, and the
//! dashboard kept its own in `workspace-ordering.json` (rows) and in `state.toml`'s
//! `web_ui_state` (`aoe-repo-group-order-v1`, Repository headers). `workspace-ordering.json`
//! is now the one store. Where the dashboard has none, the sort v065 left in
//! `legacy_sort_order` seeds it, so the list keeps the order it had: the TUI order under
//! Custom, the removed sort's order under Oldest or Last activity, and nothing otherwise:
//! Newest first and Name are sorts of their own.
//! A `sort_index` left from an earlier Custom stint is not what the user saw, so it is
//! dropped unused. The dashboard's header order is copied, not removed, because the
//! dashboard still reads it. `sort_index` and `legacy_sort_order` are dropped only after
//! the shared order is written, so an interrupted run retries from them. An install that
//! ran an earlier v065 without `legacy_sort_order` falls back to the sort v065 left.
//! A `sessions.json` that cannot be read is left untouched and the run completes without
//! it; the sort and the skipped files are kept in `PENDING_FILE`, and a later run, `aoe
//! migrate` or a retried upgrade, adds only those profiles' order to the shared one. A run
//! that skips a profile records that before it writes, so an interrupted one still owes it.

use super::sessions_file;
use super::v065_shared_sidebar_view::{web_effective_sort, LEGACY_SORT_KEY, WEB_SORT_KEY};
use crate::time::Timestamp;
use anyhow::{Context, Result};
use serde_json::Value;
use std::cmp::Reverse;
use std::fs;
use std::path::Path;
use tracing::{info, warn};

const WEB_REPO_ORDER_KEY: &str = "aoe-repo-group-order-v1";
/// The legacy sort on its first line, then the `sessions.json` paths, relative to the app
/// directory, that still owe their order, or, while an upgrade is unfinished, the orders it
/// seeds, each after [`UNFINISHED`].
const PENDING_FILE: &str = ".v066_custom_order_pending";
const UNFINISHED: &str = "+";

pub fn run() -> Result<()> {
    run_in(&crate::session::get_app_dir()?, Pass::Upgrade)
}

/// Seed the order of profiles the upgrade skipped, once they read again.
pub(super) fn recover() -> Result<()> {
    run_in(&crate::session::get_app_dir()?, Pass::Recovery)
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Pass {
    Upgrade,
    /// Adds the rows a skipped profile owes to the order the upgrade wrote.
    Recovery,
}

/// A live row's place in each order v066 can seed.
struct LiveRow {
    workspace: String,
    repository: String,
    created: Option<Timestamp>,
    accessed: Option<Timestamp>,
}

fn run_in(app_dir: &Path, pass: Pass) -> Result<()> {
    let pending_path = app_dir.join(PENDING_FILE);
    let pending_file = read_if_present(&pending_path)?;
    let mut pending_lines = pending_file.as_deref().map(str::lines);
    let pending = pending_lines
        .as_mut()
        .and_then(Iterator::next)
        .map(|sort| sort.trim().to_owned());
    let lines: Option<Vec<&str>> = pending_lines.map(|lines| lines.map(str::trim).collect());
    // An unfinished upgrade reruns over every profile, into the orders it was seeding.
    let seeding: Option<Vec<&str>> = lines.as_ref().and_then(|lines| {
        let names: Vec<&str> = lines
            .iter()
            .filter_map(|line| line.strip_prefix(UNFINISHED))
            .collect();
        (!names.is_empty()).then_some(names)
    });
    // Profiles an earlier run migrated are done; only the skipped ones still seed.
    let owed = lines.filter(|_| seeding.is_none());
    if pass == Pass::Recovery && pending.is_none() {
        return Ok(());
    }
    let mut placed: Vec<(u64, Option<Timestamp>, String)> = Vec::new();
    let mut live: Vec<LiveRow> = Vec::new();
    let mut to_strip: Vec<(std::path::PathBuf, String)> = Vec::new();
    let mut skipped: Vec<String> = Vec::new();
    for path in sessions_file::session_files(app_dir)? {
        let relative = path
            .strip_prefix(app_dir)
            .unwrap_or(&path)
            .to_string_lossy()
            .into_owned();
        if owed
            .as_ref()
            .is_some_and(|owed| !owed.contains(&relative.as_str()))
        {
            continue;
        }
        let (content, rows) = match read_session_rows(&path) {
            SessionList::Rows(content, rows) => (content, rows),
            SessionList::Missing => continue,
            SessionList::Unreadable => {
                skipped.push(relative);
                continue;
            }
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
                placed.push((index, time("created_at"), workspace.clone()));
            }
            live.push(LiveRow {
                workspace,
                repository: repository_path(row).to_string(),
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
    let in_state = state
        .as_ref()
        .and_then(|state| state.get(LEGACY_SORT_KEY))
        .and_then(toml::Value::as_str)
        .map(str::to_owned);
    // A retried upgrade finds the marker gone but the sort still pending.
    let legacy_sort = match pass {
        Pass::Upgrade => in_state
            .clone()
            .or(pending)
            .or_else(|| sort_left_by_v065(state.as_ref())),
        Pass::Recovery => pending.clone(),
    };

    let ordering_path = app_dir.join("workspace-ordering.json");
    let mut ordering: crate::session::WorkspaceOrdering = read_if_present(&ordering_path)?
        .map(|content| serde_json::from_str(&content))
        .transpose()?
        .unwrap_or_default();
    let mut changed = false;
    let mut seeds: Vec<&str> = Vec::new();
    let legacy = legacy_sort
        .as_deref()
        .and_then(|sort| legacy_rank(live, sort))
        .unwrap_or_default();
    let legacy_repositories = dedup(legacy.iter().map(|row| row.repository.clone()));
    let seeded = match legacy_sort.as_deref() {
        Some("custom") => tui_order,
        _ => dedup(legacy.iter().map(|row| row.workspace.clone())),
    };
    let seeded_repositories = if web_repositories.is_empty() {
        legacy_repositories
    } else {
        web_repositories
    };
    for (name, current, seeded) in [
        ("order", &mut ordering.order, seeded),
        (
            "repositories",
            &mut ordering.repositories,
            seeded_repositories,
        ),
    ] {
        let next = match (pass, &owed, &seeding) {
            (Pass::Upgrade, None, None) if current.is_empty() => {
                seeds.push(name);
                seeded
            }
            (Pass::Upgrade, None, None) => continue,
            (_, None, Some(seeding)) if seeding.contains(&name) => {
                seeds.push(name);
                add_missing(current, &seeded)
            }
            (_, None, Some(_)) => continue,
            // Earlier entries keep their place, which the user may have moved since. A
            // retried upgrade adds the profiles an earlier run skipped the same way.
            _ => add_missing(current, &seeded),
        };
        if !next.is_empty() && next != *current {
            *current = next;
            changed = true;
        }
    }
    if let Some(sort) = legacy_sort
        .as_ref()
        .filter(|_| changed && owed.is_none() && !skipped.is_empty())
    {
        let seeds: String = seeds
            .iter()
            .map(|name| format!("{UNFINISHED}{name}\n"))
            .collect();
        crate::session::atomic_write(&pending_path, format!("{sort}\n{seeds}").as_bytes())?;
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
    // `state.toml` drops keys this binary does not know on its next save, so the pending
    // sort lives in its own file.
    match &legacy_sort {
        Some(sort) if !skipped.is_empty() => crate::session::atomic_write(
            &pending_path,
            format!("{sort}\n{}\n", skipped.join("\n")).as_bytes(),
        )?,
        _ if pending_file.is_some() => fs::remove_file(&pending_path)?,
        _ => {}
    }
    if in_state.is_some() {
        super::config_file::rewrite_strict(&app_dir.join("state.toml"), "v066", |state| {
            state.remove(LEGACY_SORT_KEY).is_some()
        })?;
    }
    Ok(())
}

/// The sort an earlier v065, which kept no `legacy_sort_order`, left: the dashboard's
/// pick while its key remains, else Custom only when that was the TUI's sort.
fn sort_left_by_v065(state: Option<&toml::Table>) -> Option<String> {
    let state = state?;
    let web = state
        .get("web_ui_state")
        .and_then(|web| web.get(WEB_SORT_KEY))
        .and_then(toml::Value::as_str);
    match web {
        Some(web) => Some(web_effective_sort(web).to_owned()),
        None => (state.get("sort_order").and_then(toml::Value::as_str) == Some("custom"))
            .then(|| "custom".to_owned()),
    }
}

/// `current` with each `seeded` entry it lacks placed after the entry it follows in
/// `seeded`, or first when none does.
fn add_missing(current: &[String], seeded: &[String]) -> Vec<String> {
    let mut merged = current.to_vec();
    let mut previous: Option<usize> = None;
    for entry in seeded {
        previous = Some(match merged.iter().position(|e| e == entry) {
            Some(at) => at,
            None => {
                let at = previous.map_or(0, |at| at + 1);
                merged.insert(at, entry.clone());
                at
            }
        });
    }
    merged
}

/// Live rows in the order the removed sort `sort` showed them, or `None` for a sort with
/// nothing to seed; see v065's `tui_effective_sort`. Deduplicated, a workspace or
/// repository then ranks by its newest session for last activity and by its oldest for
/// oldest.
fn legacy_rank(mut live: Vec<LiveRow>, sort: &str) -> Option<Vec<LiveRow>> {
    match sort {
        "oldest" => live.sort_by_key(|row| (row.created.is_none(), row.created)),
        "last_activity" => live.sort_by_key(|row| {
            (
                row.accessed.is_none(),
                Reverse(row.accessed),
                Reverse(row.created),
            )
        }),
        _ => return None,
    }
    Some(live)
}

enum SessionList {
    Rows(String, Vec<Value>),
    Missing,
    /// Left untouched for the recovery that owns it.
    Unreadable,
}

fn read_session_rows(path: &Path) -> SessionList {
    let content = match read_if_present(path) {
        Ok(Some(content)) => content,
        Ok(None) => return SessionList::Missing,
        Err(error) => {
            warn!(target: "migrations.v066", path = %path.display(), %error, "Preserved unreadable session list; repair it and run aoe migrate");
            return SessionList::Unreadable;
        }
    };
    match serde_json::from_str::<Value>(&content) {
        Ok(Value::Array(rows)) => SessionList::Rows(content, rows),
        Ok(_) => {
            warn!(target: "migrations.v066", path = %path.display(), "Preserved a file that is not a session list; repair it and run aoe migrate");
            SessionList::Unreadable
        }
        Err(error) => {
            warn!(target: "migrations.v066", path = %path.display(), %error, "Preserved a session list that does not parse; repair it and run aoe migrate");
            SessionList::Unreadable
        }
    }
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
    let repo = repository_path(row);
    match worktree(row) {
        Some(worktree) => format!("{repo}::{}", text(worktree.get("branch"))),
        None => format!("{repo}::__session__::{}", text(row.get("id"))),
    }
}

/// The repository a row's workspace belongs to: its worktree's main repo, else its project.
fn repository_path(row: &serde_json::Map<String, Value>) -> &str {
    match worktree(row) {
        Some(worktree) => text(worktree.get("main_repo_path")),
        None => text(row.get("project_path")),
    }
    .trim_end_matches('/')
}

fn worktree(row: &serde_json::Map<String, Value>) -> Option<&Value> {
    row.get("worktree_info").filter(|w| !w.is_null())
}

fn text(value: Option<&Value>) -> &str {
    value.and_then(Value::as_str).unwrap_or_default()
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
                // Tied, so the later instant leads, though its text sorts first.
                {"id": "e", "project_path": "/r/five", "sort_index": 2, "created_at": "2026-01-01T05:00:00+05:00"},
                {"id": "f", "project_path": "/r/six", "sort_index": 2, "created_at": "2026-01-01T01:00:00Z"},
            ]),
        );
        fs::write(
            dir.path().join("state.toml"),
            format!("{LEGACY_SORT_KEY} = \"custom\"\n[web_ui_state]\n\"aoe-repo-group-order-v1\" = '[\"/r/two\",\"/r/one\"]'\n"),
        )
        .unwrap();

        run_in(dir.path(), Pass::Upgrade).unwrap();

        let ordering = ordering(dir.path());
        assert_eq!(
            ordering.order,
            [
                "/r/two::feat",
                "/r/one::__session__::a",
                "/r/six::__session__::f",
                "/r/five::__session__::e",
            ]
        );
        assert_eq!(ordering.repositories, ["/r/two", "/r/one"]);
        let rows = fs::read_to_string(dir.path().join("profiles/main/sessions.json")).unwrap();
        assert!(!rows.contains("sort_index"));
        let state = fs::read_to_string(dir.path().join("state.toml")).unwrap();
        assert!(
            state.contains(WEB_REPO_ORDER_KEY),
            "the dashboard still reads it"
        );

        // A second run over migrated state changes nothing.
        run_in(dir.path(), Pass::Upgrade).unwrap();
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
            assert!(run_in(dir.path(), Pass::Upgrade).is_err(), "{file}");
            let rows = fs::read_to_string(dir.path().join("profiles/main/sessions.json")).unwrap();
            assert!(rows.contains("sort_index"), "{file}");
            assert_eq!(fs::read_to_string(dir.path().join(file)).unwrap(), broken);
        }
    }

    /// A session list that cannot be read is skipped and left for its recovery, while the
    /// other profiles migrate; once repaired, recovery adds its order. An unreadable
    /// `workspace-ordering.json` fails the run untouched, since writing over it could lose
    /// an order.
    #[test]
    fn an_unreadable_input_is_left_untouched() {
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
            fs::write(
                dir.path().join("state.toml"),
                format!("{LEGACY_SORT_KEY} = \"custom\"\n"),
            )
            .unwrap();
            let (file, content) = broken.split_once(':').unwrap_or((broken, ""));
            let path = dir.path().join(file);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            if content.is_empty() {
                // A directory in the file's place cannot be read but is not missing.
                fs::create_dir(&path).unwrap();
            } else {
                fs::write(&path, content).unwrap();
            }
            let read_back = || match content {
                "" => path.is_dir(),
                content => fs::read_to_string(&path).unwrap() == content,
            };
            let rows =
                || fs::read_to_string(dir.path().join("profiles/main/sessions.json")).unwrap();
            if file == "workspace-ordering.json" {
                assert!(run_in(dir.path(), Pass::Upgrade).is_err(), "{broken}");
                assert!(rows().contains("sort_index"), "{broken}");
                assert!(read_back(), "{broken}");
                continue;
            }
            run_in(dir.path(), Pass::Upgrade).unwrap();
            assert!(!rows().contains("sort_index"), "{broken}");
            assert_eq!(
                ordering(dir.path()).order,
                ["/r/one::__session__::a"],
                "{broken}"
            );
            assert!(read_back(), "{broken}");
            let pending = dir.path().join(PENDING_FILE);
            assert_eq!(
                fs::read_to_string(&pending).unwrap(),
                "custom\nprofiles/other/sessions.json\n",
                "{broken}"
            );
            assert!(!fs::read_to_string(dir.path().join("state.toml"))
                .unwrap()
                .contains(LEGACY_SORT_KEY));

            // Still unreadable, recovery waits; repaired, its rows join the shared order.
            run_in(dir.path(), Pass::Recovery).unwrap();
            assert!(pending.exists(), "{broken}");
            if path.is_dir() {
                fs::remove_dir(&path).unwrap();
            }
            fs::write(
                &path,
                serde_json::json!([
                    {"id": "b", "project_path": "/r/two", "sort_index": 1},
                    {"id": "c", "project_path": "/r/three", "sort_index": 0},
                ])
                .to_string(),
            )
            .unwrap();
            run_in(dir.path(), Pass::Recovery).unwrap();
            assert_eq!(
                ordering(dir.path()).order,
                [
                    "/r/three::__session__::c",
                    "/r/two::__session__::b",
                    "/r/one::__session__::a"
                ],
                "{broken}"
            );
            assert!(!fs::read_to_string(&path).unwrap().contains("sort_index"));
            assert!(!pending.exists(), "{broken}");
            run_in(dir.path(), Pass::Recovery).unwrap();
        }
    }

    /// An upgrade that ran but was not recorded runs again without the marker; the pending
    /// sort, not the folded `sort_order`, is still the one a skipped profile owes.
    #[test]
    fn a_rerun_upgrade_keeps_the_pending_sort() {
        let dir = tempfile::tempdir().unwrap();
        seed(dir.path(), serde_json::json!([]));
        fs::create_dir_all(dir.path().join("profiles/other/sessions.json")).unwrap();
        fs::write(
            dir.path().join("state.toml"),
            format!("sort_order = \"custom\"\n{LEGACY_SORT_KEY} = \"oldest\"\n"),
        )
        .unwrap();
        let pending = dir.path().join(PENDING_FILE);
        for _ in 0..2 {
            run_in(dir.path(), Pass::Upgrade).unwrap();
            assert_eq!(
                fs::read_to_string(&pending).unwrap(),
                "oldest\nprofiles/other/sessions.json\n"
            );
        }
    }

    /// An upgrade that skipped a profile and stopped after any of its writes still seeds
    /// that profile once it reads, and finishes the profiles it had not stripped.
    #[test]
    fn an_interrupted_upgrade_still_owes_the_skipped_profile() {
        let (a, b) = ("/r/a::__session__::a", "/r/b::__session__::b");
        let indexed = serde_json::json!([{"id": "a", "project_path": "/r/a", "sort_index": 0}]);
        let stripped = serde_json::json!([{"id": "a", "project_path": "/r/a"}]);
        // Each stop: whether the shared order, the stripped rows and the final record landed.
        for (written, strip, recorded) in [
            (false, false, false),
            (true, false, false),
            (true, true, false),
            (true, true, true),
        ] {
            let dir = tempfile::tempdir().unwrap();
            let other = dir.path().join("profiles/other/sessions.json");
            fs::create_dir_all(&other).unwrap();
            seed(dir.path(), indexed.clone());
            fs::write(
                dir.path().join("state.toml"),
                format!("{LEGACY_SORT_KEY} = \"custom\"\n"),
            )
            .unwrap();
            run_in(dir.path(), Pass::Upgrade).unwrap();
            let pending = dir.path().join(PENDING_FILE);
            if !recorded {
                fs::write(&pending, "custom\n+order\n").unwrap();
            }
            if !written {
                fs::remove_file(dir.path().join("workspace-ordering.json")).unwrap();
            }
            seed(
                dir.path(),
                if strip {
                    stripped.clone()
                } else {
                    indexed.clone()
                },
            );
            fs::write(
                dir.path().join("state.toml"),
                format!("{LEGACY_SORT_KEY} = \"custom\"\n"),
            )
            .unwrap();
            fs::remove_dir(&other).unwrap();
            fs::write(
                &other,
                serde_json::json!([{"id": "b", "project_path": "/r/b", "sort_index": 1}])
                    .to_string(),
            )
            .unwrap();

            run_in(dir.path(), Pass::Upgrade).unwrap();

            let mut order = ordering(dir.path()).order;
            order.sort();
            assert_eq!(order, [a, b], "{written} {strip} {recorded}");
            for profile in ["main", "other"] {
                let rows = dir.path().join(format!("profiles/{profile}/sessions.json"));
                assert!(!fs::read_to_string(rows).unwrap().contains("sort_index"));
            }
            assert!(!pending.exists(), "{written} {strip} {recorded}");
        }
    }

    /// A run after one that skipped a profile seeds from that profile alone, whether it is
    /// `aoe migrate` or a retried upgrade: the repaired profile joins the stored order
    /// before its `sort_index` goes, and a profile that already migrated adds nothing.
    #[test]
    fn a_later_run_seeds_only_the_skipped_profiles() {
        for pass in [Pass::Upgrade, Pass::Recovery] {
            let dir = tempfile::tempdir().unwrap();
            seed(
                dir.path(),
                serde_json::json!([{"id": "a", "project_path": "/r/a", "sort_index": 0,
                    "created_at": "2026-01-01T00:00:00Z"}]),
            );
            let other = dir.path().join("profiles/other/sessions.json");
            fs::create_dir_all(&other).unwrap();
            fs::write(
                dir.path().join("state.toml"),
                format!("{LEGACY_SORT_KEY} = \"oldest\"\n"),
            )
            .unwrap();
            run_in(dir.path(), Pass::Upgrade).unwrap();
            assert_eq!(ordering(dir.path()).order, ["/r/a::__session__::a"]);

            // A session created since in the migrated profile stays unplaced.
            seed(
                dir.path(),
                serde_json::json!([
                    {"id": "a", "project_path": "/r/a", "created_at": "2026-01-01T00:00:00Z"},
                    {"id": "n", "project_path": "/r/n", "created_at": "2025-01-01T00:00:00Z"},
                ]),
            );
            fs::remove_dir(&other).unwrap();
            fs::write(
                &other,
                serde_json::json!([{"id": "b", "project_path": "/r/b", "sort_index": 0,
                    "created_at": "2026-01-02T00:00:00Z"}])
                .to_string(),
            )
            .unwrap();
            run_in(dir.path(), pass).unwrap();
            assert_eq!(
                ordering(dir.path()).order,
                ["/r/b::__session__::b", "/r/a::__session__::a"]
            );
            assert!(!fs::read_to_string(&other).unwrap().contains("sort_index"));
            assert!(!dir.path().join(PENDING_FILE).exists());
        }
    }

    /// An install whose v065 kept no `legacy_sort_order` seeds from the sort v065 left:
    /// the TUI order under Custom, the dashboard's pick while its key remains.
    #[test]
    fn an_earlier_v065_without_the_marker_keeps_the_custom_order() {
        let (a, b) = ("/r/a::__session__::a", "/r/b::__session__::b");
        for (state, expected) in [
            ("sort_order = \"custom\"\n", vec![a]),
            ("sort_order = \"name\"\n", vec![]),
            (
                "sort_order = \"custom\"\n[web_ui_state]\n\"aoe-sidebar-sort-mode\" = \"lastActivity\"\n",
                vec![b, a],
            ),
        ] {
            let dir = tempfile::tempdir().unwrap();
            seed(
                dir.path(),
                serde_json::json!([
                    {"id": "a", "project_path": "/r/a", "sort_index": 0,
                     "last_accessed_at": "2026-01-01T00:00:00Z"},
                    {"id": "b", "project_path": "/r/b", "last_accessed_at": "2026-01-02T00:00:00Z"},
                ]),
            );
            fs::write(dir.path().join("state.toml"), state).unwrap();
            run_in(dir.path(), Pass::Upgrade).unwrap();
            let seeded = if expected.is_empty() {
                assert!(!dir.path().join("workspace-ordering.json").exists(), "{state}");
                Vec::new()
            } else {
                ordering(dir.path()).order
            };
            assert_eq!(seeded, expected, "{state}");
            assert_eq!(fs::read_to_string(dir.path().join("state.toml")).unwrap(), state);
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
        fs::write(
            dir.path().join("state.toml"),
            format!("{LEGACY_SORT_KEY} = \"custom\"\n"),
        )
        .unwrap();
        run_in(dir.path(), Pass::Upgrade).unwrap();
        assert_eq!(ordering(dir.path()).order, ["/r/web::main"]);
    }

    /// With no TUI or dashboard order, the removed sort v065 kept seeds one that shows the
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
        let repos = |ids: &[&str]| -> Vec<String> {
            ids.iter()
                .map(|id| id.split("::").next().unwrap().to_string())
                .collect()
        };
        for (legacy, ordering, expected, repositories) in [
            (Some("oldest"), None, vec![d, a, c, b], repos(&[d, a, c, b])),
            (
                Some("last_activity"),
                None,
                vec![d, c, a, b],
                repos(&[d, c, a, b]),
            ),
            // Newest, Name and the dashboard's Manual leave Custom's default.
            (None, None, vec![], vec![]),
            // A dashboard order is never overwritten, rows or headers.
            (
                Some("oldest"),
                Some(r#"{"order": ["/r/web::main"], "repositories": ["/r/web"]}"#),
                vec!["/r/web::main"],
                vec!["/r/web".to_string()],
            ),
            // Headers seed on their own when only the rows were arranged.
            (
                Some("last_activity"),
                Some(r#"{"order": ["/r/web::main"]}"#),
                vec!["/r/web::main"],
                repos(&[d, c, a, b]),
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

            run_in(dir.path(), Pass::Upgrade).unwrap();

            let seeded = if expected.is_empty() {
                assert!(
                    !dir.path().join("workspace-ordering.json").exists(),
                    "{legacy:?}"
                );
                Default::default()
            } else {
                self::ordering(dir.path())
            };
            assert_eq!(seeded.order, expected, "{legacy:?} {ordering:?}");
            assert_eq!(seeded.repositories, repositories, "{legacy:?} {ordering:?}");
            let state = fs::read_to_string(dir.path().join("state.toml")).unwrap();
            assert!(!state.contains(LEGACY_SORT_KEY), "{legacy:?}");
            assert!(state.contains("sort_order"), "{legacy:?}");

            // A second run over migrated state changes nothing.
            run_in(dir.path(), Pass::Upgrade).unwrap();
            assert_eq!(
                fs::read_to_string(dir.path().join("state.toml")).unwrap(),
                state
            );
            if !expected.is_empty() {
                let again = self::ordering(dir.path());
                assert_eq!(again.order, expected);
                assert_eq!(again.repositories, repositories);
            }
        }
    }

    /// `sort_index` seeds the order only when Custom was the sort on screen; a stale one
    /// from an earlier Custom stint gives way to the sort the user had.
    #[test]
    fn a_tui_order_seeds_only_under_custom() {
        let (a, b) = ("/r/a::__session__::a", "/r/b::__session__::b");
        for (legacy, web_order, expected) in [
            ("custom", None, vec![a]),
            ("newest", None, vec![]),
            ("oldest", None, vec![b, a]),
            (
                "custom",
                Some(r#"{"order": ["/r/web::main"]}"#),
                vec!["/r/web::main"],
            ),
        ] {
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
                format!("{LEGACY_SORT_KEY} = \"{legacy}\"\n"),
            )
            .unwrap();
            if let Some(order) = web_order {
                fs::write(dir.path().join("workspace-ordering.json"), order).unwrap();
            }
            run_in(dir.path(), Pass::Upgrade).unwrap();
            let seeded = if expected.is_empty() {
                assert!(
                    !dir.path().join("workspace-ordering.json").exists(),
                    "{legacy}"
                );
                Vec::new()
            } else {
                ordering(dir.path()).order
            };
            assert_eq!(seeded, expected, "{legacy} {web_order:?}");
            let rows = fs::read_to_string(dir.path().join("profiles/main/sessions.json")).unwrap();
            assert!(!rows.contains("sort_index"), "{legacy}");
        }
    }
}
