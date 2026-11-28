//! Migration v068: one grouping and sort for every Client (#5896).
//!
//! `state.toml` held the TUI's `group_by` and `sort_order`, and its `web_ui_state` table
//! held the dashboard's own `aoe-sidebar-axis` and `aoe-sidebar-sort-mode`. Both pairs fold
//! into `group_by` and `sort_order`, a value picked on the web winning, and the removed
//! choices map to the nearest kept one. A TUI user who never picked a grouping keeps
//! Groups, the TUI's old default; everyone else gets the new Repository default. The
//! org and status collapse lists go with their groupings. The web keys stay, since the
//! dashboard still reads them.

use anyhow::{Context, Result};
use std::fs;
use std::io;
use std::path::Path;
use tracing::info;

const WEB_AXIS_KEY: &str = "aoe-sidebar-axis";
const WEB_SORT_KEY: &str = "aoe-sidebar-sort-mode";

pub fn run() -> Result<()> {
    run_in(&crate::session::get_app_dir()?, false)
}

pub(super) fn reconcile() -> Result<()> {
    run_in(&crate::session::get_app_dir()?, true)
}

fn run_in(app_dir: &Path, reconcile: bool) -> Result<()> {
    let path = app_dir.join("state.toml");
    let content = match fs::read_to_string(&path) {
        Ok(content) => content,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.into()),
    };
    let mut state: toml::Table = content.parse().with_context(|| {
        let version = if reconcile { "v069" } else { "v068" };
        format!(
            "Failed to parse {} during {version} migration",
            path.display()
        )
    })?;
    if !migrate(&mut state, reconcile) {
        return Ok(());
    }
    crate::session::backup_before_migration(&path)?;
    crate::session::atomic_write(&path, toml::to_string_pretty(&state)?.as_bytes())
}

fn migrate(state: &mut toml::Table, reconcile: bool) -> bool {
    let tui_group = state.get("group_by").and_then(toml::Value::as_str);
    let tui_sort = state.get("sort_order").and_then(toml::Value::as_str);
    let removed_collapse =
        state.contains_key("org_group_collapsed") || state.contains_key("status_group_collapsed");
    // A v068 marker is ambiguous. Retained web mirrors may predate a shared pick.
    let preserve_shared = reconcile
        && tui_group.is_none_or(|mode| matches!(mode, "group" | "project"))
        && tui_sort.is_none_or(|order| matches!(order, "custom" | "newest" | "name"))
        && !removed_collapse;
    if preserve_shared && (tui_group == Some("group") || tui_sort == Some("name")) {
        return false;
    }
    let web = state.get("web_ui_state").and_then(toml::Value::as_table);
    let web_pref = |key| {
        web.and_then(|table| table.get(key))
            .and_then(toml::Value::as_str)
    };
    let group = tui_group
        .filter(|_| preserve_shared)
        .map(tui_group_by)
        .or_else(|| web_pref(WEB_AXIS_KEY).and_then(web_group))
        .or_else(|| tui_group.map(tui_group_by))
        .or_else(|| {
            let seen_tui = state
                .get("has_seen_welcome")
                .and_then(toml::Value::as_bool)
                .unwrap_or(false);
            seen_tui.then_some("group")
        });
    // The web offered Manual, Last activity and Attention; none of them orders by name.
    let sort = tui_sort
        .filter(|_| preserve_shared)
        .map(tui_sort_order)
        .or_else(|| {
            web_pref(WEB_SORT_KEY)
                .filter(|mode| matches!(*mode, "manual" | "lastActivity" | "attention"))
                .map(|_| "custom")
        })
        .or_else(|| tui_sort.map(tui_sort_order));

    if group == tui_group
        && sort == tui_sort
        && !(group.is_none() && state.contains_key("group_by"))
        && !(sort.is_none() && state.contains_key("sort_order"))
        && !removed_collapse
    {
        return false;
    }
    match group {
        Some(group) => state.insert("group_by".into(), group.into()),
        None => state.remove("group_by"),
    };
    match sort {
        Some(sort) => state.insert("sort_order".into(), sort.into()),
        None => state.remove("sort_order"),
    };
    state.remove("org_group_collapsed");
    state.remove("status_group_collapsed");

    info!(target: "migrations.sidebar_view", ?group, ?sort, "folded the sidebar grouping and sort into one shared value");
    true
}

/// An axis the web never wrote is not a pick, so it leaves the TUI's grouping in place.
fn web_group(axis: &str) -> Option<&'static str> {
    match axis {
        "group" => Some("group"),
        "repo" | "repo+group" | "org" | "status" => Some("project"),
        _ => None,
    }
}

fn tui_group_by(mode: &str) -> &'static str {
    match mode {
        "manual" | "group" => "group",
        _ => "project",
    }
}

fn tui_sort_order(order: &str) -> &'static str {
    match order {
        "a_z" | "az" | "z_a" | "za" | "name" => "name",
        "newest" => "newest",
        _ => "custom",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run_on(state: &str) -> String {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.toml");
        std::fs::write(&path, state).unwrap();
        run_in(dir.path(), false).unwrap();
        std::fs::read_to_string(&path).unwrap()
    }

    fn view(out: &str) -> (Option<String>, Option<String>, Option<toml::Table>) {
        let table: toml::Table = out.parse().unwrap();
        let get = |key: &str| table.get(key).and_then(|v| v.as_str()).map(str::to_owned);
        (
            get("group_by"),
            get("sort_order"),
            table
                .get("web_ui_state")
                .and_then(|v| v.as_table())
                .cloned(),
        )
    }

    #[test]
    fn folds_both_clients_into_one_value() {
        let some = |s: &str| Some(s.to_string());
        let cases = [
            // Nothing picked: a TUI user keeps Groups, a new or web-only user gets defaults.
            ("has_seen_welcome = true\n", some("group"), None),
            ("has_seen_welcome = false\n", None, None),
            // The TUI's removed choices map to the nearest kept one.
            ("group_by = \"manual\"\nsort_order = \"newest\"\n", some("group"), some("newest")),
            ("group_by = \"manual\"\nsort_order = \"oldest\"\n", some("group"), some("custom")),
            ("group_by = \"org\"\nsort_order = \"attention\"\n", some("project"), some("custom")),
            ("group_by = \"status\"\nsort_order = \"z_a\"\n", some("project"), some("name")),
            ("group_by = \"project\"\nsort_order = \"a_z\"\n", some("project"), some("name")),
            // A web pick wins over the TUI's.
            (
                "group_by = \"project\"\nsort_order = \"a_z\"\n[web_ui_state]\n\"aoe-sidebar-axis\" = \"group\"\n\"aoe-sidebar-sort-mode\" = \"lastActivity\"\n",
                some("group"),
                some("custom"),
            ),
            (
                "group_by = \"manual\"\n[web_ui_state]\n\"aoe-sidebar-axis\" = \"repo+group\"\n",
                some("project"),
                None,
            ),
            // A value the web never wrote is not a pick.
            (
                "group_by = \"manual\"\nsort_order = \"a_z\"\n[web_ui_state]\n\"aoe-sidebar-axis\" = \"future\"\n\"aoe-sidebar-sort-mode\" = \"future\"\n",
                some("group"),
                some("name"),
            ),
        ];
        for (input, group, sort) in cases {
            let (g, s, _) = view(&run_on(input));
            assert_eq!((g, s), (group, sort), "{input}");
        }
    }

    #[test]
    fn drops_removed_collapse_lists_but_keeps_the_rest() {
        let out = run_on(
            "org_group_collapsed = [\"acme\"]\nstatus_group_collapsed = [\"Idle\"]\nproject_group_collapsed = [\"api\"]\n[web_ui_state]\n\"aoe-sidebar-axis\" = \"org\"\n\"aoe-repo-collapsed-x\" = \"1\"\n",
        );
        let table: toml::Table = out.parse().unwrap();
        assert!(table.get("org_group_collapsed").is_none());
        assert!(table.get("status_group_collapsed").is_none());
        assert!(table.get("project_group_collapsed").is_some());
        let (_, _, web) = view(&out);
        let web = web.unwrap();
        assert_eq!(web.get(WEB_AXIS_KEY).and_then(|v| v.as_str()), Some("org"));
        assert_eq!(
            web.get("aoe-repo-collapsed-x").and_then(|v| v.as_str()),
            Some("1")
        );
        // A second run over migrated state changes nothing.
        assert_eq!(run_on(&out), out);
    }
}
