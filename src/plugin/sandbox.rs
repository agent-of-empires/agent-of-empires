//! Sandbox backends for plugin workers.
//!
//! A [`SandboxBackend`] sits between a resolved launch and the actual spawn,
//! transforming how the worker process is isolated. The honest v1 model (D8 in
//! `docs/development/internals/plugin-system.md`): capability gating at the
//! host API boundary stops a cooperative plugin from reaching resources it did
//! not declare. It does NOT contain an adversarial plugin: a worker that wants
//! to read the filesystem or open a socket can, because [`NoSandbox`] runs it
//! as an ordinary child process. OS-level isolation (a restricted environment,
//! landlock, `sandbox-exec`) arrives later as additional backends behind this
//! same trait, with no change to the supervisor or the resolver.

use std::collections::BTreeMap;
use std::path::PathBuf;

use crate::plugin::launch::ResolvedLaunch;

/// A launch after the sandbox backend has had its say. Today this is the same
/// shape as [`ResolvedLaunch`] because [`NoSandbox`] is a pass-through, but a
/// future backend (a container wrapper, a `sandbox-exec` profile) rewrites the
/// program / args / env here without the supervisor knowing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreparedLaunch {
    pub program: PathBuf,
    pub args: Vec<String>,
    pub cwd: PathBuf,
    pub env: BTreeMap<String, String>,
}

/// How a plugin worker process is isolated from the host. The only v1 backend
/// is [`NoSandbox`]; the trait exists so OS-level isolation can be added later
/// at the same call site.
pub trait SandboxBackend: Send + Sync {
    /// A short stable name for diagnostics and the install prompt.
    fn name(&self) -> &'static str;

    /// Transform a resolved launch into the command actually spawned.
    fn prepare(&self, launch: &ResolvedLaunch) -> anyhow::Result<PreparedLaunch>;
}

/// The v1 backend: run the worker as an ordinary child process, unchanged.
/// Honest about offering no OS-level isolation; see the module docs.
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
