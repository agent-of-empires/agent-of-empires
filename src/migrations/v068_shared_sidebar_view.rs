//! Migration v068: one grouping and sort for every Client (#5896).
//!
//! `state.toml` held the TUI's `group_by` and `sort_order`, and its `web_ui_state` table
//! held the dashboard's own `aoe-sidebar-axis` and `aoe-sidebar-sort-mode`. Both pairs fold
//! into `group_by` and `sort_order`, a value picked on the web winning, and the removed
//! choices map to the nearest kept one. A TUI user who never picked a grouping keeps
//! Groups, the TUI's old default; everyone else gets the new Repository default. The
//! org and status collapse lists go with their groupings. The web keys stay, since the
//! dashboard still reads them. The sort in effect before the fold is kept in
//! `legacy_sort_order` so v065 can seed the Custom order that matches it; v065 removes it.

use anyhow::Result;
use std::path::Path;
use tracing::info;

const WEB_AXIS_KEY: &str = "aoe-sidebar-axis";
const WEB_SORT_KEY: &str = "aoe-sidebar-sort-mode";
pub(super) const LEGACY_SORT_KEY: &str = "legacy_sort_order";

pub fn run() -> Result<()> {
    run_in(&crate::session::get_app_dir()?)
}

fn run_in(app_dir: &Path) -> Result<()> {
    let path = app_dir.join("state.toml");
    if !path.exists() {
        return Ok(());
    }
    crate::session::backup_before_migration(&path)?;
    super::config_file::rewrite_strict(&path, "v068", migrate)
}

fn migrate(state: &mut toml::Table) -> bool {
    let before = state.clone();
    let web = state.get("web_ui_state").and_then(toml::Value::as_table);
    let web_axis = web
        .and_then(|t| t.get(WEB_AXIS_KEY))
        .and_then(toml::Value::as_str)
        .map(str::to_owned);
    // A sort the web never wrote is not a pick, so it leaves the TUI's sort in place.
    let web_sort = web
        .and_then(|t| t.get(WEB_SORT_KEY))
        .and_then(toml::Value::as_str)
        .filter(|mode| matches!(*mode, "manual" | "lastActivity" | "attention"))
        .map(str::to_owned);

    let tui_group = state.get("group_by").and_then(toml::Value::as_str);
    let group = web_axis
        .as_deref()
        .and_then(web_group)
        .or_else(|| tui_group.map(tui_group_by))
        .or_else(|| {
            let seen_tui = state
                .get("has_seen_welcome")
                .and_then(toml::Value::as_bool)
                .unwrap_or(false);
            seen_tui.then_some("group")
        });
    let tui_sort = state.get("sort_order").and_then(toml::Value::as_str);
    // The web offered Manual, Last activity and Attention; none of them orders by name.
    let sort = web_sort
        .as_ref()
        .map(|_| "custom")
        .or_else(|| tui_sort.map(tui_sort_order));
    let legacy = match web_sort.as_deref() {
        Some(web) => web_effective_sort(web),
        None => tui_effective_sort(tui_sort.unwrap_or("newest")),
    };

    match group {
        Some(group) => state.insert("group_by".into(), group.into()),
        None => state.remove("group_by"),
    };
    match sort {
        Some(sort) => state.insert("sort_order".into(), sort.into()),
        None => state.remove("sort_order"),
    };
    state
        .entry(LEGACY_SORT_KEY)
        .or_insert_with(|| legacy.into());
    state.remove("org_group_collapsed");
    state.remove("status_group_collapsed");

    let changed = *state != before;
    if changed {
        info!(target: "migrations.v068", ?group, ?sort, "folded the sidebar grouping and sort into one shared value");
    }
    changed
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

/// The sort a TUI user saw, for v065. Only `custom` means its `sort_index` was the order
/// on screen. Attention ranked rows by live status, which no stored order can follow, so
/// it becomes last activity, the stable order closest to it.
fn tui_effective_sort(sort: &str) -> &'static str {
    match sort {
        "custom" => "custom",
        "oldest" => "oldest",
        "last_activity" | "attention" => "last_activity",
        "a_z" | "az" | "z_a" | "za" | "name" => "name",
        _ => "newest",
    }
}

/// The sort a web pick showed, for v065. Manual's drag order is already in the shared store.
fn web_effective_sort(sort: &str) -> &'static str {
    match sort {
        "lastActivity" | "attention" => "last_activity",
        _ => "manual",
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
        run_in(dir.path()).unwrap();
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
    fn keeps_the_effective_sort_for_v065() {
        let web = |sort: &str| format!("[web_ui_state]\n\"aoe-sidebar-sort-mode\" = \"{sort}\"\n");
        for (input, legacy) in [
            ("sort_order = \"oldest\"\n".to_string(), Some("oldest")),
            (
                "sort_order = \"last_activity\"\n".into(),
                Some("last_activity"),
            ),
            ("sort_order = \"attention\"\n".into(), Some("last_activity")),
            ("sort_order = \"newest\"\n".into(), Some("newest")),
            ("has_seen_welcome = true\n".into(), Some("newest")),
            ("sort_order = \"custom\"\n".into(), Some("custom")),
            ("sort_order = \"a_z\"\n".into(), Some("name")),
            (web("lastActivity"), Some("last_activity")),
            (web("attention"), Some("last_activity")),
            // The web's pick wins over the TUI's.
            (
                format!("sort_order = \"custom\"\n{}", web("manual")),
                Some("manual"),
            ),
        ] {
            let out = run_on(&input);
            let table: toml::Table = out.parse().unwrap();
            assert_eq!(
                table.get(LEGACY_SORT_KEY).and_then(toml::Value::as_str),
                legacy,
                "{input}"
            );
            assert_eq!(run_on(&out), out, "a second run changes nothing: {input}");
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
