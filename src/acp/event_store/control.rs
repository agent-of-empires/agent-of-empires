use std::borrow::Cow;

use anyhow::{Context, Result};
use rusqlite::{params, Connection};
use serde::{de::IgnoredAny, Deserialize};

use crate::acp::establishment::NativeOperationCorrelation;
use crate::acp::event_sender::EventMetadata;
use crate::acp::operation_effect::{ControlEffect, ControlKind, OperationEffects};
use crate::acp::state::{
    AcpConnectionCause, AcpConnectionPhase, AcpConnectionSource, AcpConnectionUnavailability,
    NativeEventSubject, NativeSubjectEvent,
};

// Only control fields are decoded. Transcript payloads are visited without copying them.
#[derive(Deserialize)]
enum ControlPayload<'a> {
    ThinkingStarted,
    PromptRuntimeError {
        #[serde(rename = "message")]
        _message: IgnoredAny,
    },
    Stopped {
        #[serde(borrow)]
        reason: Cow<'a, str>,
    },
    RateLimit {
        #[serde(rename = "info")]
        _info: IgnoredAny,
    },
    UserPromptSent {
        #[serde(rename = "text")]
        _text: IgnoredAny,
    },
    UserDiffCommentsPrompt {
        #[serde(rename = "assembledMarkdown")]
        _text: IgnoredAny,
    },
    AgentMessageChunk {
        #[serde(rename = "text")]
        _text: IgnoredAny,
    },
    ToolCallStarted {
        #[serde(rename = "tool_call")]
        _tool_call: IgnoredAny,
    },
    AgentSwitched {},
    SessionCleared,
    SessionContextReset {},
    AcpConnectionUnavailable {
        source: AcpConnectionSource,
        cause: AcpConnectionCause,
        phase: AcpConnectionPhase,
        native_subject: Option<NativeEventSubject>,
    },
    AgentStartupError {
        #[serde(rename = "message")]
        _message: IgnoredAny,
        native_subject: Option<NativeEventSubject>,
    },
    RateLimitAutoResumed {
        #[serde(rename = "resets_at")]
        _resets_at: chrono::DateTime<chrono::Utc>,
        #[serde(default)]
        manual: bool,
    },
    AvailableCommandsUpdated {},
    ModesAvailable {},
    CurrentModeChanged {},
    AcpSessionAssigned {},
    PromptCapabilities {},
    AuthStatusUpdated {},
}

impl ControlPayload<'_> {
    fn discriminant(&self) -> &'static str {
        match self {
            Self::ThinkingStarted => "ThinkingStarted",
            Self::PromptRuntimeError { .. } => "PromptRuntimeError",
            Self::Stopped { .. } => "Stopped",
            Self::RateLimit { .. } => "RateLimit",
            Self::UserPromptSent { .. } => "UserPromptSent",
            Self::UserDiffCommentsPrompt { .. } => "UserDiffCommentsPrompt",
            Self::AgentMessageChunk { .. } => "AgentMessageChunk",
            Self::ToolCallStarted { .. } => "ToolCallStarted",
            Self::AgentSwitched { .. } => "AgentSwitched",
            Self::SessionCleared => "SessionCleared",
            Self::SessionContextReset { .. } => "SessionContextReset",
            Self::AcpConnectionUnavailable { .. } => "AcpConnectionUnavailable",
            Self::AgentStartupError { .. } => "AgentStartupError",
            Self::RateLimitAutoResumed { .. } => "RateLimitAutoResumed",
            Self::AvailableCommandsUpdated { .. } => "AvailableCommandsUpdated",
            Self::ModesAvailable { .. } => "ModesAvailable",
            Self::CurrentModeChanged { .. } => "CurrentModeChanged",
            Self::AcpSessionAssigned { .. } => "AcpSessionAssigned",
            Self::PromptCapabilities { .. } => "PromptCapabilities",
            Self::AuthStatusUpdated { .. } => "AuthStatusUpdated",
        }
    }

    fn unavailable(&self) -> Option<(AcpConnectionUnavailability, Option<NativeEventSubject>)> {
        match self {
            Self::AcpConnectionUnavailable {
                source,
                cause,
                phase,
                native_subject,
            } => Some((
                AcpConnectionUnavailability {
                    source: *source,
                    cause: *cause,
                    phase: *phase,
                },
                *native_subject,
            )),
            _ => None,
        }
    }

    fn kind(&self) -> ControlKind<'_> {
        match self {
            Self::ThinkingStarted => ControlKind::Admission,
            Self::PromptRuntimeError { .. } => ControlKind::Diagnostic,
            Self::Stopped { reason } => ControlKind::Stopped(reason),
            Self::RateLimit { .. } => ControlKind::Quota,
            Self::UserPromptSent { .. } => ControlKind::Prompt,
            Self::UserDiffCommentsPrompt { .. } => ControlKind::DiffPrompt,
            Self::AgentMessageChunk { .. } | Self::ToolCallStarted { .. } => ControlKind::AgentWork,
            Self::AgentSwitched { .. } => ControlKind::Switched,
            Self::SessionCleared => ControlKind::Cleared,
            Self::SessionContextReset { .. } => ControlKind::ContextReset,
            Self::AcpConnectionUnavailable {
                source,
                native_subject,
                ..
            } => ControlKind::Unavailable {
                channel_loss: source.is_channel_loss()
                    && !native_subject.is_some_and(|subject| {
                        matches!(subject.kind, NativeSubjectEvent::AdmissionRefusal(_))
                    }),
            },
            Self::AgentStartupError { .. } => ControlKind::StartupError,
            Self::RateLimitAutoResumed { manual, .. } => ControlKind::Resumed { manual: *manual },
            _ => ControlKind::Other,
        }
    }

    fn startup_channel_loss(&self) -> bool {
        matches!(self, Self::AgentStartupError { native_subject: Some(subject), .. }
            if matches!(subject.kind, NativeSubjectEvent::ChannelUnavailable(known)
                if known.source.is_channel_loss()))
    }
}

pub(super) struct ControlRow<'a> {
    pub seq: u64,
    pub created_at_ms: i64,
    pub kind: ControlKind<'a>,
    pub owner_before: Option<NativeOperationCorrelation>,
    pub effect: ControlEffect,
    pub startup_channel_loss: bool,
}

fn scan_controls(
    conn: &Connection,
    session_id: &str,
    from: u64,
    effects: &mut OperationEffects,
    mut consume: impl FnMut(ControlRow<'_>, &EventMetadata) -> Result<()>,
) -> Result<()> {
    let from = i64::try_from(from).context("control bound exceeds SQLite sequence range")?;
    let mut statement = conn.prepare_cached(
        "SELECT seq, created_at, discriminant, scope, qualification_json, event_json
         FROM acp_events WHERE session_id = ?1 AND seq >= ?2
           AND discriminant IN (
               'ThinkingStarted', 'PromptRuntimeError', 'Stopped', 'RateLimit',
               'UserPromptSent', 'UserDiffCommentsPrompt', 'AgentMessageChunk', 'ToolCallStarted',
               'AgentSwitched', 'SessionCleared', 'SessionContextReset',
               'AcpConnectionUnavailable', 'AgentStartupError', 'RateLimitAutoResumed',
               'AvailableCommandsUpdated', 'ModesAvailable', 'CurrentModeChanged',
               'AcpSessionAssigned', 'PromptCapabilities', 'AuthStatusUpdated')
         ORDER BY seq ASC",
    )?;
    let mut rows = statement.query(params![session_id, from])?;
    while let Some(row) = rows.next()? {
        let seq = u64::try_from(row.get::<_, i64>(0)?).context("negative ACP control sequence")?;
        let json = row.get_ref(5)?.as_str()?;
        let payload: ControlPayload<'_> = serde_json::from_str(json)
            .with_context(|| format!("invalid ACP control payload at {session_id}@{seq}"))?;
        anyhow::ensure!(
            payload.discriminant() == row.get_ref(2)?.as_str()?,
            "ACP control discriminant differs at {session_id}@{seq}"
        );
        if matches!(
            payload,
            ControlPayload::ThinkingStarted | ControlPayload::SessionCleared
        ) {
            anyhow::ensure!(
                json.trim_start().starts_with('"'),
                "ACP unit control has an object payload at {session_id}@{seq}"
            );
        }
        let metadata =
            EventMetadata::decode_private(row.get(3)?, row.get_ref(4)?.as_str_or_null()?)
                .with_context(|| {
                    format!("invalid ACP control qualification at {session_id}@{seq}")
                })?;
        let kind = payload.kind();
        metadata
            .validate_control(kind, payload.unavailable())
            .with_context(|| format!("invalid ACP control metadata at {session_id}@{seq}"))?;
        let owner_before = effects.active();
        let effect = effects.apply(kind, &metadata);
        consume(
            ControlRow {
                seq,
                created_at_ms: row.get(1)?,
                kind,
                owner_before,
                effect,
                startup_channel_loss: payload.startup_channel_loss(),
            },
            &metadata,
        )?;
    }
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ControlKnowledge<T> {
    Known(T),
    Indeterminate,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Budget {
    pub spent: i64,
    pub armed: bool,
}

#[derive(Debug, Clone, Copy)]
pub(super) struct QuotaAnchor {
    pub seq: u64,
    pub created_at_ms: i64,
}

#[derive(Debug, Clone, Copy)]
pub(super) struct ParkAnchor {
    pub seq: u64,
    pub quota: Option<QuotaAnchor>,
    pub recorded_at_ms: i64,
    pub cap_reached: bool,
    pub last_resume_attempt_ms: Option<i64>,
}

#[derive(Debug, Clone, Copy)]
pub(super) enum BudgetStep {
    Arm,
    Spend,
    Disarm,
    Reset,
}

#[derive(Debug, Clone, Copy)]
pub(super) struct ControlProjection {
    pub park: ControlKnowledge<Option<ParkAnchor>>,
    pub last_quota: Option<QuotaAnchor>,
    pub interrupted_prompt: ControlKnowledge<Option<u64>>,
    pub interrupted_agent: ControlKnowledge<bool>,
    pub main_open: ControlKnowledge<bool>,
    pub unsuccessful_main: ControlKnowledge<bool>,
    pub budget: ControlKnowledge<Budget>,
    latest_prompt: ControlKnowledge<Option<u64>>,
    operation_prompt: ControlKnowledge<Option<u64>>,
    agent_work: bool,
}

impl ControlProjection {
    fn new(empty: bool) -> Self {
        use ControlKnowledge::{Indeterminate, Known};
        Self {
            park: if empty { Known(None) } else { Indeterminate },
            last_quota: None,
            interrupted_prompt: if empty { Known(None) } else { Indeterminate },
            interrupted_agent: if empty { Known(false) } else { Indeterminate },
            main_open: if empty { Known(false) } else { Indeterminate },
            unsuccessful_main: if empty { Known(false) } else { Indeterminate },
            budget: Indeterminate,
            latest_prompt: if empty { Known(None) } else { Indeterminate },
            operation_prompt: if empty { Known(None) } else { Indeterminate },
            agent_work: false,
        }
    }

    fn advance_budget(&mut self, step: BudgetStep) -> Result<()> {
        use ControlKnowledge::{Indeterminate, Known};
        self.budget = match (step, self.budget) {
            (BudgetStep::Reset, _) => Known(Budget {
                spent: 0,
                armed: false,
            }),
            (_, Indeterminate) => Indeterminate,
            (BudgetStep::Arm, Known(mut budget)) => {
                budget.armed = true;
                Known(budget)
            }
            (BudgetStep::Spend, Known(mut budget)) => {
                if budget.armed {
                    budget.spent = budget
                        .spent
                        .checked_add(1)
                        .context("rate-limit budget overflow")?;
                }
                budget.armed = false;
                Known(budget)
            }
            (BudgetStep::Disarm, Known(mut budget)) => {
                budget.armed = false;
                Known(budget)
            }
        };
        Ok(())
    }

    fn unknown_budget(&mut self) {
        self.budget = ControlKnowledge::Indeterminate;
    }

    fn consume(&mut self, row: ControlRow<'_>, metadata: &EventMetadata) -> Result<()> {
        use crate::acp::state::RATE_LIMIT_EXHAUSTED_RETRIES_REASON;
        use ControlKnowledge::{Indeterminate, Known};

        if row.effect == ControlEffect::Indeterminate {
            match row.kind {
                ControlKind::Quota => self.park = Indeterminate,
                ControlKind::Diagnostic | ControlKind::Stopped(_) => {
                    self.park = Indeterminate;
                    self.main_open = Indeterminate;
                    self.unsuccessful_main = Indeterminate;
                    self.interrupted_prompt = Indeterminate;
                    self.interrupted_agent = Indeterminate;
                    if !matches!(row.kind, ControlKind::Stopped("rate_limited")) {
                        self.unknown_budget();
                    }
                }
                ControlKind::StartupError => {
                    self.unknown_budget();
                    if row.startup_channel_loss {
                        self.main_open = Indeterminate;
                    }
                }
                _ => {}
            }
            return Ok(());
        }
        if row.effect == ControlEffect::Historical {
            return Ok(());
        }
        match row.kind {
            ControlKind::Prompt | ControlKind::DiffPrompt => {
                if !metadata.host_action {
                    return Ok(());
                }
                self.latest_prompt = Known(Some(row.seq));
                self.park = Known(None);
                self.unsuccessful_main = Known(false);
                if matches!(row.kind, ControlKind::Prompt) {
                    self.advance_budget(BudgetStep::Spend)?;
                }
            }
            ControlKind::Admission => {
                if metadata.operation.is_none() {
                    return Ok(());
                }
                self.operation_prompt = self.latest_prompt;
                self.main_open = Known(true);
                self.unsuccessful_main = Known(false);
                self.park = Known(None);
                self.agent_work = false;
            }
            ControlKind::AgentWork => {
                if row.owner_before.is_some()
                    && metadata.operation.map(|operation| operation.correlation) == row.owner_before
                {
                    self.agent_work = true;
                }
            }
            ControlKind::Quota => {
                let quota = QuotaAnchor {
                    seq: row.seq,
                    created_at_ms: row.created_at_ms,
                };
                self.last_quota = Some(quota);
                self.park = Known(Some(ParkAnchor {
                    seq: row.seq,
                    quota: Some(quota),
                    recorded_at_ms: row.created_at_ms,
                    cap_reached: false,
                    last_resume_attempt_ms: None,
                }));
            }
            ControlKind::Diagnostic => {
                if row.effect == ControlEffect::Refusal
                    && row.owner_before.is_some()
                    && metadata.operation.map(|operation| operation.correlation) == row.owner_before
                {
                    self.main_open = Known(false);
                }
                if matches!(row.effect, ControlEffect::Outcome) {
                    self.main_open = Known(false);
                    self.unsuccessful_main = Known(true);
                    self.park = Known(None);
                    self.advance_budget(BudgetStep::Reset)?;
                }
            }
            ControlKind::Stopped(reason) => {
                if row.effect == ControlEffect::Refusal {
                    return Ok(());
                }
                let cap = reason == RATE_LIMIT_EXHAUSTED_RETRIES_REASON;
                if reason == "rate_limited" || cap {
                    let mut park = match self.park {
                        Known(Some(park)) => park,
                        _ => ParkAnchor {
                            seq: row.seq,
                            quota: None,
                            recorded_at_ms: row.created_at_ms,
                            cap_reached: false,
                            last_resume_attempt_ms: None,
                        },
                    };
                    if cap {
                        park.seq = row.seq;
                        park.cap_reached = true;
                    }
                    self.park = Known(Some(park));
                    self.interrupted_prompt = self.operation_prompt;
                    self.interrupted_agent = match self.operation_prompt {
                        Known(None) => Known(self.agent_work),
                        Known(Some(_)) => Known(false),
                        Indeterminate => Indeterminate,
                    };
                } else {
                    self.park = Known(None);
                    self.interrupted_prompt = Known(None);
                    self.interrupted_agent = Known(false);
                }
                self.main_open = Known(false);
                self.unsuccessful_main =
                    Known(matches!(reason, "prompt_failed" | "rate_limited") || cap);
                if reason != "rate_limited" && row.effect != ControlEffect::Companion {
                    self.advance_budget(BudgetStep::Reset)?;
                }
            }
            ControlKind::Switched | ControlKind::Cleared | ControlKind::ContextReset => {
                self.latest_prompt = Known(None);
                self.operation_prompt = Known(None);
                self.agent_work = false;
                self.park = Known(None);
                self.interrupted_prompt = Known(None);
                self.interrupted_agent = Known(false);
                self.main_open = Known(false);
                self.unsuccessful_main = Known(false);
                if matches!(row.kind, ControlKind::Switched) {
                    self.advance_budget(BudgetStep::Reset)?;
                }
            }
            ControlKind::Unavailable { channel_loss: true } => {
                self.advance_budget(BudgetStep::Disarm)?;
                self.main_open = Known(false);
            }
            ControlKind::StartupError => {
                self.advance_budget(BudgetStep::Disarm)?;
                if row.startup_channel_loss {
                    self.main_open = Known(false);
                }
            }
            ControlKind::Resumed { manual } => match self.park {
                Known(Some(mut park)) => {
                    park.last_resume_attempt_ms = Some(row.created_at_ms);
                    self.park = Known(Some(park));
                    self.advance_budget(if manual {
                        BudgetStep::Disarm
                    } else {
                        BudgetStep::Arm
                    })?;
                }
                Known(None) => {}
                Indeterminate => self.unknown_budget(),
            },
            _ => {}
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct ControlCursor {
    pub(super) seq: u64,
    pub(super) projection: ControlProjection,
    effects: OperationEffects,
    empty_without_budget: bool,
}

impl ControlCursor {
    pub(super) fn load(
        conn: &Connection,
        schema: &crate::events::Schema,
        session_id: &str,
    ) -> Result<Self> {
        use rusqlite::OptionalExtension;
        use ControlKnowledge::Known;

        let highest: Option<i64> = conn.query_row(
            "SELECT MAX(seq) FROM acp_events WHERE session_id = ?1",
            [session_id],
            |row| row.get(0),
        )?;
        let seq = u64::try_from(highest.unwrap_or(0)).context("negative ACP journal frontier")?;
        let durable: Option<(i64, i64, i64)> = conn
            .query_row(
                &format!(
                    "SELECT spent, armed, known FROM {} WHERE session_id = ?1",
                    schema.rate_limit_budgets_table()
                ),
                [session_id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .optional()?;
        if let Some((spent, armed, known)) = durable {
            anyhow::ensure!(
                spent >= 0 && matches!(armed, 0 | 1) && matches!(known, 0 | 1),
                "invalid durable rate-limit budget"
            );
        }
        let mut from = seq;
        {
            let mut expected = highest.unwrap_or(0);
            let mut statement = conn.prepare_cached(
                "SELECT seq FROM acp_events WHERE session_id = ?1 ORDER BY seq DESC",
            )?;
            let mut rows = statement.query([session_id])?;
            while let Some(row) = rows.next()? {
                let retained: i64 = row.get(0)?;
                if retained != expected {
                    break;
                }
                from = u64::try_from(retained).context("negative ACP retained sequence")?;
                expected -= 1;
            }
        }
        let mut cursor = Self {
            seq,
            projection: ControlProjection::new(highest.is_none()),
            effects: OperationEffects::default(),
            empty_without_budget: highest.is_none() && durable.is_none(),
        };
        let projection = &mut cursor.projection;
        scan_controls(
            conn,
            session_id,
            from,
            &mut cursor.effects,
            |row, metadata| projection.consume(row, metadata),
        )?;
        cursor.projection.budget = match durable {
            Some((spent, armed, 1)) => Known(Budget {
                spent,
                armed: armed == 1,
            }),
            _ => ControlKnowledge::Indeterminate,
        };
        Ok(cursor)
    }

    pub(super) fn initialize_first_append_budget(&mut self, seq: u64) {
        if self.empty_without_budget && seq == 1 {
            self.projection.budget = ControlKnowledge::Known(Budget {
                spent: 0,
                armed: false,
            });
        }
    }

    pub(super) fn apply(
        &mut self,
        seq: u64,
        created_at_ms: i64,
        event: &crate::acp::state::Event,
        metadata: &EventMetadata,
    ) -> Result<()> {
        anyhow::ensure!(
            seq > self.seq,
            "ACP control append is not beyond its journal frontier"
        );
        if seq != self.seq + 1 {
            self.projection = ControlProjection {
                budget: self.projection.budget,
                ..ControlProjection::new(false)
            };
            self.effects = OperationEffects::default();
        }
        let kind = ControlKind::from_event(event);
        let owner_before = self.effects.active();
        let effect = self.effects.apply(kind, metadata);
        let startup_channel_loss = matches!(event,
            crate::acp::state::Event::AgentStartupError { native_subject: Some(subject), .. }
                if matches!(subject.kind, NativeSubjectEvent::ChannelUnavailable(known)
                    if known.source.is_channel_loss()));
        self.projection.consume(
            ControlRow {
                seq,
                created_at_ms,
                kind,
                owner_before,
                effect,
                startup_channel_loss,
            },
            metadata,
        )?;
        self.seq = seq;
        self.empty_without_budget = false;
        Ok(())
    }
}

impl super::EventStore {
    pub(crate) fn validate_budget_knowledge(conn: &Connection) -> Result<()> {
        use rusqlite::OptionalExtension;
        let column: Option<(String, i64)> = conn
            .query_row(
                "SELECT type, \"notnull\" FROM pragma_table_info('acp_rate_limit_budgets')
             WHERE name = 'known'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        anyhow::ensure!(
            column.is_some_and(
                |(kind, required)| kind.eq_ignore_ascii_case("INTEGER") && required == 1
            ),
            "ACP budget knowledge requires migration57"
        );
        let invalid: bool = conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM acp_rate_limit_budgets
             WHERE typeof(spent) != 'integer' OR spent < 0
                OR typeof(armed) != 'integer' OR armed NOT IN (0,1)
                OR typeof(known) != 'integer' OR known NOT IN (0,1))",
            [],
            |row| row.get(0),
        )?;
        anyhow::ensure!(!invalid, "invalid durable rate-limit budget");
        Ok(())
    }

    pub(super) fn with_control_snapshot<T>(
        &self,
        session_id: &str,
        read: impl FnOnce(&Connection, &ControlCursor) -> Result<T>,
    ) -> Result<T> {
        self.control_cache.with_control(session_id, |cached| {
            let mut conn = self.conn();
            let tx = conn.transaction()?;
            let cursor = match *cached {
                Some(cursor) => cursor,
                None => ControlCursor::load(&tx, &self.schema, session_id)?,
            };
            let result = read(&tx, &cursor)?;
            *cached = Some(cursor);
            Ok(result)
        })
    }

    pub(super) fn append_controlled(
        &self,
        session_id: &str,
        seq: u64,
        event: &crate::acp::state::Event,
        metadata: &EventMetadata,
        created_at_ms: i64,
        insert: impl FnOnce(&rusqlite::Transaction<'_>) -> Result<super::RecordDisposition>,
    ) -> Result<super::RecordDisposition> {
        self.control_cache.with_control(session_id, |cached| {
            let mut conn = self.conn();
            let tx = conn.transaction()?;
            let mut next = match *cached {
                Some(cursor) => cursor,
                None => ControlCursor::load(&tx, &self.schema, session_id)?,
            };
            let before = next.projection.budget;
            let recorded = insert(&tx)?;
            if recorded == super::RecordDisposition::Inserted {
                next.initialize_first_append_budget(seq);
                next.apply(seq, created_at_ms, event, metadata)?;
                if before != next.projection.budget {
                    Self::write_control_budget(&tx, session_id, next.projection.budget)?;
                }
            }
            tx.commit()?;
            *cached = Some(next);
            if recorded == super::RecordDisposition::Inserted {
                crate::events::prune_retention(
                    &conn,
                    &self.schema,
                    session_id,
                    self.max_events_per_session,
                    super::RETENTION_PINNED_EVENT_DISCRIMINANTS,
                );
                if self.max_events_per_session != 0
                    && seq.is_multiple_of(super::COMPLETED_WORK_SWEEP_INTERVAL)
                {
                    super::prune_completed_background_work(
                        &conn,
                        &self.schema,
                        session_id,
                        self.max_events_per_session,
                    );
                }
            }
            Ok(recorded)
        })
    }

    fn write_control_budget(
        tx: &rusqlite::Transaction<'_>,
        session_id: &str,
        budget: ControlKnowledge<Budget>,
    ) -> Result<()> {
        match budget {
            ControlKnowledge::Known(budget) => {
                tx.execute(
                    "INSERT INTO acp_rate_limit_budgets (session_id,spent,armed,known)
                     VALUES (?1,?2,?3,1) ON CONFLICT(session_id) DO UPDATE SET
                         spent=excluded.spent, armed=excluded.armed, known=1",
                    params![session_id, budget.spent, i64::from(budget.armed)],
                )?;
            }
            ControlKnowledge::Indeterminate => {
                tx.execute(
                    "UPDATE acp_rate_limit_budgets SET known=0 WHERE session_id=?1",
                    [session_id],
                )?;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::acp::event_store::{test_support::open_store, EventStore};
    use crate::acp::state::Event;
    use ControlKnowledge::{Indeterminate, Known};

    fn budget(store: &EventStore) -> Option<(i64, i64, i64)> {
        use rusqlite::OptionalExtension;
        store
            .conn()
            .query_row(
                "SELECT spent,armed,known FROM acp_rate_limit_budgets WHERE session_id='s'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .optional()
            .unwrap()
    }

    #[test]
    fn local_refusal_settles_only_its_admission_without_changing_budget() {
        use crate::acp::event_sender::EventScope;
        use crate::acp::protocol::AcpOperationMetadata;

        for known in [0, 1] {
            for case in ["matching", "foreign", "stale", "uncorrelated", "legacy"] {
                let (tmp, store) = open_store(0);
                let origin = crate::acp::state::NativeEventOrigin {
                    context: serde_json::from_value(serde_json::to_value([31u8; 32]).unwrap())
                        .unwrap(),
                    generation: 1,
                    attempt_id: serde_json::from_value(serde_json::json!(uuid::Uuid::new_v4()))
                        .unwrap(),
                    producer: uuid::Uuid::new_v4(),
                    owner: None,
                };
                let first = NativeOperationCorrelation::new(None);
                let second = NativeOperationCorrelation::new(None);
                let qualified = |correlation| EventMetadata {
                    scope: EventScope::OperationCurrent,
                    native_origin: Some(origin),
                    operation: Some(AcpOperationMetadata {
                        correlation,
                        outcome: None,
                    }),
                    ..EventMetadata::history()
                };
                let diagnostic = Event::PromptRuntimeError {
                    message: "local admission denied".into(),
                    evidence: None,
                    native_subject: None,
                };
                let stopped = Event::Stopped {
                    reason: "prompt_failed".into(),
                };
                store
                    .record("s", 1, &Event::ThinkingStarted, &qualified(first))
                    .unwrap();
                store.conn().execute(
                    "UPDATE acp_rate_limit_budgets SET spent=4,armed=1,known=?1 WHERE session_id='s'",
                    [known],
                ).unwrap();
                store.control_cache.forget("s");
                assert_eq!(store.has_in_flight_main_turn("s").unwrap(), Known(true));

                let mut seq = 2;
                if case == "stale" {
                    store
                        .record("s", seq, &diagnostic, &qualified(first))
                        .unwrap();
                    seq += 1;
                    store.record("s", seq, &stopped, &qualified(first)).unwrap();
                    seq += 1;
                    store
                        .record("s", seq, &Event::ThinkingStarted, &qualified(second))
                        .unwrap();
                    seq += 1;
                }
                let metadata = match case {
                    "foreign" => qualified(second),
                    "uncorrelated" => EventMetadata {
                        operation: None,
                        ..qualified(first)
                    },
                    "legacy" => EventMetadata::history(),
                    _ => qualified(first),
                };
                metadata.validate_for_event(&diagnostic).unwrap();
                metadata.validate_for_event(&stopped).unwrap();
                let expected = Known(case != "matching");
                let preserved_park = store.rate_limit_park_snapshot("s").unwrap().1;
                let preserved = store
                    .with_control_snapshot("s", |_, cursor| {
                        Ok((
                            cursor.projection.interrupted_prompt,
                            cursor.projection.interrupted_agent,
                            cursor.projection.unsuccessful_main,
                        ))
                    })
                    .unwrap();
                let assert_state = |store: &EventStore| {
                    assert_eq!(
                        store.has_in_flight_main_turn("s").unwrap(),
                        expected,
                        "{case}, known={known}"
                    );
                    assert_eq!(store.has_in_flight_turn("s").unwrap(), expected);
                    assert_eq!(
                        store.rate_limit_park_snapshot("s").unwrap().1,
                        preserved_park
                    );
                    assert_eq!(budget(store), Some((4, 1, known)));
                    assert_eq!(
                        store.rate_limit_redelivery_streak("s").unwrap(),
                        if known == 1 { Known(4) } else { Indeterminate }
                    );
                    store
                        .with_control_snapshot("s", |_, cursor| {
                            assert_eq!(
                                (
                                    cursor.projection.interrupted_prompt,
                                    cursor.projection.interrupted_agent,
                                    cursor.projection.unsuccessful_main,
                                ),
                                preserved
                            );
                            Ok(())
                        })
                        .unwrap();
                };
                store.record("s", seq, &diagnostic, &metadata).unwrap();
                assert_state(&store);
                store.record("s", seq + 1, &stopped, &metadata).unwrap();
                assert_state(&store);
                drop(store);
                let cold = EventStore::open(&tmp.path().join("acp.db"), 0).unwrap();
                assert_state(&cold);
            }
        }
    }

    #[test]
    fn missing_retained_rows_revoke_older_absence_not_durable_budget() {
        for (removed, new_input, append_gap, known_absence) in [
            (None, false, false, true),
            (Some(2), false, false, false),
            (Some(1), false, false, false),
            (Some(2), true, false, true),
            (None, false, true, false),
        ] {
            let (tmp, store) = open_store(0);
            let prompt = crate::acp::event_store::test_support::user_prompt("older input");
            store
                .record("s", 1, &prompt, &EventMetadata::host(&prompt))
                .unwrap();
            let metadata = Event::AvailableCommandsUpdated { commands: vec![] };
            store
                .record("s", 2, &metadata, &EventMetadata::history())
                .unwrap();
            store
                .record("s", 3, &metadata, &EventMetadata::history())
                .unwrap();
            {
                let conn = store.conn();
                conn.execute("UPDATE acp_rate_limit_budgets SET spent=4,armed=1,known=0 WHERE session_id='s'", []).unwrap();
                if let Some(seq) = removed {
                    conn.execute(
                        "DELETE FROM acp_events WHERE session_id='s' AND seq=?1",
                        [seq],
                    )
                    .unwrap();
                }
            }
            store.control_cache.forget("s");
            if new_input {
                store
                    .record("s", 4, &prompt, &EventMetadata::host(&prompt))
                    .unwrap();
            }
            if append_gap {
                store
                    .record("s", 5, &metadata, &EventMetadata::history())
                    .unwrap();
                assert!(matches!(store.rate_limit_park("s").unwrap(), Indeterminate));
            }
            drop(store);
            let cold = EventStore::open(&tmp.path().join("acp.db"), 0).unwrap();
            match (known_absence, cold.rate_limit_park("s").unwrap()) {
                (true, Known(None)) | (false, Indeterminate) => {}
                (_, actual) => panic!("removed={removed:?}, new_input={new_input}: {actual:?}"),
            }
            assert_eq!(
                cold.rate_limit_redelivery_streak("s").unwrap(),
                Indeterminate
            );
            assert_eq!(budget(&cold), Some((4, 1, 0)));
        }
    }

    #[test]
    fn first_insert_initializes_only_a_truly_empty_history_without_budget() {
        for (prior, retained, expected) in [
            (None, false, Some((0, 0, 1))),
            (Some((7, 1, 1)), false, Some((7, 1, 1))),
            (Some((4, 1, 0)), false, Some((4, 1, 0))),
            (None, true, None),
        ] {
            let (_tmp, store) = open_store(0);
            {
                let conn = store.conn();
                if let Some((spent, armed, known)) = prior {
                    conn.execute(
                        "INSERT INTO acp_rate_limit_budgets VALUES('s',?1,?2,?3)",
                        params![spent, armed, known],
                    )
                    .unwrap();
                }
                if retained {
                    conn.execute(
                        "INSERT INTO acp_events VALUES('s',1,'{\"AvailableCommandsUpdated\":{\"commands\":[]}}',0,'AvailableCommandsUpdated',0,NULL)", [],
                    ).unwrap();
                }
            }
            let event = Event::AvailableCommandsUpdated { commands: vec![] };
            store
                .record(
                    "s",
                    if retained { 2 } else { 1 },
                    &event,
                    &EventMetadata::history(),
                )
                .unwrap();
            assert_eq!(budget(&store), expected);
            assert_eq!(
                store.rate_limit_redelivery_streak("s").unwrap(),
                match expected {
                    Some((spent, _, 1)) => Known(spent),
                    _ => Indeterminate,
                }
            );
        }
    }

    #[test]
    fn duplicate_and_rollback_cannot_restore_unknown_budget_knowledge() {
        let (_tmp, store) = open_store(0);
        let event = Event::AvailableCommandsUpdated { commands: vec![] };
        store
            .record("s", 1, &event, &EventMetadata::history())
            .unwrap();
        store
            .conn()
            .execute(
                "UPDATE acp_rate_limit_budgets SET spent=4,armed=1,known=0 WHERE session_id='s'",
                [],
            )
            .unwrap();
        store.control_cache.forget("s");
        assert_eq!(
            store
                .record("s", 1, &event, &EventMetadata::history())
                .unwrap(),
            crate::acp::event_store::RecordDisposition::Existing { seq: 1 }
        );
        assert_eq!(budget(&store), Some((4, 1, 0)));
        store.conn().execute_batch(
            "CREATE TEMP TRIGGER reject_budget BEFORE UPDATE ON acp_rate_limit_budgets BEGIN SELECT RAISE(ABORT,'budget fault'); END;",
        ).unwrap();
        let stop = Event::Stopped {
            reason: "explicit_stop".into(),
        };
        assert!(store
            .record("s", 2, &stop, &EventMetadata::host(&stop))
            .is_err());
        assert!(store.replay_from("s", 1).unwrap().is_empty());
        assert_eq!(budget(&store), Some((4, 1, 0)));
        assert_eq!(
            store.rate_limit_redelivery_streak("s").unwrap(),
            Indeterminate
        );
        store
            .conn()
            .execute_batch("DROP TRIGGER reject_budget")
            .unwrap();
        store
            .record("s", 2, &stop, &EventMetadata::host(&stop))
            .unwrap();
        assert_eq!(budget(&store), Some((0, 0, 1)));
        assert_eq!(store.rate_limit_redelivery_streak("s").unwrap(), Known(0));
    }
}
