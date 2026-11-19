//! Profile-scoped queries and writes: items, dismissals, presets, mappings,
//! checks and the snapshot read.

use super::*;

/// Drops what a provider's worker published, in every profile, so a
/// reinstalled or changed plugin starts `pending`. Sources, dismissals,
/// presets and mappings are user state and stay.
pub(crate) fn forget_publication(conn: &Connection, provider_id: &str) -> rusqlite::Result<()> {
    let tx = conn.unchecked_transaction()?;
    for table in [
        "suggestion_fixes",
        "suggestion_checks",
        "suggestion_stale",
        "suggestion_items",
        "suggestion_providers",
    ] {
        tx.execute(
            &format!("DELETE FROM {table} WHERE provider_id = ?1"),
            [provider_id],
        )?;
    }
    tx.commit()
}

/// The worker method behind the provider's current error fix, if any.
pub(crate) fn fix_method(
    conn: &Connection,
    profile: &str,
    provider_id: &str,
) -> rusqlite::Result<Option<String>> {
    conn.query_row(
        "SELECT method FROM suggestion_fixes WHERE profile = ?1 AND provider_id = ?2",
        params![profile, provider_id],
        |row| row.get(0),
    )
    .optional()
}

fn load_checks(
    conn: &Connection,
    profile: &str,
    provider_id: &str,
) -> anyhow::Result<Vec<SetupCheck>> {
    let stored: Option<String> = conn
        .query_row(
            "SELECT checks_json FROM suggestion_checks WHERE profile = ?1 AND provider_id = ?2",
            params![profile, provider_id],
            |row| row.get(0),
        )
        .optional()?;
    Ok(stored
        .and_then(|json| match serde_json::from_str(&json) {
            Ok(checks) => Some(checks),
            Err(e) => {
                tracing::warn!(
                    target: "plugin.suggestions",
                    provider = %provider_id,
                    "ignoring undecodable stored checks: {e}"
                );
                None
            }
        })
        .unwrap_or_default())
}

pub(super) fn record_status(
    conn: &Connection,
    profile: &str,
    provider_id: &str,
    error: Option<&str>,
) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT INTO suggestion_providers (profile, provider_id, error, updated_at)
         VALUES (?1, ?2, ?3, ?4)
         ON CONFLICT (profile, provider_id) DO UPDATE SET error = ?3, updated_at = ?4",
        params![profile, provider_id, error, chrono::Utc::now().timestamp()],
    )?;
    // A new status replaces the old error's fix; `error` stores its own afterwards.
    conn.execute(
        "DELETE FROM suggestion_fixes WHERE profile = ?1 AND provider_id = ?2",
        params![profile, provider_id],
    )?;
    Ok(())
}

pub(crate) fn set_source(
    conn: &Connection,
    profile: &str,
    provider_id: &str,
    criterion: &str,
    enabled: bool,
) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT INTO suggestion_sources (profile, provider_id, criterion, enabled)
         VALUES (?1, ?2, ?3, ?4)
         ON CONFLICT (profile, provider_id, criterion) DO UPDATE SET enabled = ?4",
        params![profile, provider_id, criterion, enabled],
    )?;
    Ok(())
}

/// Stored items whose criterion is currently enabled.
fn load_items(
    conn: &Connection,
    profile: &str,
    provider: &Provider,
) -> anyhow::Result<Vec<Suggestion>> {
    let overrides = overrides(conn, profile, &provider.id)?;
    let rows: Vec<String> = conn
        .prepare(
            "SELECT item_json FROM suggestion_items
             WHERE profile = ?1 AND provider_id = ?2 AND spec_hash = ?3 ORDER BY position",
        )?
        .query_map(
            params![profile, provider.id, spec_hash(&provider.spec)],
            |row| row.get(0),
        )?
        .collect::<Result<_, _>>()?;
    let mut items = Vec::with_capacity(rows.len());
    for row in rows {
        let item: Suggestion = serde_json::from_str(&row)?;
        if effective_enabled(&provider.spec, &overrides, &item.criterion) {
            items.push(item);
        }
    }
    Ok(items)
}

pub(crate) fn find_item(
    conn: &Connection,
    profile: &str,
    provider: &Provider,
    id: &str,
) -> anyhow::Result<Option<Suggestion>> {
    let stored: Option<String> = conn
        .query_row(
            "SELECT item_json FROM suggestion_items
             WHERE profile = ?1 AND provider_id = ?2 AND item_id = ?3 AND spec_hash = ?4",
            params![profile, provider.id, id, spec_hash(&provider.spec)],
            |row| row.get(0),
        )
        .optional()?;
    let Some(stored) = stored else {
        return Ok(None);
    };
    let item: Suggestion = serde_json::from_str(&stored)?;
    let overrides = overrides(conn, profile, &provider.id)?;
    Ok(effective_enabled(&provider.spec, &overrides, &item.criterion).then_some(item))
}

/// Whether `origin` names an active provider's live, non-stale item in the
/// profile's store: what a session may be recorded as started from, the same
/// bar `prepare` applies.
pub(crate) fn is_published(
    conn: &Connection,
    profile: &str,
    providers: &[Provider],
    origin: &SuggestionOrigin,
) -> anyhow::Result<bool> {
    let Some(provider) = providers.iter().find(|p| p.id == origin.provider_id) else {
        return Ok(false);
    };
    Ok(find_item(conn, profile, provider, &origin.id)?.is_some_and(|item| !item.stale))
}

/// `false` when the provider has no such live item, so a stale client cannot
/// accumulate dismissals for ids that were never published.
pub(crate) fn set_dismissed(
    conn: &Connection,
    profile: &str,
    provider: &Provider,
    id: &str,
    dismissed: bool,
) -> anyhow::Result<bool> {
    if find_item(conn, profile, provider, id)?.is_none() {
        return Ok(false);
    }
    if dismissed {
        conn.execute(
            "INSERT OR IGNORE INTO suggestion_dismissals (profile, provider_id, item_id) VALUES (?1, ?2, ?3)",
            params![profile, provider.id, id],
        )?;
    } else {
        conn.execute(
            "DELETE FROM suggestion_dismissals WHERE profile = ?1 AND provider_id = ?2 AND item_id = ?3",
            params![profile, provider.id, id],
        )?;
    }
    Ok(true)
}

/// Driven from the items table, capped per provider, so the read tracks live
/// items, not historical dismissals, which are never pruned.
const LIVE_DISMISSALS_SQL: &str = "SELECT i.provider_id, i.item_id FROM suggestion_items i
     WHERE i.profile = ?1 AND EXISTS (
         SELECT 1 FROM suggestion_dismissals d
         WHERE d.profile = i.profile AND d.provider_id = i.provider_id AND d.item_id = i.item_id)";

/// Round-robin across providers in order, each keeping its own item order, so
/// a client reading a prefix of the snapshot sees every provider.
fn interleave(lists: Vec<Vec<Value>>) -> Vec<Value> {
    let mut iters: Vec<_> = lists.into_iter().map(Vec::into_iter).collect();
    let mut out = Vec::new();
    loop {
        let before = out.len();
        for it in &mut iters {
            out.extend(it.next());
        }
        if out.len() == before {
            return out;
        }
    }
}

pub(crate) fn snapshot(
    conn: &Connection,
    profile: &str,
    providers: &[Provider],
) -> anyhow::Result<Value> {
    let dismissed: HashSet<(String, String)> = conn
        .prepare(LIVE_DISMISSALS_SQL)?
        .query_map([profile], |row| Ok((row.get(0)?, row.get(1)?)))?
        .collect::<Result<_, _>>()?;
    let stored = provider_prefs(conn, profile)?;
    let order = ordered_indexes(providers, &stored);
    let mut out_providers = Vec::with_capacity(providers.len());
    let mut per_provider: Vec<Vec<Value>> = Vec::new();
    for &index in &order {
        let provider = &providers[index];
        let enabled = stored.get(&provider.id).is_none_or(|p| p.0);
        let status: Option<(Option<String>, i64)> = conn
            .query_row(
                "SELECT error, updated_at FROM suggestion_providers WHERE profile = ?1 AND provider_id = ?2",
                params![profile, provider.id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        let overrides = overrides(conn, profile, &provider.id)?;
        let mut entry = json!({
            "id": provider.id,
            "name": provider.name,
            "enabled": enabled,
            "status": match &status {
                None => "pending",
                Some((None, _)) => "ok",
                Some((Some(_), _)) => "error",
            },
            "sources": provider.spec.sources.iter().map(|s| json!({
                "criterion": s.criterion,
                "label": s.label,
                "description": s.description,
                "variables": s.variables,
                "enabled": effective_enabled(&provider.spec, &overrides, &s.criterion),
            })).collect::<Vec<_>>(),
            "checks": load_checks(conn, profile, &provider.id)?,
        });
        if provider.spec.uses_agent {
            entry["uses_agent"] = json!(true);
        }
        if let Some((error, updated_at)) = status {
            entry["updated_at"] = json!(updated_at);
            if let Some(message) = error {
                entry["error"] = json!(message);
                let label: Option<String> = conn
                    .query_row(
                        "SELECT label FROM suggestion_fixes WHERE profile = ?1 AND provider_id = ?2",
                        params![profile, provider.id],
                        |row| row.get(0),
                    )
                    .optional()?;
                if let Some(label) = label {
                    entry["fix"] = json!({ "label": label });
                }
            }
        }
        out_providers.push(entry);
        if !enabled {
            continue;
        }
        let mut items = Vec::new();
        for item in load_items(conn, profile, provider)? {
            let is_dismissed = dismissed.contains(&(provider.id.clone(), item.id.clone()));
            let mut value = serde_json::to_value(&item)?;
            value["provider_id"] = json!(provider.id);
            value["dismissed"] = json!(is_dismissed);
            items.push(value);
        }
        per_provider.push(items);
    }
    let out_items = interleave(per_provider);
    let order_ids: Vec<&str> = order.iter().map(|&i| providers[i].id.as_str()).collect();
    let prefs = prefs_json(load_prefs(conn, profile)?, &order_ids);
    Ok(json!({ "providers": out_providers, "items": out_items, "prefs": prefs }))
}

/// The primary Session of each linked item, as snapshot JSON keyed by
/// `(provider_id, id)`, for links whose Session is live. Trashed Sessions do not count.
pub(crate) fn started_sessions<'a>(
    links: &[(String, String, String)],
    instances: impl IntoIterator<Item = &'a Instance>,
) -> HashMap<(String, String), Value> {
    let live: HashMap<&str, &Instance> = instances
        .into_iter()
        .filter(|inst| !inst.is_trashed())
        .map(|inst| (inst.id.as_str(), inst))
        .collect();
    links
        .iter()
        .filter_map(|(provider_id, id, session_id)| {
            let inst = live.get(session_id.as_str())?;
            Some((
                (provider_id.clone(), id.clone()),
                json!({
                    "id": inst.id,
                    "title": inst.title,
                    "status": inst.status.wire_str(),
                    "archived": inst.is_archived(),
                    "created_at": inst.created_at,
                }),
            ))
        })
        .collect()
}

/// Sets `started_session` on the snapshot items that have one.
pub(crate) fn attach_started(snapshot: &mut Value, started: &HashMap<(String, String), Value>) {
    let Some(items) = snapshot["items"].as_array_mut() else {
        return;
    };
    for item in items {
        let key = (
            item["provider_id"].as_str().unwrap_or_default().to_string(),
            item["id"].as_str().unwrap_or_default().to_string(),
        );
        if let Some(session) = started.get(&key) {
            item["started_session"] = session.clone();
        }
    }
}

pub(super) fn check_mapping_key(key: &str) -> Result<(), String> {
    if key.trim().is_empty() {
        return Err("mapping_key must not be empty".into());
    }
    if key.len() > MAX_KEY_BYTES {
        return Err(format!("mapping_key is limited to {MAX_KEY_BYTES} bytes"));
    }
    Ok(())
}

/// A mapping's paths as the registered paths they name, or the first
/// violation. Run before storing; stored rows may go stale afterwards.
pub(crate) fn normalize_mapping(
    mapping_key: &str,
    paths: &[String],
    registered: &[Project],
) -> Result<Vec<String>, String> {
    check_mapping_key(mapping_key)?;
    if paths.is_empty() || paths.len() > MAX_MAPPING_PROJECTS {
        return Err(format!(
            "projects must list 1 to {MAX_MAPPING_PROJECTS} registered projects"
        ));
    }
    let mut stored: Vec<String> = Vec::with_capacity(paths.len());
    for path in paths {
        let project = resolve_path(registered, path)
            .ok_or_else(|| format!("{path:?} is not exactly one registered project"))?;
        if stored.contains(&project.path) {
            return Err(format!("{path:?} is listed twice"));
        }
        stored.push(project.path);
    }
    Ok(stored)
}

pub(crate) fn put_mapping(
    conn: &Connection,
    profile: &str,
    provider_id: &str,
    mapping_key: &str,
    projects: &[String],
) -> anyhow::Result<()> {
    conn.execute(
        "INSERT INTO suggestion_mappings (profile, provider_id, mapping_key, projects_json)
         VALUES (?1, ?2, ?3, ?4)
         ON CONFLICT (profile, provider_id, mapping_key) DO UPDATE SET projects_json = ?4",
        params![
            profile,
            provider_id,
            mapping_key,
            serde_json::to_string(projects)?
        ],
    )?;
    Ok(())
}

/// `true` when a row existed.
pub(crate) fn delete_mapping(
    conn: &Connection,
    profile: &str,
    provider_id: &str,
    mapping_key: &str,
) -> rusqlite::Result<bool> {
    let rows = conn.execute(
        "DELETE FROM suggestion_mappings WHERE profile = ?1 AND provider_id = ?2 AND mapping_key = ?3",
        params![profile, provider_id, mapping_key],
    )?;
    Ok(rows > 0)
}

pub(super) fn get_mapping(
    conn: &Connection,
    profile: &str,
    provider_id: &str,
    mapping_key: &str,
) -> anyhow::Result<Option<Vec<String>>> {
    let stored: Option<String> = conn
        .query_row(
            "SELECT projects_json FROM suggestion_mappings
             WHERE profile = ?1 AND provider_id = ?2 AND mapping_key = ?3",
            params![profile, provider_id, mapping_key],
            |row| row.get(0),
        )
        .optional()?;
    Ok(stored.map(|s| serde_json::from_str(&s)).transpose()?)
}

/// Every stored mapping of the profile, including those of inactive
/// providers, so orphaned rows stay visible and removable. Paths are returned
/// as stored, stale or not.
pub(crate) fn list_mappings(conn: &Connection, profile: &str) -> anyhow::Result<Value> {
    let rows: Vec<(String, String, String)> = conn
        .prepare(
            "SELECT provider_id, mapping_key, projects_json FROM suggestion_mappings
             WHERE profile = ?1 ORDER BY provider_id, mapping_key",
        )?
        .query_map([profile], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))?
        .collect::<Result<_, _>>()?;
    let mappings = rows
        .into_iter()
        .map(|(provider_id, mapping_key, projects)| {
            let projects: Vec<String> = serde_json::from_str(&projects)?;
            Ok(json!({ "provider_id": provider_id, "mapping_key": mapping_key, "projects": projects }))
        })
        .collect::<anyhow::Result<Vec<_>>>()?;
    Ok(json!({ "mappings": mappings }))
}

#[cfg(test)]
mod tests {
    use super::fixtures::*;
    use super::*;

    #[test]
    fn providers_interleave_in_saved_order_each_keeping_its_own_order() {
        let conn = store();
        let other = Provider {
            id: "acme.other".into(),
            ..provider()
        };
        let providers = [provider(), other];
        let publish_for = |id: &str, ids: &[&str]| {
            let items: Vec<_> = ids.iter().map(|i| item(i, "assigned")).collect();
            set(&conn, "p", id, &spec(), &json!({ "items": items })).unwrap();
        };
        publish_for("acme.github", &["a1", "a2", "a3"]);
        publish_for("acme.other", &["b1"]);
        let read = |conn: &Connection| {
            let snapshot = snapshot(conn, "p", &providers).unwrap();
            ids(&snapshot)
                .into_iter()
                .map(str::to_string)
                .collect::<Vec<_>>()
        };
        assert_eq!(read(&conn), ["a1", "b1", "a2", "a3"]);

        let patch =
            serde_json::from_value(json!({ "provider_order": ["acme.other", "acme.github"] }))
                .unwrap();
        let known = ["acme.github".to_string(), "acme.other".to_string()];
        apply_prefs(&conn, "p", &known, &patch).unwrap().unwrap();
        assert_eq!(read(&conn), ["b1", "a1", "a2", "a3"]);
    }

    #[test]
    fn dismissals_persist_across_publishes_and_need_a_live_item() {
        let conn = store();
        let p = provider();
        assert!(!set_dismissed(&conn, "p", &p, "a", true).unwrap());
        publish(&conn, "p", &[item("a", "assigned")]);
        assert!(set_dismissed(&conn, "p", &p, "a", true).unwrap());
        publish(&conn, "p", &[]);
        publish(&conn, "p", &[item("a", "assigned")]);
        assert_eq!(snap(&conn, "p")["items"][0]["dismissed"], true);
        assert_eq!(snap(&conn, "other")["items"], json!([]));
        assert!(set_dismissed(&conn, "p", &p, "a", false).unwrap());
        assert_eq!(snap(&conn, "p")["items"][0]["dismissed"], false);
    }

    #[test]
    fn items_keep_the_published_order_and_a_reorder_keeps_dismissals() {
        let conn = store();
        let p = provider();
        publish(
            &conn,
            "p",
            &[item("urgent-42", "assigned"), item("backlog-1", "assigned")],
        );
        assert_eq!(ids(&snap(&conn, "p")), ["urgent-42", "backlog-1"]);
        assert!(set_dismissed(&conn, "p", &p, "urgent-42", true).unwrap());
        publish(
            &conn,
            "p",
            &[item("backlog-1", "assigned"), item("urgent-42", "assigned")],
        );
        let snapshot = snap(&conn, "p");
        assert_eq!(ids(&snapshot), ["backlog-1", "urgent-42"]);
        assert_eq!(snapshot["items"][1]["dismissed"], true);
    }

    #[test]
    fn a_publication_validated_against_an_older_spec_stays_hidden() {
        let conn = store();
        let new = || {
            let mut p = provider();
            p.spec.sources[0].label = "Renamed".into();
            p
        };
        publish(&conn, "p", &[item("a", "assigned")]);
        assert_eq!(ids(&snapshot(&conn, "p", &[provider()]).unwrap()), ["a"]);

        // The manifest changed, the old worker's rows are still stored.
        assert!(ids(&snapshot(&conn, "p", &[new()]).unwrap()).is_empty());
        assert!(find_item(&conn, "p", &new(), "a").unwrap().is_none());
        assert!(!set_dismissed(&conn, "p", &new(), "a", true).unwrap());

        // A late write under the old spec stays hidden; one under the new spec shows.
        publish(&conn, "p", &[item("a", "assigned")]);
        assert!(ids(&snapshot(&conn, "p", &[new()]).unwrap()).is_empty());
        set(
            &conn,
            "p",
            "acme.github",
            &new().spec,
            &json!({ "items": [item("a", "assigned")] }),
        )
        .unwrap();
        assert_eq!(ids(&snapshot(&conn, "p", &[new()]).unwrap()), ["a"]);
    }

    #[test]
    fn forgetting_a_publication_keeps_user_state() {
        let conn = store();
        let p = provider();
        for profile in ["p", "q"] {
            publish(&conn, profile, &[item("a", "assigned")]);
            set_source(&conn, profile, "acme.github", "quiet", true).unwrap();
            assert!(set_dismissed(&conn, profile, &p, "a", true).unwrap());
            for (table, columns, values) in [
                ("suggestion_checks", "checks_json", "'[]'"),
                (
                    "suggestion_fixes",
                    "label, method",
                    "'Fix', 'suggestions.fix.k'",
                ),
                (
                    "suggestion_presets",
                    "criterion, preset_json",
                    "'assigned', '{}'",
                ),
                (
                    "suggestion_mappings",
                    "mapping_key, projects_json",
                    "'k', '[]'",
                ),
            ] {
                conn.execute(
                    &format!(
                        "INSERT INTO {table} (profile, provider_id, {columns}) VALUES ('{profile}', 'acme.github', {values})"
                    ),
                    [],
                )
                .unwrap();
            }
        }
        publish(&conn, "p", &[item("a", "assigned")]);
        conn.execute(
            "INSERT INTO suggestion_providers (profile, provider_id, updated_at) VALUES ('p', 'other', 0)",
            [],
        )
        .unwrap();

        forget_publication(&conn, "acme.github").unwrap();

        let count = |table: &str| -> i64 {
            conn.query_row(
                &format!("SELECT COUNT(*) FROM {table} WHERE provider_id = 'acme.github'"),
                [],
                |row| row.get(0),
            )
            .unwrap()
        };
        for table in [
            "suggestion_providers",
            "suggestion_items",
            "suggestion_checks",
            "suggestion_fixes",
        ] {
            assert_eq!(count(table), 0, "{table}");
        }
        for table in [
            "suggestion_sources",
            "suggestion_dismissals",
            "suggestion_presets",
            "suggestion_mappings",
        ] {
            assert_eq!(count(table), 2, "{table}");
        }
        let other: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM suggestion_providers WHERE provider_id = 'other'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(other, 1);
        let after = snap(&conn, "p");
        assert_eq!(after["providers"][0]["status"], "pending");
        assert!(ids(&after).is_empty());
        publish(&conn, "p", &[item("a", "assigned")]);
        assert_eq!(snap(&conn, "p")["items"][0]["dismissed"], true);
    }

    #[test]
    fn dismissal_read_searches_dismissals_by_full_key() {
        let conn = store();
        let plan: Vec<String> = conn
            .prepare(&format!("EXPLAIN QUERY PLAN {LIVE_DISMISSALS_SQL}"))
            .unwrap()
            .query_map(["p"], |row| row.get::<_, String>(3))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        let dismissals: Vec<&String> = plan
            .iter()
            .filter(|step| step.contains("suggestion_dismissals"))
            .collect();
        assert!(
            !dismissals.is_empty()
                && dismissals
                    .iter()
                    .all(|s| s.starts_with("SEARCH") && s.contains("item_id=?")),
            "{plan:?}"
        );
    }

    #[test]
    fn default_profile_publishes_and_reads_under_the_effective_profile() {
        let dir = tempfile::tempdir().unwrap();
        let _home = crate::session::test_support::isolate_app_dir_at(dir.path());
        let host =
            crate::plugin::host_api::HostApiState::open(&dir.path().join("e.db"), "", 100).unwrap();
        host.with_suggestions_db(|conn, profile| {
            assert_eq!(profile, crate::session::config::effective_profile(""));
            assert!(!profile.is_empty());
            publish(conn, profile, &[item("a", "assigned")]);
        });
        host.with_suggestions_db(|conn, profile| {
            assert_eq!(ids(&snap(conn, profile)), ["a"]);
            assert!(ids(&snap(conn, "")).is_empty());
        });
    }

    #[test]
    #[serial_test::serial]
    fn normalize_mapping_accepts_only_distinct_registered_paths() {
        let _home = crate::session::test_support::isolate_app_dir();
        let dir = tempfile::tempdir().unwrap();
        let (a, b) = (dir.path().join("a"), dir.path().join("b"));
        let scope = projects::ProjectScope::Global;
        for (name, path) in [("a", &a), ("b", &b)] {
            std::fs::create_dir(path).unwrap();
            let path = path.to_string_lossy().into_owned();
            projects::add("default", scope, Project::new(name, path, scope), false).unwrap();
        }
        let registered = projects::load_merged("default").unwrap();
        let (a, b) = (
            a.to_string_lossy().into_owned(),
            b.to_string_lossy().into_owned(),
        );
        let norm = |key: &str, paths: &[&str]| {
            let paths: Vec<String> = paths.iter().map(|p| p.to_string()).collect();
            normalize_mapping(key, &paths, &registered)
        };
        let registered_path = |p: &str| projects::canonical_key(p);
        assert_eq!(
            norm("o/r", &[&b, &a]).unwrap(),
            [registered_path(&b), registered_path(&a)]
        );
        let many: Vec<&str> = vec![a.as_str(); MAX_MAPPING_PROJECTS + 1];
        let cases = [
            ("not exactly one", norm("o/r", &["a"])),
            ("not exactly one", norm("o/r", &["/nowhere"])),
            ("listed twice", norm("o/r", &[&a, &a])),
            ("1 to 16", norm("o/r", &[])),
            ("1 to 16", norm("o/r", &many)),
            ("must not be empty", norm(" ", &[&a])),
            ("limited to 256", norm(&"k".repeat(257), &[&a])),
        ];
        for (expected, result) in cases {
            let err = result.unwrap_err();
            assert!(err.contains(expected), "{expected:?} not in {err:?}");
        }
    }

    #[test]
    fn mappings_are_scoped_ordered_replaced_and_deletable() {
        let conn = store();
        let paths = |v: &[&str]| v.iter().map(|p| p.to_string()).collect::<Vec<_>>();
        put_mapping(&conn, "p", "acme.github", "o/r", &paths(&["/b", "/a"])).unwrap();
        put_mapping(&conn, "p", "acme.github", "o/z", &paths(&["/c"])).unwrap();
        put_mapping(&conn, "p", "gone.provider", "o/r", &paths(&["/d"])).unwrap();
        put_mapping(&conn, "other", "acme.github", "o/r", &paths(&["/e"])).unwrap();
        put_mapping(&conn, "p", "acme.github", "o/r", &paths(&["/b", "/c"])).unwrap();

        let listed = list_mappings(&conn, "p").unwrap();
        let rows = listed["mappings"].as_array().unwrap();
        let keys: Vec<_> = rows
            .iter()
            .map(|m| {
                (
                    m["provider_id"].as_str().unwrap(),
                    m["mapping_key"].as_str().unwrap(),
                )
            })
            .collect();
        assert_eq!(
            keys,
            [
                ("acme.github", "o/r"),
                ("acme.github", "o/z"),
                ("gone.provider", "o/r")
            ],
            "inactive providers stay listed"
        );
        assert_eq!(rows[0]["projects"], json!(["/b", "/c"]), "upsert replaces");

        let p = provider();
        publish(
            &conn,
            "p",
            &[{
                let mut i = item("a", "assigned");
                i.mapping_key = Some("o/r".into());
                i
            }],
        );
        let (_, mapped) = find_item_with_mapping(&conn, "p", &p, "a")
            .unwrap()
            .unwrap();
        assert_eq!(mapped, Some(paths(&["/b", "/c"])));
        assert!(find_item_with_mapping(&conn, "other", &p, "a")
            .unwrap()
            .is_none());

        assert!(delete_mapping(&conn, "p", "gone.provider", "o/r").unwrap());
        assert!(!delete_mapping(&conn, "p", "gone.provider", "o/r").unwrap());
        assert_eq!(
            list_mappings(&conn, "other").unwrap()["mappings"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
    }

    #[test]
    fn a_stale_item_is_not_a_session_origin() {
        let conn = store();
        let mut stale = item("old", "assigned");
        stale.stale = true;
        publish(&conn, "p", &[item("live", "assigned"), stale]);
        let origin = |id: &str| crate::session::SuggestionOrigin {
            provider_id: "acme.github".into(),
            id: id.into(),
            profile: None,
        };
        let providers = [provider()];
        let published = |id: &str| is_published(&conn, "p", &providers, &origin(id)).unwrap();
        assert!(published("live"));
        assert!(
            !published("old"),
            "prepare refuses it, so a session may not link it"
        );
        assert!(!published("never"));
    }

    #[test]
    fn started_session_is_the_linked_live_one() {
        let session = |id: &str| {
            let mut i = Instance::new(id, "/tmp/x");
            i.id = id.to_string();
            i
        };
        let mut trashed = session("trashed");
        trashed.trash();
        let mut archived = session("archived");
        archived.archived_at = Some(chrono::Utc::now());
        let instances = [session("unlinked"), archived, trashed];
        let link = |item: &str, session: &str| {
            (
                "acme.github".to_string(),
                item.to_string(),
                session.to_string(),
            )
        };
        let links = [
            link("a", "archived"),
            link("b", "trashed"),
            link("c", "gone"),
        ];
        let started = started_sessions(&links, &instances);
        assert_eq!(started.len(), 1);
        let key = ("acme.github".to_string(), "a".to_string());
        assert_eq!(started[&key]["id"], "archived");
        assert_eq!(started[&key]["archived"], true);

        let mut snapshot = json!({ "items": [
            { "provider_id": "acme.github", "id": "a" },
            { "provider_id": "acme.github", "id": "b" },
        ]});
        attach_started(&mut snapshot, &started);
        assert_eq!(snapshot["items"][0]["started_session"]["id"], "archived");
        assert!(snapshot["items"][1].get("started_session").is_none());
    }
}
