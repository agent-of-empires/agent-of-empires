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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_sandbox_is_pass_through() {
        let mut env = BTreeMap::new();
        env.insert("AOE_PLUGIN_ID".to_string(), "acme.worker".to_string());
        let launch = ResolvedLaunch {
            program: PathBuf::from("/usr/bin/python3"),
            args: vec!["-m".into(), "acme.main".into()],
            cwd: PathBuf::from("/plugins/acme.worker"),
            env: env.clone(),
        };
        let prepared = NoSandbox.prepare(&launch).unwrap();
        assert_eq!(prepared.program, launch.program);
        assert_eq!(prepared.args, launch.args);
        assert_eq!(prepared.cwd, launch.cwd);
        assert_eq!(prepared.env, env);
        assert_eq!(NoSandbox.name(), "none");
    }
}
