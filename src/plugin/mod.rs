//! Plugin registry, installation, contributions, discovery, and worker host.

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
