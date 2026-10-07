//! Sandbox backends for plugin workers.

use std::collections::BTreeMap;
use std::path::PathBuf;

use crate::plugin::launch::ResolvedLaunch;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreparedLaunch {
    pub program: PathBuf,
    pub args: Vec<String>,
    pub cwd: PathBuf,
    pub env: BTreeMap<String, String>,
}

pub trait SandboxBackend: Send + Sync {
    fn name(&self) -> &'static str;

    fn prepare(&self, launch: &ResolvedLaunch) -> anyhow::Result<PreparedLaunch>;
}

pub struct NoSandbox;

impl SandboxBackend for NoSandbox {
    fn name(&self) -> &'static str {
        "none"
    }

    fn prepare(&self, launch: &ResolvedLaunch) -> anyhow::Result<PreparedLaunch> {
        Ok(PreparedLaunch {
            program: launch.program.clone(),
            args: launch.args.clone(),
            cwd: launch.cwd.clone(),
            env: launch.env.clone(),
        })
    }
}
