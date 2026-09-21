//! Recall cache of the ACP `config_options` each agent last advertised.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Mutex;

use serde::{Deserialize, Serialize};

use crate::acp::state::ConfigOptionDescriptor;

const CATALOG_VERSION: u32 = 1;
const CATALOG_FILE: &str = "acp_option_catalog.json";

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

/// Options with `current_value` cleared, for storage and comparison.
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

/// Record an agent's advertised options.
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
    use crate::session::test_support::isolate_app_dir;
    use serial_test::serial;

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
    fn record_strips_current_value_debounces_and_skips_empty_agents() {
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

        // Only current_value differs, so the stored snapshot is unchanged.
        record(
            "opencode",
            &[descriptor("gpt-4")],
            "2026-07-03T09:00:00Z".into(),
        )
        .unwrap();
        assert_eq!(load().agents["opencode"].updated_at, "2026-07-03T00:00:00Z");

        record("", &[descriptor("gpt-5")], "2026-07-03T00:00:00Z".into()).unwrap();
        assert_eq!(load().agents.len(), 1, "an empty agent name is ignored");
    }
}
