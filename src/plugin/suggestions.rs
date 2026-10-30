//! Host side of plugin suggestions: validation of worker RPCs and the
//! profile-scoped store behind them.
//!
//! Rows are keyed by `effective_profile`, resolved by the callers on publish and
//! on read alike. A provider's display name and source metadata come from the
//! manifest; only status, items, source overrides and dismissals are stored.
use std::collections::{BTreeSet, HashMap, HashSet};

use aoe_plugin_api::suggestions::MAX_KEY_BYTES;
use aoe_plugin_api::{SetSuggestions, Suggestion, SuggestionsError, SuggestionsSpec};
use rusqlite::{params, Connection, OptionalExtension};
use serde_json::{json, Value};

use super::host_api::DispatchError;
use crate::session::config::profile_config::resolve_config_or_warn;
use crate::session::config::repo_config::resolve_config_with_repo_or_warn;
use crate::session::projects::{self, Project};
use std::path::Path;

const MAX_ITEMS: usize = 500;
const MAX_TITLE_BYTES: usize = 512;
const MAX_REASON_BYTES: usize = 2048;
const MAX_URL_BYTES: usize = 2048;
const MAX_PROJECT_BYTES: usize = 4096;
const MAX_PREVIEW_BYTES: usize = 16 * 1024;
const MAX_PROMPT_BYTES: usize = 64 * 1024;
const MAX_VARIABLE_VALUE_BYTES: usize = 2048;
const MAX_ERROR_BYTES: usize = 4096;

/// An active plugin that declares `[suggestions]` and has a worker.
pub(crate) struct Provider {
    pub id: String,
    pub name: String,
    pub spec: SuggestionsSpec,
}

pub(crate) fn providers() -> Vec<Provider> {
    let registry = crate::plugin::registry();
    registry
        .active()
        .filter(|p| p.manifest.runtime.is_some())
        .filter_map(|p| {
            Some(Provider {
                id: p.id().to_string(),
                name: p.manifest.name.clone(),
                spec: p.manifest.suggestions.clone()?,
            })
        })
        .collect()
}

pub(super) fn init(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS suggestion_providers (
             profile     TEXT NOT NULL,
             provider_id TEXT NOT NULL,
             error       TEXT,
             updated_at  INTEGER NOT NULL,
             PRIMARY KEY (profile, provider_id)
         );
         CREATE TABLE IF NOT EXISTS suggestion_items (
             profile     TEXT NOT NULL,
             provider_id TEXT NOT NULL,
             item_id     TEXT NOT NULL,
             item_json   TEXT NOT NULL,
             PRIMARY KEY (profile, provider_id, item_id)
         );
         CREATE TABLE IF NOT EXISTS suggestion_sources (
             profile     TEXT NOT NULL,
             provider_id TEXT NOT NULL,
             criterion   TEXT NOT NULL,
             enabled     INTEGER NOT NULL,
             PRIMARY KEY (profile, provider_id, criterion)
         );
         CREATE TABLE IF NOT EXISTS suggestion_dismissals (
             profile     TEXT NOT NULL,
             provider_id TEXT NOT NULL,
             item_id     TEXT NOT NULL,
             PRIMARY KEY (profile, provider_id, item_id)
         );",
    )
}

fn db_error(e: rusqlite::Error) -> DispatchError {
    DispatchError::internal(format!("suggestions store: {e}"))
}

/// Stored per-criterion switches; a criterion without a row follows the
/// manifest default.
fn overrides(
    conn: &Connection,
    profile: &str,
    provider_id: &str,
) -> rusqlite::Result<HashMap<String, bool>> {
    conn.prepare(
        "SELECT criterion, enabled FROM suggestion_sources WHERE profile = ?1 AND provider_id = ?2",
    )?
    .query_map(params![profile, provider_id], |row| {
        Ok((row.get(0)?, row.get(1)?))
    })?
    .collect()
}

fn effective_enabled(
    spec: &SuggestionsSpec,
    overrides: &HashMap<String, bool>,
    criterion: &str,
) -> bool {
    overrides.get(criterion).copied().unwrap_or_else(|| {
        spec.sources
            .iter()
            .find(|s| s.criterion == criterion)
            .is_some_and(|s| s.default_enabled)
    })
}

pub(super) fn sources(
    conn: &Connection,
    profile: &str,
    provider_id: &str,
    spec: &SuggestionsSpec,
) -> Result<Value, DispatchError> {
    let overrides = overrides(conn, profile, provider_id).map_err(db_error)?;
    let sources: Vec<Value> = spec
        .sources
        .iter()
        .map(|s| json!({ "criterion": s.criterion, "enabled": effective_enabled(spec, &overrides, &s.criterion) }))
        .collect();
    Ok(json!({ "sources": sources }))
}

pub(super) fn set(
    conn: &Connection,
    profile: &str,
    provider_id: &str,
    spec: &SuggestionsSpec,
    params: &Value,
) -> Result<Value, DispatchError> {
    let request: SetSuggestions = serde_json::from_value(params.clone()).map_err(|e| {
        DispatchError::invalid_params(format!("invalid suggestions.set params: {e}"))
    })?;
    let total = request.items.len();
    let overrides = overrides(conn, profile, provider_id).map_err(db_error)?;
    let accepted =
        validate_items(spec, &overrides, request.items).map_err(DispatchError::invalid_params)?;
    let tx = conn.unchecked_transaction().map_err(db_error)?;
    tx.execute(
        "DELETE FROM suggestion_items WHERE profile = ?1 AND provider_id = ?2",
        params![profile, provider_id],
    )
    .map_err(db_error)?;
    for item in &accepted {
        let json = serde_json::to_string(item)
            .map_err(|e| DispatchError::internal(format!("encode suggestion: {e}")))?;
        tx.execute(
            "INSERT INTO suggestion_items (profile, provider_id, item_id, item_json) VALUES (?1, ?2, ?3, ?4)",
            params![profile, provider_id, item.id, json],
        )
        .map_err(db_error)?;
    }
    record_status(&tx, profile, provider_id, None).map_err(db_error)?;
    tx.commit().map_err(db_error)?;
    Ok(json!({ "ok": true, "accepted": accepted.len(), "dropped": total - accepted.len() }))
}

pub(super) fn error(
    conn: &Connection,
    profile: &str,
    provider_id: &str,
    params: &Value,
) -> Result<Value, DispatchError> {
    let request: SuggestionsError = serde_json::from_value(params.clone()).map_err(|e| {
        DispatchError::invalid_params(format!("invalid suggestions.error params: {e}"))
    })?;
    if request.message.len() > MAX_ERROR_BYTES {
        return Err(DispatchError::invalid_params(format!(
            "suggestions.error message is limited to {MAX_ERROR_BYTES} bytes"
        )));
    }
    record_status(conn, profile, provider_id, Some(&request.message)).map_err(db_error)?;
    Ok(json!({ "ok": true }))
}

fn record_status(
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
             WHERE profile = ?1 AND provider_id = ?2 ORDER BY item_id",
        )?
        .query_map(params![profile, provider.id], |row| row.get(0))?
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
    Ok(load_items(conn, profile, provider)?
        .into_iter()
        .find(|item| item.id == id))
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

pub(crate) fn snapshot(
    conn: &Connection,
    profile: &str,
    providers: &[Provider],
) -> anyhow::Result<Value> {
    let dismissed: HashSet<(String, String)> = conn
        .prepare("SELECT provider_id, item_id FROM suggestion_dismissals WHERE profile = ?1")?
        .query_map([profile], |row| Ok((row.get(0)?, row.get(1)?)))?
        .collect::<Result<_, _>>()?;
    let mut out_providers = Vec::with_capacity(providers.len());
    let mut out_items = Vec::new();
    for provider in providers {
        let status: Option<(Option<String>,)> = conn
            .query_row(
                "SELECT error FROM suggestion_providers WHERE profile = ?1 AND provider_id = ?2",
                params![profile, provider.id],
                |row| Ok((row.get(0)?,)),
            )
            .optional()?;
        let overrides = overrides(conn, profile, &provider.id)?;
        let mut entry = json!({
            "id": provider.id,
            "name": provider.name,
            "status": match &status {
                None => "pending",
                Some((None,)) => "ok",
                Some((Some(_),)) => "error",
            },
            "sources": provider.spec.sources.iter().map(|s| json!({
                "criterion": s.criterion,
                "label": s.label,
                "description": s.description,
                "enabled": effective_enabled(&provider.spec, &overrides, &s.criterion),
            })).collect::<Vec<_>>(),
        });
        if let Some((Some(message),)) = status {
            entry["error"] = json!(message);
        }
        out_providers.push(entry);
        for item in load_items(conn, profile, provider)? {
            let is_dismissed = dismissed.contains(&(provider.id.clone(), item.id.clone()));
            let mut value = serde_json::to_value(&item)?;
            value["provider_id"] = json!(provider.id);
            value["dismissed"] = json!(is_dismissed);
            out_items.push(value);
        }
    }
    Ok(json!({ "providers": out_providers, "items": out_items }))
}

/// Returns the items of enabled criteria, or the first contract violation.
fn validate_items(
    spec: &SuggestionsSpec,
    overrides: &HashMap<String, bool>,
    items: Vec<Suggestion>,
) -> Result<Vec<Suggestion>, String> {
    if items.len() > MAX_ITEMS {
        return Err(format!("suggestions.set accepts at most {MAX_ITEMS} items"));
    }
    let mut ids = HashSet::new();
    for item in &items {
        validate_item(spec, item)?;
        if !ids.insert(item.id.as_str()) {
            return Err(format!("duplicate suggestion id {:?}", item.id));
        }
    }
    Ok(items
        .into_iter()
        .filter(|item| effective_enabled(spec, overrides, &item.criterion))
        .collect())
}

fn validate_item(spec: &SuggestionsSpec, item: &Suggestion) -> Result<(), String> {
    let id = &item.id;
    let source = spec
        .sources
        .iter()
        .find(|s| s.criterion == item.criterion)
        .ok_or_else(|| format!("undeclared suggestion criterion {:?}", item.criterion))?;
    for (name, value) in [
        ("id", id),
        ("title", &item.title),
        ("source_ref", &item.source_ref),
    ] {
        if value.trim().is_empty() {
            return Err(format!("suggestion {id:?}: {name} must not be empty"));
        }
    }
    let bounded = [
        ("id", id.len(), MAX_KEY_BYTES),
        ("title", item.title.len(), MAX_TITLE_BYTES),
        ("reason", item.reason.len(), MAX_REASON_BYTES),
        ("source_url", item.source_url.len(), MAX_URL_BYTES),
        ("source_ref", item.source_ref.len(), MAX_URL_BYTES),
        ("preview", item.preview.len(), MAX_PREVIEW_BYTES),
        ("draft.title", item.draft.title.len(), MAX_TITLE_BYTES),
        ("draft.prompt", item.draft.prompt.len(), MAX_PROMPT_BYTES),
        (
            "project",
            item.project.as_ref().map_or(0, String::len),
            MAX_PROJECT_BYTES,
        ),
    ];
    if let Some((name, _, max)) = bounded.iter().find(|(_, len, max)| len > max) {
        return Err(format!(
            "suggestion {id:?}: {name} is limited to {max} bytes"
        ));
    }
    if !is_http_url(&item.source_url) {
        return Err(format!(
            "suggestion {id:?}: source_url must be an http(s) URL"
        ));
    }
    let declared: BTreeSet<&str> = source.variables.iter().map(String::as_str).collect();
    let given: BTreeSet<&str> = item.variables.keys().map(String::as_str).collect();
    if declared != given {
        return Err(format!(
            "suggestion {id:?}: variables must be exactly the ones source {:?} declares",
            source.criterion
        ));
    }
    if item
        .variables
        .values()
        .any(|v| v.len() > MAX_VARIABLE_VALUE_BYTES)
    {
        return Err(format!(
            "suggestion {id:?}: variable values are limited to {MAX_VARIABLE_VALUE_BYTES} bytes"
        ));
    }
    Ok(())
}

fn is_http_url(url: &str) -> bool {
    let rest = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"));
    rest.is_some_and(|r| !r.is_empty() && !r.chars().any(|c| c.is_whitespace() || c.is_control()))
}

/// The registered project `wanted` names, by exact name or path. Zero or
/// several matches resolve to nothing: a project is never guessed.
fn resolve_project(profile: &str, wanted: &str) -> Option<Project> {
    let projects = projects::load_merged(profile).ok()?;
    let mut hits: Vec<&Project> = projects.iter().filter(|p| p.name == wanted).collect();
    if hits.is_empty() {
        let key = projects::canonical_key(wanted);
        hits = projects
            .iter()
            .filter(|p| projects::canonical_key(&p.path) == key)
            .collect();
    }
    match hits[..] {
        [only] => Some(only.clone()),
        _ => None,
    }
}

/// Wizard prefill for `item`, resolved through the normal config cascade of its
/// project, or the saved profile settings when it names none.
pub(crate) fn prepare(profile: &str, item: &Suggestion) -> Value {
    let project = item
        .project
        .as_deref()
        .and_then(|wanted| resolve_project(profile, wanted));
    let mut config = match &project {
        Some(p) => resolve_config_with_repo_or_warn(profile, Path::new(&p.path)),
        None => resolve_config_or_warn(profile),
    };
    if let Some(p) = &project {
        if let Some(enabled) = p.overrides.worktree_enabled {
            config.worktree.enabled = enabled;
        }
        if let Some(enabled) = p.overrides.sandbox_enabled {
            config.sandbox.enabled_by_default = enabled;
        }
    }
    json!({
        "title": item.draft.title,
        "initial_prompt": item.draft.prompt,
        "source_ref": item.source_ref,
        "project": project.map(|p| json!({ "name": p.name, "path": p.path })),
        "tool": config.session.default_tool.unwrap_or_else(|| "claude".to_string()),
        "yolo": config.session.yolo_mode_default,
        "sandbox": config.sandbox.enabled_by_default,
        "worktree": config.worktree.enabled,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use aoe_plugin_api::{SuggestionDraft, SuggestionSource};

    fn spec() -> SuggestionsSpec {
        let source = |criterion: &str, variables: &[&str], default_enabled| SuggestionSource {
            criterion: criterion.into(),
            label: criterion.into(),
            description: String::new(),
            variables: variables.iter().map(|v| v.to_string()).collect(),
            default_enabled,
        };
        SuggestionsSpec {
            sources: vec![
                source("assigned", &["repo"], true),
                source("quiet", &[], false),
            ],
        }
    }

    fn item(id: &str, criterion: &str) -> Suggestion {
        Suggestion {
            id: id.into(),
            title: "Work".into(),
            reason: "Assigned".into(),
            source_url: "https://example.com/1".into(),
            source_ref: "example/1".into(),
            preview: "body".into(),
            project: None,
            criterion: criterion.into(),
            variables: if criterion == "assigned" {
                [("repo".to_string(), "acme/widgets".to_string())].into()
            } else {
                Default::default()
            },
            draft: SuggestionDraft {
                title: "Work".into(),
                prompt: "Do work".into(),
            },
        }
    }

    #[test]
    fn disabled_criteria_are_dropped_and_enabled_ones_kept() {
        let kept = validate_items(
            &spec(),
            &HashMap::new(),
            vec![item("a", "assigned"), item("q", "quiet")],
        )
        .unwrap();
        assert_eq!(kept.len(), 1);
        assert_eq!(kept[0].id, "a");
    }

    #[test]
    fn contract_violations_are_rejected() {
        type Mutate = fn(&mut Suggestion);
        let cases: [(&str, Mutate); 9] = [
            ("undeclared", |i| i.criterion = "nope".into()),
            ("must not be empty", |i| i.title = " ".into()),
            ("http(s)", |i| i.source_url = "javascript:alert(1)".into()),
            ("http(s)", |i| i.source_url = "https://".into()),
            ("limited to 256", |i| i.id = "x".repeat(257)),
            ("limited to 16384", |i| {
                i.preview = "x".repeat(16 * 1024 + 1)
            }),
            ("variables must be exactly", |i| i.variables.clear()),
            ("variables must be exactly", |i| {
                i.variables.insert("extra".into(), "v".into());
            }),
            ("variable values", |i| {
                i.variables.insert("repo".into(), "x".repeat(2049));
            }),
        ];
        for (expected, mutate) in cases {
            let mut bad = item("a", "assigned");
            mutate(&mut bad);
            let err = validate_items(&spec(), &HashMap::new(), vec![bad]).unwrap_err();
            assert!(err.contains(expected), "{expected:?} not in {err:?}");
        }
        let dup = validate_items(
            &spec(),
            &HashMap::new(),
            vec![item("a", "assigned"), item("a", "assigned")],
        );
        assert!(dup.unwrap_err().contains("duplicate"));
        let many = (0..=MAX_ITEMS)
            .map(|n| item(&n.to_string(), "assigned"))
            .collect();
        assert!(validate_items(&spec(), &HashMap::new(), many)
            .unwrap_err()
            .contains("at most 500"));
    }

    fn store() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        init(&conn).unwrap();
        conn
    }

    fn provider() -> Provider {
        Provider {
            id: "acme.github".into(),
            name: "GitHub".into(),
            spec: spec(),
        }
    }

    fn publish(conn: &Connection, profile: &str, items: &[Suggestion]) -> Value {
        set(
            conn,
            profile,
            "acme.github",
            &spec(),
            &json!({ "items": items }),
        )
        .unwrap()
    }

    fn snap(conn: &Connection, profile: &str) -> Value {
        snapshot(conn, profile, &[provider()]).unwrap()
    }

    fn ids(snapshot: &Value) -> Vec<&str> {
        snapshot["items"]
            .as_array()
            .unwrap()
            .iter()
            .map(|i| i["id"].as_str().unwrap())
            .collect()
    }

    #[test]
    fn set_reports_counts_and_sources_report_defaults() {
        let conn = store();
        assert_eq!(
            publish(&conn, "p", &[item("a", "assigned"), item("q", "quiet")]),
            json!({ "ok": true, "accepted": 1, "dropped": 1 })
        );
        let bad = json!({ "items": [], "provider_id": "x" });
        assert!(set(&conn, "p", "acme.github", &spec(), &bad).is_err());
        assert_eq!(
            sources(&conn, "p", "acme.github", &spec()).unwrap(),
            json!({ "sources": [
                { "criterion": "assigned", "enabled": true },
                { "criterion": "quiet", "enabled": false },
            ]})
        );
    }

    #[test]
    fn profiles_are_isolated_and_status_tracks_the_last_report() {
        let conn = store();
        assert_eq!(snap(&conn, "p")["providers"][0]["status"], "pending");
        publish(&conn, "p", &[item("a", "assigned")]);
        assert_eq!(ids(&snap(&conn, "p")), ["a"]);
        assert!(ids(&snap(&conn, "other")).is_empty());
        assert_eq!(snap(&conn, "p")["providers"][0]["status"], "ok");

        error(
            &conn,
            "p",
            "acme.github",
            &json!({ "message": "rate limited" }),
        )
        .unwrap();
        let failed = snap(&conn, "p");
        assert_eq!(failed["providers"][0]["status"], "error");
        assert_eq!(failed["providers"][0]["error"], "rate limited");
        assert_eq!(ids(&failed), ["a"], "last-good items survive an error");

        publish(&conn, "p", &[]);
        let empty = snap(&conn, "p");
        assert_eq!(empty["providers"][0]["status"], "ok");
        assert!(empty["providers"][0].get("error").is_none());
        assert!(ids(&empty).is_empty(), "an empty publish clears items");
    }

    #[test]
    fn source_override_beats_default_and_hides_items_at_read() {
        let conn = store();
        publish(&conn, "p", &[item("a", "assigned")]);
        set_source(&conn, "p", "acme.github", "assigned", false).unwrap();
        assert!(ids(&snap(&conn, "p")).is_empty());
        assert_eq!(
            snap(&conn, "p")["providers"][0]["sources"][0]["enabled"],
            false
        );

        set_source(&conn, "p", "acme.github", "quiet", true).unwrap();
        let dropped = publish(&conn, "p", &[item("a", "assigned"), item("q", "quiet")]);
        assert_eq!(dropped["accepted"], 1);
        assert_eq!(ids(&snap(&conn, "p")), ["q"]);
    }

    #[test]
    fn dismissals_persist_across_publishes_and_need_a_live_item() {
        let conn = store();
        let p = provider();
        assert!(!set_dismissed(&conn, "p", &p, "a", true).unwrap());
        publish(&conn, "p", &[item("a", "assigned")]);
        assert!(set_dismissed(&conn, "p", &p, "a", true).unwrap());
        publish(&conn, "p", &[item("a", "assigned")]);
        assert_eq!(snap(&conn, "p")["items"][0]["dismissed"], true);
        assert_eq!(snap(&conn, "other")["items"], json!([]));
        assert!(set_dismissed(&conn, "p", &p, "a", false).unwrap());
        assert_eq!(snap(&conn, "p")["items"][0]["dismissed"], false);
    }

    #[test]
    fn default_profile_publishes_and_reads_under_the_effective_profile() {
        let dir = tempfile::tempdir().unwrap();
        let _home = crate::session::test_support::isolate_app_dir_at(dir.path());
        let host =
            super::super::host_api::HostApiState::open(&dir.path().join("e.db"), "", 100).unwrap();
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
    fn prepare_resolves_only_an_unambiguous_registered_project() {
        let _home = crate::session::test_support::isolate_app_dir();
        let add = |scope, name: &str, path: &str| {
            projects::add("default", scope, Project::new(name, path, scope), false).unwrap();
        };
        let (global, profile) = (
            projects::ProjectScope::Global,
            projects::ProjectScope::Profile,
        );
        add(global, "widgets", "/tmp/aoe-widgets");
        add(global, "twin", "/tmp/aoe-twin-a");
        add(profile, "twin", "/tmp/aoe-twin-b");
        let prepared = |project: Option<&str>| {
            let mut i = item("a", "assigned");
            i.project = project.map(String::from);
            prepare("default", &i)
        };
        for hit in ["widgets", "/tmp/aoe-widgets"] {
            assert_eq!(prepared(Some(hit))["project"]["name"], "widgets", "{hit}");
        }
        for miss in [Some("twin"), Some("unknown"), None] {
            assert!(prepared(miss)["project"].is_null(), "{miss:?}");
        }
        let out = prepared(None);
        assert_eq!(out["initial_prompt"], "Do work");
        assert_eq!(out["source_ref"], "example/1");
    }

    #[test]
    fn error_message_is_bounded() {
        let conn = store();
        let long = json!({ "message": "x".repeat(MAX_ERROR_BYTES + 1) });
        assert!(error(&conn, "p", "acme.github", &json!({ "message": "boom" })).is_ok());
        assert!(error(&conn, "p", "acme.github", &long).is_err());
    }
}
