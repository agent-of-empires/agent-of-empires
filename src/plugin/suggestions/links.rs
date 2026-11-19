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
    /// Another start is still provisioning.
    Starting,
}

/// Claims the primary link of an item for `token`. A link whose Session is not in
/// `live` (trashed or gone) or whose claim went stale is taken over.
pub(crate) fn claim_link(
    conn: &Connection,
    profile: &str,
    provider_id: &str,
    item_id: &str,
    token: &str,
    live: &HashSet<String>,
    now: i64,
) -> rusqlite::Result<Claim> {
    let tx = conn.unchecked_transaction()?;
    let key = params![profile, provider_id, item_id];
    let inserted = tx.execute(
        "INSERT OR IGNORE INTO suggestion_links (profile, provider_id, item_id, holder, pending, claimed_at)
         VALUES (?1, ?2, ?3, ?4, 1, ?5)",
        params![profile, provider_id, item_id, token, now],
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
        (now - claimed_at < PENDING_TTL_SECS).then_some(Claim::Starting)
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
        params![profile, provider_id, item_id, token, now],
    )?;
    tx.commit()?;
    Ok(Claim::Won)
}

/// Records the Session that now holds a claimed link.
pub(crate) fn settle_link(
    conn: &Connection,
    profile: &str,
    provider_id: &str,
    item_id: &str,
    token: &str,
    session_id: &str,
) -> rusqlite::Result<()> {
    conn.execute(
        "UPDATE suggestion_links SET holder = ?5, pending = 0
         WHERE profile = ?1 AND provider_id = ?2 AND item_id = ?3 AND holder = ?4",
        params![profile, provider_id, item_id, token, session_id],
    )?;
    Ok(())
}

/// Frees a claim whose start failed, so Try again can take it.
pub(crate) fn release_link(
    conn: &Connection,
    profile: &str,
    provider_id: &str,
    item_id: &str,
    token: &str,
) -> rusqlite::Result<()> {
    conn.execute(
        "DELETE FROM suggestion_links
         WHERE profile = ?1 AND provider_id = ?2 AND item_id = ?3 AND holder = ?4",
        params![profile, provider_id, item_id, token],
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
        let claim = |token: &str, live: &HashSet<String>, now| {
            claim_link(&conn, "p", "acme.github", "a", token, live, now).unwrap()
        };
        assert_eq!(claim("t1", &none, 0), Claim::Won);
        assert_eq!(claim("t2", &none, 1), Claim::Starting, "still provisioning");
        settle_link(&conn, "p", "acme.github", "a", "t1", "s1").unwrap();
        let live = HashSet::from(["s1".to_string()]);
        assert_eq!(claim("t3", &live, 2), Claim::Session("s1".into()));
        // Another item and another profile are independent.
        assert_eq!(
            claim_link(&conn, "p", "acme.github", "b", "t4", &live, 2).unwrap(),
            Claim::Won
        );
        assert_eq!(
            claim_link(&conn, "q", "acme.github", "a", "t5", &live, 2).unwrap(),
            Claim::Won
        );
        // Trash (or deletion) frees it: the holder is no longer live.
        assert_eq!(claim("t6", &none, 3), Claim::Won);
        // A failed start releases, and only its own claim.
        release_link(&conn, "p", "acme.github", "a", "t3").unwrap();
        assert_eq!(claim("t7", &none, 4), Claim::Starting);
        release_link(&conn, "p", "acme.github", "a", "t6").unwrap();
        assert_eq!(claim("t8", &none, 5), Claim::Won);
        // A claim that never settled is taken over once stale.
        assert_eq!(claim("t9", &none, 5 + PENDING_TTL_SECS), Claim::Won);
    }
}
