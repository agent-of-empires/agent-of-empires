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

/// How the Session behind a link was started.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum StartedVia {
    /// `POST /api/suggestions/start`: the preset's own prompt, agent and project.
    Start,
    /// A create carrying a suggestion, from "Start with changes".
    Wizard,
}

impl StartedVia {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Start => "start",
            Self::Wizard => "wizard",
        }
    }
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
/// is created. A link is taken over when its Session is not `is_live` (trashed or
/// gone; asked inside the claim, so the answer is not older than the row) and its
/// claim is no longer in flight.
pub(crate) fn claim_link(
    conn: &Connection,
    profile: &str,
    provider_id: &str,
    item_id: &str,
    session_id: &str,
    via: StartedVia,
    is_live: &dyn Fn(&str) -> bool,
) -> rusqlite::Result<Claim> {
    let tx = conn.unchecked_transaction()?;
    let inserted = tx.execute(
        "INSERT OR IGNORE INTO suggestion_links
             (profile, provider_id, item_id, holder, pending, started_via)
         VALUES (?1, ?2, ?3, ?4, 1, ?5)",
        params![profile, provider_id, item_id, session_id, via.as_str()],
    )?;
    if inserted == 1 {
        tx.commit()?;
        return Ok(Claim::Won);
    }
    let (holder, pending): (String, bool) = tx.query_row(
        "SELECT holder, pending FROM suggestion_links
         WHERE profile = ?1 AND provider_id = ?2 AND item_id = ?3",
        params![profile, provider_id, item_id],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    if pending && is_starting(&holder) {
        return Ok(Claim::Starting(holder));
    }
    if is_live(&holder) {
        // A Session that was persisted but whose claim never settled: it is the
        // primary, whatever happened to the process that created it.
        tx.execute(
            "UPDATE suggestion_links SET pending = 0
             WHERE profile = ?1 AND provider_id = ?2 AND item_id = ?3",
            params![profile, provider_id, item_id],
        )?;
        tx.commit()?;
        return Ok(Claim::Session(holder));
    }
    tx.execute(
        "UPDATE suggestion_links SET holder = ?4, pending = 1, started_via = ?5
         WHERE profile = ?1 AND provider_id = ?2 AND item_id = ?3",
        params![profile, provider_id, item_id, session_id, via.as_str()],
    )?;
    tx.commit()?;
    Ok(Claim::Won)
}

/// A primary link: the item, its Session and how that Session was started.
pub(crate) struct Link {
    pub provider_id: String,
    pub item_id: String,
    pub session_id: String,
    pub started_via: StartedVia,
}

/// Every primary link of `profile`. A claim still provisioning names a Session
/// that is not recorded yet, so a join to live Sessions drops it.
pub(crate) fn links(conn: &Connection, profile: &str) -> rusqlite::Result<Vec<Link>> {
    conn.prepare(
        "SELECT provider_id, item_id, holder, started_via FROM suggestion_links WHERE profile = ?1",
    )?
    .query_map([profile], |row| {
        let via: String = row.get(3)?;
        Ok(Link {
            provider_id: row.get(0)?,
            item_id: row.get(1)?,
            session_id: row.get(2)?,
            started_via: if via == "start" {
                StartedVia::Start
            } else {
                StartedVia::Wizard
            },
        })
    })?
    .collect()
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
        let none = |_: &str| false;
        let claim = |session: &str, live: &dyn Fn(&str) -> bool| {
            claim_link(
                &conn,
                "p",
                "acme.github",
                "a",
                session,
                StartedVia::Start,
                live,
            )
            .unwrap()
        };
        let first = mark_starting("s1");
        assert_eq!(claim("s1", &none), Claim::Won);
        assert_eq!(claim("s2", &none), Claim::Starting("s1".into()));
        settle_link(&conn, "p", "acme.github", "a", "s1").unwrap();
        drop(first);
        let live = |id: &str| id == "s1";
        assert_eq!(claim("s3", &live), Claim::Session("s1".into()));
        // Another item and another profile are independent.
        assert_eq!(
            claim_link(
                &conn,
                "p",
                "acme.github",
                "b",
                "s4",
                StartedVia::Start,
                &live
            )
            .unwrap(),
            Claim::Won
        );
        assert_eq!(
            claim_link(
                &conn,
                "q",
                "acme.github",
                "a",
                "s5",
                StartedVia::Start,
                &live
            )
            .unwrap(),
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
        let claim = |session: &str| {
            claim_link(
                &conn,
                "p",
                "acme.github",
                "a",
                session,
                StartedVia::Start,
                &|_| false,
            )
            .unwrap()
        };
        // A long create is still in flight, however long it has run.
        let slow = mark_starting("slow");
        assert_eq!(claim("slow"), Claim::Won);
        assert_eq!(claim("other"), Claim::Starting("slow".into()));
        // The process dies: nothing is in flight any more, and the row is left.
        drop(slow);
        assert_eq!(claim("after-crash"), Claim::Won, "no timer to wait out");
    }

    #[test]
    fn a_pending_claim_whose_session_was_persisted_is_settled_not_taken_over() {
        let conn = store();
        let live = |id: &str| id == "made";
        let claim = |session: &str| {
            claim_link(
                &conn,
                "p",
                "acme.github",
                "a",
                session,
                StartedVia::Wizard,
                &live,
            )
            .unwrap()
        };
        // The create persisted its Session, then the process died (or the settle
        // failed) before the row left pending.
        let starting = mark_starting("made");
        assert_eq!(claim("made"), Claim::Won);
        drop(starting);
        assert_eq!(claim("dup"), Claim::Session("made".into()));
        let rows = links(&conn, "p").unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].session_id, "made");
        assert_eq!(rows[0].started_via, StartedVia::Wizard);
        let pending: bool = conn
            .query_row("SELECT pending FROM suggestion_links", [], |r| r.get(0))
            .unwrap();
        assert!(!pending, "recovery settles the row");
    }

    #[test]
    fn a_link_remembers_how_it_was_started_and_a_takeover_replaces_it() {
        let conn = store();
        let via = || links(&conn, "p").unwrap()[0].started_via;
        assert_eq!(
            claim_link(
                &conn,
                "p",
                "acme.github",
                "a",
                "s1",
                StartedVia::Start,
                &|_| false
            )
            .unwrap(),
            Claim::Won
        );
        assert_eq!(via(), StartedVia::Start);
        // The holder is gone: the next claim takes the row over and its own way.
        assert_eq!(
            claim_link(
                &conn,
                "p",
                "acme.github",
                "a",
                "s2",
                StartedVia::Wizard,
                &|_| false
            )
            .unwrap(),
            Claim::Won
        );
        assert_eq!(via(), StartedVia::Wizard);
    }

    #[test]
    fn a_link_from_before_started_via_reads_as_a_wizard_start() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(&format!(
            "CREATE TABLE suggestion_schema (id INTEGER PRIMARY KEY CHECK (id = 1), version INTEGER NOT NULL);
             INSERT INTO suggestion_schema VALUES (1, {SCHEMA_VERSION});
             CREATE TABLE suggestion_links (
                 profile TEXT NOT NULL, provider_id TEXT NOT NULL, item_id TEXT NOT NULL,
                 holder TEXT NOT NULL, pending INTEGER NOT NULL,
                 PRIMARY KEY (profile, provider_id, item_id));
             INSERT INTO suggestion_links VALUES ('p', 'acme.github', 'a', 'old', 0);"
        ))
        .unwrap();
        init(&conn).unwrap();
        let rows = links(&conn, "p").unwrap();
        assert_eq!(rows.len(), 1, "the link survives");
        assert_eq!(rows[0].started_via, StartedVia::Wizard);
    }
}
