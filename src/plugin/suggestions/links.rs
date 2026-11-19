//! The primary link between a suggestion and the Session started from it.
//!
//! One row per item, so the database refuses a second primary. A start claims
//! the row before it provisions and settles it with the Session id afterwards.

use super::*;

/// A claim that never settled (a crash mid-create) is taken over after this long.
const PENDING_TTL_SECS: i64 = 10 * 60;

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Claim {
    Won,
    /// A live Session already holds the link.
    Session(String),
    /// Another start is still provisioning this Session.
    Starting(String),
}

/// Claims the primary link of an item for the Session `session_id`, named before it is created. A link whose Session is not in
/// `live` (trashed or gone) or whose claim went stale is taken over.
pub(crate) fn claim_link(
    conn: &Connection,
    profile: &str,
    provider_id: &str,
    item_id: &str,
    session_id: &str,
    live: &HashSet<String>,
    now: i64,
) -> rusqlite::Result<Claim> {
    let tx = conn.unchecked_transaction()?;
    let key = params![profile, provider_id, item_id];
    let inserted = tx.execute(
        "INSERT OR IGNORE INTO suggestion_links (profile, provider_id, item_id, holder, pending, claimed_at)
         VALUES (?1, ?2, ?3, ?4, 1, ?5)",
        params![profile, provider_id, item_id, session_id, now],
    )?;
    if inserted == 1 {
        tx.commit()?;
        return Ok(Claim::Won);
    }
    let (holder, pending, claimed_at): (String, bool, i64) = tx.query_row(
        "SELECT holder, pending, claimed_at FROM suggestion_links
         WHERE profile = ?1 AND provider_id = ?2 AND item_id = ?3",
        key,
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
    )?;
    let held = if pending {
        (now - claimed_at < PENDING_TTL_SECS).then(|| Claim::Starting(holder.clone()))
    } else {
        live.contains(&holder)
            .then(|| Claim::Session(holder.clone()))
    };
    if let Some(held) = held {
        return Ok(held);
    }
    tx.execute(
        "UPDATE suggestion_links SET holder = ?4, pending = 1, claimed_at = ?5
         WHERE profile = ?1 AND provider_id = ?2 AND item_id = ?3",
        params![profile, provider_id, item_id, session_id, now],
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
        let claim = |session: &str, live: &HashSet<String>, now| {
            claim_link(&conn, "p", "acme.github", "a", session, live, now).unwrap()
        };
        assert_eq!(claim("s1", &none, 0), Claim::Won);
        let starting = claim("s2", &none, 1);
        assert_eq!(starting, Claim::Starting("s1".into()), "names the Session");
        settle_link(&conn, "p", "acme.github", "a", "s1").unwrap();
        let live = HashSet::from(["s1".to_string()]);
        assert_eq!(claim("s3", &live, 2), Claim::Session("s1".into()));
        // Another item and another profile are independent.
        assert_eq!(
            claim_link(&conn, "p", "acme.github", "b", "s4", &live, 2).unwrap(),
            Claim::Won
        );
        assert_eq!(
            claim_link(&conn, "q", "acme.github", "a", "s5", &live, 2).unwrap(),
            Claim::Won
        );
        // Trash (or deletion) frees it: the holder is no longer live.
        assert_eq!(claim("s6", &none, 3), Claim::Won);
        // A failed start releases, and only its own claim.
        release_link(&conn, "p", "acme.github", "a", "s3").unwrap();
        assert_eq!(claim("s7", &none, 4), Claim::Starting("s6".into()));
        release_link(&conn, "p", "acme.github", "a", "s6").unwrap();
        assert_eq!(claim("s8", &none, 5), Claim::Won);
        // A claim that never settled is taken over once stale.
        assert_eq!(claim("s9", &none, 5 + PENDING_TTL_SECS), Claim::Won);
    }
}
