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
