//! The settings schema: field descriptors emitted by `#[derive(SettingsSection)]`
//! that drive the TUI, web dashboard, server validation, and override merging.
//! See docs/development/adding-settings.md.

use serde::{Deserialize, Serialize};

mod merge;
mod plugin;
mod policy;
mod registry;
mod resolved;
mod validate;

pub use merge::{apply_changed_leaves, clear_path, merge_json};
pub use plugin::{
    plugin_field_descriptors, plugin_section_id, rewrite_plugin_sections, section_plugin_id,
    storage_leaf as plugin_storage_leaf, storage_value as plugin_storage_value, PLUGIN_CATEGORY,
    PLUGIN_SECTION_PREFIX,
};
pub use policy::{strip_local_only, validate_patch, validate_patch_with, PatchRejection, Scope};
pub use registry::{descriptor, runtime_schema, schema, schema_ref, section_in_schema};
pub use resolved::{resolve, resolve_all, Candidate, ResolvedSetting, SettingSource};
pub use validate::{validate_value, ValidationError};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum WidgetKind {
    Toggle,
    Text {
        #[serde(default)]
        multiline: bool,
        #[serde(default)]
        mono: bool,
    },
    /// Clearing it stores null.
    OptionalText {
        #[serde(default)]
        mono: bool,
    },
    /// Bounds are advisory; [`ValidationKind`] is the server gate.
    Number {
        #[serde(skip_serializing_if = "Option::is_none")]
        min: Option<i64>,
        #[serde(skip_serializing_if = "Option::is_none")]
        max: Option<i64>,
    },
    Slider {
        min: i64,
        max: i64,
        step: i64,
    },
    Select {
        options: Vec<SelectOption>,
    },
    List,
    /// Choices resolved by the host at render time, parameterized by sibling `depends_on` fields.
    DynamicSelect {
        source: OptionSource,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        depends_on: Vec<String>,
    },
    /// Items are objects with a stable id under `id_field`; one level deep.
    ObjectList {
        id_field: String,
        fields: Vec<ObjectFieldDescriptor>,
        #[serde(skip_serializing_if = "Option::is_none")]
        min_items: Option<u32>,
        #[serde(skip_serializing_if = "Option::is_none")]
        max_items: Option<u32>,
    },
    Cron,
    /// A bespoke widget registered under `id` on both the web and TUI.
    Custom {
        id: String,
    },
}

/// Mirrors `aoe_plugin_api::OptionSource`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OptionSource {
    AcpAgents,
    AcpModels,
    AcpModes,
    Projects,
    Groups,
}

impl From<aoe_plugin_api::OptionSource> for OptionSource {
    fn from(s: aoe_plugin_api::OptionSource) -> Self {
        use aoe_plugin_api::OptionSource as A;
        match s {
            A::AcpAgents => Self::AcpAgents,
            A::AcpModels => Self::AcpModels,
            A::AcpModes => Self::AcpModes,
            A::Projects => Self::Projects,
            A::Groups => Self::Groups,
        }
    }
}

/// A field of a [`WidgetKind::ObjectList`] item.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ObjectFieldDescriptor {
    pub field: String,
    pub label: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub description: String,
    #[serde(default)]
    pub required: bool,
    pub widget: ObjectFieldWidget,
    pub validation: ValidationKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default: Option<serde_json::Value>,
}

/// A subset of [`WidgetKind`] without object lists, keeping the schema non-recursive.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ObjectFieldWidget {
    Toggle,
    Text {
        #[serde(default)]
        multiline: bool,
        #[serde(default)]
        mono: bool,
    },
    Number {
        #[serde(skip_serializing_if = "Option::is_none")]
        min: Option<i64>,
        #[serde(skip_serializing_if = "Option::is_none")]
        max: Option<i64>,
    },
    Select {
        options: Vec<SelectOption>,
    },
    DynamicSelect {
        source: OptionSource,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        depends_on: Vec<String>,
    },
    /// Stores an array of chosen option values.
    DynamicMultiSelect {
        source: OptionSource,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        depends_on: Vec<String>,
    },
    Cron,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SelectOption {
    pub value: String,
    pub label: String,
}

impl SelectOption {
    pub fn new(value: &str, label: &str) -> Self {
        Self {
            value: value.to_string(),
            label: label.to_string(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "policy", rename_all = "snake_case")]
pub enum WebWritePolicy {
    Allow,
    /// Needs passphrase elevation.
    RequiresElevation {
        reason: String,
    },
    /// A host execution surface the server never accepts from the web.
    LocalOnly {
        reason: String,
    },
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum RepoPolicy {
    Allow,
    /// Default, so an undescribed field fails closed.
    #[default]
    Deny,
}

/// Server-authoritative validation applied before a value is merged.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "rule", rename_all = "snake_case")]
pub enum ValidationKind {
    None,
    /// Inclusive bounds.
    RangeU64 {
        min: u64,
        max: Option<u64>,
    },
    /// Non-empty after trimming.
    NonEmptyString,
    /// Any JSON string; type-only check for host-resolved values.
    #[serde(rename = "str")]
    StringValue,
    /// Any array of strings; type-only check for host-resolved values.
    #[serde(rename = "str_list")]
    StringListValue,
    #[serde(rename = "bool")]
    BoolValue,
    /// Signed inclusive range with optional bounds.
    RangeI64 {
        min: Option<i64>,
        max: Option<i64>,
    },
    /// Docker memory-limit grammar (`512m`, `2g`); empty allowed.
    MemoryLimit,
    /// `host:container[:options]` entries.
    VolumeList,
    /// Bare `KEY` or `KEY=VALUE` entries.
    EnvList,
    /// `host:container` numeric port entries.
    PortMappingList,
    CapabilityList,
    SecurityOptList,
    /// Empty, `none`, `bridge`, or a named network; `host` would defeat isolation.
    Network,
    /// A closed set, for plugin selects (core selects carry options in the widget).
    OneOf {
        options: Vec<String>,
    },
    /// A 5-field cron expression in the plugin scheduler's `croner` dialect.
    Cron,
    /// Item count, unique non-empty ids, declared and required fields, and each field's own rule.
    ObjectList {
        id_field: String,
        fields: Vec<ObjectFieldDescriptor>,
        min_items: Option<u32>,
        max_items: Option<u32>,
    },
}

/// One configurable field, emitted by the `SettingsSection` derive.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FieldDescriptor {
    /// The `[section]` table in `config.toml` and the profile override key.
    pub section: String,
    pub field: String,
    /// TUI tab.
    pub category: String,
    pub label: String,
    pub description: String,
    pub widget: WidgetKind,
    pub web_write: WebWritePolicy,
    #[serde(skip)]
    pub repo_policy: RepoPolicy,
    /// `false` for global-only fields.
    pub profile_overridable: bool,
    pub validation: ValidationKind,
    /// Shown under an "Advanced" fold on both surfaces.
    #[serde(default)]
    pub advanced: bool,
    /// Manifest default for plugin fields; core fields always have a value in `Config`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default: Option<serde_json::Value>,
}

impl FieldDescriptor {
    /// `section.field`, the stable id in the web payload.
    pub fn path(&self) -> String {
        format!("{}.{}", self.section, self.field)
    }
}
