//! Plugin manifest types for the Agent of Empires plugin system.
//!
//! This crate is the stable surface a plugin author (and the in-tree host)
//! compiles against: the `aoe-plugin.toml` manifest schema and the validation
//! rules that gate a manifest before it loads. This is the minimal core;
//! contribution sections (settings, keybinds, themes, commands, status
//! detection, UI, panes, runtime workers) and the capability taxonomy return in
//! follow-up PRs. See `docs/development/internals/plugin-system.md`.

mod id;
mod manifest;

pub use id::{InvalidPluginId, PluginId};
pub use manifest::{ManifestError, PluginManifest};

/// Version of the manifest schema and host API this crate describes.
///
/// A manifest declares the `api_version` it was written against; the host
/// refuses manifests targeting a newer version than it understands.
pub const API_VERSION: u32 = 1;
