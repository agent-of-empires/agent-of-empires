//! Resolve a plugin's declared `[runtime]` into a concrete, launchable
//! command, dispatched off the runtime kind.
//!
//! The host, not the plugin, decides how to turn a `RuntimeSpec` into a real
//! program path. This module is the single place that branching lives: a
//! `Command` runtime resolves its `argv[0]` on `PATH` or inside the plugin
//! directory; a `ReleaseBinary` runtime points at the per-platform binary
//! installation already placed in the plugin directory. Adding a new runtime
//! kind later is a new match arm in [`resolve_launch`], not a rewrite of the
//! supervisor or the transport: they only ever see a [`ResolvedLaunch`].
//!
//! Resolution is language-agnostic. The Python reference plugin declares a
//! console-script entrypoint (`aoe-github-worker`) or an interpreter
//! invocation (`python -m aoe_github_plugin.main`); a Rust/native plugin
//! ships a `release-binary`. Both reach the worker through the same path.
//!
//! Filesystem and `PATH` probing go through the [`LaunchResolver`] trait so
//! the resolution policy is unit-testable without touching the real
//! filesystem.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use aoe_plugin_api::RuntimeSpec;

use crate::plugin::registry::LoadedPlugin;

/// Everything `std::process::Command` needs to launch a worker, computed once
/// and free of any `RuntimeSpec` branching. The supervisor takes this, applies
/// the sandbox backend, wires stdio, and spawns; it never re-inspects the
/// manifest.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedLaunch {
    /// Absolute program path to execute.
    pub program: PathBuf,
    /// Arguments after the program (the manifest argv tail, or empty).
    pub args: Vec<String>,
    /// Working directory: the plugin's installed directory.
    pub cwd: PathBuf,
    /// Environment overlay applied on top of the inherited host environment.
    pub env: BTreeMap<String, String>,
}

/// Why a plugin's runtime could not be resolved into a launchable command.
/// Every variant carries the plugin id and an actionable hint, matching the
/// project's error-with-hint style.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum LaunchError {
    #[error("plugin {plugin_id} declares no [runtime]; it has no worker to launch")]
    NoRuntime { plugin_id: String },

    #[error("plugin {plugin_id} declares a runtime but has no installed directory")]
    NoPluginDir { plugin_id: String },

    #[error(
        "plugin {plugin_id}: worker program {program:?} was not found on PATH. \
         Install it (for example `python3`), or declare a plugin-relative path such as `bin/{program}`."
    )]
    ProgramNotOnPath { plugin_id: String, program: String },

    #[error(
        "plugin {plugin_id}: argv[0] {arg:?} is an absolute path. \
         Use a PATH program (such as `python3`) or a plugin-relative path (such as `bin/worker`)."
    )]
    AbsoluteArgv0 { plugin_id: String, arg: String },

    #[error("plugin {plugin_id}: worker path {arg:?} escapes the plugin directory")]
    PathEscape { plugin_id: String, arg: String },

    #[error(
        "plugin {plugin_id}: worker program {path} is missing. \
         Reinstall with `aoe plugin update {plugin_id}`."
    )]
    InTreeMissing { plugin_id: String, path: PathBuf },

    #[error("plugin {plugin_id}: worker program {path} is not executable. Run `chmod +x {path}`.")]
    NotExecutable { plugin_id: String, path: PathBuf },

    #[error(
        "plugin {plugin_id}: no prebuilt worker binary {path} for this platform ({os}-{arch}). \
         Reinstall with `aoe plugin update {plugin_id}`, or publish a release asset for this platform."
    )]
    ReleaseBinaryMissing {
        plugin_id: String,
        path: PathBuf,
        os: String,
        arch: String,
    },
}

/// Indirection over `PATH` lookup and filesystem probing, so the resolution
/// policy in [`resolve_launch`] can be exercised by unit tests with a fake
/// that never touches the real filesystem. The real implementation is
/// [`OsLaunchResolver`].
pub trait LaunchResolver {
    /// Resolve a bare program name on `PATH`, returning its absolute path.
    fn which(&self, program: &str) -> Option<PathBuf>;
    /// Whether `path` exists.
    fn exists(&self, path: &Path) -> bool;
    /// Whether `path` is a regular file with an executable bit (Unix) or
    /// simply a file (non-Unix).
    fn is_executable(&self, path: &Path) -> bool;
}

/// The production [`LaunchResolver`]: real `PATH` and filesystem.
pub struct OsLaunchResolver;

impl LaunchResolver for OsLaunchResolver {
    fn which(&self, program: &str) -> Option<PathBuf> {
        which::which(program).ok()
    }

    fn exists(&self, path: &Path) -> bool {
        path.exists()
    }

    fn is_executable(&self, path: &Path) -> bool {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::metadata(path)
                .map(|m| m.is_file() && (m.permissions().mode() & 0o111) != 0)
                .unwrap_or(false)
        }
        #[cfg(not(unix))]
        {
            path.is_file()
        }
    }
}

/// Resolve a plugin's runtime into a launchable command.
///
/// The single dispatch site. A new `RuntimeSpec` variant becomes a new match
/// arm here; nothing downstream changes. Builtins do not declare a runtime in
/// this release, so a builtin (or any plugin with `runtime = None`) returns
/// [`LaunchError::NoRuntime`]: it has no worker. The `aoe __plugin-worker`
/// self-exec path for builtin workers arrives with the first builtin worker.
pub fn resolve_launch(
    plugin: &LoadedPlugin,
    resolver: &dyn LaunchResolver,
) -> Result<ResolvedLaunch, LaunchError> {
    let plugin_id = plugin.id().to_string();
    let runtime = plugin
        .manifest
        .runtime
        .as_ref()
        .ok_or_else(|| LaunchError::NoRuntime {
            plugin_id: plugin_id.clone(),
        })?;
    let dir = plugin
        .dir
        .as_ref()
        .ok_or_else(|| LaunchError::NoPluginDir {
            plugin_id: plugin_id.clone(),
        })?;

    let (program, args) = match runtime {
        RuntimeSpec::Command { command } => resolve_command(&plugin_id, dir, command, resolver)?,
        RuntimeSpec::ReleaseBinary { asset, bin } => {
            let target = bin.as_deref().unwrap_or(asset.as_str());
            let path = resolve_in_tree(&plugin_id, dir, target, resolver, |path| {
                LaunchError::ReleaseBinaryMissing {
                    plugin_id: plugin_id.clone(),
                    path,
                    os: std::env::consts::OS.to_string(),
                    arch: std::env::consts::ARCH.to_string(),
                }
            })?;
            (path, Vec::new())
        }
    };

    let mut env = BTreeMap::new();
    env.insert("AOE_PLUGIN_ID".to_string(), plugin_id);

    Ok(ResolvedLaunch {
        program,
        args,
        cwd: dir.clone(),
        env,
    })
}

/// Resolve a `Command` runtime's argv into `(program, args)`.
///
/// `argv[0]` policy: an absolute path is rejected (it pins a host path and
/// breaks portability); a path containing a separator is resolved relative to
/// the plugin directory and verified executable; a bare name is resolved on
/// `PATH` via `which` (the console-script / interpreter case).
fn resolve_command(
    plugin_id: &str,
    dir: &Path,
    command: &[String],
    resolver: &dyn LaunchResolver,
) -> Result<(PathBuf, Vec<String>), LaunchError> {
    // The manifest validator guarantees a non-empty command with non-empty
    // arguments, so `split_first` cannot fail in practice; treat an empty one
    // as a missing runtime rather than panicking.
    let (head, tail) = command
        .split_first()
        .ok_or_else(|| LaunchError::NoRuntime {
            plugin_id: plugin_id.to_string(),
        })?;

    let program = if Path::new(head).is_absolute() {
        return Err(LaunchError::AbsoluteArgv0 {
            plugin_id: plugin_id.to_string(),
            arg: head.clone(),
        });
    } else if head.contains('/') || head.contains('\\') {
        resolve_in_tree(plugin_id, dir, head, resolver, |path| {
            LaunchError::InTreeMissing {
                plugin_id: plugin_id.to_string(),
                path,
            }
        })?
    } else {
        resolver
            .which(head)
            .ok_or_else(|| LaunchError::ProgramNotOnPath {
                plugin_id: plugin_id.to_string(),
                program: head.clone(),
            })?
    };

    Ok((program, tail.to_vec()))
}

/// Resolve a plugin-relative executable path under `dir`, rejecting traversal
/// and verifying the file exists and is executable. `missing` builds the
/// not-found error so callers can distinguish a command in-tree miss from a
/// release-binary platform miss.
fn resolve_in_tree(
    plugin_id: &str,
    dir: &Path,
    rel: &str,
    resolver: &dyn LaunchResolver,
    missing: impl FnOnce(PathBuf) -> LaunchError,
) -> Result<PathBuf, LaunchError> {
    // Reject explicit parent traversal before joining. This is defense in
    // depth: per the honest model (D8) the security boundary is not here, but
    // a relative worker path should never reach outside its own directory.
    if Path::new(rel)
        .components()
        .any(|c| matches!(c, std::path::Component::ParentDir))
        || Path::new(rel).is_absolute()
    {
        return Err(LaunchError::PathEscape {
            plugin_id: plugin_id.to_string(),
            arg: rel.to_string(),
        });
    }
    let candidate = dir.join(rel);
    if !resolver.exists(&candidate) {
        return Err(missing(candidate));
    }
    if !resolver.is_executable(&candidate) {
        return Err(LaunchError::NotExecutable {
            plugin_id: plugin_id.to_string(),
            path: candidate,
        });
    }
    Ok(candidate)
}

#[cfg(test)]
mod tests {
    use super::*;
    use aoe_plugin_api::{PluginManifest, TrustLevel};
    use std::collections::HashSet;

    /// A fake resolver: a fixed `PATH` map plus a set of existing and
    /// executable in-tree paths. No real filesystem access.
    struct FakeResolver {
        path: BTreeMap<String, PathBuf>,
        exists: HashSet<PathBuf>,
        executable: HashSet<PathBuf>,
    }

    impl FakeResolver {
        fn new() -> Self {
            Self {
                path: BTreeMap::new(),
                exists: HashSet::new(),
                executable: HashSet::new(),
            }
        }
        fn on_path(mut self, name: &str, at: &str) -> Self {
            self.path.insert(name.to_string(), PathBuf::from(at));
            self
        }
        fn file(mut self, path: PathBuf, executable: bool) -> Self {
            self.exists.insert(path.clone());
            if executable {
                self.executable.insert(path);
            }
            self
        }
    }

    impl LaunchResolver for FakeResolver {
        fn which(&self, program: &str) -> Option<PathBuf> {
            self.path.get(program).cloned()
        }
        fn exists(&self, path: &Path) -> bool {
            self.exists.contains(path)
        }
        fn is_executable(&self, path: &Path) -> bool {
            self.executable.contains(path)
        }
    }

    fn plugin(runtime: Option<&str>, dir: Option<&str>) -> LoadedPlugin {
        let runtime_toml = runtime.map(|r| format!("\n{r}\n")).unwrap_or_default();
        let manifest = PluginManifest::from_toml_str(&format!(
            r#"
id = "acme.worker"
name = "Worker"
version = "1.0.0"
api_version = 2
capabilities = ["runtime.worker"]
{runtime_toml}
"#
        ))
        .unwrap();
        LoadedPlugin {
            manifest,
            enabled: true,
            trust: TrustLevel::Community,
            validation: crate::plugin::registry::ValidationState::Community,
            source: Some("gh:acme/worker".into()),
            dir: dir.map(PathBuf::from),
            manifest_hash: "sha256:test".into(),
            granted: true,
        }
    }

    #[test]
    fn no_runtime_has_no_worker() {
        let p = plugin(None, Some("/plugins/acme.worker"));
        let err = resolve_launch(&p, &FakeResolver::new()).unwrap_err();
        assert!(matches!(err, LaunchError::NoRuntime { .. }));
    }

    #[test]
    fn command_bare_name_resolves_on_path() {
        let p = plugin(
            Some("[runtime]\nkind = \"command\"\ncommand = [\"python3\", \"-m\", \"acme.main\"]"),
            Some("/plugins/acme.worker"),
        );
        let resolver = FakeResolver::new().on_path("python3", "/usr/bin/python3");
        let launch = resolve_launch(&p, &resolver).unwrap();
        assert_eq!(launch.program, PathBuf::from("/usr/bin/python3"));
        assert_eq!(launch.args, vec!["-m".to_string(), "acme.main".to_string()]);
        assert_eq!(launch.cwd, PathBuf::from("/plugins/acme.worker"));
        assert_eq!(
            launch.env.get("AOE_PLUGIN_ID").map(String::as_str),
            Some("acme.worker")
        );
    }

    #[test]
    fn command_console_script_missing_on_path_fails_loudly() {
        let p = plugin(
            Some("[runtime]\nkind = \"command\"\ncommand = [\"aoe-github-worker\"]"),
            Some("/plugins/acme.worker"),
        );
        let err = resolve_launch(&p, &FakeResolver::new()).unwrap_err();
        match err {
            LaunchError::ProgramNotOnPath { program, .. } => {
                assert_eq!(program, "aoe-github-worker");
            }
            other => panic!("expected ProgramNotOnPath, got {other:?}"),
        }
    }

    #[test]
    fn command_relative_path_resolves_in_plugin_dir() {
        let p = plugin(
            Some("[runtime]\nkind = \"command\"\ncommand = [\"bin/worker\"]"),
            Some("/plugins/acme.worker"),
        );
        let bin = PathBuf::from("/plugins/acme.worker/bin/worker");
        let resolver = FakeResolver::new().file(bin.clone(), true);
        let launch = resolve_launch(&p, &resolver).unwrap();
        assert_eq!(launch.program, bin);
    }

    #[test]
    fn command_relative_path_not_executable_fails() {
        let p = plugin(
            Some("[runtime]\nkind = \"command\"\ncommand = [\"bin/worker\"]"),
            Some("/plugins/acme.worker"),
        );
        let bin = PathBuf::from("/plugins/acme.worker/bin/worker");
        let resolver = FakeResolver::new().file(bin, false);
        let err = resolve_launch(&p, &resolver).unwrap_err();
        assert!(matches!(err, LaunchError::NotExecutable { .. }));
    }

    #[test]
    fn command_absolute_argv0_rejected() {
        // `Path::is_absolute` is platform-specific: a Unix-style path is not
        // absolute on Windows (it lacks a drive/UNC prefix), so pick an
        // argv[0] that is absolute under the host's own semantics.
        let argv0 = if cfg!(windows) {
            "C:/Windows/py.exe"
        } else {
            "/usr/bin/python3"
        };
        let p = plugin(
            Some(&format!(
                "[runtime]\nkind = \"command\"\ncommand = [\"{argv0}\"]"
            )),
            Some("/plugins/acme.worker"),
        );
        let err = resolve_launch(&p, &FakeResolver::new()).unwrap_err();
        assert!(matches!(err, LaunchError::AbsoluteArgv0 { .. }));
    }

    #[test]
    fn command_parent_traversal_rejected() {
        let p = plugin(
            Some("[runtime]\nkind = \"command\"\ncommand = [\"../escape\"]"),
            Some("/plugins/acme.worker"),
        );
        let err = resolve_launch(&p, &FakeResolver::new()).unwrap_err();
        assert!(matches!(err, LaunchError::PathEscape { .. }));
    }

    #[test]
    fn release_binary_resolves_in_tree() {
        let p = plugin(
            Some("[runtime]\nkind = \"release-binary\"\nasset = \"worker-${os}-${arch}\"\nbin = \"bin/worker\""),
            Some("/plugins/acme.worker"),
        );
        let bin = PathBuf::from("/plugins/acme.worker/bin/worker");
        let resolver = FakeResolver::new().file(bin.clone(), true);
        let launch = resolve_launch(&p, &resolver).unwrap();
        assert_eq!(launch.program, bin);
        assert!(launch.args.is_empty());
    }

    #[test]
    fn release_binary_missing_names_platform() {
        let p = plugin(
            Some("[runtime]\nkind = \"release-binary\"\nasset = \"worker\""),
            Some("/plugins/acme.worker"),
        );
        let err = resolve_launch(&p, &FakeResolver::new()).unwrap_err();
        match err {
            LaunchError::ReleaseBinaryMissing { os, arch, .. } => {
                assert_eq!(os, std::env::consts::OS);
                assert_eq!(arch, std::env::consts::ARCH);
            }
            other => panic!("expected ReleaseBinaryMissing, got {other:?}"),
        }
    }
}
