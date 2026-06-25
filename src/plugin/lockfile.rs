//! `plugins.lock`: the exact resolved identity of every externally installed
//! plugin.
//!
//! Lives at `<app_dir>/plugins.lock` and is TOML, matching `config.toml` and
//! `aoe-plugin.toml`. Like `Cargo.lock` it is deterministic and merge-friendly:
//! plugins are keyed by id (a `BTreeMap`, stable order) and no timestamps are
//! stored. It records what was actually resolved so an install can be
//! reproduced and an update can tell whether anything changed.

use std::collections::BTreeMap;
use std::path::PathBuf;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

/// Current lockfile schema version.
const LOCK_VERSION: u32 = 1;

/// The parsed `plugins.lock`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Lockfile {
    /// Lockfile schema version, for forward migrations.
    pub lock_version: u32,
    /// External plugins keyed by id.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub plugins: BTreeMap<String, LockedPlugin>,
}

impl Default for Lockfile {
    fn default() -> Self {
        Self {
            lock_version: LOCK_VERSION,
            plugins: BTreeMap::new(),
        }
    }
}

/// One external plugin's resolved identity.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LockedPlugin {
    /// Canonical source slug: `gh:owner/repo` or a local path.
    pub source: String,
    /// The ref the user asked for (branch / tag / commit), if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub requested_ref: Option<String>,
    /// The exact commit the source resolved to (GitHub sources only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resolved_commit: Option<String>,
    /// The plugin version from its manifest.
    pub version: String,
    /// `sha256:<hex>` of the installed manifest bytes.
    pub manifest_hash: String,
    /// `builtin` or `community`.
    pub trust: String,
    /// The release tag the worker binary was pulled from (release-binary only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub release_tag: Option<String>,
    /// The release asset name downloaded (release-binary only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub asset_name: Option<String>,
    /// `sha256:<hex>` of the downloaded asset (release-binary only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub asset_sha256: Option<String>,
}

impl Lockfile {
    fn path() -> Result<PathBuf> {
        Ok(crate::session::get_app_dir()?.join("plugins.lock"))
    }

    /// Load the lockfile, returning an empty one if the file does not exist.
    pub fn load() -> Result<Self> {
        let path = Self::path()?;
        if !path.exists() {
            return Ok(Self::default());
        }
        let text = std::fs::read_to_string(&path)
            .with_context(|| format!("reading {}", path.display()))?;
        let lockfile: Lockfile =
            toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
        // Refuse a lockfile written by a newer aoe: saving it back would silently
        // drop fields this version does not know, breaking the forward-migration
        // contract. The user should upgrade rather than downgrade-corrupt it.
        if lockfile.lock_version > LOCK_VERSION {
            anyhow::bail!(
                "{} is lock_version {} but this aoe understands {}; upgrade aoe",
                path.display(),
                lockfile.lock_version,
                LOCK_VERSION
            );
        }
        Ok(lockfile)
    }

    /// Persist the lockfile.
    pub fn save(&self) -> Result<()> {
        let path = Self::path()?;
        let text = toml::to_string_pretty(self).context("serializing plugins.lock")?;
        std::fs::write(&path, text).with_context(|| format!("writing {}", path.display()))
    }

    pub fn get(&self, id: &str) -> Option<&LockedPlugin> {
        self.plugins.get(id)
    }

    /// Insert or replace a plugin's lock entry.
    pub fn upsert(&mut self, id: &str, locked: LockedPlugin) {
        self.lock_version = LOCK_VERSION;
        self.plugins.insert(id.to_string(), locked);
    }

    /// Remove a plugin's lock entry; returns whether one was present.
    pub fn remove(&mut self, id: &str) -> bool {
        self.plugins.remove(id).is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_through_toml() {
        let mut lf = Lockfile::default();
        lf.upsert(
            "acme.widget",
            LockedPlugin {
                source: "gh:acme/widget".into(),
                requested_ref: Some("v1.0.0".into()),
                resolved_commit: Some("deadbeef".into()),
                version: "1.0.0".into(),
                manifest_hash: "sha256:abc".into(),
                trust: "community".into(),
                release_tag: Some("v1.0.0".into()),
                asset_name: Some("widget-x86_64.tar.gz".into()),
                asset_sha256: Some("sha256:def".into()),
            },
        );
        let text = toml::to_string_pretty(&lf).unwrap();
        let back: Lockfile = toml::from_str(&text).unwrap();
        assert_eq!(back.lock_version, LOCK_VERSION);
        assert_eq!(
            back.plugins.get("acme.widget"),
            lf.plugins.get("acme.widget")
        );
    }

    #[test]
    fn remove_reports_presence() {
        let mut lf = Lockfile::default();
        lf.upsert(
            "acme.widget",
            LockedPlugin {
                source: "/local/path".into(),
                requested_ref: None,
                resolved_commit: None,
                version: "0.1.0".into(),
                manifest_hash: "sha256:abc".into(),
                trust: "community".into(),
                release_tag: None,
                asset_name: None,
                asset_sha256: None,
            },
        );
        assert!(lf.remove("acme.widget"));
        assert!(!lf.remove("acme.widget"));
    }
}
