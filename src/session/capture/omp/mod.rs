//! Oh My Pi (OMP) store layout and launch instrumentation.
//!
//! Store layout and launch identity live in tmux and are reloaded at launch so
//! a restarted process cannot attach to a superseded pane generation.
//!
//! Launch marker, exactly 4 lines: terminal id (tty leaf, `/` as `-`), launch id,
//! non-empty pending pre-launch session path, routing fingerprint (64 hex).
//! `wrap_omp_launch` (sh) and OMP itself must change together.

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

mod layout;
mod options;

use crate::session::capture::container_exec_command;

pub(crate) use layout::{
    omp_host_routing_environment, resolve_omp_store_layout_in_container_with_environment,
    resolve_omp_store_layout_with_environment,
};
pub(crate) use options::{reject_omp_secret_args, OmpCliCaptureOptions};

const COMMAND_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_DOTENV_BYTES: usize = 1024 * 1024;
const MAX_CONTAINER_PROBE_BYTES: usize = 16 * 1024;
pub(crate) const OMP_STORE_ENV_KEYS: [&str; 9] = [
    "HOME",
    "NODE_ENV",
    "OMP_PROFILE",
    "PI_PROFILE",
    "PI_CODING_AGENT_DIR",
    "PI_CODING_AGENT_SESSION_DIR",
    "PI_CONFIG_DIR",
    "XDG_DATA_HOME",
    "XDG_STATE_HOME",
];

/// Shape of the effective OMP session store.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum OmpStoreKind {
    /// OMP's normal bucket-per-cwd `sessions/<bucket>/<session>.jsonl` layout.
    Managed,
    /// An explicit flat `--session-dir` / `PI_CODING_AGENT_SESSION_DIR` layout.
    Custom,
}

/// Absolute roots used by one OMP process.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct OmpStoreLayout {
    pub sessions: PathBuf,
    /// Managed store retained even when `sessions` is an explicit custom store.
    pub managed_sessions: PathBuf,
    pub terminal_sessions: PathBuf,
    pub kind: OmpStoreKind,
}

#[derive(Debug)]
pub(crate) struct OmpResolvedContext {
    pub(crate) layout: OmpStoreLayout,
    pub(crate) routing_fingerprint: String,
    pub(crate) launcher_routing: Vec<(String, Option<String>)>,
    pub(crate) profile: Option<String>,
    pub(crate) cwd: PathBuf,
    pub(crate) agent_dir: PathBuf,
}

fn omp_routing_values(environment: &HashMap<String, String>) -> Vec<(String, Option<String>)> {
    OMP_STORE_ENV_KEYS
        .iter()
        .map(|key| ((*key).to_owned(), environment.get(*key).cloned()))
        .collect()
}

/// Transient launch snapshot; only the routing fingerprint reaches pane metadata.
#[derive(Clone, Eq, PartialEq)]
pub(crate) struct OmpCapturePlan {
    pub layout: OmpStoreLayout,
    pub routing_fingerprint: String,
    pub launch_id: String,
    pub launch_marker: String,
    pub container_runtime: Option<crate::session::config::ContainerRuntimeName>,
}

/// Stable capture inputs persisted with a tmux session.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct OmpCaptureMetadata {
    pub layout: OmpStoreLayout,
    pub launched_at_ms: u64,
    #[serde(default)]
    pub launch_id: String,
    #[serde(default)]
    pub launch_marker: String,
    #[serde(default)]
    pub routing_fingerprint: String,
    #[serde(default)]
    pub container_runtime: Option<crate::session::config::ContainerRuntimeName>,
}

/// The per-instance sandbox launch marker path, reused across relaunches and
/// never unlinked.
pub(crate) fn omp_sandbox_launch_marker(instance_id: &str) -> String {
    let safe = instance_id
        .bytes()
        .filter(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
        .map(char::from)
        .collect::<String>();
    format!("/tmp/aoe-omp-launch-{safe}")
}
