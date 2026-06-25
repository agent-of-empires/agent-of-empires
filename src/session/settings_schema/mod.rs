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

pub use merge::{clear_path, merge_json};
pub use plugin::{
    plugin_field_descriptors, plugin_section_id, rewrite_plugin_sections, section_plugin_id,
    storage_leaf as plugin_storage_leaf, storage_value as plugin_storage_value, PLUGIN_CATEGORY,
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
    /// Escape hatch: a bespoke widget keyed by `id`. The web and TUI keep a
    /// registry mapping the id to a hand-written component (e.g. the logging
    /// per-target matrix). The field stays in the schema so it is never
    /// silently web-unwritable.
    Custom { id: String },
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
    /// Docker memory-limit grammar (`512m`, `2g`, ...). Empty allowed.
    MemoryLimit,
    /// Each list entry must be `host:container[:options]`.
    VolumeList,
    /// Each list entry must be a sandbox env entry: bare `KEY` or `KEY=VALUE`
    /// (key is letters, digits, underscores; must not start with a digit).
    EnvList,
    /// Each list entry must be a `host:container` port mapping (digits only).
    PortMappingList,
    /// Value must be one of a closed set of strings. Used by plugin `select`
    /// settings so an off-menu value cannot be persisted (core selects encode
    /// their options in the widget and need no separate rule).
    OneOf {
        options: Vec<String>,
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
