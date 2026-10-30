//! Host side of plugin suggestions: validation of worker RPCs and the
//! profile-scoped store behind them.
//!
//! Rows are keyed by `effective_profile`, resolved by the callers on publish and
//! on read alike. A provider's display name and source metadata come from the
//! manifest; only status, items, source overrides, dismissals and prompt
//! templates are stored.
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

use aoe_plugin_api::suggestions::MAX_KEY_BYTES;
use aoe_plugin_api::{
    SetChecks, SetSuggestions, SetupCheck, Suggestion, SuggestionsError, SuggestionsSpec,
};
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
pub(crate) const MAX_PROMPT_BYTES: usize = 64 * 1024;
const MAX_VARIABLE_VALUE_BYTES: usize = 2048;
const MAX_ERROR_BYTES: usize = 4096;
const MAX_CHECKS: usize = 32;
const MAX_CHECK_TEXT_BYTES: usize = 2048;
pub(crate) const MAX_MAPPING_PROJECTS: usize = 16;

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
         );
         CREATE TABLE IF NOT EXISTS suggestion_templates (
             profile     TEXT NOT NULL,
             provider_id TEXT NOT NULL,
             criterion   TEXT NOT NULL,
             template    TEXT NOT NULL,
             PRIMARY KEY (profile, provider_id, criterion)
         );
         CREATE TABLE IF NOT EXISTS suggestion_mappings (
             profile       TEXT NOT NULL,
             provider_id   TEXT NOT NULL,
             mapping_key   TEXT NOT NULL,
             projects_json TEXT NOT NULL,
             PRIMARY KEY (profile, provider_id, mapping_key)
         );
         CREATE TABLE IF NOT EXISTS suggestion_checks (
             profile     TEXT NOT NULL,
             provider_id TEXT NOT NULL,
             checks_json TEXT NOT NULL,
             PRIMARY KEY (profile, provider_id)
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
    // `unchecked_`: the connection is only reachable as `&Connection` behind the host mutex.
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

/// Drops what a provider's worker published, in every profile, so a
/// reinstalled or changed plugin starts `pending`. Sources, dismissals,
/// templates and mappings are user state and stay.
pub(super) fn forget_publication(conn: &Connection, provider_id: &str) -> rusqlite::Result<()> {
    let tx = conn.unchecked_transaction()?;
    for table in [
        "suggestion_checks",
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

/// Replaces the provider's whole check list.
pub(super) fn set_checks(
    conn: &Connection,
    profile: &str,
    provider_id: &str,
    params: &Value,
) -> Result<Value, DispatchError> {
    let request: SetChecks = serde_json::from_value(params.clone()).map_err(|e| {
        DispatchError::invalid_params(format!("invalid suggestions.checks.set params: {e}"))
    })?;
    validate_checks(&request.checks).map_err(DispatchError::invalid_params)?;
    let json = serde_json::to_string(&request.checks)
        .map_err(|e| DispatchError::internal(format!("encode checks: {e}")))?;
    conn.execute(
        "INSERT INTO suggestion_checks (profile, provider_id, checks_json) VALUES (?1, ?2, ?3)
         ON CONFLICT (profile, provider_id) DO UPDATE SET checks_json = ?3",
        params![profile, provider_id, json],
    )
    .map_err(db_error)?;
    Ok(json!({ "ok": true, "stored": request.checks.len() }))
}

fn validate_checks(checks: &[SetupCheck]) -> Result<(), String> {
    if checks.len() > MAX_CHECKS {
        return Err(format!(
            "suggestions.checks.set accepts at most {MAX_CHECKS} checks"
        ));
    }
    let mut ids = HashSet::new();
    for check in checks {
        let id = &check.id;
        if id.trim().is_empty() || check.label.trim().is_empty() {
            return Err(format!("check {id:?}: id and label must not be empty"));
        }
        let bounded = [
            ("id", id.len(), MAX_KEY_BYTES),
            ("label", check.label.len(), MAX_TITLE_BYTES),
            ("detail", check.detail.len(), MAX_CHECK_TEXT_BYTES),
            ("fix", check.fix.len(), MAX_CHECK_TEXT_BYTES),
        ];
        if let Some((name, _, max)) = bounded.iter().find(|(_, len, max)| len > max) {
            return Err(format!("check {id:?}: {name} is limited to {max} bytes"));
        }
        if !ids.insert(id.as_str()) {
            return Err(format!("duplicate check id {id:?}"));
        }
    }
    Ok(())
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

/// The user's prompt template for a criterion; no row means the provider's
/// `draft.prompt`, while an empty template means an empty prompt.
pub(crate) fn template(
    conn: &Connection,
    profile: &str,
    provider_id: &str,
    criterion: &str,
) -> rusqlite::Result<Option<String>> {
    conn.query_row(
        "SELECT template FROM suggestion_templates
         WHERE profile = ?1 AND provider_id = ?2 AND criterion = ?3",
        params![profile, provider_id, criterion],
        |row| row.get(0),
    )
    .optional()
}

pub(crate) fn set_template(
    conn: &Connection,
    profile: &str,
    provider_id: &str,
    criterion: &str,
    template: &str,
) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT INTO suggestion_templates (profile, provider_id, criterion, template)
         VALUES (?1, ?2, ?3, ?4)
         ON CONFLICT (profile, provider_id, criterion) DO UPDATE SET template = ?4",
        params![profile, provider_id, criterion, template],
    )?;
    Ok(())
}

pub(crate) fn delete_template(
    conn: &Connection,
    profile: &str,
    provider_id: &str,
    criterion: &str,
) -> rusqlite::Result<()> {
    conn.execute(
        "DELETE FROM suggestion_templates
         WHERE profile = ?1 AND provider_id = ?2 AND criterion = ?3",
        params![profile, provider_id, criterion],
    )?;
    Ok(())
}

/// Every stored template of the profile as `(provider_id, criterion, template)`.
pub(crate) fn templates(
    conn: &Connection,
    profile: &str,
) -> rusqlite::Result<Vec<(String, String, String)>> {
    conn.prepare(
        "SELECT provider_id, criterion, template FROM suggestion_templates
         WHERE profile = ?1 ORDER BY provider_id, criterion",
    )?
    .query_map([profile], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))?
    .collect()
}

/// An expanded prompt would exceed [`MAX_PROMPT_BYTES`].
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct PromptTooLong;

/// Replaces `{name}` with the item's variable of that name, scanning left to
/// right: `{a{pr_number}}` becomes `{a42}`. Anything else, including unknown
/// names and stray braces, stays as typed, and substituted values are never
/// scanned again: they are untrusted text. Stops once the output would pass
/// [`MAX_PROMPT_BYTES`].
fn expand(template: &str, variables: &BTreeMap<String, String>) -> Result<String, PromptTooLong> {
    fn push(out: &mut String, text: &str) -> Result<(), PromptTooLong> {
        if out.len() + text.len() > MAX_PROMPT_BYTES {
            return Err(PromptTooLong);
        }
        out.push_str(text);
        Ok(())
    }
    let mut out = String::new();
    let mut rest = template;
    while let Some(open) = rest.find('{') {
        push(&mut out, &rest[..open])?;
        let after = &rest[open + 1..];
        // Names hold no braces, so the first brace after `{` decides.
        let known = after
            .find(['{', '}'])
            .filter(|&i| after.as_bytes()[i] == b'}')
            .and_then(|close| variables.get(&after[..close]).map(|value| (value, close)));
        match known {
            Some((value, close)) => {
                push(&mut out, value)?;
                rest = &after[close + 1..];
            }
            None => {
                push(&mut out, "{")?;
                rest = after;
            }
        }
    }
    push(&mut out, rest)?;
    Ok(out)
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
    let stored: Option<String> = conn
        .query_row(
            "SELECT item_json FROM suggestion_items
             WHERE profile = ?1 AND provider_id = ?2 AND item_id = ?3",
            params![profile, provider.id, id],
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

pub(crate) fn snapshot(
    conn: &Connection,
    profile: &str,
    providers: &[Provider],
) -> anyhow::Result<Value> {
    let dismissed: HashSet<(String, String)> = conn
        .prepare(LIVE_DISMISSALS_SQL)?
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
            "checks": load_checks(conn, profile, &provider.id)?,
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
    if let Some(key) = &item.mapping_key {
        check_mapping_key(key).map_err(|e| format!("suggestion {id:?}: {e}"))?;
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

/// An http(s) URL whose displayed host is the host a browser opens: ASCII
/// only, and an authority that is non-empty, has no userinfo and no
/// percent-encoding (browsers decode it before IDNA), and names a host.
fn is_http_url(url: &str) -> bool {
    if !url.is_ascii() || url.contains(|c: char| c.is_whitespace() || c.is_control() || c == '\\') {
        return false;
    }
    let Some((scheme, rest)) = url.split_once("://") else {
        return false;
    };
    if !scheme.eq_ignore_ascii_case("http") && !scheme.eq_ignore_ascii_case("https") {
        return false;
    }
    let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
    !authority.is_empty() && !authority.contains(['@', '%']) && !authority.starts_with(':')
}

fn check_mapping_key(key: &str) -> Result<(), String> {
    if key.trim().is_empty() {
        return Err("mapping_key must not be empty".into());
    }
    if key.len() > MAX_KEY_BYTES {
        return Err(format!("mapping_key is limited to {MAX_KEY_BYTES} bytes"));
    }
    Ok(())
}

/// The registered project `wanted` names, by exact name or path. Zero or
/// several matches resolve to nothing: a project is never guessed.
fn resolve_hint(projects: &[Project], wanted: &str) -> Option<Project> {
    let mut hits: Vec<&Project> = projects.iter().filter(|p| p.name == wanted).collect();
    if hits.is_empty() {
        hits = by_path(projects, wanted);
    }
    match hits[..] {
        [only] => Some(only.clone()),
        _ => None,
    }
}

fn by_path<'a>(projects: &'a [Project], path: &str) -> Vec<&'a Project> {
    let key = projects::canonical_key(path);
    projects
        .iter()
        .filter(|p| projects::canonical_key(&p.path) == key)
        .collect()
}

/// The registered project at exactly `path`, never matched by name.
fn resolve_path(projects: &[Project], path: &str) -> Option<Project> {
    match by_path(projects, path)[..] {
        [only] => Some(only.clone()),
        _ => None,
    }
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

fn get_mapping(
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

/// The live item plus the mapping its `mapping_key` names, if any.
pub(crate) fn find_item_with_mapping(
    conn: &Connection,
    profile: &str,
    provider: &Provider,
    id: &str,
) -> anyhow::Result<Option<(Suggestion, Option<Vec<String>>)>> {
    let Some(item) = find_item(conn, profile, provider, id)? else {
        return Ok(None);
    };
    let mapped = match &item.mapping_key {
        Some(key) => get_mapping(conn, profile, &provider.id, key)?,
        None => None,
    };
    Ok(Some((item, mapped)))
}

/// Wizard prefill for `item`, resolved through the normal config cascade of its
/// project, or the saved profile settings when it names none. A `template`
/// replaces the provider's prompt, expanded from the item's variables.
///
/// A mapping whose first path resolves supplies the project and the rest of
/// its list as extras. Otherwise the provider's `project` hint applies; an
/// extra is never promoted to primary.
pub(crate) fn prepare(
    profile: &str,
    item: &Suggestion,
    template: Option<&str>,
    mapped: Option<&[String]>,
) -> Result<Value, PromptTooLong> {
    let prompt = match template {
        Some(t) => expand(t, &item.variables)?,
        None => item.draft.prompt.clone(),
    };
    let registered = projects::load_merged(profile).unwrap_or_default();
    let from_mapping = mapped
        .and_then(|paths| paths.split_first())
        .and_then(|(first, rest)| {
            let primary = resolve_path(&registered, first)?;
            let extras: Vec<Project> = rest
                .iter()
                .filter_map(|path| resolve_path(&registered, path))
                .collect();
            Some((primary, extras))
        });
    let (project, extras) = from_mapping.unzip();
    let project = project.or_else(|| {
        item.project
            .as_deref()
            .and_then(|wanted| resolve_hint(&registered, wanted))
    });
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
    let describe = |p: &Project| json!({ "name": p.name, "path": p.path });
    Ok(json!({
        "title": item.draft.title,
        "initial_prompt": prompt,
        "source_ref": item.source_ref,
        "project": project.as_ref().map(describe),
        "extra_projects": extras.unwrap_or_default().iter().map(describe).collect::<Vec<_>>(),
        "tool": config.session.default_tool.unwrap_or_else(|| "claude".to_string()),
        "yolo": config.session.yolo_mode_default,
        "sandbox": config.sandbox.enabled_by_default,
        "worktree": config.worktree.enabled,
    }))
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
            mapping_key: None,
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
        let cases: [(&str, Mutate); 22] = [
            ("undeclared", |i| i.criterion = "nope".into()),
            ("must not be empty", |i| i.title = " ".into()),
            ("http(s)", |i| i.source_url = "javascript:alert(1)".into()),
            ("http(s)", |i| i.source_url = "https://".into()),
            ("http(s)", |i| i.source_url = "https:///x".into()),
            ("http(s)", |i| i.source_url = "https://#f".into()),
            ("http(s)", |i| i.source_url = "https://:80/x".into()),
            ("http(s)", |i| {
                i.source_url = "https://real.com@evil.com/".into()
            }),
            ("http(s)", |i| {
                i.source_url = "https://exam\u{200b}ple.com/".into()
            }),
            ("http(s)", |i| {
                i.source_url = "https://%65xample.com/".into()
            }),
            ("http(s)", |i| {
                i.source_url = "https://exam%E2%80%8Bple.com/".into()
            }),
            ("http(s)", |i| i.source_url = "data:text/html,x".into()),
            ("http(s)", |i| i.source_url = "file:///etc/passwd".into()),
            ("http(s)", |i| i.source_url = "ftp://h/".into()),
            ("limited to 2048", |i| {
                i.source_url = format!("https://h/{}", "x".repeat(2040))
            }),
            ("limited to 256", |i| i.id = "x".repeat(257)),
            ("limited to 16384", |i| {
                i.preview = "x".repeat(16 * 1024 + 1)
            }),
            ("variables must be exactly", |i| i.variables.clear()),
            ("variables must be exactly", |i| {
                i.variables.insert("extra".into(), "v".into());
            }),
            ("mapping_key must not be empty", |i| {
                i.mapping_key = Some(" ".into())
            }),
            ("mapping_key is limited", |i| {
                i.mapping_key = Some("k".repeat(257))
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
        for url in ["HTTPS://example.com/x", "http://host:8080/p?q#f"] {
            let mut ok = item("a", "assigned");
            ok.source_url = url.into();
            assert!(validate_items(&spec(), &HashMap::new(), vec![ok]).is_ok());
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
        publish(&conn, "p", &[]);
        publish(&conn, "p", &[item("a", "assigned")]);
        assert_eq!(snap(&conn, "p")["items"][0]["dismissed"], true);
        assert_eq!(snap(&conn, "other")["items"], json!([]));
        assert!(set_dismissed(&conn, "p", &p, "a", false).unwrap());
        assert_eq!(snap(&conn, "p")["items"][0]["dismissed"], false);
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
                    "suggestion_templates",
                    "criterion, template",
                    "'assigned', 't'",
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
        ] {
            assert_eq!(count(table), 0, "{table}");
        }
        for table in [
            "suggestion_sources",
            "suggestion_dismissals",
            "suggestion_templates",
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
            prepare("default", &i, None, None).unwrap()
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
        assert_eq!(out["extra_projects"], json!([]));
    }

    #[test]
    #[serial_test::serial]
    fn mapping_beats_hint_and_a_stale_primary_never_promotes_an_extra() {
        let _home = crate::session::test_support::isolate_app_dir();
        let dir = tempfile::tempdir().unwrap();
        let mut paths = Vec::new();
        for name in ["api", "web", "hinted"] {
            let path = dir.path().join(name);
            std::fs::create_dir(&path).unwrap();
            let path = path.to_string_lossy().into_owned();
            let scope = projects::ProjectScope::Global;
            projects::add("default", scope, Project::new(name, &path, scope), false).unwrap();
            paths.push(path);
        }
        let gone = dir.path().join("gone").to_string_lossy().into_owned();
        let mapped = |list: &[&String]| list.iter().map(|p| p.to_string()).collect::<Vec<_>>();
        let run = |hint: Option<&str>, mapping: Option<Vec<String>>| {
            let mut i = item("a", "assigned");
            i.project = hint.map(String::from);
            prepare("default", &i, None, mapping.as_deref()).unwrap()
        };
        let names = |v: &Value| -> Vec<String> {
            v["extra_projects"]
                .as_array()
                .unwrap()
                .iter()
                .map(|p| p["name"].as_str().unwrap().to_string())
                .collect()
        };

        let mapping = mapped(&[&paths[0], &gone, &paths[1]]);
        let out = run(Some("hinted"), Some(mapping));
        assert_eq!(out["project"]["name"], "api", "mapping beats the hint");
        assert_eq!(names(&out), ["web"], "stale extras drop, order kept");

        let stale_primary = vec![gone.clone(), paths[1].clone()];
        let out = run(Some("hinted"), Some(stale_primary.clone()));
        assert_eq!(out["project"]["name"], "hinted", "stale primary falls back");
        assert!(names(&out).is_empty(), "an extra is never promoted");
        assert!(run(None, Some(stale_primary))["project"].is_null());
        assert!(run(None, None)["project"].is_null());
        assert_eq!(run(Some("hinted"), None)["project"]["name"], "hinted");
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

    fn check(id: &str, status: &str) -> Value {
        json!({ "id": id, "label": id, "status": status, "detail": "d", "fix": "run it" })
    }

    #[test]
    fn checks_replace_per_provider_and_profile_and_default_to_empty() {
        let conn = store();
        assert_eq!(snap(&conn, "p")["providers"][0]["checks"], json!([]));
        let first = json!({ "checks": [check("cli", "ok"), check("auth", "fail")] });
        assert_eq!(
            set_checks(&conn, "p", "acme.github", &first).unwrap(),
            json!({ "ok": true, "stored": 2 })
        );
        assert_eq!(
            snap(&conn, "p")["providers"][0]["checks"],
            json!([check("cli", "ok"), check("auth", "fail")])
        );
        assert_eq!(snap(&conn, "other")["providers"][0]["checks"], json!([]));
        set_checks(
            &conn,
            "p",
            "acme.github",
            &json!({ "checks": [check("auth", "warn")] }),
        )
        .unwrap();
        assert_eq!(
            snap(&conn, "p")["providers"][0]["checks"],
            json!([check("auth", "warn")])
        );
        set_checks(&conn, "p", "acme.github", &json!({ "checks": [] })).unwrap();
        assert_eq!(snap(&conn, "p")["providers"][0]["checks"], json!([]));
    }

    #[test]
    fn undecodable_stored_checks_read_as_empty_instead_of_failing_the_snapshot() {
        let conn = store();
        conn.execute(
            "INSERT INTO suggestion_checks (profile, provider_id, checks_json) VALUES ('p', 'acme.github', 'not json')",
            [],
        )
        .unwrap();
        assert_eq!(snap(&conn, "p")["providers"][0]["checks"], json!([]));
    }

    #[test]
    fn invalid_checks_are_rejected_and_keep_the_previous_list() {
        let conn = store();
        let good = json!({ "checks": [check("cli", "ok")] });
        set_checks(&conn, "p", "acme.github", &good).unwrap();
        let long = |n| "x".repeat(n);
        let many: Vec<Value> = (0..=MAX_CHECKS)
            .map(|n| check(&n.to_string(), "ok"))
            .collect();
        let cases = [
            ("at most 32", json!({ "checks": many })),
            ("must not be empty", json!({ "checks": [check(" ", "ok")] })),
            (
                "duplicate",
                json!({ "checks": [check("a", "ok"), check("a", "ok")] }),
            ),
            (
                "limited to 256",
                json!({ "checks": [check(&long(257), "ok")] }),
            ),
            (
                "fix is limited to 2048",
                json!({ "checks": [{ "id": "a", "label": "A", "status": "ok", "fix": long(2049) }] }),
            ),
            ("invalid", json!({ "checks": [check("a", "maybe")] })),
        ];
        for (expected, bad) in cases {
            let err = set_checks(&conn, "p", "acme.github", &bad).unwrap_err();
            assert!(
                err.message.contains(expected),
                "{expected:?} not in {:?}",
                err.message
            );
        }
        assert_eq!(
            snap(&conn, "p")["providers"][0]["checks"],
            json!([check("cli", "ok")])
        );
    }

    #[test]
    fn expand_substitutes_only_known_names_once() {
        let vars: BTreeMap<String, String> = [
            ("pr_number".to_string(), "42".to_string()),
            ("repo".to_string(), "{pr_number}".to_string()),
            ("name".to_string(), "日本語".to_string()),
        ]
        .into();
        let cases = [
            ("/aoe-review #{pr_number}", "/aoe-review #42"),
            ("{pr_number}{pr_number}", "4242"),
            ("{repo}", "{pr_number}"),
            ("{bogus} {} { {pr_number", "{bogus} {} { {pr_number"),
            ("{a{pr_number}}", "{a42}"),
            ("no placeholders", "no placeholders"),
            ("", ""),
            ("é{pr_number}日 {é}", "é42日 {é}"),
            ("{name}é{", "日本語é{"),
        ];
        for (template, expected) in cases {
            assert_eq!(
                expand(template, &vars).as_deref(),
                Ok(expected),
                "{template:?}"
            );
        }
    }

    #[test]
    fn expand_stops_at_the_prompt_bound() {
        let vars: BTreeMap<String, String> = [("x".to_string(), "ab".to_string())].into();
        assert!(expand(&"a".repeat(MAX_PROMPT_BYTES), &vars).is_ok());
        assert_eq!(
            expand(&"a".repeat(MAX_PROMPT_BYTES + 1), &vars),
            Err(PromptTooLong)
        );
        // Exactly at the bound, then one byte over, through substitution.
        let at = format!("{}{{x}}", "a".repeat(MAX_PROMPT_BYTES - 2));
        assert_eq!(expand(&at, &vars).unwrap().len(), MAX_PROMPT_BYTES);
        let over = format!("{}{{x}}", "a".repeat(MAX_PROMPT_BYTES - 1));
        assert_eq!(expand(&over, &vars), Err(PromptTooLong));
        // Many braces stay linear and verbatim.
        let braces = "{".repeat(MAX_PROMPT_BYTES);
        assert_eq!(expand(&braces, &vars).unwrap(), braces);
    }

    #[test]
    fn templates_are_stored_per_profile_and_criterion() {
        let conn = store();
        let get = |profile, criterion| template(&conn, profile, "acme.github", criterion).unwrap();
        assert_eq!(get("p", "assigned"), None);
        set_template(&conn, "p", "acme.github", "assigned", "first").unwrap();
        set_template(&conn, "p", "acme.github", "assigned", "").unwrap();
        set_template(&conn, "p", "acme.github", "quiet", "q").unwrap();
        assert_eq!(get("p", "assigned").as_deref(), Some(""));
        assert_eq!(get("other", "assigned"), None);
        assert_eq!(templates(&conn, "p").unwrap().len(), 2);
        delete_template(&conn, "p", "acme.github", "assigned").unwrap();
        assert_eq!(get("p", "assigned"), None);
        assert_eq!(get("p", "quiet").as_deref(), Some("q"));
    }

    #[test]
    #[serial_test::serial]
    fn prepare_applies_the_template_in_place_of_the_draft_prompt() {
        let _home = crate::session::test_support::isolate_app_dir();
        let i = item("a", "assigned");
        let prompt =
            |template| prepare("default", &i, template, None).map(|v| v["initial_prompt"].clone());
        assert_eq!(prompt(None), Ok(json!("Do work")));
        assert_eq!(
            prompt(Some("Fix {repo} {nope}")),
            Ok(json!("Fix acme/widgets {nope}"))
        );
        assert_eq!(prompt(Some("")), Ok(json!("")));
        // 12-byte value, template within the stored bound: expansion is not.
        let bomb = "{repo}".repeat(MAX_PROMPT_BYTES / "{repo}".len());
        assert!(bomb.len() <= MAX_PROMPT_BYTES);
        assert_eq!(prompt(Some(&bomb)), Err(PromptTooLong));
    }

    #[test]
    fn error_message_is_bounded() {
        let conn = store();
        let long = json!({ "message": "x".repeat(MAX_ERROR_BYTES + 1) });
        assert!(error(&conn, "p", "acme.github", &json!({ "message": "boom" })).is_ok());
        assert!(error(&conn, "p", "acme.github", &long).is_err());
    }
}
