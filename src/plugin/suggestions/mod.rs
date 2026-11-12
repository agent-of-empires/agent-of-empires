//! Host side of plugin suggestions: validation of worker RPCs and the
//! profile-scoped store behind them.
//!
//! Rows are keyed by `effective_profile`, resolved by the callers on publish and
//! on read alike. A provider's display name and source metadata come from the
//! manifest; only status, items, source overrides, dismissals and prompt
//! templates are stored.
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

use aoe_plugin_api::suggestions::{MAX_KEY_BYTES, SUGGESTIONS_FIX_PREFIX};
use aoe_plugin_api::{
    SetChecks, SetSuggestions, SetupCheck, Suggestion, SuggestionFix, SuggestionsError,
    SuggestionsSpec,
};
use rusqlite::{params, Connection, OptionalExtension};
use serde_json::{json, Value};

use super::host_api::DispatchError;
use crate::session::config::profile_config::resolve_config_or_warn;
use crate::session::config::repo_config::resolve_config_with_repo_or_warn;
use crate::session::config::StructuredView;
use crate::session::projects::{self, Project};
use crate::session::{Instance, MAX_INITIAL_PROMPT_BYTES};
use std::path::Path;

const MAX_ITEMS: usize = 500;
const MAX_TITLE_BYTES: usize = 512;
const MAX_FIX_BYTES: usize = 64;
const MAX_REASON_BYTES: usize = 2048;
const MAX_URL_BYTES: usize = 2048;
const MAX_PROJECT_BYTES: usize = 4096;
const MAX_PREVIEW_BYTES: usize = 16 * 1024;
const MAX_VARIABLE_VALUE_BYTES: usize = 2048;
const MAX_ERROR_BYTES: usize = 4096;
const MAX_CHECKS: usize = 32;
/// How long an item stays listed after its provider marked it stale.
const STALE_TTL_SECS: i64 = 24 * 60 * 60;
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

const SCHEMA_VERSION: i64 = 2;

/// Every suggestion table, including the legacy `suggestion_policy`, dropped
/// when the recorded schema version differs.
const TABLES: [&str; 12] = [
    "suggestion_providers",
    "suggestion_items",
    "suggestion_sources",
    "suggestion_dismissals",
    "suggestion_templates",
    "suggestion_mappings",
    "suggestion_fixes",
    "suggestion_stale",
    "suggestion_checks",
    "suggestion_preferences",
    "suggestion_provider_prefs",
    "suggestion_policy",
];

pub(crate) fn init(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS suggestion_schema (
             id      INTEGER PRIMARY KEY CHECK (id = 1),
             version INTEGER NOT NULL
         );",
    )?;
    let version: Option<i64> = conn
        .query_row(
            "SELECT version FROM suggestion_schema WHERE id = 1",
            [],
            |r| r.get(0),
        )
        .optional()?;
    if version != Some(SCHEMA_VERSION) {
        for table in TABLES {
            conn.execute_batch(&format!("DROP TABLE IF EXISTS {table};"))?;
        }
        conn.execute(
            "INSERT OR REPLACE INTO suggestion_schema (id, version) VALUES (1, ?1)",
            params![SCHEMA_VERSION],
        )?;
    }
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
             position    INTEGER NOT NULL DEFAULT 0,
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
         CREATE TABLE IF NOT EXISTS suggestion_fixes (
             profile     TEXT NOT NULL,
             provider_id TEXT NOT NULL,
             label       TEXT NOT NULL,
             method      TEXT NOT NULL,
             PRIMARY KEY (profile, provider_id)
         );
         CREATE TABLE IF NOT EXISTS suggestion_stale (
             profile     TEXT NOT NULL,
             provider_id TEXT NOT NULL,
             item_id     TEXT NOT NULL,
             since       INTEGER NOT NULL,
             PRIMARY KEY (profile, provider_id, item_id)
         );
         CREATE TABLE IF NOT EXISTS suggestion_checks (
             profile     TEXT NOT NULL,
             provider_id TEXT NOT NULL,
             checks_json TEXT NOT NULL,
             PRIMARY KEY (profile, provider_id)
         );
         CREATE TABLE IF NOT EXISTS suggestion_preferences (
             profile         TEXT PRIMARY KEY,
             homepage        INTEGER NOT NULL DEFAULT 1,
             sidebar         INTEGER NOT NULL DEFAULT 0,
             refresh_minutes INTEGER NOT NULL DEFAULT 15,
             start_without_review INTEGER NOT NULL DEFAULT 0,
             show_source_ref INTEGER NOT NULL DEFAULT 1
         );
         CREATE TABLE IF NOT EXISTS suggestion_provider_prefs (
             profile     TEXT NOT NULL,
             provider_id TEXT NOT NULL,
             enabled     INTEGER NOT NULL DEFAULT 1,
             sort_order  INTEGER NOT NULL DEFAULT 2147483647,
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

mod ingest;
mod prefs;
mod prepare;
mod store;

pub(crate) use ingest::*;
pub(crate) use prefs::*;
pub(crate) use prepare::*;
pub(crate) use store::*;

#[cfg(test)]
mod fixtures {
    use super::*;
    use aoe_plugin_api::{SuggestionDraft, SuggestionSource};

    pub(super) fn spec() -> SuggestionsSpec {
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
            uses_agent: false,
        }
    }

    pub(super) fn item(id: &str, criterion: &str) -> Suggestion {
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
            stale: false,
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

    pub(super) fn store() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        init(&conn).unwrap();
        conn
    }

    pub(super) fn provider() -> Provider {
        Provider {
            id: "acme.github".into(),
            name: "GitHub".into(),
            spec: spec(),
        }
    }

    pub(super) fn publish(conn: &Connection, profile: &str, items: &[Suggestion]) -> Value {
        set(
            conn,
            profile,
            "acme.github",
            &spec(),
            &json!({ "items": items }),
        )
        .unwrap()
    }

    pub(super) fn snap(conn: &Connection, profile: &str) -> Value {
        snapshot(conn, profile, &[provider()]).unwrap()
    }

    pub(super) fn ids(snapshot: &Value) -> Vec<&str> {
        snapshot["items"]
            .as_array()
            .unwrap()
            .iter()
            .map(|i| i["id"].as_str().unwrap())
            .collect()
    }

    pub(super) fn check(id: &str, status: &str) -> Value {
        json!({ "id": id, "label": id, "status": status, "detail": "d", "fix": "run it" })
    }
}

#[cfg(test)]
mod tests {
    use super::fixtures::*;
    use super::*;

    #[test]
    fn a_store_from_another_schema_version_is_rebuilt() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE suggestion_items (
                 profile TEXT NOT NULL, provider_id TEXT NOT NULL, item_id TEXT NOT NULL,
                 item_json TEXT NOT NULL, revision INTEGER NOT NULL,
                 PRIMARY KEY (profile, provider_id, item_id)
             );
             CREATE TABLE suggestion_policy (profile TEXT PRIMARY KEY);
             CREATE TABLE suggestion_schema (id INTEGER PRIMARY KEY, version INTEGER NOT NULL);
             INSERT INTO suggestion_schema VALUES (1, 0);",
        )
        .unwrap();
        init(&conn).unwrap();
        assert_eq!(publish(&conn, "p", &[item("a", "assigned")])["accepted"], 1);
        let version: i64 = conn
            .query_row("SELECT version FROM suggestion_schema", [], |r| r.get(0))
            .unwrap();
        assert_eq!(version, SCHEMA_VERSION);
        let policy: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE name = 'suggestion_policy'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(policy, 0);
        // A matching version keeps the data.
        init(&conn).unwrap();
        assert_eq!(ids(&snap(&conn, "p")), ["a"]);
    }
}
