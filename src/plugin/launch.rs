//! Resolve a plugin's declared `[runtime]` into a concrete, launchable

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use aoe_plugin_api::RuntimeSpec;

use crate::plugin::registry::LoadedPlugin;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedLaunch {
    pub program: PathBuf,
    pub args: Vec<String>,
    pub cwd: PathBuf,
    pub env: BTreeMap<String, String>,
}

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

pub trait LaunchResolver {
    fn which(&self, program: &str) -> Option<PathBuf>;
    fn exists(&self, path: &Path) -> bool;
    fn is_executable(&self, path: &Path) -> bool;
}

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
        RuntimeSpec::Command { command, .. } => {
            resolve_command(&plugin_id, dir, command, resolver)?
        }
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

pub(crate) fn resolve_command(
    plugin_id: &str,
    dir: &Path,
    command: &[String],
    resolver: &dyn LaunchResolver,
) -> Result<(PathBuf, Vec<String>), LaunchError> {
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

fn resolve_in_tree(
    plugin_id: &str,
    dir: &Path,
    rel: &str,
    resolver: &dyn LaunchResolver,
    missing: impl FnOnce(PathBuf) -> LaunchError,
) -> Result<PathBuf, LaunchError> {
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
    fn command_resolves_bare_names_on_path_and_relative_paths_in_the_plugin_dir() {
        let p = plugin(
            Some("[runtime]\nkind = \"command\"\ncommand = [\"python3\", \"-m\", \"acme.main\"]\nsystem = true"),
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

        // A relative path must exist in the plugin dir and be executable.
        let p = plugin(
            Some("[runtime]\nkind = \"command\"\ncommand = [\"bin/worker\"]"),
            Some("/plugins/acme.worker"),
        );
        let bin = PathBuf::from("/plugins/acme.worker/bin/worker");
        let resolver = FakeResolver::new().file(bin.clone(), true);
        assert_eq!(resolve_launch(&p, &resolver).unwrap().program, bin);
        let resolver = FakeResolver::new().file(bin, false);
        let err = resolve_launch(&p, &resolver).unwrap_err();
        assert!(matches!(err, LaunchError::NotExecutable { .. }), "{err:?}");
    }

    #[test]
    fn unusable_runtimes_are_refused_with_their_own_error() {
        let cases = [
            ("no runtime at all", None, "no-runtime"),
            (
                "console script missing from PATH",
                Some("[runtime]\nkind = \"command\"\ncommand = [\"aoe-github-worker\"]\nsystem = true"),
                "not-on-path",
            ),
            (
                "command escaping the plugin dir",
                Some("[runtime]\nkind = \"command\"\ncommand = [\"../escape\"]"),
                "path-escape",
            ),
        ];
        for (label, runtime, expected) in cases {
            let p = plugin(runtime, Some("/plugins/acme.worker"));
            let err = resolve_launch(&p, &FakeResolver::new()).unwrap_err();
            let seen = match &err {
                LaunchError::NoRuntime { .. } => "no-runtime",
                LaunchError::ProgramNotOnPath { program, .. } => {
                    assert_eq!(program, "aoe-github-worker", "{label}");
                    "not-on-path"
                }
                LaunchError::PathEscape { .. } => "path-escape",
                other => panic!("{label}: unexpected {other:?}"),
            };
            assert_eq!(seen, expected, "{label}");
        }

        let argv0 = if cfg!(windows) {
            "C:/Windows/py.exe"
        } else {
            "/usr/bin/python3"
        };
        let err = resolve_command(
            "acme.worker",
            Path::new("/plugins/acme.worker"),
            &[argv0.to_string()],
            &FakeResolver::new(),
        )
        .unwrap_err();
        assert!(matches!(err, LaunchError::AbsoluteArgv0 { .. }), "{err:?}");
    }

    #[test]
    fn release_binary_resolves_in_tree_or_names_the_platform() {
        let p = plugin(
            Some("[runtime]\nkind = \"release-binary\"\nasset = \"worker-${os}-${arch}\"\nbin = \"bin/worker\""),
            Some("/plugins/acme.worker"),
        );
        let bin = PathBuf::from("/plugins/acme.worker/bin/worker");
        let launch = resolve_launch(&p, &FakeResolver::new().file(bin.clone(), true)).unwrap();
        assert_eq!(launch.program, bin);
        assert!(launch.args.is_empty());

        let p = plugin(
            Some("[runtime]\nkind = \"release-binary\"\nasset = \"worker\""),
            Some("/plugins/acme.worker"),
        );
        match resolve_launch(&p, &FakeResolver::new()).unwrap_err() {
            LaunchError::ReleaseBinaryMissing { os, arch, .. } => {
                assert_eq!(os, std::env::consts::OS);
                assert_eq!(arch, std::env::consts::ARCH);
            }
            other => panic!("expected ReleaseBinaryMissing, got {other:?}"),
        }
    }
}
