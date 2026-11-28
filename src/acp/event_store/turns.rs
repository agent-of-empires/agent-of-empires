//! Session-state probes derived from the log: turns, pending requests, and snapshots.

use std::collections::HashMap;

use crate::acp::establishment::{NativeResourceSelector, NativeResourceWitness};
use crate::acp::event_sender::EventMetadata;
use anyhow::Result;
use rusqlite::{params, OptionalExtension};
use std::sync::Arc;

use super::{
    decode_logged, logged, query_strings, ControlKnowledge, EventStore,
    NON_SUBSTANTIVE_EVENT_DISCRIMINANTS,
};
use crate::acp::approvals::{Approval, Nonce};
use crate::acp::state::{BackgroundTaskLaunch, Event, Plan, SubagentRecord, WorkflowAgents};
use crate::events;

/// A background agent silent this long without completing no longer holds a turn open.
const BACKGROUND_AGENT_STALE_AFTER_MS: i64 = 6 * 60 * 1000;

/// What the terminal-repair pass needs to decide, and to publish safely.
pub struct TerminalRepairProbe {
    /// Newest seq of any kind, for the seq-conditional publish.
    pub latest_seq: u64,
    /// Newest substantive event, which decides whether to repair.
    pub substantive: Event,
    /// `created_at` of `substantive`, in ms since epoch.
    pub substantive_at_ms: i64,
}

/// One unresolved `BackgroundAgentLaunched` row. See
/// [`EventStore::unresolved_background_agent_launches`].
#[derive(Debug, Clone)]
pub struct UnresolvedBackgroundAgentLaunch {
    pub agent_id: String,
    pub output_file: String,
    pub(crate) witness: Option<std::sync::Arc<crate::acp::establishment::NativeResourceWitness>>,
}

/// Only a matching allocation's later completion can retire its launch.
const UNRESOLVED_BACKGROUND_AGENT_WHERE: &str = "
     FROM acp_events AS started
     WHERE started.session_id = ?1
       AND started.discriminant = 'BackgroundAgentLaunched'
       AND json_extract(started.event_json, '$.BackgroundAgentLaunched.agent_id') IS NOT NULL
       AND NOT EXISTS (
           SELECT 1 FROM acp_events later
           WHERE later.session_id = started.session_id
             AND later.discriminant = started.discriminant AND later.seq > started.seq
             AND json_extract(later.event_json, '$.BackgroundAgentLaunched.agent_id') =
                 json_extract(started.event_json, '$.BackgroundAgentLaunched.agent_id')
             AND (json_extract(later.qualification_json, '$.resource_ref') IS NOT NULL
                 OR json_extract(started.qualification_json, '$.resource_ref') IS NULL)
       )
       AND NOT EXISTS (
           SELECT 1 FROM acp_events finished
           WHERE finished.session_id = started.session_id
             AND finished.discriminant = 'BackgroundAgentCompleted' AND finished.seq > started.seq
             AND json_extract(finished.event_json, '$.BackgroundAgentCompleted.agent_id') =
                 json_extract(started.event_json, '$.BackgroundAgentLaunched.agent_id')
             AND json_extract(finished.qualification_json, '$.resource_ref')
                 IS json_extract(started.qualification_json, '$.resource_ref')
       )
     ORDER BY started.seq ASC";

const UNRESOLVED_BACKGROUND_TASKS_SQL: &str =
    "SELECT json_extract(started.event_json, '$.BackgroundTaskStarted.task_id') AS task_id,
            json_extract(started.event_json, '$.BackgroundTaskStarted.tool_call_id'),
            json_extract(started.event_json, '$.BackgroundTaskStarted.workflow'),
            started.scope, started.qualification_json
     FROM acp_events AS started
     WHERE started.session_id = ?1
       AND started.discriminant = 'BackgroundTaskStarted'
       AND json_extract(started.event_json, '$.BackgroundTaskStarted.task_id') IS NOT NULL
       AND NOT EXISTS (
           SELECT 1 FROM acp_events AS later
           WHERE later.session_id = ?1
             AND later.discriminant = 'BackgroundTaskStarted'
             AND later.seq > started.seq
             AND json_extract(later.event_json, '$.BackgroundTaskStarted.task_id') =
                 json_extract(started.event_json, '$.BackgroundTaskStarted.task_id')
             AND (json_extract(later.qualification_json, '$.resource_ref') IS NOT NULL
                 OR json_extract(started.qualification_json, '$.resource_ref') IS NULL)
       )
       AND NOT EXISTS (
           SELECT 1 FROM acp_events AS finished
           WHERE finished.session_id = ?1
             AND finished.discriminant = 'BackgroundTaskFinished'
             AND finished.seq > started.seq
             AND json_extract(finished.event_json, '$.BackgroundTaskFinished.task_id') =
                 json_extract(started.event_json, '$.BackgroundTaskStarted.task_id')
             AND json_extract(finished.qualification_json, '$.resource_ref')
                 IS json_extract(started.qualification_json, '$.resource_ref')
       )
     ORDER BY started.seq ASC";

const LATER_PLAN_DROP_SQL: &str = "SELECT EXISTS(
     SELECT 1 FROM acp_events
     WHERE session_id = ?1
       AND seq > ?2
       AND (discriminant IN ('SessionCleared', 'AgentSwitched')
            OR (discriminant = 'PlanRemoved'
                AND json_extract(event_json, '$.PlanRemoved.plan_id') = ?3)))";

/// Each subagent's latest snapshot, where it is still working. An
/// `AgentSwitched` drops the children announced before it. SQLite takes the
/// bare `event_json` from the `MAX(seq)` row of each group.
const OPEN_SUBAGENTS_SQL: &str = "SELECT latest.event_json
     FROM (SELECT event_json, MAX(seq) AS seq
           FROM acp_events
           WHERE session_id = ?1
             AND discriminant = 'SubagentUpdated'
             AND seq > COALESCE((SELECT MAX(seq) FROM acp_events
                                 WHERE session_id = ?1
                                   AND discriminant = 'AgentSwitched'), 0)
           GROUP BY json_extract(event_json, '$.SubagentUpdated.subagent.session_id')) AS latest
     WHERE json_extract(latest.event_json, '$.SubagentUpdated.subagent.state')
           IN ('running', 'requires_action')
     ORDER BY latest.seq ASC";

/// Group child allocations once for all retained workflow parents.
fn workflow_agents(
    conn: &rusqlite::Connection,
    session_id: &str,
) -> Result<HashMap<String, HashMap<Option<uuid::Uuid>, WorkflowAgents>>> {
    const SQL: &str = "
        SELECT json_extract(started.event_json, '$.BackgroundAgentLaunched.agent_id'),
               json_extract(started.event_json, '$.BackgroundAgentLaunched.output_file'),
               json_extract(started.event_json, '$.BackgroundAgentLaunched.tool_call_id'),
               started.scope, started.qualification_json,
               json_extract(started.event_json, '$.BackgroundAgentLaunched.workflow.run_id'),
               EXISTS (
                   SELECT 1 FROM acp_events finished
                   WHERE finished.session_id = started.session_id
                     AND finished.discriminant = 'BackgroundAgentCompleted'
                     AND finished.seq > started.seq
                     AND json_extract(finished.event_json, '$.BackgroundAgentCompleted.agent_id') =
                         json_extract(started.event_json, '$.BackgroundAgentLaunched.agent_id')
                     AND json_extract(finished.qualification_json, '$.resource_ref')
                         IS json_extract(started.qualification_json, '$.resource_ref')
               )
        FROM acp_events started
        WHERE started.session_id = ?1 AND started.discriminant = 'BackgroundAgentLaunched'
          AND json_extract(started.event_json, '$.BackgroundAgentLaunched.workflow.run_id') IS NOT NULL
        ORDER BY started.seq ASC";
    let mut statement = conn.prepare_cached(SQL)?;
    let mut rows = statement.query([session_id])?;
    let mut groups: HashMap<String, HashMap<Option<uuid::Uuid>, WorkflowAgents>> = HashMap::new();
    while let Some(row) = rows.next()? {
        let agent: String = row.get(0)?;
        let run = optional_text(row, 5)?.ok_or_else(|| invalid_launch("workflow run is absent"))?;
        let witness = launch_witness(row, 3, 4, session_id)?;
        let parent = match witness.as_deref() {
            Some(witness) => {
                if !witness.matches_agent_launch(
                    &agent,
                    optional_text(row, 2)?.unwrap_or_default(),
                    optional_text(row, 1)?.unwrap_or_default(),
                    Some(run),
                ) {
                    return Err(invalid_launch(
                        "workflow resource selector differs from its launch",
                    )
                    .into());
                }
                match witness.selector.as_ref() {
                    NativeResourceSelector::WorkflowChild {
                        parent_allocation, ..
                    } => Some(*parent_allocation),
                    _ => {
                        return Err(
                            invalid_launch("workflow launch has no parent allocation").into()
                        )
                    }
                }
            }
            None => None,
        };
        if !groups.contains_key(run) {
            groups.insert(run.to_owned(), HashMap::new());
        }
        let known = groups
            .get_mut(run)
            .expect("run inserted")
            .entry(parent)
            .or_default();
        known.launched.insert(agent.clone());
        if row.get::<_, bool>(6)? {
            known.completed.insert(agent.clone());
        } else {
            known.completed.remove(&agent);
        }
        if let Some(witness) = witness {
            known.resources.insert(agent, witness);
        }
    }
    Ok(groups)
}

fn invalid_launch(message: &'static str) -> rusqlite::Error {
    rusqlite::Error::FromSqlConversionFailure(
        4,
        rusqlite::types::Type::Text,
        anyhow::anyhow!(message).into(),
    )
}

fn optional_text<'a>(
    row: &'a rusqlite::Row<'_>,
    column: usize,
) -> rusqlite::Result<Option<&'a str>> {
    row.get_ref(column)?.as_str_or_null().map_err(|error| {
        rusqlite::Error::FromSqlConversionFailure(
            column,
            rusqlite::types::Type::Text,
            Box::new(error),
        )
    })
}

fn launch_witness(
    row: &rusqlite::Row<'_>,
    scope: usize,
    qualification: usize,
    session_id: &str,
) -> rusqlite::Result<Option<Arc<NativeResourceWitness>>> {
    let metadata =
        EventMetadata::decode_private(row.get(scope)?, optional_text(row, qualification)?)
            .map_err(|error| {
                rusqlite::Error::FromSqlConversionFailure(
                    qualification,
                    rusqlite::types::Type::Text,
                    error.into(),
                )
            })?;
    if let Some(witness) = metadata.resource_witness.as_deref() {
        if witness.session_id != session_id || metadata.host_action || metadata.availability_denial
        {
            return Err(invalid_launch(
                "resource qualification has a different owner or role",
            ));
        }
    } else if metadata.resource_ref.is_some() {
        return Err(invalid_launch("resource launch has no first-I/O witness"));
    }
    Ok(metadata.resource_witness)
}

impl EventStore {
    /// The plan the strip shows: `None` once a later clear, agent switch, or
    /// matching removal dropped it, as `AcpState` folds them.
    pub fn latest_plan(&self, session_id: &str) -> Option<Plan> {
        let conn = self.conn();
        let (seq, json) =
            events::latest_by_discriminant(&conn, &self.schema, session_id, "PlanUpdated")?;
        let Event::PlanUpdated { plan } = decode_logged(&json, session_id, "latest_plan")? else {
            return None;
        };
        let dropped = logged(
            conn.query_row(
                LATER_PLAN_DROP_SQL,
                params![session_id, seq as i64, plan.plan_id],
                |row| row.get::<_, bool>(0),
            ),
            "latest_plan",
            session_id,
        )?;
        (!dropped).then_some(plan)
    }

    /// Latest advertised `(image, audio, embedded_context)` prompt capabilities.
    pub fn latest_prompt_capabilities(&self, session_id: &str) -> Option<(bool, bool, bool)> {
        let (_, json) = events::latest_by_discriminant(
            &self.conn(),
            &self.schema,
            session_id,
            "PromptCapabilities",
        )?;
        match decode_logged(&json, session_id, "latest_prompt_capabilities")? {
            Event::PromptCapabilities {
                image,
                audio,
                embedded_context,
                ..
            } => Some((image, audio, embedded_context)),
            _ => None,
        }
    }

    /// Latest `created_at` (ms) of a substantive event per session; lifecycle
    /// metadata does not reset the idle clock.
    pub fn last_event_at_for_sessions(&self, session_ids: &[String]) -> HashMap<String, i64> {
        if session_ids.is_empty() {
            return HashMap::new();
        }
        events::last_event_at_for_topics(
            &self.conn(),
            &self.schema,
            session_ids,
            NON_SUBSTANTIVE_EVENT_DISCRIMINANTS,
        )
    }

    /// Text of the session's first `UserPromptSent`.
    pub fn first_user_prompt(&self, session_id: &str) -> Option<String> {
        let json: String = logged(
            self.conn()
                .query_row(
                    "SELECT event_json FROM acp_events
                     WHERE session_id = ?1
                       AND discriminant = 'UserPromptSent'
                     ORDER BY seq ASC
                     LIMIT 1",
                    params![session_id],
                    |row| row.get(0),
                )
                .optional(),
            "first_user_prompt query",
            session_id,
        )
        .flatten()?;
        match decode_logged(&json, session_id, "first_user_prompt")? {
            Event::UserPromptSent { text, .. } => Some(text),
            _ => None,
        }
    }

    /// The first prompt and its turn's prose, capped at `max_agent_bytes`.
    pub fn first_turn_context(
        &self,
        session_id: &str,
        max_agent_bytes: usize,
    ) -> Option<(String, String)> {
        // Streamed, so the read ends with the first turn rather than loading the whole history.
        let conn = self.conn();
        let what = "first_turn_context";
        let mut stmt = logged(
            conn.prepare(
                "SELECT event_json FROM acp_events
                 WHERE session_id = ?1
                 ORDER BY seq ASC",
            ),
            what,
            session_id,
        )?;
        let rows = logged(
            stmt.query_map(params![session_id], |row| row.get::<_, String>(0)),
            what,
            session_id,
        )?;
        let mut first_prompt: Option<String> = None;
        let mut agent = String::new();
        for event in rows
            .filter_map(|row| row.ok())
            .filter_map(|json| decode_logged(&json, session_id, what))
        {
            match event {
                Event::UserPromptSent { text, .. } if first_prompt.is_none() => {
                    first_prompt = Some(text);
                }
                Event::AgentMessageChunk { text, .. }
                    if first_prompt.is_some() && agent.len() < max_agent_bytes =>
                {
                    agent.push_str(&text);
                }
                Event::Stopped { .. } | Event::AgentStartupError { .. }
                    if first_prompt.is_some() =>
                {
                    break
                }
                event if first_prompt.is_some() && event.is_channel_loss() => break,
                _ => {}
            }
        }
        let first_prompt = first_prompt?;
        if agent.len() > max_agent_bytes {
            let mut end = max_agent_bytes;
            while !agent.is_char_boundary(end) {
                end -= 1;
            }
            agent.truncate(end);
        }
        Some((first_prompt, agent))
    }

    /// Nonces whose responders were neither resolved nor invalidated by loss.
    pub fn unresolved_approval_nonces(&self, session_id: &str) -> Vec<Nonce> {
        query_strings(
            &self.conn(),
            concat!(
                "SELECT json_extract(event_json, '$.ApprovalRequested.approval.nonce') AS nonce
             FROM acp_events
             WHERE session_id = ?1
               AND discriminant = 'ApprovalRequested'
               AND json_extract(event_json, '$.ApprovalRequested.approval.nonce') NOT IN (
                   SELECT json_extract(event_json, '$.ApprovalResolved.nonce')
                   FROM acp_events
                   WHERE session_id = ?1
                     AND discriminant = 'ApprovalResolved'
               )
               AND NOT EXISTS (
                   SELECT 1 FROM acp_events loss
                   WHERE loss.session_id = ?1
                     AND loss.discriminant = 'AcpConnectionUnavailable'
                     AND ",
                native_channel_loss_sql!("loss.event_json"),
                "
                     AND loss.seq > acp_events.seq
               )
             ORDER BY seq ASC"
            ),
            "unresolved_approval_nonces",
            session_id,
        )
        .into_iter()
        .map(Nonce)
        .collect()
    }

    /// Agent ids of `BackgroundAgentLaunched` events with no later
    /// `BackgroundAgentCompleted`. Read from the durable log rather than the
    /// control cache, which may be cold for a session no reader has hydrated
    /// since a daemon restart, so a dying worker's teardown can detach
    /// sub-agents its tailer will never report on again (#4001).
    pub fn unresolved_background_agent_ids(&self, session_id: &str) -> Vec<String> {
        query_strings(
            &self.conn(),
            &format!(
                "SELECT json_extract(event_json, '$.BackgroundAgentLaunched.agent_id') AS agent_id\
                 {UNRESOLVED_BACKGROUND_AGENT_WHERE}"
            ),
            "unresolved_background_agent_ids",
            session_id,
        )
    }

    /// Task ids of `BackgroundTaskStarted` events with no later
    /// `BackgroundTaskFinished`, for the same dead-worker detach as
    /// [`Self::unresolved_background_agent_ids`].
    pub fn unresolved_background_task_ids(&self, session_id: &str) -> Vec<String> {
        query_strings(
            &self.conn(),
            UNRESOLVED_BACKGROUND_TASKS_SQL,
            "unresolved_background_task_ids",
            session_id,
        )
    }

    /// Subagents whose latest snapshot is still working, for the dead-worker
    /// close-out.
    pub fn open_subagents(&self, session_id: &str) -> Vec<SubagentRecord> {
        query_strings(
            &self.conn(),
            OPEN_SUBAGENTS_SQL,
            "open_subagents",
            session_id,
        )
        .iter()
        .filter_map(
            |json| match decode_logged(json, session_id, "open_subagents")? {
                Event::SubagentUpdated { subagent, .. } => Some(subagent),
                _ => None,
            },
        )
        .collect()
    }

    fn has_open_subagent(&self, session_id: &str) -> Result<bool> {
        Ok(self
            .reader()
            .prepare_cached(OPEN_SUBAGENTS_SQL)?
            .exists(params![session_id])?)
    }

    pub fn has_unresolved_background_tasks(&self, session_id: &str) -> bool {
        self.try_has_unresolved_background_tasks(session_id)
            .unwrap_or(false)
    }

    /// `None` when the query failed.
    fn try_has_unresolved_background_tasks(&self, session_id: &str) -> Option<bool> {
        logged(
            self.conn()
                .prepare_cached(UNRESOLVED_BACKGROUND_TASKS_SQL)
                .and_then(|mut statement| statement.exists(params![session_id])),
            "has_unresolved_background_tasks",
            session_id,
        )
    }

    pub fn unresolved_background_task_launches(
        &self,
        session_id: &str,
    ) -> Result<Vec<BackgroundTaskLaunch>> {
        let conn = self.conn();
        let tx = conn.unchecked_transaction()?;
        let mut launches = {
            let mut statement = tx.prepare_cached(UNRESOLVED_BACKGROUND_TASKS_SQL)?;
            let rows = statement.query_map([session_id], |row| {
                let workflow = optional_text(row, 2)?
                    .map(serde_json::from_str)
                    .transpose()
                    .map_err(|error| {
                        rusqlite::Error::FromSqlConversionFailure(
                            2,
                            rusqlite::types::Type::Text,
                            Box::new(error),
                        )
                    })?;
                let launch = BackgroundTaskLaunch {
                    task_id: row.get(0)?,
                    tool_call_id: row.get(1)?,
                    workflow,
                    workflow_agents: WorkflowAgents::default(),
                    witness: launch_witness(row, 3, 4, session_id)?,
                };
                if launch.witness.as_ref().is_some_and(|witness| {
                    !witness.matches_task_launch(
                        &launch.task_id,
                        launch.tool_call_id.as_deref(),
                        launch.workflow.as_ref(),
                    )
                }) {
                    return Err(invalid_launch(
                        "task resource selector differs from its launch",
                    ));
                }
                Ok(launch)
            })?;
            rows.collect::<rusqlite::Result<Vec<_>>>()?
        };
        if launches.iter().any(|launch| launch.workflow.is_some()) {
            let mut groups = workflow_agents(&tx, session_id)?;
            for launch in &mut launches {
                if let Some(workflow) = &launch.workflow {
                    let allocation = launch.witness.as_ref().map(|witness| witness.allocation);
                    launch.workflow_agents = groups
                        .get_mut(&workflow.run_id)
                        .and_then(|parents| parents.remove(&allocation))
                        .unwrap_or_default();
                }
            }
        }
        tx.commit()?;
        Ok(launches)
    }

    /// Standalone launch payload and qualification are read from the same SQLite row.
    pub fn unresolved_background_agent_launches(
        &self,
        session_id: &str,
    ) -> Result<Vec<UnresolvedBackgroundAgentLaunch>> {
        self.unresolved_background_agent_launches_inner(session_id, false)
    }

    /// Tracking retirement includes workflow children even after their parent task ended.
    pub(crate) fn unresolved_background_agent_tracking_launches(
        &self,
        session_id: &str,
    ) -> Result<Vec<UnresolvedBackgroundAgentLaunch>> {
        self.unresolved_background_agent_launches_inner(session_id, true)
    }

    fn unresolved_background_agent_launches_inner(
        &self,
        session_id: &str,
        include_workflows: bool,
    ) -> Result<Vec<UnresolvedBackgroundAgentLaunch>> {
        let conn = self.conn();
        let sql = format!(
            "SELECT json_extract(started.event_json, '$.BackgroundAgentLaunched.agent_id'),
                    json_extract(started.event_json, '$.BackgroundAgentLaunched.output_file'),
                    json_extract(started.event_json, '$.BackgroundAgentLaunched.workflow') IS NOT NULL,
                    json_extract(started.event_json, '$.BackgroundAgentLaunched.tool_call_id'),
                    started.scope, started.qualification_json,
                    json_extract(started.event_json, '$.BackgroundAgentLaunched.workflow.run_id')
             {UNRESOLVED_BACKGROUND_AGENT_WHERE}"
        );
        let mut statement = conn.prepare(&sql)?;
        let rows = statement.query_map([session_id], |row| {
            let workflow = row.get::<_, bool>(2)?;
            if workflow && !include_workflows {
                return Ok(None);
            }
            let workflow_run: Option<String> = row.get(6)?;
            if workflow && workflow_run.is_none() {
                return Err(invalid_launch("workflow agent launch has no run identity"));
            }
            let launch = UnresolvedBackgroundAgentLaunch {
                agent_id: row.get(0)?,
                output_file: row.get::<_, Option<String>>(1)?.unwrap_or_default(),
                witness: launch_witness(row, 4, 5, session_id)?,
            };
            let tool = optional_text(row, 3)?.unwrap_or_default();
            if launch.witness.as_ref().is_some_and(|witness| {
                !witness.matches_agent_launch(
                    &launch.agent_id,
                    tool,
                    &launch.output_file,
                    workflow_run.as_deref(),
                )
            }) {
                return Err(invalid_launch(
                    "agent resource selector differs from its launch",
                ));
            }
            Ok(Some(launch))
        })?;
        Ok(rows
            .filter_map(|row| row.transpose())
            .collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// Full payloads of unresolved approval requests, in request order.
    pub fn pending_approval_requests(&self, session_id: &str) -> Vec<Approval> {
        query_strings(
            &self.conn(),
            concat!(
                "SELECT event_json
             FROM acp_events
             WHERE session_id = ?1
               AND discriminant = 'ApprovalRequested'
               AND json_extract(event_json, '$.ApprovalRequested.approval.nonce') NOT IN (
                   SELECT json_extract(event_json, '$.ApprovalResolved.nonce')
                   FROM acp_events
                   WHERE session_id = ?1
                     AND discriminant = 'ApprovalResolved'
               )
               AND NOT EXISTS (
                   SELECT 1 FROM acp_events loss
                   WHERE loss.session_id = ?1
                     AND loss.discriminant = 'AcpConnectionUnavailable'
                     AND ",
                native_channel_loss_sql!("loss.event_json"),
                "
                     AND loss.seq > acp_events.seq
               )
             ORDER BY seq ASC"
            ),
            "pending_approval_requests",
            session_id,
        )
        .iter()
        .filter_map(
            |json| match decode_logged(json, session_id, "pending_approval_requests")? {
                Event::ApprovalRequested { approval } => Some(approval),
                _ => None,
            },
        )
        .collect()
    }

    /// Whether a turn ended (`Stopped` or `AgentStartupError`) after `seq`. `None` when the
    /// query failed.
    pub fn turn_ended_after(&self, session_id: &str, seq: u64) -> Option<bool> {
        logged(
            self.conn().query_row(
                "SELECT EXISTS (
                     SELECT 1 FROM acp_events
                     WHERE session_id = ?1
                       AND seq > ?2
                       AND discriminant IN ('Stopped', 'AgentStartupError'))",
                params![session_id, seq as i64],
                |row| row.get::<_, bool>(0),
            ),
            "turn_ended_after query",
            session_id,
        )
    }

    /// Elicitation nonces whose responders remain available.
    pub fn unresolved_elicitation_nonces(&self, session_id: &str) -> Vec<Nonce> {
        query_strings(
            &self.conn(),
            concat!("SELECT json_extract(event_json, '$.ElicitationRequested.elicitation.nonce') AS nonce
             FROM acp_events
             WHERE session_id = ?1
               AND discriminant = 'ElicitationRequested'
               AND json_extract(event_json, '$.ElicitationRequested.elicitation.nonce') NOT IN (
                   SELECT json_extract(event_json, '$.ElicitationResolved.nonce')
                   FROM acp_events
                   WHERE session_id = ?1
                     AND discriminant = 'ElicitationResolved'
               )
               AND NOT EXISTS (
                   SELECT 1 FROM acp_events loss
                   WHERE loss.session_id = ?1
                     AND loss.discriminant = 'AcpConnectionUnavailable'
                     AND ", native_channel_loss_sql!("loss.event_json"), "
                     AND loss.seq > acp_events.seq
               )"),
            "unresolved_elicitation_nonces",
            session_id,
        )
        .into_iter()
        .map(Nonce)
        .collect()
    }

    pub fn has_in_flight_main_turn(&self, session_id: &str) -> Result<ControlKnowledge<bool>> {
        self.with_control_snapshot(session_id, |_, cursor| Ok(cursor.projection.main_open))
    }

    /// Qualified agent output since the last actual turn boundary, including a
    /// turn started by the agent without a host prompt.
    pub(crate) fn has_open_agent_turn(&self, session_id: &str) -> Result<ControlKnowledge<bool>> {
        self.with_control_snapshot(session_id, |_, cursor| {
            use ControlKnowledge::{Indeterminate, Known};
            Ok(
                match (
                    cursor.projection.agent_open,
                    cursor.projection.autonomous_open,
                ) {
                    (Known(true), _) | (_, Known(true)) => Known(true),
                    (Known(false), Known(false)) => Known(false),
                    _ => Indeterminate,
                },
            )
        })
    }

    pub fn has_open_turn(&self, session_id: &str) -> Result<ControlKnowledge<bool>> {
        use ControlKnowledge::{Indeterminate, Known};
        let main = self.has_in_flight_turn(session_id)?;
        if main == Known(true) {
            return Ok(main);
        }
        Ok(match (main, self.has_open_agent_turn(session_id)?) {
            (_, Known(true)) => Known(true),
            (Known(false), Known(false)) => Known(false),
            _ => Indeterminate,
        })
    }

    /// A recent background finish also holds the actor until its follow-up
    /// reaches a real turn boundary. Unknown control never authorizes retirement.
    pub fn holds_live_work(&self, session_id: &str) -> Result<ControlKnowledge<bool>> {
        use ControlKnowledge::{Indeterminate, Known};
        let open = self.has_open_turn(session_id)?;
        if open == Known(true) || self.has_open_subagent(session_id)? {
            return Ok(Known(true));
        }
        Ok(match (open, self.has_unhandled_finish(session_id)?) {
            (_, Known(true)) => Known(true),
            (Known(false), Known(false)) => Known(false),
            _ => Indeterminate,
        })
    }

    fn has_unhandled_finish(&self, session_id: &str) -> Result<ControlKnowledge<bool>> {
        use ControlKnowledge::{Indeterminate, Known};
        let stale_cutoff =
            crate::time::Timestamp::now().as_millis() - BACKGROUND_AGENT_STALE_AFTER_MS;
        self.with_control_snapshot(session_id, |conn, cursor| {
            let stopped = match cursor.projection.stopped_through {
                Known(seq) => Some(i64::try_from(seq)?),
                Indeterminate => None,
            };
            let recent: bool = conn.query_row(
                "SELECT EXISTS (
                     SELECT 1 FROM acp_events
                     WHERE session_id = ?1
                       AND discriminant IN ('BackgroundTaskFinished', 'BackgroundAgentCompleted')
                       AND created_at >= ?3
                       AND (?2 IS NULL OR seq > ?2))",
                params![session_id, stopped, stale_cutoff],
                |row| row.get(0),
            )?;
            Ok(if !recent {
                Known(false)
            } else if stopped.is_some() {
                Known(true)
            } else {
                Indeterminate
            })
        })
    }

    pub fn has_unsuccessful_main_outcome(
        &self,
        session_id: &str,
    ) -> Result<ControlKnowledge<bool>> {
        self.with_control_snapshot(session_id, |_, cursor| {
            Ok(cursor.projection.unsuccessful_main)
        })
    }

    /// True while a main turn, background task, or recently reporting background agent is open.
    pub fn has_in_flight_turn(&self, session_id: &str) -> Result<ControlKnowledge<bool>> {
        if self
            .reader()
            .prepare_cached(UNRESOLVED_BACKGROUND_TASKS_SQL)?
            .exists(params![session_id])?
        {
            return Ok(ControlKnowledge::Known(true));
        }
        let main = self.has_in_flight_main_turn(session_id)?;
        if main == ControlKnowledge::Known(true) {
            return Ok(main);
        }
        let conn = self.reader();
        let stale_cutoff =
            crate::time::Timestamp::now().as_millis() - BACKGROUND_AGENT_STALE_AFTER_MS;
        let bg_in_flight: i64 = conn.query_row(
                "SELECT COUNT(*) FROM (
                     SELECT aid, MAX(created_at) AS last_at FROM (
                         SELECT json_extract(event_json, '$.BackgroundAgentLaunched.agent_id') AS aid,
                                created_at
                           FROM acp_events WHERE session_id = ?1
                             AND discriminant = 'BackgroundAgentLaunched'
                         UNION ALL
                         SELECT json_extract(event_json, '$.BackgroundAgentProgress.agent_id'),
                                created_at
                           FROM acp_events WHERE session_id = ?1
                             AND discriminant = 'BackgroundAgentProgress'
                     )
                     WHERE aid IS NOT NULL
                     GROUP BY aid
                 ) started
                 WHERE started.last_at >= ?2
                   AND started.aid NOT IN (
                     SELECT json_extract(event_json, '$.BackgroundAgentCompleted.agent_id')
                       FROM acp_events WHERE session_id = ?1
                         AND discriminant = 'BackgroundAgentCompleted'
                   )",
                params![session_id, stale_cutoff],
                |row| row.get(0),
            )?;
        Ok(if bg_in_flight > 0 {
            ControlKnowledge::Known(true)
        } else {
            main
        })
    }

    /// The agent text of the open turn when that turn has no prompt and ran no
    /// tool, else `None`.
    pub fn agent_turn_sole_text(&self, session_id: &str) -> Option<String> {
        let rows = query_strings(
            &self.conn(),
            "SELECT event_json FROM acp_events
             WHERE session_id = ?1
               AND seq > (SELECT IFNULL(MAX(seq), 0) FROM acp_events
                          WHERE session_id = ?1
                            AND discriminant IN ('Stopped', 'AgentStartupError'))
               AND discriminant IN ('UserPromptSent', 'UserDiffCommentsPrompt',
                                    'ToolCallStarted', 'AgentMessageChunk')
             ORDER BY seq ASC",
            "agent_turn_sole_text",
            session_id,
        );
        let mut text = String::new();
        for json in rows {
            match decode_logged(&json, session_id, "agent_turn_sole_text")? {
                Event::AgentMessageChunk { text: chunk, .. } => text.push_str(&chunk),
                _ => return None,
            }
        }
        (!text.is_empty()).then_some(text)
    }

    /// Read the inputs the terminal-repair pass decides on.
    pub fn terminal_repair_probe(&self, session_id: &str) -> Option<TerminalRepairProbe> {
        let conn = self.conn();
        let latest_seq = logged(
            conn.query_row(
                "SELECT MAX(seq) FROM acp_events WHERE session_id = ?1",
                params![session_id],
                |row| row.get::<_, Option<i64>>(0),
            ),
            "terminal_repair_probe latest seq",
            session_id,
        )
        .flatten()?;
        let placeholders = std::iter::repeat_n("?", NON_SUBSTANTIVE_EVENT_DISCRIMINANTS.len())
            .collect::<Vec<_>>()
            .join(",");
        let sql = format!(
            "SELECT event_json, created_at FROM acp_events
                 WHERE session_id = ? AND discriminant NOT IN ({placeholders})
                 ORDER BY seq DESC
                 LIMIT 1"
        );
        let bind =
            std::iter::once(session_id).chain(NON_SUBSTANTIVE_EVENT_DISCRIMINANTS.iter().copied());
        let (json, substantive_at_ms): (String, i64) = logged(
            conn.query_row(&sql, rusqlite::params_from_iter(bind), |row| {
                Ok((row.get(0)?, row.get(1)?))
            })
            .optional(),
            "terminal_repair_probe substantive event",
            session_id,
        )
        .flatten()?;
        Some(TerminalRepairProbe {
            latest_seq: latest_seq as u64,
            substantive: decode_logged(&json, session_id, "terminal_repair_probe")?,
            substantive_at_ms,
        })
    }

    /// True when the current turn epoch has a `ToolCallStarted` with no
    /// matching `ToolCallCompleted`. Fails closed (true) on a query error.
    pub fn has_open_tool_call_in_epoch(&self, session_id: &str) -> bool {
        let conn = self.conn();
        let epoch_start: i64 = logged(
            conn.query_row(
                concat!(
                    "SELECT MAX(seq) FROM acp_events
                 WHERE session_id = ?1
                   AND (discriminant IN ('Stopped', 'AgentStartupError')
                     OR ",
                    native_channel_loss_sql!("event_json"),
                    ")"
                ),
                params![session_id],
                |row| row.get::<_, Option<i64>>(0),
            ),
            "has_open_tool_call_in_epoch epoch query",
            session_id,
        )
        .flatten()
        .unwrap_or(0);
        logged(
            conn.query_row(
                "SELECT 1 FROM acp_events
                 WHERE session_id = ?1
                   AND seq > ?2
                   AND discriminant = 'ToolCallStarted'
                   AND json_extract(event_json, '$.ToolCallStarted.tool_call.id') NOT IN (
                       SELECT json_extract(event_json, '$.ToolCallCompleted.tool_call_id')
                         FROM acp_events
                        WHERE session_id = ?1
                          AND seq > ?2
                          AND discriminant = 'ToolCallCompleted'
                          -- `x NOT IN (a, NULL)` is NULL, which would hide every open call.
                          AND json_extract(event_json, '$.ToolCallCompleted.tool_call_id') IS NOT NULL
                   )
                 LIMIT 1",
                params![session_id, epoch_start],
                |row| row.get::<_, i64>(0),
            )
            .optional(),
            "has_open_tool_call_in_epoch",
            session_id,
        )
        .is_none_or(|open| open.is_some())
    }
}

#[cfg(test)]
mod tests {
    use super::super::test_support::*;
    use super::*;
    use crate::acp::approvals::ApprovalDecision;
    use crate::acp::state::{BackgroundAgentStatus, WorkflowLaunch, WorkflowRef};

    #[test]
    fn first_prompt_and_first_turn_context() {
        let (_tmp, store) = open_store(1000);
        assert!(store.first_user_prompt("s-1").is_none());
        assert!(store.first_turn_context("s-1", 4096).is_none());
        store
            .record(
                "s-1",
                1,
                &user_prompt("first prompt"),
                &crate::acp::event_sender::EventMetadata::history(),
            )
            .unwrap();
        store
            .record(
                "s-1",
                2,
                &user_prompt("second prompt"),
                &crate::acp::event_sender::EventMetadata::history(),
            )
            .unwrap();
        assert_eq!(
            store.first_user_prompt("s-1").as_deref(),
            Some("first prompt")
        );
        assert!(store.first_user_prompt("s-2").is_none());

        // (events, expected agent prose)
        let cases = [
            (
                vec![
                    user_prompt("p"),
                    agent_chunk("Looking at auth.rs. "),
                    agent_chunk("Patched the redirect."),
                    stopped("prompt_complete"),
                    user_prompt("now add a test"),
                    agent_chunk("second turn prose"),
                    stopped("prompt_complete"),
                ],
                "Looking at auth.rs. Patched the redirect.",
            ),
            (
                vec![
                    user_prompt("p"),
                    agent_chunk("first-turn prose"),
                    stopped("user_stopped"),
                    user_prompt("resume it"),
                    agent_chunk("second-turn prose"),
                    stopped("prompt_complete"),
                ],
                "first-turn prose",
            ),
            (vec![user_prompt("p"), stopped("prompt_complete")], ""),
        ];
        for (i, (events, agent)) in cases.into_iter().enumerate() {
            let id = format!("t-{i}");
            record_from(&store, &id, 1, events);
            assert_eq!(
                store.first_turn_context(&id, 4096),
                Some(("p".to_string(), agent.to_string())),
                "case {i}"
            );
        }

        record_from(
            &store,
            "c-1",
            1,
            [
                user_prompt("go"),
                agent_chunk(&"é".repeat(50)),
                stopped("prompt_complete"),
            ],
        );
        let (_, agent) = store.first_turn_context("c-1", 10).expect("has prompt");
        assert!(agent.len() <= 10 && agent.chars().all(|c| c == 'é'));
    }

    #[test]
    fn admitted_main_and_background_work_have_independent_knowledge() {
        use ControlKnowledge::{Indeterminate, Known};
        let progress = || Event::BackgroundAgentProgress {
            agent_id: "bg-1".into(),
            status: BackgroundAgentStatus::Running,
            tool_count: 1,
            tools: vec![],
            last_tool: None,
            last_text: None,
            at: crate::time::Timestamp::now(),
        };
        for (events, main, any) in [
            (
                vec![user_prompt("explicit but not admitted")],
                Known(false),
                Known(false),
            ),
            (
                vec![
                    user_prompt("go"),
                    Event::ThinkingStarted,
                    agent_chunk("work"),
                ],
                Known(true),
                Known(true),
            ),
            (
                vec![
                    user_prompt("go"),
                    Event::ThinkingStarted,
                    stopped("prompt_complete"),
                ],
                Known(false),
                Known(false),
            ),
            (
                vec![user_prompt("go"), Event::startup_error("boom")],
                Known(false),
                Known(false),
            ),
            (
                vec![
                    user_prompt("first"),
                    Event::ThinkingStarted,
                    stopped("prompt_complete"),
                    user_prompt("second"),
                    Event::ThinkingStarted,
                ],
                Known(true),
                Known(true),
            ),
            (
                vec![stopped("prompt_complete"), progress()],
                Known(false),
                Known(true),
            ),
            (
                vec![stopped("rate_limited"), progress()],
                Known(false),
                Known(true),
            ),
        ] {
            let (_tmp, store) = open_store(0);
            record_projection(&store, "s", 1, events);
            assert_eq!(store.has_in_flight_main_turn("s").unwrap(), main);
            assert_eq!(store.has_in_flight_turn("s").unwrap(), any);
        }
        let (_tmp, store) = open_store(0);
        store
            .record("s", 2, &progress(), &EventMetadata::history())
            .unwrap();
        assert_eq!(store.has_in_flight_main_turn("s").unwrap(), Indeterminate);
        assert_eq!(store.has_in_flight_turn("s").unwrap(), Known(true));
        let completed = Event::BackgroundAgentCompleted {
            agent_id: "bg-1".into(),
            status: BackgroundAgentStatus::Completed,
            tools: vec![],
            result: None,
            warning: None,
            ended_at: crate::time::Timestamp::now(),
        };
        store
            .record("s", 3, &completed, &EventMetadata::history())
            .unwrap();
        assert_eq!(store.has_in_flight_turn("s").unwrap(), Indeterminate);
        // A stale progress record does not turn unknown main state into known idle.
        let stale =
            crate::time::Timestamp::now().as_millis() - BACKGROUND_AGENT_STALE_AFTER_MS - 60_000;
        store.record_at("stale", 2, &progress(), stale).unwrap();
        assert_eq!(store.has_in_flight_turn("stale").unwrap(), Indeterminate);
    }

    #[test]
    fn unsuccessful_main_outcome_survives_historical_ambient_rows_until_host_boundary() {
        use ControlKnowledge::Known;
        for reason in [
            "prompt_failed",
            "prompt_rejected",
            "rate_limited",
            crate::acp::state::RATE_LIMIT_EXHAUSTED_RETRIES_REASON,
        ] {
            for boundary in [
                user_prompt("next explicit prompt"),
                Event::UserDiffCommentsPrompt {
                    intro: String::new(),
                    outro: String::new(),
                    is_multi_repo: false,
                    comments: vec![],
                    assembled_markdown: "review".into(),
                },
                stopped("prompt_complete"),
                Event::SessionCleared,
                Event::AgentSwitched {
                    from: "claude".into(),
                    to: "codex".into(),
                    reason: "explicit".into(),
                },
            ] {
                let (_tmp, store) = open_store(0);
                record_projection(
                    &store,
                    "s",
                    1,
                    [
                        user_prompt("original"),
                        Event::ThinkingStarted,
                        stopped(reason),
                    ],
                );
                record_from(
                    &store,
                    "s",
                    4,
                    [
                        agent_chunk("ambient"),
                        stopped("agent_idle"),
                        Event::PromptRuntimeError {
                            message: "later diagnostic".into(),
                            evidence: None,
                            native_subject: None,
                        },
                        Event::AcpSessionAssigned {
                            acp_session_id: "read-only".into(),
                            connection_ready: false,
                            native_subject: None,
                        },
                    ],
                );
                assert_eq!(
                    store.has_unsuccessful_main_outcome("s").unwrap(),
                    Known(true)
                );
                assert_eq!(store.has_in_flight_main_turn("s").unwrap(), Known(false));
                store
                    .record("s", 8, &boundary, &EventMetadata::host(&boundary))
                    .unwrap();
                assert_eq!(
                    store.has_unsuccessful_main_outcome("s").unwrap(),
                    Known(false)
                );
            }
        }
        // An owned diagnostic is terminal; historical ambient diagnostics are not.
        let (_tmp, store) = open_store(0);
        record_projection(
            &store,
            "s",
            1,
            [
                user_prompt("work"),
                Event::ThinkingStarted,
                Event::PromptRuntimeError {
                    message: "failed".into(),
                    evidence: None,
                    native_subject: None,
                },
            ],
        );
        assert_eq!(store.has_in_flight_main_turn("s").unwrap(), Known(false));
        assert_eq!(
            store.has_unsuccessful_main_outcome("s").unwrap(),
            Known(true)
        );
    }

    #[test]
    fn continuation_projection_read_errors_remain_errors() {
        let (_tmp, store) = open_store(0);
        store
            .conn()
            .execute_batch("DROP TABLE acp_rate_limit_budgets")
            .unwrap();
        assert!(store.has_in_flight_main_turn("s").is_err());
        assert!(store.has_unsuccessful_main_outcome("s").is_err());
        assert!(store.has_in_flight_turn("s").is_err());
    }

    #[test]
    fn a_background_finish_holds_live_work_until_the_agent_answers() {
        let (_tmp, store) = open_store(1000);
        let started = Event::BackgroundTaskStarted {
            task_id: "t".into(),
            tool_call_id: None,
            description: "sleep".into(),
            workflow: None,
            started_at: None,
        };
        let finished = Event::BackgroundTaskFinished {
            task_id: "t".into(),
            ended_at: None,
        };
        // (events, holds live work)
        let cases = [
            (vec![user_prompt("go"), stopped("prompt_complete")], false),
            (
                vec![
                    user_prompt("go"),
                    started.clone(),
                    stopped("prompt_complete"),
                    finished.clone(),
                ],
                true,
            ),
            (
                vec![
                    user_prompt("go"),
                    started,
                    stopped("prompt_complete"),
                    finished,
                    agent_chunk("done"),
                    stopped("prompt_complete"),
                ],
                false,
            ),
        ];
        for (i, (events, holds)) in cases.into_iter().enumerate() {
            let id = format!("s-{i}");
            record_projection(&store, &id, 1, events);
            assert_eq!(
                store.holds_live_work(&id).unwrap(),
                ControlKnowledge::Known(holds),
                "case {i}"
            );
        }
    }

    /// A child's latest snapshot decides; a switch drops the old agent's children.
    #[test]
    fn a_working_child_holds_live_work_until_it_idles() {
        use crate::acp::state::SubagentState;
        let (_tmp, store) = open_store(1000);
        let child = |id: &str, state| Event::SubagentUpdated {
            subagent: SubagentRecord {
                session_id: id.into(),
                parent_session_id: "root".into(),
                provenance: None,
                parent_tool_call_id: Some("msg-1".into()),
                title: None,
                description: None,
                state,
                stop_reason: None,
                error: None,
                cancel_capable: true,
                model: None,
                background: true,
                runs: 0,
            },
            at: crate::time::Timestamp::now(),
        };
        let switched = Event::AgentSwitched {
            from: "claude".into(),
            to: "codex".into(),
            reason: "user".into(),
        };
        let root_done = || {
            [
                user_prompt("go"),
                Event::ThinkingStarted,
                stopped("prompt_complete"),
            ]
        };
        // (events after the root's terminal, holds live work)
        let cases = [
            (vec![child("a", SubagentState::Running)], true),
            (vec![child("a", SubagentState::RequiresAction)], true),
            (vec![child("a", SubagentState::Unknown)], false),
            (
                vec![
                    child("a", SubagentState::Running),
                    child("a", SubagentState::Idle),
                ],
                false,
            ),
            (
                vec![
                    child("a", SubagentState::Running),
                    child("b", SubagentState::Idle),
                ],
                true,
            ),
            (vec![child("a", SubagentState::Running), switched], false),
        ];
        for (i, (events, holds)) in cases.into_iter().enumerate() {
            let id = format!("c-{i}");
            record_projection(&store, &id, 1, root_done().into_iter().chain(events));
            assert_eq!(
                store.holds_live_work(&id).unwrap(),
                ControlKnowledge::Known(holds),
                "case {i}"
            );
        }
    }

    /// An agent-initiated turn (a Monitor or background finish waking the
    /// agent) opens with no `UserPromptSent`, so only its own events show it.
    #[tokio::test]
    #[serial_test::serial]
    async fn an_agent_initiated_turn_without_a_stop_is_open_work() {
        use crate::acp::supervisor::test_support::managed::with_managed;
        for (work, closed, open) in [
            ("none", false, false),
            ("tool", false, true),
            ("thought", false, true),
            ("tool", true, false),
            ("unadmitted", false, false),
        ] {
            with_managed(|scope| Box::pin(async move {
                let mut live = crate::server::test_support::managed_control_session(
                    scope, "agent-initiated-work", "claude", None,
                ).await;
                let id = live.id().to_owned();
                let store = live.state.acp_event_store.clone();
                let prompt = live.start_prompt("go").await;
                live.finish_prompt(&prompt, None).await;
                let run = matches!(work, "tool" | "thought").then(uuid::Uuid::new_v4);
                if let Some(run) = run {
                    live.native.send(serde_json::json!({
                        "jsonrpc":"2.0","method":"_omp/autonomous_turn/start",
                        "params":{"version":1,"sessionId":live.sid,"runId":run,"source":"irc"}
                    })).await;
                }
                if work == "tool" {
                    live.native.send(serde_json::json!({
                        "jsonrpc":"2.0","method":"session/update","params":{"sessionId":live.sid,
                            "update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"woken"}}}
                    })).await;
                }
                let update = match work {
                    "tool" => Some(serde_json::json!({"sessionUpdate":"tool_call","toolCallId":"t",
                        "title":"Bash","kind":"execute","status":"in_progress","rawInput":{}})),
                    "thought" => Some(serde_json::json!({"sessionUpdate":"agent_thought_chunk",
                        "content":{"type":"text","text":"hmm"}})),
                    "unadmitted" => Some(serde_json::json!({"sessionUpdate":"agent_message_chunk",
                        "content":{"type":"text","text":"unadmitted operation"}})),
                    _ => None,
                };
                if let Some(update) = update {
                    live.native.send(serde_json::json!({
                        "jsonrpc":"2.0","method":"session/update","params":{"sessionId":live.sid,"update":update}
                    })).await;
                    tokio::time::timeout(std::time::Duration::from_secs(10), async {
                        loop {
                            let event = live.client.next_event().await.expect("actual original publication");
                            if match event {
                                Event::ToolCallStarted { tool_call } => work == "tool" && tool_call.id == "t",
                                Event::AgentThoughtChunk { text, .. } => work == "thought" && text == "hmm",
                                Event::AgentMessageChunk { text, .. } => work == "unadmitted" && text == "unadmitted operation",
                                _ => false,
                            } { break; }
                        }
                    }).await.expect("the actual body closes the observation interval");
                }
                if closed {
                    live.native.send(serde_json::json!({
                        "jsonrpc":"2.0","method":"_omp/autonomous_turn/end",
                        "params":{"version":1,"sessionId":live.sid,"runId":run.unwrap(),"source":"irc","stopReason":"end_turn"}
                    })).await;
                    tokio::time::timeout(std::time::Duration::from_secs(10), async {
                        while !matches!(live.client.next_event().await.expect("own terminal"),
                            Event::Stopped { reason } if reason == "agent_idle") {}
                    }).await.unwrap();
                }
                assert_eq!(store.has_in_flight_main_turn(&id).unwrap(), ControlKnowledge::Known(false), "{work}, closed={closed}");
                assert_eq!(store.holds_live_work(&id).unwrap(), ControlKnowledge::Known(open), "{work}, closed={closed}");
                if let Some(run) = run.filter(|_| !closed) {
                    live.native.send(serde_json::json!({
                        "jsonrpc":"2.0","method":"_omp/autonomous_turn/end",
                        "params":{"version":1,"sessionId":live.sid,"runId":run,"source":"irc","stopReason":"end_turn"}
                    })).await;
                    tokio::time::timeout(std::time::Duration::from_secs(10), async {
                        while !matches!(live.client.next_event().await.expect("cleanup own terminal"),
                            Event::Stopped { reason } if reason == "agent_idle") {}
                    }).await.unwrap();
                }
            })).await;
        }
        let (_tmp, store) = open_store(1000);
        record_projection(
            &store,
            "unmatched-operation",
            1,
            [
                user_prompt("go"),
                stopped("prompt_complete"),
                agent_chunk("unadmitted operation"),
            ],
        );
        assert_eq!(
            store
                .has_in_flight_main_turn("unmatched-operation")
                .unwrap(),
            ControlKnowledge::Known(false)
        );
        assert_eq!(
            store.holds_live_work("unmatched-operation").unwrap(),
            ControlKnowledge::Known(false)
        );
    }

    /// A failed query is an error, never "no live work"; every aggregate
    /// preserves the failure and recovers after the store becomes readable.
    #[test]
    fn a_failed_query_is_unknown_not_idle() {
        let (_tmp, store) = open_store(1000);
        record_projection(
            &store,
            "s",
            1,
            vec![user_prompt("go"), stopped("prompt_complete")],
        );
        assert_eq!(
            store.holds_live_work("s").unwrap(),
            ControlKnowledge::Known(false)
        );

        store.test_set_unavailable(true);
        assert!(store.has_in_flight_main_turn("s").is_err());
        assert!(store.has_open_agent_turn("s").is_err());
        assert!(store.has_open_turn("s").is_err());
        assert!(store.holds_live_work("s").is_err());
        assert!(store.has_in_flight_turn("s").is_err());

        store.test_set_unavailable(false);
        assert_eq!(
            store.holds_live_work("s").unwrap(),
            ControlKnowledge::Known(false)
        );
    }

    /// An unhandled finish whose follow-up turn never came does not hold the
    /// runner forever.
    #[test]
    fn an_old_unhandled_finish_no_longer_holds_live_work() {
        let (_tmp, store) = open_store(1000);
        let old =
            crate::time::Timestamp::now().as_millis() - (BACKGROUND_AGENT_STALE_AFTER_MS + 60_000);
        let finished = Event::BackgroundTaskFinished {
            task_id: "t".into(),
            ended_at: None,
        };
        record_projection(
            &store,
            "s",
            1,
            [user_prompt("go"), stopped("prompt_complete"), finished],
        );
        store
            .conn()
            .execute(
                "UPDATE acp_events SET created_at=?2 WHERE session_id=?1",
                params!["s", old],
            )
            .unwrap();
        assert_eq!(
            store.holds_live_work("s").unwrap(),
            ControlKnowledge::Known(false)
        );
    }

    #[test]
    fn snapshot_queries_read_the_latest_event() {
        use crate::acp::state::{PlanStep, PlanStepStatus};
        let (_tmp, store) = open_store(1000);
        let capabilities = |image| Event::PromptCapabilities {
            image,
            audio: false,
            embedded_context: image,
            load_session: None,
            steering: false,
        };
        let plan = |status| Event::PlanUpdated {
            plan: Plan {
                plan_id: "p".into(),
                version: 1,
                steps: vec![PlanStep {
                    id: "s-1".into(),
                    title: "Step one".into(),
                    detail: None,
                    status,
                }],
            },
        };
        assert_eq!(store.latest_prompt_capabilities("s-1"), None);
        assert!(store.latest_plan("s-1").is_none());
        record_from(
            &store,
            "s-1",
            1,
            [
                capabilities(true),
                plan(PlanStepStatus::Pending),
                Event::ThinkingStarted,
                capabilities(false),
                plan(PlanStepStatus::Done),
            ],
        );
        assert_eq!(
            store.latest_prompt_capabilities("s-1"),
            Some((false, false, false))
        );
        assert!(matches!(
            store.latest_plan("s-1").expect("plan").steps[0].status,
            PlanStepStatus::Done
        ));

        record_from(
            &store,
            "s-lifecycle",
            1,
            [
                Event::AcpSessionAssigned {
                    acp_session_id: "acp-1".into(),
                    connection_ready: false,
                    native_subject: None,
                },
                Event::ModesAvailable {
                    current_mode_id: "default".into(),
                    modes: vec![],
                },
            ],
        );
        for (id, seq, created_at) in [
            ("s-a", 1, 100),
            ("s-a", 2, 900),
            ("s-a", 3, 400),
            ("s-b", 1, 7000),
        ] {
            store
                .record_at(id, seq, &Event::ThinkingStarted, created_at)
                .unwrap();
        }
        let ids = |ids: &[&str]| ids.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert!(store.last_event_at_for_sessions(&[]).is_empty());
        let map =
            store.last_event_at_for_sessions(&ids(&["s-lifecycle", "s-a", "s-b", "s-missing"]));
        assert_eq!(
            map,
            HashMap::from([("s-a".to_string(), 900), ("s-b".to_string(), 7000)])
        );
    }

    #[test]
    fn unresolved_requests_and_background_launches_per_session() {
        use crate::acp::elicitations::{Elicitation, ElicitationOutcome};
        let (_tmp, store) = open_store(1000);
        let approval = |nonce: &str| Event::ApprovalRequested {
            approval: Approval {
                nonce: Nonce(nonce.into()),
                tool_call: tool_call("tc-1"),
                destructive: false,
                options: Vec::new(),
                choice: false,
                plan: None,
                requested_at: crate::time::Timestamp::now(),
                resolved: None,
            },
        };
        let elicitation = |nonce: &Nonce| Event::ElicitationRequested {
            elicitation: Elicitation {
                nonce: nonce.clone(),
                message: "Pick".into(),
                title: None,
                description: None,
                tool_call_id: None,
                questions: Vec::new(),
                requested_at: crate::time::Timestamp::now(),
                resolved: None,
            },
        };
        let (e_a, e_b) = (Nonce::new(), Nonce::new());
        record_from(
            &store,
            "s-1",
            1,
            [
                approval("aaaa"),
                Event::ApprovalResolved {
                    nonce: Nonce("aaaa".into()),
                    decision: ApprovalDecision::Allow,
                },
                approval("bbbb"),
                approval("cccc"),
                elicitation(&e_a),
                elicitation(&e_b),
                Event::ElicitationResolved {
                    nonce: e_a,
                    outcome: ElicitationOutcome::Accepted,
                    answers: Vec::new(),
                },
            ],
        );
        assert_eq!(
            store.unresolved_approval_nonces("s-1"),
            [Nonce("bbbb".into()), Nonce("cccc".into())]
        );
        let pending: Vec<String> = store
            .pending_approval_requests("s-1")
            .into_iter()
            .map(|a| a.nonce.0)
            .collect();
        assert_eq!(pending, ["bbbb", "cccc"]);
        assert_eq!(store.unresolved_elicitation_nonces("s-1"), [e_b]);
        assert!(store.unresolved_approval_nonces("s-2").is_empty());
        assert!(store.unresolved_elicitation_nonces("s-2").is_empty());

        // A failed request leaves the actual responders actionable. Native loss
        // retires those controls without fabricating any human decision, and a
        // later ready announcement must not resurrect the old nonces.
        record_from(
            &store,
            "s-1",
            9,
            [
                Event::PromptRuntimeError {
                    message: "ordinary request failure".into(),
                    evidence: None,
                    native_subject: None,
                },
                Event::Stopped {
                    reason: "prompt_failed".into(),
                },
            ],
        );
        assert_eq!(store.pending_approval_requests("s-1").len(), 2);
        assert_eq!(store.unresolved_elicitation_nonces("s-1").len(), 1);
        record_from(
            &store,
            "s-1",
            11,
            [
                Event::AcpConnectionUnavailable {
                    source: crate::acp::state::AcpConnectionSource::Native,
                    cause: crate::acp::state::AcpConnectionCause::StdoutEof,
                    phase: crate::acp::state::AcpConnectionPhase::Established,
                    native_subject: None,
                },
                Event::AcpSessionAssigned {
                    acp_session_id: "retained".into(),
                    connection_ready: true,
                    native_subject: None,
                },
            ],
        );
        assert!(store.unresolved_approval_nonces("s-1").is_empty());
        assert!(store.pending_approval_requests("s-1").is_empty());
        assert!(store.unresolved_elicitation_nonces("s-1").is_empty());
        let after_loss = Nonce::new();
        record_from(
            &store,
            "s-1",
            13,
            [approval("new-owned-control"), elicitation(&after_loss)],
        );
        assert_eq!(
            store.unresolved_approval_nonces("s-1"),
            [Nonce("new-owned-control".into())]
        );
        assert_eq!(store.pending_approval_requests("s-1").len(), 1);
        assert_eq!(store.unresolved_elicitation_nonces("s-1"), [after_loss]);

        // A refused management attachment does not end the retained native
        // turn or resolve its callbacks. Only observed channel loss does.
        let management_elicitation = Nonce::new();
        record_projection(
            &store,
            "management",
            1,
            [
                user_prompt("retained work"),
                Event::ThinkingStarted,
                approval("retained-approval"),
                elicitation(&management_elicitation),
                Event::MonitorArmed {
                    description: Some("retained monitor".into()),
                },
                Event::ToolCallStarted {
                    tool_call: tool_call("retained-tool"),
                },
                Event::AcpConnectionUnavailable {
                    source: crate::acp::state::AcpConnectionSource::RunnerRecord,
                    cause: crate::acp::state::AcpConnectionCause::ControlProtocolUnknown,
                    phase: crate::acp::state::AcpConnectionPhase::Establishing,
                    native_subject: None,
                },
            ],
        );
        assert_eq!(
            store.has_in_flight_turn("management").unwrap(),
            ControlKnowledge::Known(true)
        );
        assert!(store.has_open_tool_call_in_epoch("management"));
        assert_eq!(store.pending_approval_requests("management").len(), 1);
        assert_eq!(
            store.unresolved_elicitation_nonces("management"),
            [management_elicitation]
        );
        assert!(store.latest_active_monitor("management").is_some());
        let mut loss = Event::AcpConnectionUnavailable {
            source: crate::acp::state::AcpConnectionSource::Native,
            cause: crate::acp::state::AcpConnectionCause::StdoutEof,
            phase: crate::acp::state::AcpConnectionPhase::Established,
            native_subject: None,
        };
        let subject = crate::acp::state::NativeEventSubject::channel(
            serde_json::from_value(serde_json::to_value([31u8; 32]).unwrap()).unwrap(),
            None,
            &loss,
        );
        if let Event::AcpConnectionUnavailable { native_subject, .. } = &mut loss {
            *native_subject = Some(subject);
        }
        store
            .record(
                "management",
                8,
                &loss,
                &crate::acp::event_sender::EventMetadata {
                    scope: crate::acp::event_sender::EventScope::SessionCurrent,
                    availability_denial: true,
                    ..crate::acp::event_sender::EventMetadata::history()
                },
            )
            .unwrap();
        assert_eq!(
            store.has_in_flight_turn("management").unwrap(),
            ControlKnowledge::Known(false)
        );
        assert!(!store.has_open_tool_call_in_epoch("management"));
        assert!(store.pending_approval_requests("management").is_empty());
        assert!(store.unresolved_elicitation_nonces("management").is_empty());
        assert!(store.latest_active_monitor("management").is_none());

        // A background launch with no matching completion is orphaned.
        let (_tmp, store) = open_store(1000);
        record_from(
            &store,
            "s-1",
            1,
            [
                bg_launched("bg-a", "/tmp/a.jsonl"),
                Event::BackgroundAgentCompleted {
                    agent_id: "bg-a".into(),
                    status: BackgroundAgentStatus::Completed,
                    tools: Vec::new(),
                    result: None,
                    warning: None,
                    ended_at: crate::time::Timestamp::now(),
                },
                bg_launched("bg-b", "/tmp/b.jsonl"),
                bg_launched("bg-c", ""),
            ],
        );

        assert_eq!(
            store.unresolved_background_agent_ids("s-1"),
            ["bg-b".to_string(), "bg-c".to_string()]
        );
        let launches: Vec<(String, String)> = store
            .unresolved_background_agent_launches("s-1")
            .unwrap()
            .into_iter()
            .map(|l| (l.agent_id, l.output_file))
            .collect();
        assert_eq!(
            launches,
            [
                ("bg-b".to_string(), "/tmp/b.jsonl".to_string()),
                ("bg-c".to_string(), String::new()),
            ]
        );
        assert!(store.unresolved_background_agent_ids("s-2").is_empty());
        assert!(store
            .unresolved_background_agent_launches("s-2")
            .unwrap()
            .is_empty());

        // A completion row whose `agent_id` is not extractable must not blank
        // the whole scan: SQLite's `NOT IN` goes `NULL` once the subquery
        // yields one `NULL`. The store never writes such a row.
        store
            .record(
                "s-3",
                1,
                &bg_launched("bg-real-orphan", ""),
                &crate::acp::event_sender::EventMetadata::history(),
            )
            .unwrap();
        store
            .conn()
            .execute(
                "INSERT INTO acp_events (session_id, seq, event_json, created_at, discriminant)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                params![
                    "s-3",
                    2_i64,
                    "{\"BackgroundAgentCompleted\":{}}",
                    0_i64,
                    "BackgroundAgentCompleted",
                ],
            )
            .unwrap();

        assert_eq!(
            store.unresolved_background_agent_ids("s-3"),
            ["bg-real-orphan".to_string()],
            "an unextractable Completed row must not blank the whole scan"
        );
    }

    /// Retention must not erase the start of a long-running task (the open
    /// work could never be finished at teardown, and a cold fold would lose
    /// it) nor keep a start whose finish was pruned (it would read open).
    #[test]
    fn background_task_lifecycle_survives_the_retention_prune() {
        use crate::acp::state::{AcpSessionId, AcpState, AgentName};

        let (_tmp, store) = open_store(3);
        let task = |id: &str| Event::BackgroundTaskStarted {
            task_id: id.into(),
            tool_call_id: None,
            description: "sleep".into(),
            workflow: None,
            started_at: None,
        };
        let chunk = || Event::AgentMessageChunk {
            text: "x".into(),
            message_id: None,
        };
        let mut seq = 0;
        let mut record = |event: Event| {
            seq += 1;
            store
                .record(
                    "s-1",
                    seq,
                    &event,
                    &crate::acp::event_sender::EventMetadata::history(),
                )
                .unwrap();
        };
        record(task("open"));
        record(task("done"));
        record(Event::BackgroundTaskFinished {
            task_id: "done".into(),
            ended_at: None,
        });
        // An id reused after its finish: the second start is open and must
        // outlive the sweep that drops the completed first pair.
        record(task("reused"));
        record(Event::BackgroundTaskFinished {
            task_id: "reused".into(),
            ended_at: None,
        });
        record(task("reused"));
        record(bg_launched("agent-open", ""));
        record(bg_launched("agent-done", ""));
        record(Event::BackgroundAgentCompleted {
            agent_id: "agent-done".into(),
            status: BackgroundAgentStatus::Completed,
            tools: Vec::new(),
            result: None,
            warning: None,
            ended_at: crate::time::Timestamp::now(),
        });
        // Enough events for the periodic sweep of completed work to run.
        for _ in 0..150 {
            record(chunk());
        }

        assert_eq!(
            store.unresolved_background_task_ids("s-1"),
            ["open".to_string(), "reused".to_string()]
        );
        assert_eq!(
            store.unresolved_background_agent_ids("s-1"),
            ["agent-open".to_string()]
        );
        let lifecycle_rows: i64 = store
            .conn()
            .query_row(
                "SELECT count(*) FROM acp_events WHERE session_id = 's-1' AND discriminant IN
                 ('BackgroundTaskStarted', 'BackgroundTaskFinished',
                  'BackgroundAgentLaunched', 'BackgroundAgentCompleted')",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            lifecycle_rows, 7,
            "completed agents age out; recent task pairs stay for the shell panel"
        );
        let mut state = AcpState::new(AcpSessionId("s-1".into()), AgentName("claude".into()), None);
        for stored in store.replay_from("s-1", 0).unwrap() {
            state
                .apply_scoped_event(stored.seq, &stored.event, &stored.metadata)
                .unwrap();
        }
        assert_eq!(
            state
                .background_tasks
                .iter()
                .map(|task| task.task_id.as_str())
                .collect::<Vec<_>>(),
            ["open", "reused"]
        );
    }

    /// A finished shell stays replayable until it falls out of the newest
    /// `MAX_FINISHED_BACKGROUND_SHELLS`, even past the retention cap.
    #[test]
    fn finished_shells_beyond_the_keep_count_age_out_of_the_log() {
        use crate::acp::state::MAX_FINISHED_BACKGROUND_SHELLS;

        let (_tmp, store) = open_store(3);
        let mut seq = 0;
        let mut record = |event: Event| {
            seq += 1;
            store
                .record(
                    "s-1",
                    seq,
                    &event,
                    &crate::acp::event_sender::EventMetadata::history(),
                )
                .unwrap();
        };
        let total = MAX_FINISHED_BACKGROUND_SHELLS + 5;
        for i in 0..total {
            record(Event::BackgroundTaskStarted {
                task_id: format!("t{i}"),
                tool_call_id: None,
                description: "sleep".into(),
                workflow: None,
                started_at: None,
            });
            record(Event::BackgroundTaskFinished {
                task_id: format!("t{i}"),
                ended_at: None,
            });
        }
        // Open shells newer than every finished one must not use up kept slots.
        for i in 0..3 {
            record(Event::BackgroundTaskStarted {
                task_id: format!("open{i}"),
                tool_call_id: None,
                description: "sleep".into(),
                workflow: None,
                started_at: None,
            });
        }
        for _ in 0..150 {
            record(Event::AgentMessageChunk {
                text: "x".into(),
                message_id: None,
            });
        }

        let kept: Vec<String> = store
            .replay_from("s-1", 0)
            .expect("read retained events")
            .into_iter()
            .filter_map(
                |stored| match std::sync::Arc::unwrap_or_clone(stored.event) {
                    Event::BackgroundTaskStarted { task_id, .. } => Some(task_id),
                    _ => None,
                },
            )
            .collect();
        let expected: Vec<String> = (total - MAX_FINISHED_BACKGROUND_SHELLS..total)
            .map(|i| format!("t{i}"))
            .chain((0..3).map(|i| format!("open{i}")))
            .collect();
        assert_eq!(kept, expected);
        assert_eq!(store.unresolved_background_task_ids("s-1").len(), 3);
    }

    /// Workflows share the task events but must not use up the shells' kept
    /// slots, on replay or in the folded state.
    #[test]
    fn finished_workflows_do_not_displace_a_finished_shell() {
        use crate::acp::state::{
            AcpSessionId, AcpState, AgentName, WorkflowLaunch, MAX_FINISHED_BACKGROUND_SHELLS,
        };

        let (_tmp, store) = open_store(3);
        let mut seq = 0;
        let mut record = |event: Event| {
            seq += 1;
            store
                .record(
                    "s-1",
                    seq,
                    &event,
                    &crate::acp::event_sender::EventMetadata::history(),
                )
                .unwrap();
        };
        let finish = |id: &str| Event::BackgroundTaskFinished {
            task_id: id.into(),
            ended_at: None,
        };
        record(Event::BackgroundTaskStarted {
            task_id: "shell".into(),
            tool_call_id: None,
            description: "make build".into(),
            workflow: None,
            started_at: None,
        });
        record(finish("shell"));
        for i in 0..MAX_FINISHED_BACKGROUND_SHELLS + 5 {
            record(Event::BackgroundTaskStarted {
                task_id: format!("wf{i}"),
                tool_call_id: Some(format!("call-{i}")),
                description: "drafts".into(),
                workflow: Some(WorkflowLaunch {
                    run_id: format!("wf_{i}"),
                    name: "drafts".into(),
                    summary: None,
                    transcript_dir: String::new(),
                }),
                started_at: None,
            });
            record(finish(&format!("wf{i}")));
        }
        for _ in 0..150 {
            record(Event::AgentMessageChunk {
                text: "x".into(),
                message_id: None,
            });
        }

        let mut state = AcpState::new(AcpSessionId("s-1".into()), AgentName("claude".into()), None);
        for stored in store.replay_from("s-1", 0).unwrap() {
            state
                .apply_scoped_event(stored.seq, &stored.event, &stored.metadata)
                .unwrap();
        }
        assert_eq!(state.background_shells.len(), 1);
        assert_eq!(state.background_shells[0].command, "make build");
        assert!(state.background_shells[0].finished);
        assert_eq!(state.workflow_runs.len(), MAX_FINISHED_BACKGROUND_SHELLS);
    }

    /// Nothing guarantees an adapter task id stays unique for a session, so a
    /// finish only closes the starts before it.
    #[test]
    fn a_reused_task_id_is_open_after_its_earlier_finish() {
        let (_tmp, store) = open_store(1000);
        let started = |tool_call_id: &str| Event::BackgroundTaskStarted {
            task_id: "t".into(),
            tool_call_id: Some(tool_call_id.into()),
            description: "sleep".into(),
            workflow: None,
            started_at: None,
        };
        let finished = || Event::BackgroundTaskFinished {
            task_id: "t".into(),
            ended_at: None,
        };
        store
            .record(
                "s-1",
                1,
                &started("toolu_old"),
                &crate::acp::event_sender::EventMetadata::history(),
            )
            .unwrap();
        assert_eq!(
            store.has_in_flight_turn("s-1").unwrap(),
            ControlKnowledge::Known(true)
        );
        store
            .record(
                "s-1",
                2,
                &finished(),
                &crate::acp::event_sender::EventMetadata::history(),
            )
            .unwrap();
        assert!(store.unresolved_background_task_ids("s-1").is_empty());
        assert_eq!(
            store.has_in_flight_turn("s-1").unwrap(),
            ControlKnowledge::Known(false)
        );
        store
            .record(
                "s-1",
                3,
                &started("toolu_reused"),
                &crate::acp::event_sender::EventMetadata::history(),
            )
            .unwrap();
        store
            .record(
                "s-1",
                4,
                &started("toolu_latest"),
                &crate::acp::event_sender::EventMetadata::history(),
            )
            .unwrap();
        store
            .record(
                "s-2",
                1,
                &finished(),
                &crate::acp::event_sender::EventMetadata::history(),
            )
            .unwrap();
        assert_eq!(
            store.unresolved_background_task_launches("s-1").unwrap(),
            [BackgroundTaskLaunch {
                task_id: "t".into(),
                tool_call_id: Some("toolu_latest".into()),
                workflow: None,
                workflow_agents: WorkflowAgents::default(),
                witness: None,
            }]
        );
        assert_eq!(
            store.has_in_flight_turn("s-1").unwrap(),
            ControlKnowledge::Known(true)
        );
        assert_eq!(
            store.unresolved_background_task_ids("s-1"),
            ["t".to_string()]
        );
    }

    #[test]
    fn unresolved_background_task_ids_skip_finished_tasks() {
        let (_tmp, store) = open_store(1000);
        let started = |id: &str| Event::BackgroundTaskStarted {
            task_id: id.into(),
            tool_call_id: None,
            description: "sleep".into(),
            workflow: None,
            started_at: None,
        };
        store
            .record(
                "s-1",
                1,
                &started("t-1"),
                &crate::acp::event_sender::EventMetadata::history(),
            )
            .unwrap();
        store
            .record(
                "s-1",
                2,
                &started("t-2"),
                &crate::acp::event_sender::EventMetadata::history(),
            )
            .unwrap();
        store
            .record(
                "s-1",
                3,
                &Event::BackgroundTaskFinished {
                    task_id: "t-1".into(),
                    ended_at: None,
                },
                &crate::acp::event_sender::EventMetadata::history(),
            )
            .unwrap();
        assert_eq!(
            store.unresolved_background_task_ids("s-1"),
            ["t-2".to_string()]
        );
        assert!(store.unresolved_background_task_ids("s-2").is_empty());
    }

    /// A workflow run's resume carries its launch and what the log recorded
    /// for its agents, and those agents stay out of the generic agent resume.
    #[test]
    fn an_open_workflow_resumes_with_its_recorded_agents() {
        let (_tmp, store) = open_store(1000);
        let launch = WorkflowLaunch {
            run_id: "wf_1".into(),
            name: "drafts".into(),
            summary: Some("draft them".into()),
            transcript_dir: "/home/u/.claude/wf_1".into(),
        };
        let workflow_agent = |agent_id: &str, run_id: &str| {
            let mut event = bg_launched(
                agent_id,
                &format!("/home/u/.claude/wf_1/agent-{agent_id}.jsonl"),
            );
            if let Event::BackgroundAgentLaunched { workflow, .. } = &mut event {
                *workflow = Some(WorkflowRef {
                    run_id: run_id.into(),
                    name: "drafts".into(),
                    phase: "Draft".into(),
                });
            }
            event
        };
        record_from(
            &store,
            "s-1",
            1,
            [
                Event::BackgroundTaskStarted {
                    task_id: "wfx".into(),
                    tool_call_id: Some("toolu_wf".into()),
                    description: "drafts".into(),
                    workflow: Some(launch.clone()),
                    started_at: None,
                },
                workflow_agent("a1", "wf_1"),
                workflow_agent("a2", "wf_1"),
                workflow_agent("other-run", "wf_2"),
                bg_launched("plain", "/tmp/plain.jsonl"),
                Event::BackgroundAgentCompleted {
                    agent_id: "a1".into(),
                    status: BackgroundAgentStatus::Completed,
                    tools: Vec::new(),
                    result: None,
                    warning: None,
                    ended_at: crate::time::Timestamp::now(),
                },
            ],
        );
        assert_eq!(
            store.unresolved_background_task_launches("s-1").unwrap(),
            [BackgroundTaskLaunch {
                task_id: "wfx".into(),
                tool_call_id: Some("toolu_wf".into()),
                workflow: Some(launch),
                workflow_agents: WorkflowAgents {
                    launched: ["a1".to_string(), "a2".to_string()].into(),
                    completed: ["a1".to_string()].into(),
                    resources: Default::default(),
                },
                witness: None,
            }]
        );
        let generic: Vec<String> = store
            .unresolved_background_agent_launches("s-1")
            .unwrap()
            .into_iter()
            .map(|l| l.agent_id)
            .collect();
        assert_eq!(generic, ["plain"]);
        // Teardown still sees every open agent, workflow ones included.
        assert_eq!(
            store.unresolved_background_agent_ids("s-1"),
            ["a2", "other-run", "plain"].map(String::from)
        );
        record_from(
            &store,
            "s-1",
            7,
            [Event::BackgroundTaskFinished {
                task_id: "wfx".into(),
                ended_at: Some(crate::time::Timestamp::now()),
            }],
        );
        assert!(store
            .unresolved_background_task_launches("s-1")
            .unwrap()
            .is_empty());
        let tracking: Vec<_> = store
            .unresolved_background_agent_tracking_launches("s-1")
            .unwrap()
            .into_iter()
            .map(|launch| launch.agent_id)
            .collect();
        assert_eq!(tracking, ["a2", "other-run", "plain"]);
    }

    /// Probes find event types through the indexed `discriminant` column, never by
    /// parsing every row. Each assertion fails if its query parses the malformed row.
    #[test]
    fn probes_do_not_parse_unrelated_rows() {
        use crate::acp::elicitations::Elicitation;
        let (_tmp, store) = open_store(0);
        let nonce = Nonce::new();
        let elicitation = Event::ElicitationRequested {
            elicitation: Elicitation {
                nonce: nonce.clone(),
                message: "Pick".into(),
                title: None,
                description: None,
                tool_call_id: None,
                questions: Vec::new(),
                requested_at: crate::time::Timestamp::now(),
                resolved: None,
            },
        };
        let tool_done = |id: &str| Event::ToolCallCompleted {
            tool_call_id: id.into(),
            is_error: false,
            content: String::new(),
            output: Vec::new(),
            completed_at: crate::time::Timestamp::now(),
            async_subagent: false,
        };
        record_projection(
            &store,
            "s-1",
            1,
            [
                user_prompt("first"),
                Event::ThinkingStarted,
                agent_chunk("first turn"),
                stopped("prompt_complete"),
                user_prompt("second"),
                Event::ThinkingStarted,
                bg_launched("bg-1", ""),
                elicitation,
                Event::ToolCallStarted {
                    tool_call: tool_call("tc-open"),
                },
                Event::ToolCallStarted {
                    tool_call: tool_call("tc-done"),
                },
                tool_done("tc-done"),
            ],
        );
        // An unprompted turn the rate limit interrupted.
        record_projection(
            &store,
            "s-2",
            1,
            [
                Event::SessionCleared,
                Event::ThinkingStarted,
                agent_chunk("work"),
            ],
        );
        let mut outcome = store
            .replay_from("s-2", 0)
            .unwrap()
            .last()
            .unwrap()
            .metadata
            .clone();
        outcome.operation.as_mut().unwrap().outcome =
            Some(crate::acp::protocol::AcpOriginalRequestResult::Failed);
        store
            .record("s-2", 4, &stopped("rate_limited"), &outcome)
            .unwrap();
        {
            let conn = store.hold_writer();
            for (id, seq) in [("s-1", 12), ("s-2", 5)] {
                events::insert_event(&conn, &store.schema, id, seq, "{\"Opaque\":{", 0).unwrap();
            }
        }

        assert_eq!(store.unresolved_elicitation_nonces("s-1"), [nonce]);
        assert_eq!(
            store.has_in_flight_turn("s-1").unwrap(),
            ControlKnowledge::Known(true)
        );
        assert_eq!(
            store.rate_limit_interrupted_agent_turn("s-2").unwrap(),
            ControlKnowledge::Known(true)
        );
        // Fails closed (true) if a tool filter parses the malformed row, still in this epoch.
        assert!(store.has_open_tool_call_in_epoch("s-1"));
        store
            .record(
                "s-1",
                13,
                &tool_done("tc-open"),
                &super::EventMetadata::history(),
            )
            .unwrap();
        assert!(!store.has_open_tool_call_in_epoch("s-1"));
        // A closed prompt leaves the background agent as the open work.
        let terminal = stopped("prompt_complete");
        store
            .record("s-1", 14, &terminal, &super::EventMetadata::host(&terminal))
            .unwrap();
        assert_eq!(
            store.has_in_flight_turn("s-1").unwrap(),
            ControlKnowledge::Known(true)
        );
    }

    fn bg_launched(agent_id: &str, output_file: &str) -> Event {
        Event::BackgroundAgentLaunched {
            agent_id: agent_id.into(),
            tool_call_id: format!("tc-{agent_id}"),
            description: "map backend".into(),
            prompt: "do it".into(),
            model: "claude-opus-4-8".into(),
            output_file: output_file.into(),
            started_at: crate::time::Timestamp::now(),
            workflow: None,
        }
    }
}
