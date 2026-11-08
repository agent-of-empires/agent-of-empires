//! Plugin registry, installation, contributions, discovery, and worker host.

pub mod agent_integration;
pub mod auto_update;
pub mod changelog;
pub mod contributions;
pub mod discover;
pub mod featured;
pub mod fetch;
pub mod install;
pub mod integrity;
pub mod lockfile;
pub mod registry;
pub mod source;
pub mod update_check;
pub mod view;
pub mod worker_logs;

pub(crate) mod automation_policy;
pub mod host;
pub mod host_api;
pub mod protocol;
pub mod sandbox;
pub mod session_api;
pub mod ui_state;

pub mod launch;

use std::path::PathBuf;
use std::sync::{Arc, RwLock};

pub fn plugins_dir() -> anyhow::Result<PathBuf> {
    Ok(crate::session::get_app_dir()?.join("plugins"))
}

static REMOTE_EXPOSURE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Called by `aoe serve --remote`; dev plugins then refuse to spawn workers.
pub fn set_remote_exposure() {
    REMOTE_EXPOSURE.store(true, std::sync::atomic::Ordering::SeqCst);
}

pub(crate) fn remote_exposure() -> bool {
    REMOTE_EXPOSURE.load(std::sync::atomic::Ordering::SeqCst)
}

pub(crate) type NodeEnv = Vec<(&'static str, std::ffi::OsString)>;

fn resolve_node() -> Option<crate::acp::node::ResolvedNode> {
    let config = crate::session::Config::load_or_warn();
    let app_dir = crate::session::get_app_dir().ok()?;
    let node = crate::acp::node::resolve(&config.acp.node_path, &app_dir).ok()?;
    // Builds and workers run in their own cwd, so a relative or bare path must not reach them.
    let path = absolute_node_path(&node.path, std::env::var_os("PATH"))?;
    Some(crate::acp::node::ResolvedNode { path, ..node })
}

fn absolute_node_path(
    path: &std::path::Path,
    search: Option<std::ffi::OsString>,
) -> Option<PathBuf> {
    if path.components().count() == 1 {
        which::which_in(path, search, std::env::current_dir().ok()?).ok()
    } else {
        std::path::absolute(path).ok()
    }
}

/// Whether installing or launching `manifest` runs a build step or a non-system worker.
pub(crate) fn needs_node(manifest: &aoe_plugin_api::PluginManifest) -> bool {
    match &manifest.runtime {
        Some(aoe_plugin_api::RuntimeSpec::Command { build, system, .. }) => {
            !build.is_empty() || !system
        }
        _ => false,
    }
}

/// `AOE_NODE` plus a `PATH` led by that node's directory, for build steps and
/// workers. Empty when no Node resolves.
pub(crate) fn node_env() -> NodeEnv {
    resolve_node()
        .map(|node| node_env_for(&node.path))
        .unwrap_or_default()
}

fn node_env_for(node: &std::path::Path) -> NodeEnv {
    let mut env = vec![("AOE_NODE", node.as_os_str().to_owned())];
    if let Some(bin_dir) = node.parent() {
        let inherited = std::env::var_os("PATH").unwrap_or_default();
        let dirs = std::iter::once(bin_dir.to_path_buf()).chain(std::env::split_paths(&inherited));
        match std::env::join_paths(dirs) {
            Ok(path) => env.push(("PATH", path)),
            Err(error) => {
                tracing::warn!(target: "plugin", %error, node = %node.display(), "cannot put the Node directory on PATH");
                return NodeEnv::new();
            }
        }
    }
    env
}

/// Download the pinned Node when nothing resolves, so a build step that needs
/// it can run on a machine without a system Node.
pub(crate) async fn ensure_node() {
    if tokio::task::spawn_blocking(resolve_node)
        .await
        .ok()
        .flatten()
        .is_some()
    {
        return;
    }
    let Ok(app_dir) = crate::session::get_app_dir() else {
        return;
    };
    if let Err(error) = crate::acp::node::download(&app_dir).await {
        tracing::warn!(target: "plugin.install", %error, "bundled Node download failed");
    }
}

pub use registry::{LoadedPlugin, PluginRegistry};
pub use view::PluginView;

pub(crate) trait RwLockSafe<T> {
    fn read_safe(&self) -> std::sync::RwLockReadGuard<'_, T>;
    fn write_safe(&self) -> std::sync::RwLockWriteGuard<'_, T>;
}

impl<T> RwLockSafe<T> for RwLock<T> {
    fn read_safe(&self) -> std::sync::RwLockReadGuard<'_, T> {
        self.read().unwrap_or_else(|e| e.into_inner())
    }
    fn write_safe(&self) -> std::sync::RwLockWriteGuard<'_, T> {
        self.write().unwrap_or_else(|e| e.into_inner())
    }
}

static REGISTRY: RwLock<Option<Arc<PluginRegistry>>> = RwLock::new(None);

pub fn registry() -> Arc<PluginRegistry> {
    if let Some(reg) = REGISTRY.read_safe().as_ref() {
        return reg.clone();
    }
    let mut slot = REGISTRY.write_safe();
    if let Some(reg) = slot.as_ref() {
        return reg.clone();
    }
    let config = crate::session::Config::load_or_warn();
    let reg = Arc::new(PluginRegistry::load(&config));
    *slot = Some(reg.clone());
    reg
}

pub fn active_plugin_themes() -> Vec<(String, PathBuf)> {
    let reg = registry();
    let active: Vec<&LoadedPlugin> = reg.active().collect();
    contributions::active_themes(&active)
}

pub fn reload_registry() -> Arc<PluginRegistry> {
    let config = crate::session::Config::load_or_warn();
    let reg = Arc::new(PluginRegistry::load(&config));
    *REGISTRY.write_safe() = Some(reg.clone());
    reg
}

#[cfg(test)]
pub(crate) struct ReloadRegistryOnDrop;

#[cfg(test)]
impl Drop for ReloadRegistryOnDrop {
    fn drop(&mut self) {
        let _lock = crate::session::test_support::EnvGuard::unset(&[]);
        reload_registry();
    }
}

/// A `node` script that reports the pinned version, for `AOE_ACP_NODE`.
#[cfg(all(test, unix))]
pub(crate) fn write_fake_node(dir: &std::path::Path, name: &str) -> std::path::PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let node = dir.join(name);
    std::fs::write(
        &node,
        format!(
            "#!/bin/sh\necho {}\n",
            crate::acp::node::PINNED_NODE_VERSION
        ),
    )
    .unwrap();
    std::fs::set_permissions(&node, std::fs::Permissions::from_mode(0o755)).unwrap();
    node
}

#[cfg(test)]
mod node_env_tests {
    use super::node_env_for;
    use std::ffi::OsString;
    use std::path::Path;

    #[test]
    fn node_path_is_made_absolute() {
        let relative = super::absolute_node_path(Path::new("tools/node"), None).unwrap();
        assert!(relative.is_absolute() && relative.ends_with("tools/node"));
    }

    #[cfg(unix)]
    #[test]
    fn a_bare_node_name_resolves_through_the_given_search_path() {
        let temp = tempfile::tempdir().unwrap();
        let node = super::write_fake_node(temp.path(), "aoe-bare-node");
        let search = Some(temp.path().as_os_str().to_owned());
        assert_eq!(
            super::absolute_node_path(Path::new("aoe-bare-node"), search),
            Some(node)
        );
    }

    #[cfg(unix)]
    #[test]
    #[serial_test::serial]
    fn node_env_resolves_aoe_acp_node() {
        let temp = tempfile::tempdir().unwrap();
        let node = super::write_fake_node(temp.path(), "node");
        let _env = crate::session::test_support::EnvGuard::set(&[("AOE_ACP_NODE", &node)]);

        let env = super::node_env();
        let get = |k: &str| {
            env.iter()
                .find(|(key, _)| *key == k)
                .map(|(_, v)| v.clone())
        };
        assert_eq!(get("AOE_NODE"), Some(node.into_os_string()));
        let path = get("PATH").expect("PATH");
        assert_eq!(std::env::split_paths(&path).next().unwrap(), temp.path());
    }

    #[test]
    fn node_env_sets_aoe_node_and_leads_path_with_its_dir() {
        let env = node_env_for(Path::new("/opt/aoe/node/bin/node"));
        let get = |k: &str| {
            env.iter()
                .find(|(key, _)| *key == k)
                .map(|(_, v)| v.clone())
        };
        assert_eq!(
            get("AOE_NODE"),
            Some(OsString::from("/opt/aoe/node/bin/node"))
        );
        let path = get("PATH").expect("PATH");
        let first = std::env::split_paths(&path).next().unwrap();
        assert_eq!(first, Path::new("/opt/aoe/node/bin"));
    }

    #[cfg(unix)]
    #[test]
    fn a_node_dir_that_cannot_join_path_yields_no_env() {
        assert!(node_env_for(Path::new("/opt/a:b/bin/node")).is_empty());
    }
}
