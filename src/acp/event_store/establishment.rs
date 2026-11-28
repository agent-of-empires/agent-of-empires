//! Private historical commit metadata for one original establishment per session.

use std::sync::Arc;

use anyhow::{Context, Result};
use rusqlite::{params, OptionalExtension};
use serde::{Deserialize, Serialize};

use super::{EventStore, RecordDisposition};
use crate::acp::establishment::{
    EstablishmentAttempt, EstablishmentRefusal, NativeEstablishmentReceipt,
};
use crate::acp::event_sender::{EstablishmentDelivery, EventMetadata};
use crate::acp::operation_effect::NativePromptResult;
use crate::acp::protocol::AcpOriginalRequestResult;
use crate::acp::state::Event;

pub(super) const TABLE: &str = "acp_establishment_commit";

/// In-memory accounting of actual append results, not establishment authority.
pub struct ReplayCommit {
    attempt: Arc<EstablishmentAttempt>,
    first: u64,
    next: u64,
    first_seq: Option<u64>,
    last_seq: Option<u64>,
    failed: bool,
}

impl ReplayCommit {
    pub(crate) fn new(attempt: Arc<EstablishmentAttempt>, first: u64) -> Self {
        Self {
            attempt,
            first,
            next: first,
            first_seq: None,
            last_seq: None,
            failed: false,
        }
    }

    pub(crate) fn matches(&self, attempt: &EstablishmentAttempt) -> bool {
        self.attempt.as_ref() == attempt
    }

    pub(crate) fn reject(&mut self) {
        self.failed = true;
    }

    fn appended(&mut self, ordinal: u64, seq: u64, persisted: bool) {
        self.failed |= !persisted
            || ordinal != self.next
            || self.last_seq.is_some_and(|previous| seq <= previous);
        match ordinal.checked_add(1) {
            Some(next) => self.next = next,
            None => self.failed = true,
        }
        self.first_seq.get_or_insert(seq);
        self.last_seq = Some(seq);
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct CommitData {
    version: u8,
    receipt: NativeEstablishmentReceipt,
    first: u64,
    through: u64,
    first_seq: Option<u64>,
    last_seq: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    clear_after_seq: Option<u64>,
}

impl CommitData {
    fn decode(json: &str, session_id: &str) -> Result<Self> {
        let data: Self = serde_json::from_str(json).context(EstablishmentRefusal::Unconfirmed)?;
        anyhow::ensure!(
            data.version == 1
                && data.receipt.attempt.session_id == session_id
                && data.first <= data.through
                && match (data.first_seq, data.last_seq) {
                    (None, None) => data.first == data.through,
                    (Some(first), Some(last)) =>
                        data.first < data.through && first > 0 && first <= last,
                    _ => false,
                },
            EstablishmentRefusal::Unconfirmed
        );
        anyhow::ensure!(
            data.clear_after_seq.is_none_or(|cut| {
                data.first_seq
                    .is_none_or(|first| first.checked_sub(1) == Some(cut))
            }),
            EstablishmentRefusal::Unconfirmed
        );
        Ok(data)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum PromptDiagnosticKind {
    RuntimeError,
    RateLimit,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RecordedPromptDiagnostic {
    kind: PromptDiagnosticKind,
    seq: u64,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PromptCommit {
    version: u8,
    key: NativePromptResult,
    diagnostic: Option<RecordedPromptDiagnostic>,
    stopped: Option<u64>,
}

#[derive(Clone, Copy)]
enum PromptComponent {
    Diagnostic(PromptDiagnosticKind),
    Stopped,
}

const MAX_PROMPT_COMMIT_BYTES: usize = 16 * 1024;

impl PromptCommit {
    fn decode(json: &str) -> Result<Self> {
        anyhow::ensure!(
            json.len() <= MAX_PROMPT_COMMIT_BYTES,
            "oversized Native prompt commit"
        );
        let commit: Self = serde_json::from_str(json).context("decode Native prompt commit")?;
        anyhow::ensure!(
            commit.version == 1
                && commit.key.request_id > 0
                && commit
                    .key
                    .origin
                    .owner
                    .is_some_and(|owner| owner.birth_fingerprint.is_some())
                && !commit.key.origin.producer.is_nil()
                && (commit.diagnostic.is_some() || commit.stopped.is_some())
                && commit
                    .diagnostic
                    .as_ref()
                    .is_none_or(|known| known.seq <= i64::MAX as u64
                        && commit.key.outcome == AcpOriginalRequestResult::Failed
                        && commit.stopped != Some(known.seq))
                && commit.stopped.is_none_or(|seq| seq <= i64::MAX as u64),
            "invalid Native prompt commit"
        );
        Ok(commit)
    }

    fn new(key: NativePromptResult) -> Self {
        Self {
            version: 1,
            key,
            diagnostic: None,
            stopped: None,
        }
    }

    fn recorded(&self, component: PromptComponent) -> Result<Option<u64>> {
        match component {
            PromptComponent::Stopped => Ok(self.stopped),
            PromptComponent::Diagnostic(kind) => {
                anyhow::ensure!(
                    self.diagnostic
                        .as_ref()
                        .is_none_or(|known| known.kind == kind),
                    "conflicting Native prompt diagnostic"
                );
                Ok(self.diagnostic.as_ref().map(|known| known.seq))
            }
        }
    }

    fn record(&mut self, component: PromptComponent, seq: u64) {
        match component {
            PromptComponent::Stopped => self.stopped = Some(seq),
            PromptComponent::Diagnostic(kind) => {
                self.diagnostic = Some(RecordedPromptDiagnostic { kind, seq })
            }
        }
    }
}

/// Only the checked journal reader/writer constructs this witness.
pub(crate) struct EstablishmentSeal(CommitData);

impl EstablishmentSeal {
    pub(crate) fn matches(&self, receipt: &NativeEstablishmentReceipt) -> bool {
        self.0.version == 1
            && self.0.receipt == *receipt
            && self.0.receipt.same_store(receipt.store.as_ref())
            && receipt.owner.birth_fingerprint.is_some()
            && receipt.replay.closed
            && !receipt.replay.lost
            && receipt.session_id().is_some()
    }

    pub(crate) fn clear_after_seq(&self) -> Result<u64> {
        self.0
            .first_seq
            .and_then(|first| first.checked_sub(1))
            .or(self.0.clear_after_seq)
            .ok_or_else(|| EstablishmentRefusal::Unconfirmed.into())
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum AutonomousBoundaryDisposition {
    Retired,
    Unavailable,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct AutonomousBoundary {
    seq: u64,
    disposition: AutonomousBoundaryDisposition,
}

/// One bounded original run fact on the existing establishment row.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct AutonomousCommit {
    version: u8,
    pub(super) identity: Arc<crate::acp::event_sender::NativeAutonomousIdentity>,
    pub(super) started: u64,
    ended: Option<u64>,
    stop_reason: Option<String>,
    boundary: Option<AutonomousBoundary>,
}

impl AutonomousCommit {
    fn decode(json: &str) -> Result<Self> {
        anyhow::ensure!(
            json.len() <= crate::acp::control_protocol::MAX_COMPLETION_MARKER_BYTES,
            "oversized retained Native lifecycle"
        );
        let fact: Self = serde_json::from_str(json)?;
        anyhow::ensure!(
            fact.version == 1
                && fact.started > 0
                && fact.identity.ordinal > 0
                && !fact.identity.run_id.is_nil()
                && !fact.identity.root_session_id.0.is_empty()
                && fact
                    .identity
                    .origin
                    .owner
                    .is_some_and(|owner| owner.birth_fingerprint.is_some())
                && fact.ended.is_none_or(|ended| ended > fact.started)
                && fact
                    .boundary
                    .is_none_or(|boundary| boundary.seq >= fact.started)
                && (fact.ended.is_some() || fact.stop_reason.is_none()),
            "malformed retained Native lifecycle"
        );
        i64::try_from(fact.frontier())?;
        Ok(fact)
    }

    pub(super) fn frontier(&self) -> u64 {
        self.started
            .max(self.ended.unwrap_or(0))
            .max(self.boundary.map_or(0, |boundary| boundary.seq))
    }

    pub(super) fn activity_before(&self, seq: u64) -> super::control::ControlKnowledge<bool> {
        use super::control::ControlKnowledge::{Indeterminate, Known};
        if self.started >= seq {
            return Known(false);
        }
        if self.ended.is_some_and(|ended| ended < seq) {
            return Known(false);
        }
        match self.boundary.filter(|boundary| boundary.seq < seq) {
            Some(AutonomousBoundary {
                disposition: AutonomousBoundaryDisposition::Retired,
                ..
            }) => Known(false),
            Some(AutonomousBoundary {
                disposition: AutonomousBoundaryDisposition::Unavailable,
                ..
            }) => Indeterminate,
            None => Known(true),
        }
    }

    pub(super) fn activity(&self) -> super::control::ControlKnowledge<bool> {
        use super::control::ControlKnowledge::{Indeterminate, Known};
        match self.boundary {
            Some(AutonomousBoundary {
                disposition: AutonomousBoundaryDisposition::Retired,
                ..
            }) => Known(false),
            Some(AutonomousBoundary {
                disposition: AutonomousBoundaryDisposition::Unavailable,
                ..
            }) if self.ended.is_none() => Indeterminate,
            _ => Known(self.ended.is_none()),
        }
    }

    pub(crate) fn restore_state(&self, state: &mut crate::acp::state::AcpState) {
        state.restore_autonomous(
            &self.identity,
            self.activity() == super::control::ControlKnowledge::Known(true),
        );
        state.last_seq = state.last_seq.max(self.frontier());
    }
}

impl EventStore {
    pub(crate) fn create_establishment_metadata(conn: &rusqlite::Connection) -> Result<()> {
        conn.execute_batch(&format!(
            "CREATE TABLE IF NOT EXISTS {TABLE} (
                session_id TEXT PRIMARY KEY NOT NULL,
                seal_json TEXT NOT NULL,
                prompt_commit_json TEXT,
                autonomous_commit_json TEXT
            );"
        ))
        .context("create private establishment commit metadata")
    }

    pub(crate) fn validate_prompt_commit_schema(conn: &rusqlite::Connection) -> Result<()> {
        let shape: Option<(String, bool, i64)> = conn
            .query_row(
                "SELECT type, \"notnull\", pk FROM pragma_table_info('acp_establishment_commit')
             WHERE name = 'prompt_commit_json'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .optional()?;
        anyhow::ensure!(
            shape.is_some_and(|(kind, required, key)| kind.eq_ignore_ascii_case("TEXT")
                && !required
                && key == 0),
            "ACP prompt publication metadata requires migration56"
        );
        let mut statement = conn.prepare(
            "SELECT prompt_commit_json FROM acp_establishment_commit
             WHERE prompt_commit_json IS NOT NULL",
        )?;
        for json in statement.query_map([], |row| row.get::<_, String>(0))? {
            PromptCommit::decode(&json?)?;
        }
        Ok(())
    }

    pub(crate) fn validate_autonomous_commit_schema(conn: &rusqlite::Connection) -> Result<()> {
        let shape: Option<(String, bool, i64)> = conn
            .query_row(
                "SELECT type,\"notnull\",pk FROM pragma_table_info('acp_establishment_commit')
             WHERE name='autonomous_commit_json'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .optional()?;
        anyhow::ensure!(
            shape.is_some_and(|(kind, required, key)| kind.eq_ignore_ascii_case("TEXT")
                && !required
                && key == 0),
            "Native lifecycle metadata requires ACP lineage reconciliation"
        );
        let mut statement = conn.prepare(
            "SELECT session_id FROM acp_establishment_commit WHERE autonomous_commit_json IS NOT NULL",
        )?;
        for id in statement.query_map([], |row| row.get::<_, String>(0))? {
            Self::autonomous_commit(conn, &id?)?;
        }
        Ok(())
    }

    pub(super) fn autonomous_commit(
        conn: &rusqlite::Connection,
        session_id: &str,
    ) -> Result<Option<AutonomousCommit>> {
        let stored: Option<(String, Option<String>)> = conn
            .query_row(
                &format!(
                    "SELECT seal_json,autonomous_commit_json FROM {TABLE} WHERE session_id=?1"
                ),
                [session_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        let Some((seal, Some(json))) = stored else {
            return Ok(None);
        };
        let fact = AutonomousCommit::decode(&json)?;
        let base = CommitData::decode(&seal, session_id)?;
        let receipt = &base.receipt;
        let same_source = crate::acp::state::NativeEventOrigin::owned_attempt(
            &receipt.attempt,
            Some(receipt.owner),
        ) == Some(fact.identity.origin)
            && receipt.session_id() == Some(fact.identity.root_session_id.0.as_ref())
            && receipt.replay.closed
            && !receipt.replay.lost;
        anyhow::ensure!(
            same_source
                || fact.boundary.is_some_and(|boundary| matches!(
                    boundary.disposition,
                    AutonomousBoundaryDisposition::Retired
                )),
            "retained Native lifecycle differs from its original establishment"
        );
        Ok(Some(fact))
    }

    fn write_autonomous_commit(
        tx: &rusqlite::Transaction<'_>,
        session_id: &str,
        fact: &AutonomousCommit,
    ) -> Result<()> {
        let encoded = serde_json::to_string(fact)?;
        AutonomousCommit::decode(&encoded)?;
        anyhow::ensure!(
            tx.execute(
                &format!("UPDATE {TABLE} SET autonomous_commit_json=?2 WHERE session_id=?1"),
                params![session_id, encoded],
            )? == 1,
            "Native lifecycle lost its original establishment row"
        );
        Ok(())
    }

    pub(super) fn retained_publication_state(
        conn: &rusqlite::Connection,
        session_id: &str,
    ) -> Result<(u64, Option<AutonomousCommit>)> {
        let prompt: Option<Option<String>> = conn
            .query_row(
                &format!("SELECT prompt_commit_json FROM {TABLE} WHERE session_id=?1"),
                [session_id],
                |row| row.get(0),
            )
            .optional()?;
        let prompt = prompt
            .flatten()
            .as_deref()
            .map(PromptCommit::decode)
            .transpose()?;
        let prompt_seq = prompt.as_ref().map_or(0, |commit| {
            commit
                .diagnostic
                .as_ref()
                .map_or(0, |diagnostic| diagnostic.seq)
                .max(commit.stopped.unwrap_or(0))
        });
        let autonomous = Self::autonomous_commit(conn, session_id)?;
        let frontier = prompt_seq.max(autonomous.as_ref().map_or(0, AutonomousCommit::frontier));
        Ok((frontier, autonomous))
    }

    pub(super) fn record_autonomous_result(
        tx: &rusqlite::Transaction<'_>,
        session_id: &str,
        seq: u64,
        json: &str,
        created_at: i64,
        metadata: &EventMetadata,
    ) -> Result<Option<RecordDisposition>> {
        use crate::acp::control_protocol::NativeAutonomousPhase;
        let Some(autonomous) = metadata
            .native_autonomous
            .as_deref()
            .filter(|fact| fact.qualified)
        else {
            return Ok(None);
        };
        let seal: String = tx.query_row(
            &format!("SELECT seal_json FROM {TABLE} WHERE session_id=?1"),
            [session_id],
            |row| row.get(0),
        )?;
        let base = CommitData::decode(&seal, session_id)?;
        anyhow::ensure!(
            crate::acp::state::NativeEventOrigin::owned_attempt(
                &base.receipt.attempt,
                Some(base.receipt.owner),
            ) == Some(autonomous.identity.origin)
                && base.receipt.session_id()
                    == Some(autonomous.identity.root_session_id.0.as_ref())
                && base.receipt.replay.closed
                && !base.receipt.replay.lost,
            EstablishmentRefusal::Superseded
        );
        let previous = Self::autonomous_commit(tx, session_id)?;
        let older = previous.as_ref().is_some_and(|previous| {
            previous.identity.origin == autonomous.identity.origin
                && previous.identity.ordinal > autonomous.identity.ordinal
        });
        let mut next = match previous {
            Some(previous) if previous.identity.origin == autonomous.identity.origin => {
                match autonomous.identity.ordinal.cmp(&previous.identity.ordinal) {
                    std::cmp::Ordering::Less => None,
                    std::cmp::Ordering::Equal => {
                        anyhow::ensure!(
                            previous.identity == autonomous.identity,
                            "conflicting Native lifecycle identity"
                        );
                        let stored = match autonomous.phase {
                            NativeAutonomousPhase::Started => Some(previous.started),
                            NativeAutonomousPhase::Ended => previous.ended,
                        };
                        if let Some(seq) = stored {
                            if autonomous.phase == NativeAutonomousPhase::Ended {
                                anyhow::ensure!(
                                    previous.stop_reason == autonomous.stop_reason,
                                    "conflicting Native lifecycle terminal"
                                );
                            }
                            return Ok(Some(RecordDisposition::Existing { seq }));
                        }
                        anyhow::ensure!(
                            previous.boundary.is_none(),
                            EstablishmentRefusal::Superseded
                        );
                        Some(previous)
                    }
                    std::cmp::Ordering::Greater => {
                        anyhow::ensure!(
                            previous.activity() == super::control::ControlKnowledge::Known(false),
                            "Native lifecycle overlapped an unclosed original"
                        );
                        None
                    }
                }
            }
            _ => None,
        };
        if next.is_none() && autonomous.phase == NativeAutonomousPhase::Started {
            if !older {
                next = Some(AutonomousCommit {
                    version: 1,
                    identity: Arc::clone(&autonomous.identity),
                    started: seq,
                    ended: None,
                    stop_reason: None,
                    boundary: None,
                });
            }
        } else if autonomous.phase == NativeAutonomousPhase::Ended {
            if let Some(next) = next.as_mut() {
                anyhow::ensure!(
                    seq > next.started,
                    "Native terminal precedes its original start"
                );
                next.ended = Some(seq);
                next.stop_reason = autonomous.stop_reason.clone();
            } else {
                anyhow::ensure!(older, "Native terminal has no positive original start");
            }
        }
        let inserted = Self::insert_scoped_event(tx, session_id, seq, json, created_at, metadata)?;
        if inserted == 0 {
            return Ok(Some(RecordDisposition::Existing { seq }));
        }
        if let Some(next) = next {
            Self::write_autonomous_commit(tx, session_id, &next)?;
        }
        Ok(Some(RecordDisposition::Inserted))
    }

    pub(super) fn record_autonomous_boundary(
        tx: &rusqlite::Transaction<'_>,
        session_id: &str,
        seq: u64,
        event: &Event,
        metadata: &EventMetadata,
        retires_limited_run: bool,
    ) -> Result<()> {
        use crate::acp::operation_effect::ControlKind;
        let current = matches!(
            metadata.scope,
            crate::acp::event_sender::EventScope::SessionCurrent
                | crate::acp::event_sender::EventScope::OperationCurrent
        );
        let disposition = match ControlKind::from_event(event) {
            ControlKind::Switched | ControlKind::Cleared | ControlKind::ContextReset
                if current || metadata.host_action =>
            {
                AutonomousBoundaryDisposition::Retired
            }
            ControlKind::Stopped("orphaned_at_restart") if metadata.host_action => {
                AutonomousBoundaryDisposition::Retired
            }
            ControlKind::Stopped("rate_limited") if retires_limited_run => {
                AutonomousBoundaryDisposition::Retired
            }
            ControlKind::Unavailable { channel_loss: true }
                if current && metadata.availability_denial =>
            {
                AutonomousBoundaryDisposition::Unavailable
            }
            _ => return Ok(()),
        };
        if let Some(mut fact) = Self::autonomous_commit(tx, session_id)? {
            if seq > fact.frontier() {
                fact.boundary = Some(AutonomousBoundary { seq, disposition });
                Self::write_autonomous_commit(tx, session_id, &fact)?;
            }
        }
        Ok(())
    }

    pub(super) fn record_prompt_result(
        tx: &rusqlite::Transaction<'_>,
        session_id: &str,
        seq: u64,
        event: &Event,
        json: &str,
        created_at: i64,
        metadata: &EventMetadata,
    ) -> Result<Option<RecordDisposition>> {
        let Some(request_id) = metadata.native_prompt_id else {
            return Ok(None);
        };
        let component = match event {
            Event::PromptRuntimeError { .. } => {
                PromptComponent::Diagnostic(PromptDiagnosticKind::RuntimeError)
            }
            Event::RateLimit { .. } => PromptComponent::Diagnostic(PromptDiagnosticKind::RateLimit),
            Event::Stopped { .. } => PromptComponent::Stopped,
            _ => return Ok(None),
        };
        let key =
            NativePromptResult::from_metadata(metadata).ok_or(EstablishmentRefusal::Unconfirmed)?;
        anyhow::ensure!(
            !matches!(component, PromptComponent::Diagnostic(_))
                || key.outcome == AcpOriginalRequestResult::Failed,
            "successful Native result has an error diagnostic"
        );
        let sql_seq = i64::try_from(seq).context("ACP sequence exceeds SQLite range")?;
        let requested: Option<(bool, i64, Option<String>)> = tx
            .query_row(
                "SELECT event_json = ?3, scope, qualification_json
             FROM acp_events WHERE session_id = ?1 AND seq = ?2",
                params![session_id, sql_seq, json],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .optional()?;
        if let Some((same_payload, scope, qualification)) = &requested {
            anyhow::ensure!(
                *same_payload
                    && *scope == metadata.scope as i64
                    && *qualification == metadata.encode_private()?,
                "conflicting ACP journal append at {session_id}@{seq}"
            );
        }
        let stored: Option<Option<String>> = tx
            .query_row(
                &format!("SELECT prompt_commit_json FROM {TABLE} WHERE session_id = ?1"),
                [session_id],
                |row| row.get(0),
            )
            .optional()?;
        let stored = stored.ok_or(EstablishmentRefusal::Unconfirmed)?;
        let previous = stored.as_deref().map(PromptCommit::decode).transpose()?;
        let mut next = match previous {
            Some(previous) if previous.key.origin.owner == key.origin.owner => {
                match request_id.cmp(&previous.key.request_id) {
                    std::cmp::Ordering::Less => None,
                    std::cmp::Ordering::Equal => {
                        anyhow::ensure!(previous.key == key, "conflicting Native prompt result");
                        if let Some(seq) = previous.recorded(component)? {
                            return Ok(Some(RecordDisposition::Existing { seq }));
                        }
                        Some(previous)
                    }
                    std::cmp::Ordering::Greater => Some(PromptCommit::new(key)),
                }
            }
            _ => {
                let base: String = tx.query_row(
                    &format!("SELECT seal_json FROM {TABLE} WHERE session_id = ?1"),
                    [session_id],
                    |row| row.get(0),
                )?;
                let base = EstablishmentSeal(CommitData::decode(&base, session_id)?);
                anyhow::ensure!(
                    base.matches(&base.0.receipt),
                    EstablishmentRefusal::Unconfirmed
                );
                let receipt = &base.0.receipt;
                if key.origin.owner == Some(receipt.owner)
                    && key.origin.attempt_id == receipt.attempt.id
                    && Some(key.origin.producer) == receipt.attempt.producer
                {
                    Some(PromptCommit::new(key))
                } else {
                    None
                }
            }
        };
        if requested.is_some() {
            anyhow::ensure!(
                next.is_none(),
                "Native prompt component has no matching commit"
            );
            return Ok(Some(RecordDisposition::Existing { seq }));
        }
        let inserted = Self::insert_scoped_event(tx, session_id, seq, json, created_at, metadata)?;
        anyhow::ensure!(inserted == 1, "Native prompt component was not inserted");
        if let Some(commit) = next.as_mut() {
            commit.record(component, seq);
            let encoded = serde_json::to_string(commit)?;
            anyhow::ensure!(
                encoded.len() <= MAX_PROMPT_COMMIT_BYTES,
                "oversized Native prompt commit"
            );
            let updated = tx.execute(
                &format!("UPDATE {TABLE} SET prompt_commit_json = ?2 WHERE session_id = ?1"),
                params![session_id, encoded],
            )?;
            anyhow::ensure!(
                updated == 1,
                "Native prompt commit lost its original journal row"
            );
        }
        Ok(Some(RecordDisposition::Inserted))
    }

    /// Compare the complete private payload at insertion, before normal retention.
    pub(crate) fn record_replay(
        &self,
        session_id: &str,
        seq: u64,
        event: &Event,
        metadata: &EventMetadata,
        ordinal: u64,
        replay: &mut ReplayCommit,
    ) -> Result<RecordDisposition> {
        let result = (|| {
            metadata.validate_for_event(event)?;
            anyhow::ensure!(
                session_id == replay.attempt.session_id,
                "replay append belongs to a different session"
            );
            let json = Self::encode_event(event)?;
            let now_ms = crate::time::Timestamp::now().as_millis();
            self.append_controlled(session_id, seq, event, metadata, now_ms, |tx| {
                Ok(
                    match Self::insert_scoped_event(tx, session_id, seq, &json, now_ms, metadata)? {
                        0 => RecordDisposition::Existing { seq },
                        _ => RecordDisposition::Inserted,
                    },
                )
            })
        })();
        replay.appended(ordinal, seq, result.is_ok());
        result
    }

    /// The caller holds the native session exclusion through certification and CAS.
    pub(crate) fn seal_establishment(
        &self,
        replay: &ReplayCommit,
        delivery: &EstablishmentDelivery,
    ) -> Result<EstablishmentSeal> {
        let receipt = delivery
            .receipt
            .as_ref()
            .ok_or(EstablishmentRefusal::Unconfirmed)?;
        anyhow::ensure!(
            delivery.original
                && delivery.complete
                && delivery.failure.is_none()
                && !replay.failed
                && replay.matches(&delivery.attempt)
                && receipt.attempt.as_ref() == delivery.attempt.as_ref()
                && replay.first == delivery.first
                && replay.next == delivery.through,
            EstablishmentRefusal::Unconfirmed
        );
        let data = CommitData {
            version: 1,
            receipt: receipt.as_ref().clone(),
            first: replay.first,
            through: replay.next,
            first_seq: replay.first_seq,
            last_seq: replay.last_seq,
            clear_after_seq: Some(match replay.first_seq {
                Some(first) => first
                    .checked_sub(1)
                    .ok_or(EstablishmentRefusal::Unconfirmed)?,
                None => self.highest_seq(&receipt.attempt.session_id)?,
            }),
        };
        let seal = EstablishmentSeal(data);
        anyhow::ensure!(seal.matches(receipt), EstablishmentRefusal::Unconfirmed);
        self.control_cache
            .with_control(&receipt.attempt.session_id, |cached| -> Result<()> {
                let mut conn = self.conn();
                let tx =
                    conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
                if let Some(mut previous) =
                    Self::autonomous_commit(&tx, &receipt.attempt.session_id)?
                {
                    if crate::acp::state::NativeEventOrigin::owned_attempt(
                        &receipt.attempt,
                        Some(receipt.owner),
                    ) != Some(previous.identity.origin)
                        || receipt.session_id()
                            != Some(previous.identity.root_session_id.0.as_ref())
                    {
                        previous.boundary = Some(AutonomousBoundary {
                            seq: seal.0.clear_after_seq.unwrap_or(0).max(previous.frontier()),
                            disposition: AutonomousBoundaryDisposition::Retired,
                        });
                        Self::write_autonomous_commit(&tx, &receipt.attempt.session_id, &previous)?;
                    }
                }
                tx.execute(
                    &format!(
                        "INSERT INTO {TABLE} (session_id, seal_json) VALUES (?1, ?2)
                    ON CONFLICT(session_id) DO UPDATE SET seal_json = excluded.seal_json"
                    ),
                    params![receipt.attempt.session_id, serde_json::to_string(&seal.0)?],
                )?;
                tx.commit()?;
                *cached = None;
                Ok(())
            })?;
        Ok(seal)
    }

    pub(crate) fn establishment_seal(
        &self,
        receipt: &NativeEstablishmentReceipt,
    ) -> Result<EstablishmentSeal> {
        let seal = self.read_establishment_seal(&receipt.attempt.session_id)?;
        anyhow::ensure!(seal.matches(receipt), EstablishmentRefusal::Unconfirmed);
        Ok(seal)
    }

    /// Pending Fresh borrows only its sealed predecessor's store, never its authority.
    pub(crate) fn certified_attach_store(
        &self,
        row: &crate::session::Instance,
        storage: &crate::session::Storage,
        owner: &crate::acp::runner_lifecycle::RunnerIdentity,
    ) -> Result<Option<crate::session::ExecutionBinding>> {
        use crate::acp::establishment::{
            AcpEstablishment, AuthorityPhase, EstablishmentOrigin, EstablishmentTarget,
        };
        let authority = row
            .acp_establishment
            .as_ref()
            .ok_or(EstablishmentRefusal::Unconfirmed)?;
        let sealed_attempt_id = match authority {
            AcpEstablishment::Applied { attempt_id } => *attempt_id,
            AcpEstablishment::Pending { attempt }
                if attempt.origin == EstablishmentOrigin::Fresh
                    && attempt.target == EstablishmentTarget::New
                    && attempt.producer.is_some() =>
            {
                attempt
                    .preimage
                    .authority
                    .filter(|key| key.phase == AuthorityPhase::Applied && key.id != attempt.id)
                    .map(|key| key.id)
                    .ok_or(EstablishmentRefusal::Unconfirmed)?
            }
            _ => return Err(EstablishmentRefusal::Unconfirmed.into()),
        };
        let seal = self.read_establishment_seal(&row.id)?;
        let receipt = &seal.0.receipt;
        anyhow::ensure!(
            seal.matches(receipt)
                && receipt.attempt.id == sealed_attempt_id
                && receipt.attempt.producer.is_some()
                && owner.birth_fingerprint.is_some()
                && receipt.owner == *owner,
            EstablishmentRefusal::Unconfirmed
        );
        match authority {
            AcpEstablishment::Applied { .. } => {
                receipt.current_applied_postimage(row, storage, owner)?;
            }
            AcpEstablishment::Pending { attempt } => {
                anyhow::ensure!(
                    attempt.matches_pending_source(row, storage)?
                        && receipt.attempt.matches_physical_source(row, storage)?
                        && receipt.postimage(attempt.source_context)?.choice == attempt.preimage,
                    EstablishmentRefusal::Superseded
                );
            }
            _ => return Err(EstablishmentRefusal::Unconfirmed.into()),
        }
        Ok(receipt.store.clone())
    }

    fn read_establishment_seal(&self, session_id: &str) -> Result<EstablishmentSeal> {
        let json: String = self
            .conn()
            .query_row(
                &format!("SELECT seal_json FROM {TABLE} WHERE session_id = ?1"),
                params![session_id],
                |row| row.get(0),
            )
            .optional()?
            .ok_or(EstablishmentRefusal::Unconfirmed)?;
        Ok(EstablishmentSeal(CommitData::decode(&json, session_id)?))
    }
}
