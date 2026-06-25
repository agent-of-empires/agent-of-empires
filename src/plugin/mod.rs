//! Plugin core: load the compiled-in first-party plugins and expose their
//! enabled/disabled state to every surface (CLI, TUI, web).
//!
//! This is the minimal core: a registry of builtin plugins you can enable or
//! disable. The manifest types live in the `aoe-plugin-api` crate. External
//! installs, capability grants, and the Tier 0 / Tier 1 contribution surface
//! return in follow-up PRs.

pub mod contributions;
pub mod featured;
pub mod fetch;
pub mod install;
pub mod integrity;
pub mod lockfile;
pub mod registry;
pub mod source;
pub mod view;

// The Tier 1 worker host runs only in the `aoe serve` daemon, where the event
// store and session storage it serves over the capability-gated API live. A
// TUI-only build has no host, so these modules are gated with it.
#[cfg(feature = "serve")]
pub mod host;
#[cfg(feature = "serve")]
pub mod host_api;
#[cfg(feature = "serve")]
pub mod launch;
#[cfg(feature = "serve")]
pub mod protocol;
#[cfg(feature = "serve")]
pub mod sandbox;

use std::path::PathBuf;
use std::sync::{Arc, RwLock};

/// Directory holding externally installed plugins, one subdir per plugin id:
/// `<app_dir>/plugins/<id>/`.
pub fn plugins_dir() -> anyhow::Result<PathBuf> {
    Ok(crate::session::get_app_dir()?.join("plugins"))
}

pub use registry::{LoadedPlugin, PluginRegistry};
pub use view::PluginView;

/// Lock recovery for the process-wide registry slot: a panic elsewhere must
/// not poison it and take a TUI redraw / tokio task down on the next access.
/// Recovering via `into_inner` is correct: the held data is a rebuildable
/// cache, not partial-mutation-sensitive state.
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

/// The process-wide plugin registry, loaded on first use from the global
/// config. Surfaces that toggle a plugin call [`reload_registry`] after
/// persisting the change.
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

/// Themes contributed by the active plugin set, as `(name, resolved path)`
/// pairs. The theme registry layers these below builtins and user themes.
pub fn active_plugin_themes() -> Vec<(String, PathBuf)> {
    let reg = registry();
    let active: Vec<&LoadedPlugin> = reg.active().collect();
    contributions::active_themes(&active)
}

/// Rebuild the registry from the current on-disk config (after an
/// enable/disable), so the change is reflected the next time any surface reads
/// the active set.
pub fn reload_registry() -> Arc<PluginRegistry> {
    let config = crate::session::Config::load_or_warn();
    let reg = Arc::new(PluginRegistry::load(&config));
    *REGISTRY.write_safe() = Some(reg.clone());
    reg
}
