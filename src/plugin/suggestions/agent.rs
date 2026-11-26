//! The built-in provider: items pushed by agent sessions (MCP tool, CLI) instead
//! of a plugin worker. Items persist in the same store as plugin items.

use super::*;

pub(crate) const PROVIDER_ID: &str = "builtin.agent";
pub(crate) const CRITERION: &str = "agent";

pub(crate) fn provider() -> Provider {
    Provider {
        id: PROVIDER_ID.to_string(),
        name: "Agent".to_string(),
        spec: SuggestionsSpec {
            sources: vec![aoe_plugin_api::suggestions::SuggestionSource {
                criterion: CRITERION.to_string(),
                label: "Agent suggestions".to_string(),
                description: "Suggestions an agent session pushed.".to_string(),
                variables: Vec::new(),
                default_enabled: true,
            }],
            uses_agent: false,
        },
    }
}

/// What a session or the CLI supplies for one suggestion.
pub(crate) struct Push {
    /// A stable key; pushing it again replaces the item. Generated when absent.
    pub id: Option<String>,
    pub title: String,
    pub reason: String,
    pub prompt: String,
    pub project: Option<String>,
    /// Who pushed it, shown as the item's source reference.
    pub pushed_by: String,
}

/// Adds or replaces one item. Returns its id.
pub(crate) fn push(conn: &Connection, profile: &str, input: Push) -> Result<Value, DispatchError> {
    let provider = provider();
    let id = input
        .id
        .filter(|id| !id.trim().is_empty())
        .unwrap_or_else(|| uuid::Uuid::new_v4().simple().to_string());
    let item = Suggestion {
        id: id.clone(),
        title: input.title.clone(),
        reason: input.reason,
        source_url: String::new(),
        source_ref: input.pushed_by,
        preview: String::new(),
        project: input.project.filter(|p| !p.trim().is_empty()),
        mapping_key: None,
        criterion: CRITERION.to_string(),
        stale: false,
        variables: BTreeMap::new(),
        draft: aoe_plugin_api::SuggestionDraft {
            title: input.title,
            prompt: input.prompt,
        },
    };
    ingest::validate_item(&provider.spec, &item, true).map_err(DispatchError::invalid_params)?;
    let json = serde_json::to_string(&item)
        .map_err(|e| DispatchError::internal(format!("encode suggestion: {e}")))?;
    let tx = conn.unchecked_transaction().map_err(db_error)?;
    let (count, known): (i64, bool) = tx
        .query_row(
            "SELECT COUNT(*), COALESCE(MAX(item_id = ?3), 0) FROM suggestion_items
             WHERE profile = ?1 AND provider_id = ?2",
            params![profile, PROVIDER_ID, id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .map_err(db_error)?;
    if !known && count as usize >= MAX_ITEMS {
        return Err(DispatchError::invalid_params(format!(
            "at most {MAX_ITEMS} agent suggestions; dismiss some first"
        )));
    }
    // New items list first.
    tx.execute(
        "INSERT INTO suggestion_items (profile, provider_id, item_id, item_json, position, spec_hash)
         VALUES (?1, ?2, ?3, ?4,
                 (SELECT COALESCE(MIN(position), 0) - 1 FROM suggestion_items
                  WHERE profile = ?1 AND provider_id = ?2),
                 ?5)
         ON CONFLICT (profile, provider_id, item_id)
         DO UPDATE SET item_json = ?4, spec_hash = ?5",
        params![profile, PROVIDER_ID, id, json, spec_hash(&provider.spec)],
    )
    .map_err(db_error)?;
    record_status(&tx, profile, PROVIDER_ID, None).map_err(db_error)?;
    tx.commit().map_err(db_error)?;
    Ok(json!({ "ok": true, "id": id }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn conn() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        init(&conn).unwrap();
        conn
    }

    fn push_one(conn: &Connection, id: Option<&str>, title: &str) -> Result<Value, DispatchError> {
        push(
            conn,
            "p",
            Push {
                id: id.map(str::to_string),
                title: title.into(),
                reason: "why".into(),
                prompt: "do it".into(),
                project: None,
                pushed_by: "Session A".into(),
            },
        )
    }

    #[test]
    fn pushed_items_show_without_setup_and_replace_by_id() {
        let conn = conn();
        let first = push_one(&conn, None, "One").unwrap();
        assert!(first["id"].as_str().is_some_and(|id| !id.is_empty()));
        push_one(&conn, Some("k"), "Two").unwrap();
        push_one(&conn, Some("k"), "Two again").unwrap();

        let snap = snapshot(&conn, "p", &[provider()]).unwrap();
        assert_eq!(snap["providers"][0]["enabled"], true);
        assert_eq!(snap["providers"][0]["status"], "ok");
        let titles: Vec<&str> = snap["items"]
            .as_array()
            .unwrap()
            .iter()
            .map(|i| i["title"].as_str().unwrap())
            .collect();
        assert_eq!(
            titles,
            ["Two again", "One"],
            "newest first, same id replaced"
        );
        assert_eq!(snap["items"][0]["source_ref"], "Session A");
    }

    #[test]
    fn empty_provider_is_ok_and_refused_input_is_invalid_params() {
        let conn = conn();
        let snap = snapshot(&conn, "p", &[provider()]).unwrap();
        assert_eq!(snap["providers"][0]["status"], "ok");
        let err = push_one(&conn, None, "  ").unwrap_err();
        assert_eq!(err.code, crate::plugin::protocol::codes::INVALID_PARAMS);
    }
}
