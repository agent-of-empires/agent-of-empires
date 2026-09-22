//! Settings resolution with provenance, for `aoe settings explain` and `GET /api/settings/resolved`.
//!
//! Core keys: the user value, else the schema default (plugin `setting_defaults`
//! are reported but never applied). Plugin keys: the stored value, else the manifest default.

use serde::Serialize;
use serde_json::Value;

use super::{runtime_schema, section_plugin_id, FieldDescriptor};

/// Where a resolved value came from.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SettingSource {
    User,
    /// Reported only; never applied to runtime config.
    PluginDefault {
        plugin: String,
    },
    ManifestDefault {
        plugin: String,
    },
    SchemaDefault,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Candidate {
    pub source: SettingSource,
    pub value: Value,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ResolvedSetting {
    /// `section.field` or `plugin:<id>.<key>`.
    pub key: String,
    pub value: Value,
    pub source: SettingSource,
    /// Highest precedence first.
    pub candidates: Vec<Candidate>,
}

/// The field is the last segment, since plugin sections may contain dots.
fn split_key(key: &str) -> Option<(&str, &str)> {
    key.rsplit_once('.')
}

/// `None` for an unknown key.
pub fn resolve(key: &str) -> Option<ResolvedSetting> {
    let cfg = serde_json::to_value(crate::session::Config::load_or_warn()).ok()?;
    let default_cfg = serde_json::to_value(crate::session::Config::default()).ok()?;
    resolve_with(key, &cfg, &default_cfg, &runtime_schema())
}

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

    let user = stored.filter(|v| *v != schema_default);

    let mut candidates = Vec::new();
    if let Some(v) = &user {
        candidates.push(Candidate {
            source: SettingSource::User,
            value: v.clone(),
        });
    }

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
        let _app = crate::session::test_support::isolate_app_dir();
        let r = resolve("acp.default_agent").expect("known core key");
        assert_eq!(r.source, SettingSource::SchemaDefault);
        assert_eq!(
            r.candidates.last().unwrap().source,
            SettingSource::SchemaDefault
        );
    }
}
