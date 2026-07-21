//! Plugin settings as virtual schema sections.
//!
//! A plugin's declared settings render through the same generic schema path as
//! core settings, under a virtual section id `plugin:<id>`. The API, validation,
//! TUI, and web all speak that flat `plugin:<id>.<key>` shape; only the disk
//! storage differs (a plugin value lives in `plugins.<id>.settings.<key>` in the
//! serialized `Config`). That section-id to storage-path translation is the one
//! thing that lives here, so no consumer scatters `starts_with("plugin:")`
//! checks of its own.

use aoe_plugin_api::{SettingContribution, SettingType};
use serde_json::{json, Value};

use super::{
    FieldDescriptor, ObjectFieldDescriptor, ObjectFieldWidget, OptionSource as SchemaOptionSource,
    SelectOption, ValidationKind, WebWritePolicy, WidgetKind,
};

/// Prefix marking a virtual plugin settings section.
pub const PLUGIN_SECTION_PREFIX: &str = "plugin:";

/// TUI category / web tab plugin settings sit under.
pub const PLUGIN_CATEGORY: &str = "Plugins";

/// The virtual schema section id for a plugin's settings.
pub fn plugin_section_id(plugin_id: &str) -> String {
    format!("{PLUGIN_SECTION_PREFIX}{plugin_id}")
}

/// The plugin id if `section` is a virtual plugin settings section.
pub fn section_plugin_id(section: &str) -> Option<&str> {
    section.strip_prefix(PLUGIN_SECTION_PREFIX)
}

/// Read a plugin setting's stored value from a serialized `Config` JSON value
/// (`plugins.<id>.settings.<field>`).
pub fn storage_value<'a>(root: &'a Value, plugin_id: &str, field: &str) -> Option<&'a Value> {
    root.get("plugins")?
        .get(plugin_id)?
        .get("settings")?
        .get(field)
}

/// The nested `Config`-shaped leaf that writes a plugin setting:
/// `{"plugins": {"<id>": {"settings": {"<field>": leaf}}}}`.
pub fn storage_leaf(plugin_id: &str, field: &str, leaf: Value) -> Value {
    json!({ "plugins": { plugin_id: { "settings": { field: leaf } } } })
}

/// Rewrite a settings PATCH body in place: every top-level `plugin:<id>`
/// section is folded into `plugins.<id>.settings.*`, matching on-disk storage.
/// Core sections are left untouched. Call after validation, before merge.
pub fn rewrite_plugin_sections(body: &mut Value) {
    let Some(obj) = body.as_object_mut() else {
        return;
    };
    let plugin_keys: Vec<String> = obj
        .keys()
        .filter(|k| k.starts_with(PLUGIN_SECTION_PREFIX))
        .cloned()
        .collect();
    if plugin_keys.is_empty() {
        return;
    }
    // Pull each plugin section out, remembering its id, then fold them into the
    // `plugins.<id>.settings` subtree. One mutable borrow of `obj` throughout.
    let mut sections = Vec::new();
    for key in plugin_keys {
        if let Some(section) = obj.remove(&key) {
            let id = key[PLUGIN_SECTION_PREFIX.len()..].to_string();
            sections.push((id, section));
        }
    }
    let plugins = obj
        .entry("plugins".to_string())
        .or_insert_with(|| json!({}));
    let Some(plugins) = plugins.as_object_mut() else {
        return;
    };
    for (id, section) in sections {
        let entry = plugins.entry(id).or_insert_with(|| json!({}));
        let Some(entry) = entry.as_object_mut() else {
            continue;
        };
        let settings = entry
            .entry("settings".to_string())
            .or_insert_with(|| json!({}));
        if let (Some(settings), Some(section)) = (settings.as_object_mut(), section.as_object()) {
            for (k, v) in section {
                settings.insert(k.clone(), v.clone());
            }
        }
    }
}

/// Build the schema descriptors for one plugin's declared settings.
pub fn plugin_field_descriptors(
    plugin_id: &str,
    settings: &[SettingContribution],
) -> Vec<FieldDescriptor> {
    let section = plugin_section_id(plugin_id);
    settings
        .iter()
        .map(|s| {
            let (widget, validation) = widget_and_validation(s);
            FieldDescriptor {
                section: section.clone(),
                field: s.key.clone(),
                category: PLUGIN_CATEGORY.to_string(),
                label: if s.label.is_empty() {
                    s.key.clone()
                } else {
                    s.label.clone()
                },
                description: s.description.clone(),
                widget,
                // Plugin settings are not host-execution surfaces; the settings
                // PATCH endpoint is already elevation-gated by the auth layer.
                web_write: WebWritePolicy::Allow,
                // Global-only at Tier 0: a plugin setting has one value, stored
                // in the global config, no per-profile override.
                profile_overridable: false,
                validation,
                advanced: s.advanced,
                default: s
                    .default
                    .as_ref()
                    .and_then(|t| serde_json::to_value(t).ok()),
            }
        })
        .collect()
}

fn widget_and_validation(s: &SettingContribution) -> (WidgetKind, ValidationKind) {
    match s.value_type {
        SettingType::Bool => (WidgetKind::Toggle, ValidationKind::None),
        SettingType::String => (
            WidgetKind::Text {
                multiline: s.multiline,
                mono: false,
            },
            ValidationKind::None,
        ),
        SettingType::Integer => {
            let widget = WidgetKind::Number {
                min: s.min,
                max: s.max,
            };
            // RangeU64 is the only integer gate the host has; use it when the
            // declared bounds are non-negative (the common plugin counter
            // case), otherwise leave it ungated rather than misreport a signed
            // range.
            let validation = if s.min.unwrap_or(0) >= 0 && s.max.unwrap_or(0) >= 0 {
                ValidationKind::RangeU64 {
                    min: s.min.unwrap_or(0) as u64,
                    max: s.max.map(|m| m as u64),
                }
            } else {
                ValidationKind::None
            };
            (widget, validation)
        }
        SettingType::Select => (
            WidgetKind::Select {
                options: s.options.iter().map(|o| SelectOption::new(o, o)).collect(),
            },
            // Gate the value against the declared options server-side so an
            // off-menu value can never reach storage.
            ValidationKind::OneOf {
                options: s.options.clone(),
            },
        ),
        SettingType::DynamicSelect => (
            WidgetKind::DynamicSelect {
                // Manifest validation guarantees a source on a dynamic_select;
                // default to acp.agents defensively rather than panic.
                source: s
                    .option_source
                    .map(SchemaOptionSource::from)
                    .unwrap_or(SchemaOptionSource::AcpAgents),
                depends_on: s.depends_on.clone(),
            },
            // Options are host-resolved and revalidated at sessions.create; the
            // settings-write gate only enforces that a chosen value is a string
            // (never a number/object smuggled past widget metadata).
            ValidationKind::StringValue,
        ),
        SettingType::Cron => (WidgetKind::Cron, ValidationKind::Cron),
        SettingType::ObjectList => {
            let fields: Vec<ObjectFieldDescriptor> =
                s.fields.iter().map(object_field_descriptor).collect();
            let id_field = s.item_id_key.clone().unwrap_or_else(|| "_id".to_string());
            (
                WidgetKind::ObjectList {
                    id_field: id_field.clone(),
                    fields: fields.clone(),
                    min_items: s.min_items,
                    max_items: s.max_items,
                },
                ValidationKind::ObjectList {
                    id_field,
                    fields,
                    min_items: s.min_items,
                    max_items: s.max_items,
                },
            )
        }
    }
}

/// Map one manifest object-list item field to its runtime descriptor. The
/// widget cannot be an object list, so the mapping is total and non-recursive.
fn object_field_descriptor(f: &aoe_plugin_api::ObjectFieldContribution) -> ObjectFieldDescriptor {
    use aoe_plugin_api::ObjectFieldType as T;
    let (widget, validation) = match f.value_type {
        T::Bool => (ObjectFieldWidget::Toggle, ValidationKind::BoolValue),
        T::String => (
            ObjectFieldWidget::Text {
                multiline: f.multiline,
                mono: false,
            },
            if f.required {
                ValidationKind::NonEmptyString
            } else {
                ValidationKind::StringValue
            },
        ),
        T::Integer => (
            ObjectFieldWidget::Number {
                min: f.min,
                max: f.max,
            },
            if f.min.unwrap_or(0) >= 0 && f.max.unwrap_or(0) >= 0 {
                ValidationKind::RangeU64 {
                    min: f.min.unwrap_or(0) as u64,
                    max: f.max.map(|m| m as u64),
                }
            } else {
                // Bounds go negative; RangeU64 cannot express them.
                ValidationKind::RangeI64 {
                    min: f.min,
                    max: f.max,
                }
            },
        ),
        T::Select => (
            ObjectFieldWidget::Select {
                options: f.options.iter().map(|o| SelectOption::new(o, o)).collect(),
            },
            ValidationKind::OneOf {
                options: f.options.clone(),
            },
        ),
        T::DynamicSelect => (
            ObjectFieldWidget::DynamicSelect {
                source: f
                    .option_source
                    .map(SchemaOptionSource::from)
                    .unwrap_or(SchemaOptionSource::AcpAgents),
                depends_on: f.depends_on.clone(),
            },
            // Host-resolved + revalidated at sessions.create; non-empty when
            // required, otherwise just enforce the string type.
            if f.required {
                ValidationKind::NonEmptyString
            } else {
                ValidationKind::StringValue
            },
        ),
        T::DynamicMultiSelect => (
            ObjectFieldWidget::DynamicMultiSelect {
                source: f
                    .option_source
                    .map(SchemaOptionSource::from)
                    .unwrap_or(SchemaOptionSource::Projects),
                depends_on: f.depends_on.clone(),
            },
            // Host-resolved list, revalidated at sessions.create; enforce only
            // the array-of-strings shape here.
            ValidationKind::StringListValue,
        ),
        T::Cron => (ObjectFieldWidget::Cron, ValidationKind::Cron),
    };
    ObjectFieldDescriptor {
        field: f.key.clone(),
        label: if f.label.is_empty() {
            f.key.clone()
        } else {
            f.label.clone()
        },
        description: f.description.clone(),
        required: f.required,
        widget,
        validation,
        default: f
            .default
            .as_ref()
            .and_then(|t| serde_json::to_value(t).ok()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn contrib(key: &str, ty: SettingType) -> SettingContribution {
        SettingContribution {
            key: key.to_string(),
            label: String::new(),
            description: String::new(),
            value_type: ty,
            options: Vec::new(),
            min: None,
            max: None,
            default: None,
            advanced: false,
            multiline: false,
            option_source: None,
            depends_on: Vec::new(),
            fields: Vec::new(),
            item_id_key: None,
            min_items: None,
            max_items: None,
        }
    }

    #[test]
    fn section_id_round_trips() {
        let id = plugin_section_id("acme.kit");
        assert_eq!(id, "plugin:acme.kit");
        assert_eq!(section_plugin_id(&id), Some("acme.kit"));
        assert_eq!(section_plugin_id("acp"), None);
    }

    #[test]
    fn descriptors_map_types_to_widgets() {
        let mut s_int = contrib("retries", SettingType::Integer);
        s_int.min = Some(0);
        s_int.max = Some(9);
        s_int.default = Some(toml::Value::Integer(3));
        let descs = plugin_field_descriptors(
            "acme.kit",
            &[
                contrib("on", SettingType::Bool),
                contrib("name", SettingType::String),
                s_int,
                SettingContribution {
                    options: vec!["a".into(), "b".into()],
                    ..contrib("mode", SettingType::Select)
                },
            ],
        );
        assert_eq!(descs[0].section, "plugin:acme.kit");
        assert!(matches!(descs[0].widget, WidgetKind::Toggle));
        assert!(matches!(descs[1].widget, WidgetKind::Text { .. }));
        assert!(matches!(
            descs[2].widget,
            WidgetKind::Number {
                min: Some(0),
                max: Some(9)
            }
        ));
        assert!(matches!(
            descs[2].validation,
            ValidationKind::RangeU64 {
                min: 0,
                max: Some(9)
            }
        ));
        assert_eq!(descs[2].default, Some(serde_json::json!(3)));
        assert!(matches!(descs[3].widget, WidgetKind::Select { .. }));
        // Label falls back to the key when unset.
        assert_eq!(descs[0].label, "on");
        // Global-only at Tier 0.
        assert!(!descs[0].profile_overridable);
    }

    #[test]
    fn rewrite_folds_plugin_sections_into_storage() {
        let mut body = json!({
            "theme": { "idle_decay_minutes": 5 },
            "plugin:acme.kit": { "retries": 4, "mode": "fast" },
        });
        rewrite_plugin_sections(&mut body);
        assert_eq!(body["theme"]["idle_decay_minutes"], json!(5));
        assert!(body.get("plugin:acme.kit").is_none());
        assert_eq!(body["plugins"]["acme.kit"]["settings"]["retries"], json!(4));
        assert_eq!(
            body["plugins"]["acme.kit"]["settings"]["mode"],
            json!("fast")
        );
    }

    #[test]
    fn storage_helpers_round_trip() {
        let mut cfg = json!({});
        let leaf = storage_leaf("acme.kit", "retries", json!(7));
        super::super::merge_json(&mut cfg, &leaf);
        assert_eq!(storage_value(&cfg, "acme.kit", "retries"), Some(&json!(7)));
    }

    #[test]
    fn plugin_patch_validates_then_rewrites_to_storage() {
        // A plugin section validates against runtime descriptors through the
        // same gate as core, then folds into its storage path before merge.
        let mut s_int = contrib("retries", SettingType::Integer);
        s_int.min = Some(0);
        s_int.max = Some(5);
        let descriptors = plugin_field_descriptors("acme.kit", &[s_int]);

        let good = json!({ "plugin:acme.kit": { "retries": 4 } });
        assert!(super::super::validate_patch_with(
            &descriptors,
            &good,
            super::super::Scope::Global,
            true
        )
        .is_ok());

        // Out-of-range is rejected by the derived RangeU64 gate.
        let bad = json!({ "plugin:acme.kit": { "retries": 9 } });
        assert!(super::super::validate_patch_with(
            &descriptors,
            &bad,
            super::super::Scope::Global,
            true
        )
        .is_err());

        // Unknown plugin field is rejected.
        let unknown = json!({ "plugin:acme.kit": { "nope": 1 } });
        assert!(super::super::validate_patch_with(
            &descriptors,
            &unknown,
            super::super::Scope::Global,
            true
        )
        .is_err());

        let mut body = good;
        rewrite_plugin_sections(&mut body);
        assert_eq!(body["plugins"]["acme.kit"]["settings"]["retries"], json!(4));
    }

    #[test]
    fn select_value_is_gated_against_options() {
        let descriptors = plugin_field_descriptors(
            "acme.kit",
            &[SettingContribution {
                options: vec!["fast".into(), "slow".into()],
                ..contrib("mode", SettingType::Select)
            }],
        );
        // An on-menu value passes; an off-menu value is rejected.
        assert!(super::super::validate_patch_with(
            &descriptors,
            &json!({ "plugin:acme.kit": { "mode": "fast" } }),
            super::super::Scope::Global,
            true
        )
        .is_ok());
        assert!(super::super::validate_patch_with(
            &descriptors,
            &json!({ "plugin:acme.kit": { "mode": "turbo" } }),
            super::super::Scope::Global,
            true
        )
        .is_err());
    }
}
