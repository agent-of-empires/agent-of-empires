//! Settings resolution with provenance (#2094).
//!
//! A setting's effective value can come from more than one layer.
//!
//! For a **core** key at Tier 0 the effective value is the user's value (when it
//! differs from the baseline default), else the core schema default. A plugin's
//! `setting_defaults` override of a core key is surfaced as a *candidate* so it
//! is observable, but it does NOT win: nothing applies it during real `Config`
//! load or merge yet, so the running app uses the user value or the struct
//! default. The runtime host applies these overrides for real (#2095); until
//! then a `plugin_default` candidate is "declared, not yet in effect".
//!
//! For a **plugin's own** setting the effective value is the stored value, else
//! the plugin's manifest default.
//!
//! [`resolve`] returns the winning value, its [`SettingSource`], and every
//! candidate that was considered, so `aoe settings explain` and
//! `GET /api/settings/resolved` can show exactly why a value is what it is.

use serde::Serialize;
use serde_json::Value;

use super::{runtime_schema, section_plugin_id, FieldDescriptor};

/// Where a resolved value came from.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SettingSource {
    /// The user's stored value (a core field changed from its default, or a
    /// stored plugin setting value).
    User,
    /// A plugin's `setting_defaults` override of a core setting. At Tier 0 this
    /// only ever appears as a candidate, never as the winning source: it is
    /// declared but not yet applied at runtime (the runtime host applies it,
    /// #2095).
    PluginDefault { plugin: String },
    /// The owning plugin's manifest default for one of its own settings.
    ManifestDefault { plugin: String },
    /// The core schema (struct) default.
    SchemaDefault,
}

/// One layer that contributed a candidate value, in precedence order.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Candidate {
    pub source: SettingSource,
    pub value: Value,
}

/// A setting's resolved value plus the full provenance chain.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ResolvedSetting {
    /// Canonical key: `section.field` for core, `plugin:<id>.<key>` for plugin.
    pub key: String,
    pub value: Value,
    pub source: SettingSource,
    /// Every candidate considered, highest precedence first. The first entry
    /// equals `source`/`value`.
    pub candidates: Vec<Candidate>,
}

/// Split a canonical key into `(section, field)`. The field is the last dotted
/// segment; the section is everything before it (so `plugin:acme.kit.retries`
/// splits into `plugin:acme.kit` + `retries`, and `acp.default_agent` into
/// `acp` + `default_agent`).
fn split_key(key: &str) -> Option<(&str, &str)> {
    key.rsplit_once('.')
}

/// Resolve one setting by canonical key against the live config and the active
/// plugin set. `None` if the key is not a known setting.
pub fn resolve(key: &str) -> Option<ResolvedSetting> {
    let cfg = serde_json::to_value(crate::session::Config::load_or_warn()).ok()?;
    let default_cfg = serde_json::to_value(crate::session::Config::default()).ok()?;
    resolve_with(key, &cfg, &default_cfg, &runtime_schema())
}

/// Resolve every known setting (core plus active-plugin). Used by
/// `GET /api/settings/resolved`. Loads the config and builds the schema once
/// for the whole set rather than per field.
pub fn resolve_all() -> Vec<ResolvedSetting> {
    let cfg = serde_json::to_value(crate::session::Config::load_or_warn()).unwrap_or(Value::Null);
    let default_cfg =
        serde_json::to_value(crate::session::Config::default()).unwrap_or(Value::Null);
    let schema = runtime_schema();
    schema
        .iter()
        .filter_map(|d| {
            resolve_with(
                &format!("{}.{}", d.section, d.field),
                &cfg,
                &default_cfg,
                &schema,
            )
        })
        .collect()
}

fn resolve_with(
    key: &str,
    cfg: &Value,
    default_cfg: &Value,
    schema: &[FieldDescriptor],
) -> Option<ResolvedSetting> {
    let (section, field) = split_key(key)?;
    if !schema
        .iter()
        .any(|d| d.section == section && d.field == field)
    {
        return None;
    }
    if let Some(plugin_id) = section_plugin_id(section) {
        Some(resolve_plugin_own(key, plugin_id, field, cfg))
    } else {
        Some(resolve_core(key, section, field, cfg, default_cfg))
    }
}

fn resolve_core(
    key: &str,
    section: &str,
    field: &str,
    cfg: &Value,
    default_cfg: &Value,
) -> ResolvedSetting {
    let schema_default = default_cfg
        .get(section)
        .and_then(|s| s.get(field))
        .cloned()
        .unwrap_or(Value::Null);
    let stored = cfg.get(section).and_then(|s| s.get(field)).cloned();

    // A user value is one that differs from the baseline default.
    let user = stored.filter(|v| *v != schema_default);

    let mut candidates = Vec::new();
    if let Some(v) = &user {
        candidates.push(Candidate {
            source: SettingSource::User,
            value: v.clone(),
        });
    }

    // Plugin overrides, in active-plugin order (builtins first). At Tier 0 these
    // are recorded as candidates so they are observable, but they do NOT win:
    // nothing applies a plugin's core-default override during real Config load
    // or merge yet, so the running app uses the user value or the struct
    // default. The runtime host applies these for real (#2095). Reporting one
    // as the effective value here would misrepresent what every core consumer
    // actually reads.
    for p in crate::plugin::registry().active() {
        if let Some(tv) = p.manifest.setting_defaults.get(key) {
            if let Ok(v) = serde_json::to_value(tv) {
                candidates.push(Candidate {
                    source: SettingSource::PluginDefault {
                        plugin: p.id().to_string(),
                    },
                    value: v,
                });
            }
        }
    }

    candidates.push(Candidate {
        source: SettingSource::SchemaDefault,
        value: schema_default.clone(),
    });

    // The effective value is what the app actually uses today: the user value,
    // else the struct default. Plugin core-default overrides stay in
    // `candidates` only.
    let (source, value) = match user {
        Some(v) => (SettingSource::User, v),
        None => (SettingSource::SchemaDefault, schema_default),
    };
    ResolvedSetting {
        key: key.to_string(),
        value,
        source,
        candidates,
    }
}

fn resolve_plugin_own(key: &str, plugin_id: &str, field: &str, cfg: &Value) -> ResolvedSetting {
    let mut candidates = Vec::new();

    if let Some(v) = super::plugin_storage_value(cfg, plugin_id, field) {
        candidates.push(Candidate {
            source: SettingSource::User,
            value: v.clone(),
        });
    }

    // The owning plugin's manifest default for this key.
    if let Some(p) = crate::plugin::registry().get(plugin_id) {
        if let Some(s) = p.manifest.settings.iter().find(|s| s.key == field) {
            if let Some(tv) = &s.default {
                if let Ok(v) = serde_json::to_value(tv) {
                    candidates.push(Candidate {
                        source: SettingSource::ManifestDefault {
                            plugin: plugin_id.to_string(),
                        },
                        value: v,
                    });
                }
            }
        }
    }

    if candidates.is_empty() {
        candidates.push(Candidate {
            source: SettingSource::ManifestDefault {
                plugin: plugin_id.to_string(),
            },
            value: Value::Null,
        });
    }

    finish(key, candidates)
}

fn finish(key: &str, candidates: Vec<Candidate>) -> ResolvedSetting {
    let winner = candidates.first().cloned().unwrap_or(Candidate {
        source: SettingSource::SchemaDefault,
        value: Value::Null,
    });
    ResolvedSetting {
        key: key.to_string(),
        value: winner.value,
        source: winner.source,
        candidates,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_key_takes_last_segment() {
        assert_eq!(
            split_key("acp.default_agent"),
            Some(("acp", "default_agent"))
        );
        assert_eq!(
            split_key("plugin:acme.kit.retries"),
            Some(("plugin:acme.kit", "retries"))
        );
        assert_eq!(split_key("nodot"), None);
    }

    #[test]
    fn unknown_key_resolves_to_none() {
        assert!(resolve("acp.totally_made_up").is_none());
        assert!(resolve("nodot").is_none());
    }

    #[test]
    fn core_field_falls_back_to_schema_default() {
        // With no user override and no plugin setting_defaults, a core field
        // resolves to its schema default.
        let r = resolve("acp.default_agent").expect("known core key");
        assert_eq!(r.source, SettingSource::SchemaDefault);
        assert_eq!(
            r.candidates.last().unwrap().source,
            SettingSource::SchemaDefault
        );
    }
}
