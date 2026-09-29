use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant};

use super::{reconcile_acp_workers, ReapCadence};
use crate::acp::Event;
use crate::server::AppState;
use crate::session::test_support::AppDirGuard;
use crate::session::{Instance, View};

/// Barrier held between a continuation read and its installation. Process
/// global, so every test that reaches the installer must be
/// `#[serial_test::serial]`; the handle that arms it disarms it on drop.
static INSTALL_BARRIER: std::sync::Mutex<
    Option<tokio::sync::mpsc::UnboundedSender<(String, tokio::sync::oneshot::Sender<()>)>>,
> = std::sync::Mutex::new(None);

/// Holds the barrier armed. Each producer read reports its session id to
/// [`Self::reads`] and waits for the returned ack before it installs.
pub(crate) struct InstallBarrier {
    pub(crate) reads:
        tokio::sync::mpsc::UnboundedReceiver<(String, tokio::sync::oneshot::Sender<()>)>,
}

impl Drop for InstallBarrier {
    fn drop(&mut self) {
        *INSTALL_BARRIER
            .lock()
            .expect("INSTALL_BARRIER mutex poisoned") = None;
    }
}

/// Arm the barrier for one test.
pub(crate) fn arm_install_barrier() -> InstallBarrier {
    let (tx, reads) = tokio::sync::mpsc::unbounded_channel();
    *INSTALL_BARRIER
        .lock()
        .expect("INSTALL_BARRIER mutex poisoned") = Some(tx);
    InstallBarrier { reads }
}

/// Report a producer read to an armed barrier and block its installation on
/// the ack. The deadline keeps a producer from parking forever when no test is
/// left to release it.
pub(super) async fn await_install_barrier(id: &str) {
    let sender = INSTALL_BARRIER
        .lock()
        .expect("INSTALL_BARRIER mutex poisoned")
        .clone();
    let Some(tx) = sender else { return };
    let (ack, released) = tokio::sync::oneshot::channel();
    if tx.send((id.to_string(), ack)).is_ok() {
        let _ = tokio::time::timeout(Duration::from_secs(30), released).await;
    }
}

/// A structured instance with a bogus agent, so a spawn fails fast with
/// `UnknownAgent` before any process work.
pub(super) fn structured_instance(id: &str, project_path: &str) -> Instance {
    let mut inst = Instance::new(id, project_path);
    inst.id = id.to_string();
    inst.view = View::Structured;
    inst.agent_name = Some("aoe-no-such-agent-1027".to_string());
    inst
}

/// App state with one structured session, under an isolated app dir so the
/// worker registry cannot see real entries.
pub(super) fn test_state(id: &str) -> (AppDirGuard, Arc<AppState>, tempfile::TempDir) {
    let home = crate::session::test_support::isolate_app_dir();
    let project = tempfile::TempDir::new().unwrap();
    let inst = structured_instance(id, &project.path().to_string_lossy());
    let state = crate::server::test_support::build_test_app_state(vec![inst]);
    (home, state, project)
}

pub(super) fn enable_auto_resume(enabled: bool) {
    let app_dir = crate::session::get_app_dir().expect("isolated app dir");
    std::fs::write(
        app_dir.join("config.toml"),
        format!("[acp]\nrate_limit_auto_resume = {enabled}\n"),
    )
    .expect("write config");
}

pub(super) fn queued_prompt() -> crate::daemon::QueuedPromptEntry {
    crate::daemon::QueuedPromptEntry {
        id: "q-1".to_string(),
        seq: 1,
        text: "queued while busy".to_string(),
        attachments: Vec::new(),
        created_at: chrono::Utc::now().to_rfc3339(),
        origin_device: None,
    }
}

/// The reconciler's per-process state across ticks.
#[derive(Default)]
pub(super) struct Tick {
    pub(super) attempted: HashSet<String>,
    pub(super) respawn_history: HashMap<String, Vec<Instant>>,
    pub(super) parked: HashSet<String>,
    pub(super) capacity_deferred: HashSet<String>,
}

impl Tick {
    /// Runs one tick with the cadence-gated passes sitting out.
    pub(super) async fn run(&mut self, state: &Arc<AppState>) {
        let now = Some(Instant::now());
        let mut cadence = ReapCadence {
            idle: now,
            rate_limit: now,
            terminal_repair: now,
        };
        reconcile_acp_workers(
            state,
            &mut self.attempted,
            &mut cadence,
            &mut self.respawn_history,
            &mut self.parked,
            &mut self.capacity_deferred,
        )
        .await;
    }
}

fn count_startup_errors(state: &AppState, id: &str, filter: impl Fn(&str) -> bool) -> usize {
    state
        .acp_event_store
        .replay_from(id, 0)
        .into_iter()
        .filter(|(_, e)| matches!(e, Event::AgentStartupError { message } if filter(message)))
        .count()
}

pub(super) fn startup_errors(state: &AppState, id: &str) -> usize {
    count_startup_errors(state, id, |_| true)
}

pub(super) fn capacity_startup_errors(state: &AppState, id: &str) -> usize {
    count_startup_errors(state, id, |m| m.contains("capacity full"))
}
