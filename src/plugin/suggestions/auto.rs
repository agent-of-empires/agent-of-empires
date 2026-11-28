//! Opt-in auto start: the per provider and criterion switch, the once-only
//! ledger, and the notes saying why an item was left as a suggestion.
//!
//! Off unless a row exists. The ledger is written before a Session is created, so
//! a crash or a failed create never starts the same item twice.
use super::*;

pub(crate) const DEFAULT_CAP: u32 = 3;
pub(crate) const MAX_CAP: u32 = 10;

/// `Some(cap)` turns auto start on for the criterion, `None` off.
pub(crate) fn set_auto_start(
    conn: &Connection,
    profile: &str,
    provider_id: &str,
    criterion: &str,
    cap: Option<u32>,
) -> rusqlite::Result<()> {
    match cap {
        Some(cap) => conn.execute(
            "INSERT INTO suggestion_auto_start (profile, provider_id, criterion, cap)
             VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT (profile, provider_id, criterion) DO UPDATE SET cap = ?4",
            params![profile, provider_id, criterion, cap],
        )?,
        None => conn.execute(
            "DELETE FROM suggestion_auto_start
             WHERE profile = ?1 AND provider_id = ?2 AND criterion = ?3",
            params![profile, provider_id, criterion],
        )?,
    };
    Ok(())
}

/// The per-refresh cap of each criterion with auto start on, by `(provider, criterion)`.
pub(crate) fn auto_starts(
    conn: &Connection,
    profile: &str,
) -> rusqlite::Result<HashMap<(String, String), u32>> {
    conn.prepare(
        "SELECT provider_id, criterion, cap FROM suggestion_auto_start WHERE profile = ?1",
    )?
    .query_map([profile], |row| {
        Ok(((row.get(0)?, row.get(1)?), row.get(2)?))
    })?
    .collect()
}

/// Records the attempt before the create. `false` when the item was attempted before.
pub(crate) fn claim_auto(
    conn: &Connection,
    profile: &str,
    provider_id: &str,
    item_id: &str,
) -> rusqlite::Result<bool> {
    Ok(conn.execute(
        "INSERT OR IGNORE INTO suggestion_auto_started (profile, provider_id, item_id, failure)
         VALUES (?1, ?2, ?3, NULL)",
        params![profile, provider_id, item_id],
    )? > 0)
}

/// Keeps the ledger row and records why the create failed.
pub(crate) fn fail_auto(
    conn: &Connection,
    profile: &str,
    provider_id: &str,
    item_id: &str,
    reason: &str,
) -> rusqlite::Result<()> {
    conn.execute(
        "UPDATE suggestion_auto_started SET failure = ?4
         WHERE profile = ?1 AND provider_id = ?2 AND item_id = ?3",
        params![profile, provider_id, item_id, reason],
    )?;
    Ok(())
}

/// Replaces the provider's skip notes: `(item_id, note)`.
pub(crate) fn set_auto_skips(
    conn: &Connection,
    profile: &str,
    provider_id: &str,
    skips: &[(String, String)],
) -> rusqlite::Result<()> {
    let tx = conn.unchecked_transaction()?;
    tx.execute(
        "DELETE FROM suggestion_auto_skips WHERE profile = ?1 AND provider_id = ?2",
        params![profile, provider_id],
    )?;
    for (item_id, note) in skips {
        tx.execute(
            "INSERT OR REPLACE INTO suggestion_auto_skips (profile, provider_id, item_id, note)
             VALUES (?1, ?2, ?3, ?4)",
            params![profile, provider_id, item_id, note],
        )?;
    }
    tx.commit()
}

/// Live items of `criterion` auto start may consider, in published order: not
/// stale, dismissed, ever started by hand or by auto start.
pub(crate) fn auto_candidates(
    conn: &Connection,
    profile: &str,
    provider: &Provider,
    criterion: &str,
) -> anyhow::Result<Vec<Suggestion>> {
    let dismissed: HashSet<String> = conn
        .prepare(
            "SELECT item_id FROM suggestion_dismissals WHERE profile = ?1 AND provider_id = ?2",
        )?
        .query_map(params![profile, provider.id], |row| row.get(0))?
        .collect::<Result<_, _>>()?;
    let attempted: HashSet<String> = conn
        .prepare(
            "SELECT item_id FROM suggestion_auto_started WHERE profile = ?1 AND provider_id = ?2",
        )?
        .query_map(params![profile, provider.id], |row| row.get(0))?
        .collect::<Result<_, _>>()?;
    let linked: HashSet<String> = links(conn, profile)?
        .into_iter()
        .filter(|link| link.provider_id == provider.id)
        .map(|link| link.item_id)
        .collect();
    Ok(load_items(conn, profile, provider)?
        .into_iter()
        .filter(|item| {
            item.criterion == criterion
                && !item.stale
                && !dismissed.contains(&item.id)
                && !attempted.contains(&item.id)
                && !linked.contains(&item.id)
        })
        .collect())
}

/// Adds each source's `auto_start` (`null` when off) and, to items auto start
/// left alone or failed to start, `auto_note`.
pub(crate) fn attach_auto(
    conn: &Connection,
    profile: &str,
    snapshot: &mut Value,
) -> rusqlite::Result<()> {
    let starts = auto_starts(conn, profile)?;
    for provider in snapshot["providers"].as_array_mut().into_iter().flatten() {
        let provider_id = provider["id"].as_str().unwrap_or_default().to_string();
        for source in provider["sources"].as_array_mut().into_iter().flatten() {
            let criterion = source["criterion"].as_str().unwrap_or_default().to_string();
            source["auto_start"] = match starts.get(&(provider_id.clone(), criterion)) {
                Some(cap) => json!({ "cap": cap }),
                None => Value::Null,
            };
        }
    }
    let notes =
        |table: &str, column: &str| -> rusqlite::Result<HashMap<(String, String), String>> {
            conn.prepare(&format!(
                "SELECT provider_id, item_id, {column} FROM {table}
             WHERE profile = ?1 AND {column} IS NOT NULL"
            ))?
            .query_map([profile], |row| {
                Ok(((row.get(0)?, row.get(1)?), row.get::<_, String>(2)?))
            })?
            .collect()
        };
    let skips = notes("suggestion_auto_skips", "note")?;
    let failures = notes("suggestion_auto_started", "failure")?;
    for item in snapshot["items"].as_array_mut().into_iter().flatten() {
        let provider_id = item["provider_id"].as_str().unwrap_or_default().to_string();
        let criterion = item["criterion"].as_str().unwrap_or_default().to_string();
        let key = (
            provider_id.clone(),
            item["id"].as_str().unwrap_or_default().to_string(),
        );
        // A skip note only means something while auto start is on for the item's kind.
        let skip = starts
            .contains_key(&(provider_id, criterion))
            .then(|| skips.get(&key))
            .flatten();
        if let Some(note) = failures.get(&key).or(skip) {
            item["auto_note"] = json!(note);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::fixtures::*;
    use super::*;

    const P: &str = "acme.github";

    fn candidates(conn: &Connection) -> Vec<String> {
        auto_candidates(conn, "p", &provider(), "assigned")
            .unwrap()
            .into_iter()
            .map(|item| item.id)
            .collect()
    }

    #[test]
    fn auto_start_is_off_until_a_row_exists_and_is_profile_scoped() {
        let conn = store();
        assert!(auto_starts(&conn, "p").unwrap().is_empty());
        set_auto_start(&conn, "p", P, "assigned", Some(5)).unwrap();
        let key = (P.to_string(), "assigned".to_string());
        assert_eq!(auto_starts(&conn, "p").unwrap().get(&key), Some(&5));
        assert!(auto_starts(&conn, "other").unwrap().is_empty());
        set_auto_start(&conn, "p", P, "assigned", None).unwrap();
        assert!(auto_starts(&conn, "p").unwrap().is_empty());
    }

    #[test]
    fn an_item_is_a_candidate_until_attempted_dismissed_linked_or_stale() {
        let conn = store();
        let mut stale = item("stale", "assigned");
        stale.stale = true;
        publish(
            &conn,
            "p",
            &[
                item("open", "assigned"),
                item("tried", "assigned"),
                item("dismissed", "assigned"),
                item("linked", "assigned"),
                item("other", "quiet"),
                stale,
            ],
        );
        assert!(claim_auto(&conn, "p", P, "tried").unwrap());
        assert!(!claim_auto(&conn, "p", P, "tried").unwrap());
        conn.execute(
            "INSERT INTO suggestion_dismissals (profile, provider_id, item_id) VALUES ('p', ?1, 'dismissed')",
            [P],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO suggestion_links (profile, provider_id, item_id, holder, pending)
             VALUES ('p', ?1, 'linked', 's1', 0)",
            [P],
        )
        .unwrap();
        assert_eq!(candidates(&conn), ["open"]);
        // Attempts outlive a republish, so a dismissed-then-restored item is not retried.
        publish(
            &conn,
            "p",
            &[item("open", "assigned"), item("tried", "assigned")],
        );
        assert_eq!(candidates(&conn), ["open"]);
    }

    #[test]
    fn a_failure_and_skips_surface_as_item_notes() {
        let conn = store();
        claim_auto(&conn, "p", P, "a").unwrap();
        fail_auto(
            &conn,
            "p",
            P,
            "a",
            "Auto start failed: boom. Start it by hand.",
        )
        .unwrap();
        set_auto_skips(&conn, "p", P, &[("b".into(), "ambiguous".into())]).unwrap();
        set_auto_start(&conn, "p", P, "assigned", Some(3)).unwrap();
        let read = |conn: &Connection| {
            let mut snapshot = json!({
                "providers": [],
                "items": [
                    { "provider_id": P, "id": "a", "criterion": "assigned" },
                    { "provider_id": P, "id": "b", "criterion": "assigned" },
                ],
            });
            attach_auto(conn, "p", &mut snapshot).unwrap();
            snapshot
        };
        let snapshot = read(&conn);
        assert_eq!(
            snapshot["items"][0]["auto_note"],
            "Auto start failed: boom. Start it by hand."
        );
        assert_eq!(snapshot["items"][1]["auto_note"], "ambiguous");
        set_auto_start(&conn, "p", P, "assigned", None).unwrap();
        assert!(read(&conn)["items"][1].get("auto_note").is_none());
        assert!(read(&conn)["items"][0].get("auto_note").is_some());
    }
}
