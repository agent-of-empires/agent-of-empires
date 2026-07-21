//! Single source of truth for the settings surface (#1692).
//!
//! Each configurable field is declared once on its `Config` sub-struct via
//! `#[derive(SettingsSection)]` + `#[setting(...)]` (see the `aoe-settings-derive`
//! crate). The derive emits a flat list of [`FieldDescriptor`]s. Every surface
//! consumes that list instead of hand-wiring itself:
//!
//! - TUI settings screen builds its rows from the descriptors (no per-field
//!   `build_*_fields` / `apply_field_*` match arms).
//! - The web dashboard fetches the descriptors over `GET /api/settings/schema`
//!   and renders a generic field component (no hand-written JSX per field).
//! - The server derives its web-write allowlist / blocklist and per-field
//!   validation from the descriptors (no hand-kept `ALLOWED_*_SECTIONS` /
//!   `*_BLOCKED_FIELDS`).
//!
//! Profile and repo overrides are stored as sparse JSON ([`merge_json`]),
//! so adding a field never touches an override struct or a merge arm.

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
pub use registry::{descriptor, runtime_schema, schema};
pub use resolved::{resolve, resolve_all, Candidate, ResolvedSetting, SettingSource};
pub use validate::{validate_value, ValidationError};

/// Widget the surfaces render for a field. The variant carries everything a
/// generic renderer needs; `serde` tags it so the web payload is self-describing.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum WidgetKind {
    /// On/off switch backed by a `bool`.
    Toggle,
    /// Free-text backed by a `String`. Empty string is a valid value.
    Text {
        #[serde(default)]
        multiline: bool,
        #[serde(default)]
        mono: bool,
    },
    /// Optional free-text backed by `Option<String>`; clearing it stores null.
    OptionalText {
        #[serde(default)]
        mono: bool,
    },
    /// Integer input with optional bounds (advisory on the web, authoritative
    /// on the server via [`ValidationKind`]).
    Number {
        #[serde(skip_serializing_if = "Option::is_none")]
        min: Option<i64>,
        #[serde(skip_serializing_if = "Option::is_none")]
        max: Option<i64>,
    },
    /// Bounded integer rendered as a slider.
    Slider { min: i64, max: i64, step: i64 },
    /// Closed set of string values. `value` is the serialized form written to
    /// disk; `label` is shown to the user.
    Select { options: Vec<SelectOption> },
    /// List of strings (volumes, env entries, ...).
    List,
    /// A select whose options the host resolves at render time from an
    /// [`OptionSource`] (API v9, #2897), optionally parameterized by sibling
    /// fields named in `depends_on`. The plugin never ships the choices.
    DynamicSelect {
        source: OptionSource,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        depends_on: Vec<String>,
    },
    /// A repeatable list of structured items (API v9, #2897). Each item is a
    /// JSON object keyed by the nested field names, carrying a stable id under
    /// `id_field`. One level deep: `fields` cannot themselves be object lists.
    ObjectList {
        id_field: String,
        fields: Vec<ObjectFieldDescriptor>,
        #[serde(skip_serializing_if = "Option::is_none")]
        min_items: Option<u32>,
        #[serde(skip_serializing_if = "Option::is_none")]
        max_items: Option<u32>,
    },
    /// A cron expression, rendered as a validated text field (API v9, #2897).
    Cron,
    /// Escape hatch: a bespoke widget keyed by `id`. The web and TUI keep a
    /// registry mapping the id to a hand-written component (e.g. the logging
    /// per-target matrix). The field stays in the schema so it is never
    /// silently web-unwritable.
    Custom { id: String },
}

/// A host option source a [`WidgetKind::DynamicSelect`] draws its choices
/// from (#2897). Mirrors `aoe_plugin_api::OptionSource`; the host resolver
/// maps each variant to the corresponding daemon state.
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

/// One nested field of a [`WidgetKind::ObjectList`] item (#2897). A restricted
/// mirror of [`FieldDescriptor`] whose widget cannot be another object list,
/// so the schema is non-recursive.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ObjectFieldDescriptor {
    pub field: String,
    pub label: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub description: String,
    /// Whether the item must carry a non-empty value for this field.
    #[serde(default)]
    pub required: bool,
    pub widget: ObjectFieldWidget,
    pub validation: ValidationKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default: Option<serde_json::Value>,
}

/// The widget for an object-list item field. Deliberately a subset of
/// [`WidgetKind`] with no object-list variant, enforcing the one-level bound
/// in both the Rust type and the serialized schema.
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
    /// A host-resolved multi-select; the stored value is an array of chosen
    /// option values (API v11). Choices resolve like `DynamicSelect`.
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

/// Whether the web dashboard may write a field, and why not when it cannot.
/// This replaces the hand-kept section allowlist + `*_BLOCKED_FIELDS`: the
/// server derives both from the schema, and the pinning tests assert the
/// derived sets match (so loosening a policy is a loud, test-breaking change).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "policy", rename_all = "snake_case")]
pub enum WebWritePolicy {
    /// Writable by any authenticated dashboard client.
    Allow,
    /// Writable only after passphrase elevation (matches the existing
    /// `ELEVATION_REQUIRED_SECTIONS` gate).
    RequiresElevation { reason: String },
    /// Never writable from the web: a host-side execution surface (binary
    /// path, argv, env injection). The server rejects a PATCH touching it.
    LocalOnly { reason: String },
}

/// Server-authoritative validation applied to an incoming value before it is
/// merged. Min/max in [`WidgetKind`] is advisory UI metadata; this is the gate.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "rule", rename_all = "snake_case")]
pub enum ValidationKind {
    None,
    /// Inclusive lower bound; `max` is an optional inclusive upper bound.
    RangeU64 {
        min: u64,
        max: Option<u64>,
    },
    /// Non-empty after trimming.
    NonEmptyString,
    /// Value must be a JSON string (any content, empty allowed). Used for
    /// host-resolved optional `dynamic_select` values (revalidated at
    /// `sessions.create`): enforces the type without constraining content, so a
    /// number or object cannot be smuggled in (API v9, #2897).
    #[serde(rename = "str")]
    StringValue,
    /// Value must be a JSON array whose entries are all strings (any content,
    /// empty allowed). Used for host-resolved `dynamic_multi_select` values
    /// (revalidated at `sessions.create`): enforces the array-of-strings type
    /// without constraining membership (API v11).
    #[serde(rename = "str_list")]
    StringListValue,
    /// Value must be a JSON boolean (API v9, #2897).
    #[serde(rename = "bool")]
    BoolValue,
    /// Signed inclusive integer range; either bound optional for single-sided
    /// ranges. Used for `object_list` integer fields whose declared bounds go
    /// negative, which `RangeU64` cannot express (API v9, #2897).
    RangeI64 {
        min: Option<i64>,
        max: Option<i64>,
    },
    /// Docker memory-limit grammar (`512m`, `2g`, ...). Empty allowed.
    MemoryLimit,
    /// Each list entry must be `host:container[:options]`.
    VolumeList,
    /// Each list entry must be a sandbox env entry: bare `KEY` or `KEY=VALUE`
    /// (key is letters, digits, underscores; must not start with a digit).
    EnvList,
    /// Each list entry must be a `host:container` port mapping (digits only).
    PortMappingList,
    /// A container network mode: empty, `none`, `bridge`, or a named network
    /// (`[a-zA-Z0-9][a-zA-Z0-9_.-]*`). `host` and other namespace-sharing
    /// forms are rejected because they defeat sandbox isolation.
    Network,
    /// Value must be one of a closed set of strings. Used by plugin `select`
    /// settings so an off-menu value cannot be persisted (core selects encode
    /// their options in the widget and need no separate rule).
    OneOf {
        options: Vec<String>,
    },
    /// A 5-field cron expression (API v9, #2897). Empty is rejected; the
    /// grammar matches the plugin scheduler's `croner` dialect.
    Cron,
    /// A repeatable list of structured items (API v9, #2897). Validated
    /// recursively: item count bounds, a unique non-empty id per item under
    /// `id_field`, only declared fields, required fields present, and each
    /// field against its own descriptor. Carries the item schema so the
    /// server validates without re-deriving it from the manifest.
    ObjectList {
        id_field: String,
        fields: Vec<ObjectFieldDescriptor>,
        min_items: Option<u32>,
        max_items: Option<u32>,
    },
}

/// One configurable field, emitted by the `SettingsSection` derive. Owned
/// strings so the web payload serializes directly and the TUI can format
/// without lifetime juggling.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FieldDescriptor {
    /// Top-level config section, e.g. `"acp"`. Matches the `[section]`
    /// table in `config.toml` and the override key in a profile.
    pub section: String,
    /// Field name within the section, e.g. `"max_concurrent_workers"`.
    pub field: String,
    /// TUI settings category label (which tab the row appears under).
    pub category: String,
    pub label: String,
    pub description: String,
    pub widget: WidgetKind,
    pub web_write: WebWritePolicy,
    /// Whether a profile/repo may override this field. `false` means the
    /// value is global-only (the field is still shown, but not overridable).
    pub profile_overridable: bool,
    pub validation: ValidationKind,
    /// Operational tuning that sits under an "Advanced" fold in both surfaces.
    /// The web groups advanced fields into a collapsible section; the TUI
    /// renders them after the primary fields under an "Advanced" divider.
    #[serde(default)]
    pub advanced: bool,
    /// The field's default value, shown when no value is stored yet. Core
    /// fields leave this `None` (their value always exists in the serialized
    /// `Config` via the struct's `Default`); plugin fields carry the
    /// manifest-declared default so the surfaces and the resolution chain show
    /// it before the user has saved anything.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default: Option<serde_json::Value>,
}

impl FieldDescriptor {
    /// Dotted path used as the stable id in the web payload and for path-based
    /// lookups against a serialized `Config` value.
    pub fn path(&self) -> String {
        format!("{}.{}", self.section, self.field)
    }
}
