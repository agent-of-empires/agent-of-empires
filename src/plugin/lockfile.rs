//! `plugins.lock`: the exact resolved identity of every externally installed

use std::collections::BTreeMap;
use std::path::PathBuf;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

const LOCK_VERSION: u32 = 2;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Lockfile {
    pub lock_version: u32,
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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LockedPlugin {
    pub source: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub requested_ref: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resolved_commit: Option<String>,
    pub version: String,
    pub manifest_hash: String,
    #[serde(default)]
    pub tree_hash: String,
    pub trust: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub release_tag: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub asset_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub asset_sha256: Option<String>,
}

impl Lockfile {
    fn path() -> Result<PathBuf> {
        Ok(crate::session::get_app_dir()?.join("plugins.lock"))
    }

    pub fn load() -> Result<Self> {
        let path = Self::path()?;
        if !path.exists() {
            return Ok(Self::default());
        }
        let text = std::fs::read_to_string(&path)
            .with_context(|| format!("reading {}", path.display()))?;
        let lockfile: Lockfile =
            toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
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

    pub fn save(&self) -> Result<()> {
        let path = Self::path()?;
        let text = toml::to_string_pretty(self).context("serializing plugins.lock")?;
        std::fs::write(&path, text).with_context(|| format!("writing {}", path.display()))
    }

    pub fn get(&self, id: &str) -> Option<&LockedPlugin> {
        self.plugins.get(id)
    }

    pub fn upsert(&mut self, id: &str, locked: LockedPlugin) {
        self.lock_version = LOCK_VERSION;
        self.plugins.insert(id.to_string(), locked);
    }

    pub fn remove(&mut self, id: &str) -> bool {
        self.plugins.remove(id).is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn locked(source: &str, remote: bool) -> LockedPlugin {
        LockedPlugin {
            source: source.into(),
            requested_ref: remote.then(|| "v1.0.0".into()),
            resolved_commit: remote.then(|| "deadbeef".into()),
            version: "1.0.0".into(),
            manifest_hash: "sha256:abc".into(),
            tree_hash: "sha256:tree".into(),
            trust: "community".into(),
            release_tag: remote.then(|| "v1.0.0".into()),
            asset_name: remote.then(|| "widget-x86_64.tar.gz".into()),
            asset_sha256: remote.then(|| "sha256:def".into()),
        }
    }

    #[test]
    fn round_trips_through_toml() {
        let mut lf = Lockfile::default();
        lf.upsert("acme.widget", locked("gh:acme/widget", true));
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
        lf.upsert("acme.widget", locked("/local/path", false));
        assert!(lf.remove("acme.widget"));
        assert!(!lf.remove("acme.widget"));
    }
}
