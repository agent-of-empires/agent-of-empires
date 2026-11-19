//! The primary link between a suggestion and the Session started from it.
//!
//! One row per item, so the database refuses a second primary. A start claims
//! the row under the Session id before it provisions and settles it afterwards.
//! A pending claim is only honoured while its start is in this process's
//! in-flight set, so a crash cannot strand one, however long a create runs.

use super::*;
use std::sync::Mutex;

static STARTING: Mutex<Vec<String>> = Mutex::new(Vec::new());

/// Keeps a Session id in the in-flight set until the start settles or is dropped.
pub(crate) struct Starting(String);

/// Marks `session_id` as being provisioned by this process.
pub(crate) fn mark_starting(session_id: &str) -> Starting {
    STARTING
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .push(session_id.to_owned());
    Starting(session_id.to_owned())
}

impl Drop for Starting {
    fn drop(&mut self) {
        let mut starting = STARTING.lock().unwrap_or_else(|p| p.into_inner());
        starting.retain(|id| *id != self.0);
    }
}

fn is_starting(session_id: &str) -> bool {
    STARTING
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .iter()
        .any(|id| id == session_id)
}

/// Drops every pending claim: at daemon start nothing is in flight, so any left
/// behind belongs to a start that died with the previous process.
pub(crate) fn clear_pending_links(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute("DELETE FROM suggestion_links WHERE pending = 1", [])?;
    Ok(())
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Claim {
    Won,
    /// A live Session already holds the link.
    Session(String),
    /// Another start is still provisioning this Session.
    Starting(String),
}

/// Claims the primary link of an item for the Session `session_id`, named before it
/// is created. A link is taken over when its Session is not in `live` (trashed or
/// gone) or its claim is no longer in flight.
pub(crate) fn claim_link(
    conn: &Connection,
    profile: &str,
    provider_id: &str,
    item_id: &str,
    session_id: &str,
    live: &HashSet<String>,
) -> rusqlite::Result<Claim> {
    let tx = conn.unchecked_transaction()?;
    let key = params![profile, provider_id, item_id];
    let inserted = tx.execute(
        "INSERT OR IGNORE INTO suggestion_links (profile, provider_id, item_id, holder, pending)
         VALUES (?1, ?2, ?3, ?4, 1)",
        params![profile, provider_id, item_id, session_id],
    )?;
    if inserted == 1 {
        tx.commit()?;
        return Ok(Claim::Won);
    }
    let (holder, pending): (String, bool) = tx.query_row(
        "SELECT holder, pending FROM suggestion_links
         WHERE profile = ?1 AND provider_id = ?2 AND item_id = ?3",
        key,
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    let held = if pending {
        is_starting(&holder).then(|| Claim::Starting(holder.clone()))
    } else {
        live.contains(&holder)
            .then(|| Claim::Session(holder.clone()))
    };
    if let Some(held) = held {
        return Ok(held);
    }
    tx.execute(
        "UPDATE suggestion_links SET holder = ?4, pending = 1
         WHERE profile = ?1 AND provider_id = ?2 AND item_id = ?3",
        params![profile, provider_id, item_id, session_id],
    )?;
    tx.commit()?;
    Ok(Claim::Won)
}

/// Marks the claim of `session_id` as a created Session.
pub(crate) fn settle_link(
    conn: &Connection,
    profile: &str,
    provider_id: &str,
    item_id: &str,
    session_id: &str,
) -> rusqlite::Result<()> {
    conn.execute(
        "UPDATE suggestion_links SET pending = 0
         WHERE profile = ?1 AND provider_id = ?2 AND item_id = ?3 AND holder = ?4",
        params![profile, provider_id, item_id, session_id],
    )?;
    Ok(())
}

/// Frees the claim of `session_id` after its start failed, so Try again can take it.
pub(crate) fn release_link(
    conn: &Connection,
    profile: &str,
    provider_id: &str,
    item_id: &str,
    session_id: &str,
) -> rusqlite::Result<()> {
    conn.execute(
        "DELETE FROM suggestion_links
         WHERE profile = ?1 AND provider_id = ?2 AND item_id = ?3 AND holder = ?4",
        params![profile, provider_id, item_id, session_id],
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::fixtures::*;
    use super::*;

    #[test]
    fn the_database_admits_one_primary_link_per_item() {
        let conn = store();
        let none = HashSet::new();
        let claim = |session: &str, live: &HashSet<String>| {
            claim_link(&conn, "p", "acme.github", "a", session, live).unwrap()
        };
        let first = mark_starting("s1");
        assert_eq!(claim("s1", &none), Claim::Won);
        assert_eq!(claim("s2", &none), Claim::Starting("s1".into()));
        settle_link(&conn, "p", "acme.github", "a", "s1").unwrap();
        drop(first);
        let live = HashSet::from(["s1".to_string()]);
        assert_eq!(claim("s3", &live), Claim::Session("s1".into()));
        // Another item and another profile are independent.
        assert_eq!(
            claim_link(&conn, "p", "acme.github", "b", "s4", &live).unwrap(),
            Claim::Won
        );
        assert_eq!(
            claim_link(&conn, "q", "acme.github", "a", "s5", &live).unwrap(),
            Claim::Won
        );
        // Trash (or deletion) frees it: the holder is no longer live.
        let sixth = mark_starting("s6");
        assert_eq!(claim("s6", &none), Claim::Won);
        // A failed start releases, and only its own claim.
        release_link(&conn, "p", "acme.github", "a", "s3").unwrap();
        assert_eq!(claim("s7", &none), Claim::Starting("s6".into()));
        release_link(&conn, "p", "acme.github", "a", "s6").unwrap();
        drop(sixth);
        assert_eq!(claim("s8", &none), Claim::Won);
    }

    #[test]
    fn a_claim_stranded_by_a_crash_is_taken_over_and_a_slow_create_is_not() {
        let conn = store();
        let none = HashSet::new();
        let claim =
            |session: &str| claim_link(&conn, "p", "acme.github", "a", session, &none).unwrap();
        // A long create is still in flight, however long it has run.
        let slow = mark_starting("slow");
        assert_eq!(claim("slow"), Claim::Won);
        assert_eq!(claim("other"), Claim::Starting("slow".into()));
        // The process dies: nothing is in flight any more, and the row is left.
        drop(slow);
        assert_eq!(claim("after-crash"), Claim::Won, "no timer to wait out");
        // A restart clears what the dead process left, whoever was starting.
        clear_pending_links(&conn).unwrap();
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM suggestion_links", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 0);
    }
}
