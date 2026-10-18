//! Persistent profile-scoped snapshots published by plugin workers.
use super::host_api::HostApiState;
use aoe_plugin_api::Suggestion;
use rusqlite::{params, OptionalExtension};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::HashMap;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SuggestionIdentity {
    pub provider_id: String,
    pub id: String,
    pub revision: i64,
}
#[derive(Default)]
pub struct Generations {
    active: HashMap<String, u64>,
}
impl Generations {
    pub fn begin(&mut self, id: &str, n: u64) {
        self.active.insert(id.into(), n);
    }
    pub fn is_active(&self, id: &str, n: u64) -> bool {
        self.active.get(id) == Some(&n)
    }
    pub fn clear(&mut self, id: &str, n: u64) -> bool {
        if !self.is_active(id, n) {
            return false;
        }
        self.active.remove(id);
        true
    }
}
pub fn initialize(db: &rusqlite::Connection) -> anyhow::Result<()> {
    db.execute_batch(
        "CREATE TABLE IF NOT EXISTS suggestion_items (
            profile TEXT NOT NULL, provider_id TEXT NOT NULL, item_id TEXT NOT NULL,
            item_json TEXT NOT NULL, revision INTEGER NOT NULL,
            active INTEGER NOT NULL DEFAULT 1, dismissed INTEGER NOT NULL DEFAULT 0,
            session_id TEXT, PRIMARY KEY(profile,provider_id,item_id));
         CREATE TABLE IF NOT EXISTS suggestion_providers (
            profile TEXT NOT NULL, provider_id TEXT NOT NULL, provider_name TEXT NOT NULL,
            status TEXT NOT NULL, error TEXT, updated_at INTEGER NOT NULL,
            PRIMARY KEY(profile,provider_id));
         CREATE TABLE IF NOT EXISTS suggestion_policy (
            profile TEXT PRIMARY KEY, enabled INTEGER NOT NULL DEFAULT 0,
            rules_json TEXT NOT NULL DEFAULT '[]', last_error TEXT);",
    )?;
    Ok(())
}
impl HostApiState {
    pub fn with_suggestions_db<T>(
        &self,
        f: impl FnOnce(&rusqlite::Connection) -> anyhow::Result<T>,
    ) -> anyhow::Result<T> {
        let db = self.events.lock().unwrap_or_else(|p| p.into_inner());
        f(&db)
    }
    pub fn snapshot(&self, profile: &str) -> anyhow::Result<Value> {
        self.with_suggestions_db(|db| {
            let mut provider_query = db.prepare(
                "SELECT provider_id,provider_name,status,error,updated_at
                 FROM suggestion_providers WHERE profile=?1 ORDER BY provider_name",
            )?;
            let providers: Vec<Value> = provider_query
                .query_map([profile], |row| {
                    let updated_at: i64 = row.get(4)?;
                    Ok(json!({
                        "provider_id": row.get::<_, String>(0)?,
                        "provider_name": row.get::<_, String>(1)?,
                        "status": row.get::<_, String>(2)?,
                        "error": row.get::<_, Option<String>>(3)?,
                        "updated_at": chrono::DateTime::<chrono::Utc>::from_timestamp(updated_at, 0)
                            .map(|timestamp| timestamp.to_rfc3339())
                    }))
                })?
                .collect::<Result<_, _>>()?;

            let mut item_query = db.prepare(
                "SELECT i.item_json,i.provider_id,p.provider_name,i.revision,
                        i.dismissed,i.session_id
                 FROM suggestion_items i
                 JOIN suggestion_providers p USING(profile,provider_id)
                 WHERE i.profile=?1 AND i.active=1 ORDER BY p.provider_name,i.item_id",
            )?;
            let stored_items: Vec<(String, String, String, i64, bool, Option<String>)> = item_query
                .query_map([profile], |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                        row.get(5)?,
                    ))
                })?
                .collect::<Result<_, _>>()?;
            let items: Vec<Value> = stored_items
                .into_iter()
                .map(
                    |(text, provider_id, provider_name, revision, dismissed, session_id)| {
                        let mut item: Value = serde_json::from_str(&text)?;
                        item["provider_id"] = json!(provider_id);
                        item["provider_name"] = json!(provider_name);
                        item["revision"] = json!(revision);
                        item["dismissed"] = json!(dismissed);
                        item["session_id"] = json!(session_id);
                        item["project_path"] = Value::Null;
                        item["can_start"] = json!(false);
                        item["start_blocked_reason"] = json!("Resolving project");
                        Ok(item)
                    },
                )
                .collect::<anyhow::Result<_>>()?;
            let policy: Option<(bool, String, Option<String>)> = db
                .query_row(
                    "SELECT enabled,rules_json,last_error FROM suggestion_policy WHERE profile=?1",
                    [profile],
                    |row| Ok((row.get::<_, bool>(0)?, row.get(1)?, row.get(2)?)),
                )
                .optional()?;
            let (enabled, rules, last_error) = match policy {
                Some((enabled, rules, last_error)) => {
                    (enabled, serde_json::from_str(&rules)?, last_error)
                }
                None => (false, json!([]), None),
            };
            Ok(json!({
                "items": items,
                "providers": providers,
                "auto_create": {
                    "enabled": enabled,
                    "rules": rules,
                    "last_error": last_error
                }
            }))
        })
    }
    pub fn publish_suggestions(
        &self,
        p: &str,
        g: u64,
        name: &str,
        items: &[Suggestion],
    ) -> anyhow::Result<bool> {
        let generations = self.lock_suggestion_generations();
        if !generations.is_active(p, g) {
            return Ok(false);
        }
        let profile = self.suggestion_profile();
        self.with_suggestions_db(|db| {
            let tx = db.unchecked_transaction()?;
            tx.execute(
                "INSERT INTO suggestion_providers
                 (profile,provider_id,provider_name,status,error,updated_at)
                 VALUES(?1,?2,?3,'ok',NULL,?4)
                 ON CONFLICT(profile,provider_id) DO UPDATE SET
                 provider_name=excluded.provider_name,status='ok',error=NULL,
                 updated_at=excluded.updated_at",
                params![profile, p, name, chrono::Utc::now().timestamp()],
            )?;
            let mut live = std::collections::HashSet::with_capacity(items.len());
            for item in items {
                if !live.insert(item.id.as_str()) {
                    anyhow::bail!("duplicate suggestion id {}", item.id);
                }
                let json = serde_json::to_string(item)?;
                let old: Option<(String, i64, bool)> = tx.query_row(
                    "SELECT item_json,revision,active FROM suggestion_items
                     WHERE profile=?1 AND provider_id=?2 AND item_id=?3",
                    params![profile, p, item.id],
                    |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
                ).optional()?;
                let revision = old.as_ref().map_or(1, |(prior, revision, active)| {
                    if prior == &json && *active { *revision } else { revision + 1 }
                });
                tx.execute(
                    "INSERT INTO suggestion_items(profile,provider_id,item_id,item_json,revision)
                     VALUES(?1,?2,?3,?4,?5)
                     ON CONFLICT(profile,provider_id,item_id) DO UPDATE SET
                     item_json=excluded.item_json,revision=excluded.revision,active=1",
                    params![profile, p, item.id, json, revision],
                )?;
            }
            let keys = {
                let mut query = tx.prepare(
                    "SELECT item_id FROM suggestion_items WHERE profile=?1 AND provider_id=?2 AND active=1",
                )?;
                let rows = query.query_map(params![profile, p], |row| row.get::<_, String>(0))?
                    .collect::<Result<Vec<_>, _>>()?;
                rows
            };
            for id in keys {
                if !live.contains(id.as_str()) {
                    // Keep host decisions and revision history outside the live snapshot.
                    tx.execute(
                        "UPDATE suggestion_items SET active=0
                         WHERE profile=?1 AND provider_id=?2 AND item_id=?3",
                        params![profile, p, id],
                    )?;
                }
            }
            tx.commit()?;
            Ok(true)
        })
    }
    pub fn report_suggestion_error(
        &self,
        p: &str,
        g: u64,
        name: &str,
        msg: &str,
    ) -> anyhow::Result<bool> {
        let generations = self.lock_suggestion_generations();
        if !generations.is_active(p, g) {
            return Ok(false);
        }
        let profile = self.suggestion_profile();
        self.with_suggestions_db(|db| {
            db.execute(
                "INSERT INTO suggestion_providers
                 (profile,provider_id,provider_name,status,error,updated_at)
                 VALUES(?1,?2,?3,'error',?4,?5)
                 ON CONFLICT(profile,provider_id) DO UPDATE SET
                 provider_name=excluded.provider_name,status='error',
                 error=excluded.error,updated_at=excluded.updated_at",
                params![profile, p, name, msg, chrono::Utc::now().timestamp()],
            )?;
            Ok(true)
        })
    }
    pub fn remove_suggestion_provider(&self, provider_id: &str) -> anyhow::Result<()> {
        self.with_suggestions_db(|db| {
            // Hide snapshots while retaining host decisions for a reinstall.
            db.execute(
                "DELETE FROM suggestion_providers WHERE provider_id=?1",
                [provider_id],
            )?;
            Ok(())
        })
    }

    pub fn reconcile_suggestion_providers(
        &self,
        active_provider_ids: &std::collections::HashSet<String>,
    ) -> anyhow::Result<()> {
        self.with_suggestions_db(|db| {
            let mut statement =
                db.prepare("SELECT DISTINCT provider_id FROM suggestion_providers")?;
            let providers: Vec<String> = statement
                .query_map([], |row| row.get(0))?
                .collect::<Result<_, _>>()?;
            for provider in providers {
                if !active_provider_ids.contains(&provider) {
                    // Item rows retain host decisions, hidden by the public join.
                    db.execute(
                        "DELETE FROM suggestion_providers WHERE provider_id=?1",
                        [provider],
                    )?;
                }
            }
            Ok(())
        })
    }
    pub fn set_dismissed(
        &self,
        profile: &str,
        provider: &str,
        id: &str,
        revision: i64,
        dismissed: bool,
    ) -> anyhow::Result<bool> {
        self.with_suggestions_db(|db| Ok(db.execute(
            "UPDATE suggestion_items SET dismissed=?5 WHERE profile=?1 AND provider_id=?2 AND item_id=?3 AND revision=?4 AND active=1 AND EXISTS(SELECT 1 FROM suggestion_providers p WHERE p.profile=?1 AND p.provider_id=?2)",
            params![profile, provider, id, revision, dismissed as i64],
        )? == 1))
    }

    pub fn prepare_suggestion(
        &self,
        profile: &str,
        provider: &str,
        id: &str,
        revision: i64,
    ) -> anyhow::Result<Option<Value>> {
        self.with_suggestions_db(|db| {
            let row: Option<(String, i64)> = db.query_row(
                "SELECT i.item_json,i.revision FROM suggestion_items i JOIN suggestion_providers p USING(profile,provider_id) WHERE i.profile=?1 AND i.provider_id=?2 AND i.item_id=?3 AND i.active=1",
                params![profile, provider, id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            ).optional()?;
            let Some((json, current)) = row else { return Ok(None) };
            if current != revision { return Ok(None) }
            let item: Suggestion = serde_json::from_str(&json)?;
            Ok(Some(json!({"project_path":item.project,"title":item.draft.title,"prompt":item.draft.prompt,"source_ref":item.source_ref,"suggestion":{"provider_id":provider,"id":id,"revision":revision}})))
        })
    }
    pub(crate) fn associate_suggestion(
        &self,
        profile: &str,
        identity: &SuggestionIdentity,
        session_id: &str,
    ) -> anyhow::Result<bool> {
        self.with_suggestions_db(|db| {
            Ok(db.execute(
                "UPDATE suggestion_items SET session_id=?5
                 WHERE profile=?1 AND provider_id=?2 AND item_id=?3 AND revision=?4",
                params![
                    profile,
                    identity.provider_id,
                    identity.id,
                    identity.revision,
                    session_id
                ],
            )? == 1)
        })
    }

    pub fn update_policy(&self, profile: &str, enabled: bool, rules: &Value) -> anyhow::Result<()> {
        self.with_suggestions_db(|db| {
            db.execute(
                "INSERT INTO suggestion_policy(profile,enabled,rules_json,last_error) VALUES(?1,?2,?3,NULL) ON CONFLICT(profile) DO UPDATE SET enabled=excluded.enabled,rules_json=excluded.rules_json,last_error=NULL",
                params![profile, enabled as i64, serde_json::to_string(rules)?],
            )?;
            Ok(())
        })
    }
    pub fn set_policy_error(&self, profile: &str, error: Option<&str>) -> anyhow::Result<()> {
        self.with_suggestions_db(|db| {
            db.execute(
                "UPDATE suggestion_policy SET last_error=?2 WHERE profile=?1",
                params![profile, error],
            )?;
            Ok(())
        })
    }
}
#[cfg(test)]
mod tests {
    use super::*;

    fn suggestion() -> Suggestion {
        Suggestion {
            id: "owner/repo/pull/1".into(),
            title: "Review PR".into(),
            reason: "Review requested".into(),
            source_url: "https://github.com/owner/repo/pull/1".into(),
            source_ref: "https://github.com/owner/repo/pull/1".into(),
            preview: "Untrusted PR body".into(),
            project: Some("repo".into()),
            criterion: "review_requested".into(),
            draft: aoe_plugin_api::SuggestionDraft {
                title: "Review PR".into(),
                prompt: "Review the pull request.".into(),
            },
        }
    }

    fn host(dir: &std::path::Path) -> HostApiState {
        HostApiState::open(&dir.join("plugin_events.db"), "default", 100).unwrap()
    }

    #[test]
    fn provider_removal_hides_items_but_preserves_dismissals_for_reinstall() {
        let dir = tempfile::tempdir().unwrap();
        let host = host(dir.path());
        host.lock_suggestion_generations()
            .begin("example.provider", 1);
        assert!(host
            .publish_suggestions("example.provider", 1, "Example", &[suggestion()])
            .unwrap());
        assert!(host
            .set_dismissed("default", "example.provider", "owner/repo/pull/1", 1, true)
            .unwrap());

        host.remove_suggestion_provider("example.provider").unwrap();
        assert!(host.snapshot("default").unwrap()["items"]
            .as_array()
            .unwrap()
            .is_empty());

        host.lock_suggestion_generations()
            .begin("example.provider", 2);
        assert!(host
            .publish_suggestions("example.provider", 2, "Example", &[suggestion()])
            .unwrap());
        let snapshot = host.snapshot("default").unwrap();
        assert_eq!(snapshot["items"][0]["dismissed"], true);
    }

    #[test]
    fn provider_errors_preserve_snapshot_and_successful_empty_snapshot_clears_it() {
        let dir = tempfile::tempdir().unwrap();
        let host = host(dir.path());
        host.lock_suggestion_generations()
            .begin("example.provider", 1);
        assert!(host
            .publish_suggestions("example.provider", 1, "Example", &[suggestion()])
            .unwrap());

        assert!(host
            .report_suggestion_error("example.provider", 1, "Example", "network failure")
            .unwrap());
        let failed = host.snapshot("default").unwrap();
        assert_eq!(failed["items"].as_array().unwrap().len(), 1);
        assert_eq!(failed["providers"][0]["status"], "error");
        assert!(chrono::DateTime::parse_from_rfc3339(
            failed["providers"][0]["updated_at"]
                .as_str()
                .expect("provider timestamp is an ISO-8601 string")
        )
        .is_ok());

        assert!(host
            .publish_suggestions("example.provider", 1, "Example", &[])
            .unwrap());
        let empty = host.snapshot("default").unwrap();
        assert!(empty["items"].as_array().unwrap().is_empty());
        assert_eq!(empty["providers"][0]["status"], "ok");
    }

    #[test]
    fn suggestion_session_association_is_revision_guarded_and_persists_across_restart() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("plugin_events.db");
        let host = HostApiState::open(&db_path, "default", 100).unwrap();
        host.lock_suggestion_generations()
            .begin("example.provider", 1);
        assert!(host
            .publish_suggestions("example.provider", 1, "Example", &[suggestion()])
            .unwrap());

        let identity = SuggestionIdentity {
            provider_id: "example.provider".into(),
            id: "owner/repo/pull/1".into(),
            revision: 1,
        };
        assert!(!host
            .associate_suggestion(
                "default",
                &SuggestionIdentity {
                    revision: 2,
                    ..identity.clone()
                },
                "wrong-session"
            )
            .unwrap());
        assert!(host
            .associate_suggestion("default", &identity, "session-123")
            .unwrap());
        assert_eq!(
            host.snapshot("default").unwrap()["items"][0]["session_id"],
            "session-123"
        );

        drop(host);
        let reopened = HostApiState::open(&db_path, "default", 100).unwrap();
        assert_eq!(
            reopened.snapshot("default").unwrap()["items"][0]["session_id"],
            "session-123"
        );
    }

    #[test]
    fn stale_workers_and_duplicate_ids_cannot_mutate_durable_snapshot() {
        let dir = tempfile::tempdir().unwrap();
        let host = host(dir.path());
        let provider = "example.provider";
        let old = host.begin_ui_generation(provider);
        let current = host.begin_ui_generation(provider);
        assert!(!host.clear_ui(provider, old));
        assert!(host
            .publish_suggestions(provider, current, "Example", &[suggestion()])
            .unwrap());
        assert!(host
            .set_dismissed("default", provider, &suggestion().id, 1, true)
            .unwrap());
        assert!(host
            .report_suggestion_error(provider, current, "Example", "offline")
            .unwrap());
        let before = host.snapshot("default").unwrap();
        assert!(!host
            .publish_suggestions(provider, old, "Forged", &[])
            .unwrap());
        assert!(!host
            .report_suggestion_error(provider, old, "Forged", "late error")
            .unwrap());
        assert!(host
            .publish_suggestions(provider, current, "Changed", &[suggestion(), suggestion()])
            .is_err());
        assert_eq!(host.snapshot("default").unwrap(), before);
        host.clear_ui(provider, current);
        assert!(!host
            .publish_suggestions(provider, current, "Example", &[])
            .unwrap());
    }

    #[test]
    fn revisions_guard_actions_and_retractions_keep_host_decisions() {
        let dir = tempfile::tempdir().unwrap();
        let host = host(dir.path());
        let provider = "example.provider";
        let generation = host.begin_ui_generation(provider);
        let item = suggestion();
        host.publish_suggestions(provider, generation, "Example", std::slice::from_ref(&item))
            .unwrap();
        host.set_dismissed("default", provider, &item.id, 1, true)
            .unwrap();
        let mut changed = item.clone();
        changed.draft.prompt.push_str(" Include tests.");
        host.publish_suggestions(
            provider,
            generation,
            "Example",
            std::slice::from_ref(&changed),
        )
        .unwrap();
        assert!(host
            .prepare_suggestion("default", provider, &item.id, 1)
            .unwrap()
            .is_none());
        assert!(!host
            .set_dismissed("default", provider, &item.id, 1, false)
            .unwrap());
        assert!(host
            .prepare_suggestion("other", provider, &item.id, 2)
            .unwrap()
            .is_none());
        assert!(host
            .set_dismissed("default", provider, &item.id, 2, false)
            .unwrap());
        assert_eq!(
            host.snapshot("default").unwrap()["items"][0]["session_id"],
            Value::Null
        );
        host.associate_suggestion(
            "default",
            &SuggestionIdentity {
                provider_id: provider.into(),
                id: item.id.clone(),
                revision: 2,
            },
            "session",
        )
        .unwrap();
        host.publish_suggestions(provider, generation, "Example", &[])
            .unwrap();
        assert!(host.snapshot("default").unwrap()["items"]
            .as_array()
            .unwrap()
            .is_empty());
        assert!(host
            .prepare_suggestion("default", provider, &item.id, 2)
            .unwrap()
            .is_none());
        host.publish_suggestions(provider, generation, "Example", &[changed])
            .unwrap();
        let restored = host.snapshot("default").unwrap();
        assert_eq!(restored["items"][0]["revision"], 3);
        assert_eq!(restored["items"][0]["session_id"], "session");
        host.reconcile_suggestion_providers(&Default::default())
            .unwrap();
        assert!(host
            .prepare_suggestion("default", provider, &item.id, 3)
            .unwrap()
            .is_none());
        assert!(!host
            .set_dismissed("default", provider, &item.id, 3, true)
            .unwrap());
    }

    #[test]
    fn explicit_policy_and_errors_persist_per_profile_and_disable_clears_error() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("plugin_events.db");
        let host = HostApiState::open(&db, "default", 100).unwrap();
        assert_eq!(
            host.snapshot("work").unwrap()["auto_create"]["enabled"],
            false
        );
        let rules = json!([{"provider_id":"example.provider","criterion":"assigned"}]);
        host.update_policy("work", true, &rules).unwrap();
        host.set_policy_error("work", Some("Repository hooks require trust"))
            .unwrap();
        drop(host);
        let host = HostApiState::open(&db, "default", 100).unwrap();
        let policy = host.snapshot("work").unwrap()["auto_create"].clone();
        assert_eq!(policy["enabled"], true);
        assert_eq!(policy["rules"], rules);
        assert_eq!(policy["last_error"], "Repository hooks require trust");
        assert_eq!(
            host.snapshot("default").unwrap()["auto_create"]["enabled"],
            false
        );
        host.update_policy("work", false, &rules).unwrap();
        assert_eq!(
            host.snapshot("work").unwrap()["auto_create"]["last_error"],
            Value::Null
        );
    }
}
