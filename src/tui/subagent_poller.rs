//! Background reader for the Claude subagents of terminal sessions.
//!
//! Resolving a session's Claude store loads its profile config and the scan
//! reads transcripts, so both stay off the render loop.

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use crate::session::subagents::{
    Subagent, SubagentFocus, SubagentScan, SubagentScanner, SubagentSource,
};
use crate::session::{Instance, Status};
use crate::tui::worker::Worker;

pub type SubagentSnapshot = HashMap<String, Vec<Subagent>>;

/// The watched sessions, and the subagent whose preview is open.
type Request = (Vec<Instance>, Option<SubagentFocus>);

/// How long a resolved Claude store is trusted. Resolution reads profile config
/// and environment the instance key does not capture, so it is redone now and
/// then rather than cached for the life of the TUI.
const STORE_TTL: Duration = Duration::from_secs(60);

pub struct SubagentPoller {
    worker: Worker<Request, SubagentScan>,
}

impl SubagentPoller {
    pub fn new() -> Self {
        let mut scanner = SubagentScanner::default();
        let mut stores: HashMap<String, (Instant, Option<PathBuf>)> = HashMap::new();
        Self {
            worker: Worker::spawn("aoe-subagent-poller", move |(instances, focus): Request| {
                stores.retain(|id, (resolved_at, _)| {
                    resolved_at.elapsed() < STORE_TTL && instances.iter().any(|i| i.id == *id)
                });
                let sources: Vec<SubagentSource> = instances
                    .iter()
                    .filter_map(|inst| {
                        let (_, store) = stores.entry(inst.id.clone()).or_insert_with(|| {
                            (
                                Instant::now(),
                                crate::session::conversation_carry::claude_store_root(inst),
                            )
                        });
                        let store = store.clone()?;
                        Some(SubagentSource {
                            instance_id: inst.id.clone(),
                            store,
                            session_id: inst.agent_session_id.clone()?,
                            parent_busy: matches!(inst.status, Status::Running | Status::Waiting),
                        })
                    })
                    .collect();
                scanner.scan(&sources, focus.as_ref())
            }),
        }
    }

    /// Whether `inst` can have subagents worth listing: a live terminal
    /// session with a captured conversation id.
    pub fn watches(inst: &Instance) -> bool {
        !inst.is_structured()
            && inst.agent_session_id.is_some()
            && !inst.is_archived()
            && !inst.is_trashed()
            && matches!(
                inst.status,
                Status::Running | Status::Waiting | Status::Idle | Status::Unknown
            )
    }

    pub fn request_refresh(&self, instances: Vec<Instance>, focus: Option<SubagentFocus>) {
        self.worker.request((instances, focus));
    }

    /// A poller whose first result is `scan`; requests are ignored.
    #[cfg(test)]
    pub fn seeded_for_test(scan: SubagentScan) -> Self {
        Self {
            worker: Worker::seeded_for_test("aoe-subagent-poller-test", scan),
        }
    }

    pub fn try_recv_updates(&self) -> Result<SubagentScan, std::sync::mpsc::TryRecvError> {
        self.worker.try_recv()
    }
}

impl Default for SubagentPoller {
    fn default() -> Self {
        Self::new()
    }
}
