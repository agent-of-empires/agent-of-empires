//! Recall cache of the ACP `config_options` each agent last advertised.
//!
//! The structured view composer's model / mode / thinking dropdowns are fed by
//! the `config_options` a *live* agent advertises. The per-agent defaults
//! settings page has no live session, so it reads this cache instead: whenever
//! the daemon observes a full `ConfigOptionsUpdated` snapshot for a session, it
//! records the snapshot keyed by that session's agent. New models flow in the
//! next time the agent runs; nothing here is a hand-maintained catalog.
//!
//! This is disposable cache data, not user configuration: an owner-only JSON
//! file in the app dir, written atomically (temp + rename), version-gated, and
//! skipped when the snapshot is unchanged so a chatty session does not rewrite
//! it on every update. `current_value` is stripped because it is per-session
//! state, not catalog metadata; consumers resolve the live option at apply
//! time.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Mutex;

use serde::{Deserialize, Serialize};

use crate::acp::state::ConfigOptionDescriptor;

const CATALOG_VERSION: u32 = 1;
const CATALOG_FILE: &str = "acp_option_catalog.json";

/// Serializes the load-modify-write cycle in `record` so concurrent
/// `ConfigOptionsUpdated` events (the daemon fires one `spawn_blocking(record)`
/// per event, and several agents run in parallel) cannot both load the same
/// snapshot and clobber each other's just-written entry.
static CATALOG_LOCK: Mutex<()> = Mutex::new(());

/// The whole cache: one entry per agent name.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OptionCatalog {
    pub version: u32,
    #[serde(default)]
    pub agents: HashMap<String, AgentOptionEntry>,
}

impl Default for OptionCatalog {
    fn default() -> Self {
        Self {
            version: CATALOG_VERSION,
            agents: HashMap::new(),
        }
    }
}

/// One agent's last-observed option snapshot.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AgentOptionEntry {
    /// RFC 3339 timestamp of the last observation, so the UI can show freshness.
    pub updated_at: String,
    /// The advertised options with `current_value` cleared.
    pub options: Vec<ConfigOptionDescriptor>,
}

fn catalog_path() -> anyhow::Result<PathBuf> {
    Ok(crate::session::get_app_dir()?.join(CATALOG_FILE))
}

/// Load the cache, or an empty one if the file is missing, unreadable, or from
/// a different schema version (a version bump discards the old cache; it
/// repopulates as agents run).
pub fn load() -> OptionCatalog {
    let path = match catalog_path() {
        Ok(p) => p,
        Err(_) => return OptionCatalog::default(),
    };
    let bytes = match std::fs::read(&path) {
        Ok(b) => b,
        Err(_) => return OptionCatalog::default(),
    };
    match serde_json::from_slice::<OptionCatalog>(&bytes) {
        Ok(catalog) if catalog.version == CATALOG_VERSION => catalog,
        _ => OptionCatalog::default(),
    }
}

/// Options with `current_value` cleared, for storage and comparison. The live
/// selection is session state, not catalog metadata.
fn strip_current(options: &[ConfigOptionDescriptor]) -> Vec<ConfigOptionDescriptor> {
    options
        .iter()
        .cloned()
        .map(|mut option| {
            option.current_value = String::new();
            option
        })
        .collect()
}

/// Record an agent's advertised options. No-ops when the option set is
/// unchanged (debounce), so a chatty session does not rewrite the file on every
/// `ConfigOptionsUpdated`. `now` is an RFC 3339 timestamp supplied by the
/// caller.
pub fn record(agent: &str, options: &[ConfigOptionDescriptor], now: String) -> anyhow::Result<()> {
    if agent.trim().is_empty() {
        return Ok(());
    }
    let stripped = strip_current(options);
    // Hold the lock across load -> mutate -> write; a poisoned lock still
    // serializes (the guarded data is just `()`), so recover rather than panic.
    let _guard = CATALOG_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let mut catalog = load();
    if catalog
        .agents
        .get(agent)
        .is_some_and(|entry| entry.options == stripped)
    {
        return Ok(());
    }
    catalog.agents.insert(
        agent.to_string(),
        AgentOptionEntry {
            updated_at: now,
            options: stripped,
        },
    );
    write(&catalog)
}

fn write(catalog: &OptionCatalog) -> anyhow::Result<()> {
    let path = catalog_path()?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("json.tmp");
    let json = serde_json::to_vec_pretty(catalog)?;
    std::fs::write(&tmp, &json)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600));
    }
    std::fs::rename(&tmp, &path)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::acp::state::{ConfigOptionCategory, ConfigOptionChoice};
    use serial_test::serial;

    fn isolate_app_dir() -> tempfile::TempDir {
        let tmp = tempfile::tempdir().expect("create temp home for catalog tests");
        std::env::set_var("HOME", tmp.path());
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        std::env::set_var("XDG_CONFIG_HOME", tmp.path().join(".config"));
        tmp
    }

    fn descriptor(current: &str) -> ConfigOptionDescriptor {
        ConfigOptionDescriptor {
            id: "model".into(),
            name: "Model".into(),
            description: None,
            category: ConfigOptionCategory::Model,
            current_value: current.into(),
            options: vec![ConfigOptionChoice {
                value: "gpt-5".into(),
                name: "GPT-5".into(),
                description: None,
            }],
        }
    }

    #[test]
    #[serial]
    fn record_and_load_round_trips_with_current_value_stripped() {
        let _tmp = isolate_app_dir();
        record(
            "opencode",
            &[descriptor("gpt-5")],
            "2026-07-03T00:00:00Z".into(),
        )
        .unwrap();
        let loaded = load();
        let entry = loaded.agents.get("opencode").expect("agent recorded");
        assert_eq!(entry.updated_at, "2026-07-03T00:00:00Z");
        // current_value is stripped; the choices survive.
        assert_eq!(entry.options[0].current_value, "");
        assert_eq!(entry.options[0].options[0].value, "gpt-5");
    }

    #[test]
    #[serial]
    fn record_debounces_unchanged_snapshot() {
        let _tmp = isolate_app_dir();
        record(
            "opencode",
            &[descriptor("gpt-5")],
            "2026-07-03T00:00:00Z".into(),
        )
        .unwrap();
        // Same options but a different current_value must not rewrite the
        // timestamp: current_value is not catalog data.
        record(
            "opencode",
            &[descriptor("gpt-4")],
            "2026-07-03T09:00:00Z".into(),
        )
        .unwrap();
        assert_eq!(load().agents["opencode"].updated_at, "2026-07-03T00:00:00Z");
    }

    #[test]
    #[serial]
    fn empty_agent_name_is_ignored() {
        let _tmp = isolate_app_dir();
        record("", &[descriptor("gpt-5")], "2026-07-03T00:00:00Z".into()).unwrap();
        assert!(load().agents.is_empty());
    }
}
