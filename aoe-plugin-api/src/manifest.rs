use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::{CapabilityId, PluginId, API_VERSION};

/// Parsed and validated `aoe-plugin.toml`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[non_exhaustive]
pub struct PluginManifest {
    pub id: PluginId,
    /// Human-readable display name.
    pub name: String,
    pub version: String,
    /// Manifest schema / host API version this manifest targets.
    pub api_version: u32,
    #[serde(default)]
    pub description: String,

    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub screenshots: Vec<Screenshot>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub icon: Option<String>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub icon_asset: Option<String>,

    /// Runtime resource/effect capabilities, not static contributions.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub capabilities: Vec<CapabilityId>,

    /// Commands the plugin contributes to the palette and CLI.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub commands: Vec<CommandContribution>,

    /// Keybinds the plugin contributes.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub keybinds: Vec<KeybindContribution>,

    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub settings: Vec<SettingContribution>,

    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub setting_defaults: BTreeMap<String, toml::Value>,

    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub themes: Vec<ThemeContribution>,

    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub status: Vec<StatusContribution>,

    /// Host-rendered UI slots the plugin may populate.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub ui: Vec<UiContribution>,

    /// The worker entrypoint.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub runtime: Option<RuntimeSpec>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub aoe_version: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CommandContribution {
    pub id: String,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub description: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub action: Option<ClientAction>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum ClientAction {
    OpenUiLink { slot: UiSlot, id: String },
}

/// A keybind the plugin contributes, binding a key chord to a command.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KeybindContribution {
    /// Command id this binds to (a plugin command or a core command).
    pub command: String,
    /// Key chord, e.g. `Ctrl+K`.
    pub key: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SettingContribution {
    pub key: String,
    #[serde(default)]
    pub label: String,
    #[serde(default)]
    pub description: String,
    /// Value type. Drives the rendered widget and server-side validation.
    #[serde(rename = "type", default)]
    pub value_type: SettingType,
    /// Allowed values for a `select`; ignored otherwise.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub options: Vec<String>,
    /// Inclusive bounds for an `integer`; ignored otherwise.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default: Option<toml::Value>,
    /// Group under an "Advanced" fold on the settings surfaces.
    #[serde(default)]
    pub advanced: bool,
    #[serde(default)]
    pub multiline: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub option_source: Option<OptionSource>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub depends_on: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub fields: Vec<ObjectFieldContribution>,
    /// The item field that holds each `object_list` row's stable id (API v9).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub item_id_key: Option<String>,
    /// Inclusive item-count bounds for an `object_list` (API v9).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_items: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_items: Option<u32>,
}

/// A host option source a `dynamic_select` draws its choices from (API v9).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum OptionSource {
    #[serde(rename = "acp.agents")]
    AcpAgents,
    #[serde(rename = "acp.models")]
    AcpModels,
    #[serde(rename = "acp.modes")]
    AcpModes,
    #[serde(rename = "projects")]
    Projects,
    #[serde(rename = "groups")]
    Groups,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ObjectFieldContribution {
    pub key: String,
    #[serde(default)]
    pub label: String,
    #[serde(default)]
    pub description: String,
    #[serde(rename = "type", default)]
    pub value_type: ObjectFieldType,
    /// Whether the item must carry a non-empty value for this field.
    #[serde(default)]
    pub required: bool,
    /// Render a `string` field as a multi-line textarea. Ignored otherwise. v11.
    #[serde(default)]
    pub multiline: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub options: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default: Option<toml::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub option_source: Option<OptionSource>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub depends_on: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ObjectFieldType {
    #[default]
    String,
    #[serde(alias = "boolean")]
    Bool,
    Integer,
    Select,
    DynamicSelect,
    DynamicMultiSelect,
    Cron,
}

fn validate_object_list_settings(
    i: usize,
    s: &SettingContribution,
    setting_keys: &std::collections::HashSet<&str>,
    agent_setting_keys: &std::collections::HashSet<&str>,
    check: &mut impl FnMut(bool, String),
) {
    match s.value_type {
        SettingType::DynamicSelect => {
            check(
                s.option_source.is_some(),
                format!("settings[{i}] is a dynamic_select but declares no option_source"),
            );
            check(
                s.fields.is_empty(),
                format!("settings[{i}] is a dynamic_select and must not declare object fields"),
            );
            let mut dep_seen = std::collections::HashSet::new();
            for dep in &s.depends_on {
                check(
                    dep != &s.key,
                    format!("settings[{i}]: depends_on must not reference itself"),
                );
                check(
                    setting_keys.contains(dep.as_str()),
                    format!("settings[{i}]: depends_on {dep:?} is not a sibling setting"),
                );
                check(
                    dep_seen.insert(dep.as_str()),
                    format!("settings[{i}]: depends_on {dep:?} is listed twice"),
                );
            }
            if matches!(
                s.option_source,
                Some(OptionSource::AcpModels) | Some(OptionSource::AcpModes)
            ) {
                check(
                    s.depends_on
                        .iter()
                        .any(|d| agent_setting_keys.contains(d.as_str())),
                    format!(
                        "settings[{i}]: acp.models/acp.modes require a depends_on referencing an acp.agents setting"
                    ),
                );
            }
        }
        SettingType::ObjectList => {
            check(
                !s.fields.is_empty(),
                format!("settings[{i}] is an object_list but declares no fields"),
            );
            check(
                s.option_source.is_none() && s.depends_on.is_empty(),
                format!("settings[{i}] is an object_list; option_source/depends_on belong on its fields, not the list"),
            );
            check(
                match (s.min_items, s.max_items) {
                    (Some(lo), Some(hi)) => lo <= hi,
                    _ => true,
                },
                format!("settings[{i}].min_items must not exceed max_items"),
            );
            let id_key = s.item_id_key.as_deref().unwrap_or("_id");
            check(
                !id_key.trim().is_empty(),
                format!("settings[{i}].item_id_key must not be empty"),
            );
            let field_keys: std::collections::HashSet<&str> =
                s.fields.iter().map(|f| f.key.as_str()).collect();
            let agent_field_keys: std::collections::HashSet<&str> = s
                .fields
                .iter()
                .filter(|f| f.option_source == Some(OptionSource::AcpAgents))
                .map(|f| f.key.as_str())
                .collect();
            let mut seen = std::collections::HashSet::new();
            for (j, f) in s.fields.iter().enumerate() {
                check(
                    !f.key.is_empty(),
                    format!("settings[{i}].fields[{j}].key must not be empty"),
                );
                check(
                    seen.insert(f.key.as_str()),
                    format!("settings[{i}].fields[{j}].key {:?} is duplicated", f.key),
                );
                check(
                    f.key != id_key,
                    format!(
                        "settings[{i}].fields[{j}].key {:?} collides with the item id key",
                        f.key
                    ),
                );
                check(
                    f.value_type != ObjectFieldType::Select || !f.options.is_empty(),
                    format!("settings[{i}].fields[{j}] is a select but declares no options"),
                );
                let is_dynamic = matches!(
                    f.value_type,
                    ObjectFieldType::DynamicSelect | ObjectFieldType::DynamicMultiSelect
                );
                check(
                    is_dynamic == f.option_source.is_some(),
                    format!(
                        "settings[{i}].fields[{j}]: option_source is required for and exclusive to dynamic_select / dynamic_multi_select"
                    ),
                );
                check(
                    is_dynamic || f.depends_on.is_empty(),
                    format!(
                        "settings[{i}].fields[{j}]: depends_on is only valid on a dynamic_select / dynamic_multi_select"
                    ),
                );
                let mut dep_seen = std::collections::HashSet::new();
                for dep in &f.depends_on {
                    check(
                        dep != &f.key,
                        format!("settings[{i}].fields[{j}]: depends_on must not reference itself"),
                    );
                    check(
                        field_keys.contains(dep.as_str()),
                        format!(
                            "settings[{i}].fields[{j}]: depends_on {dep:?} is not a sibling field"
                        ),
                    );
                    check(
                        dep_seen.insert(dep.as_str()),
                        format!("settings[{i}].fields[{j}]: depends_on {dep:?} is listed twice"),
                    );
                }
                if matches!(
                    f.option_source,
                    Some(OptionSource::AcpModels) | Some(OptionSource::AcpModes)
                ) {
                    check(
                        f.depends_on
                            .iter()
                            .any(|d| agent_field_keys.contains(d.as_str())),
                        format!(
                            "settings[{i}].fields[{j}]: acp.models/acp.modes require a depends_on referencing an acp.agents field"
                        ),
                    );
                }
            }
        }
        _ => {
            check(
                s.option_source.is_none(),
                format!("settings[{i}]: option_source is only valid on a dynamic_select"),
            );
            check(
                s.depends_on.is_empty(),
                format!("settings[{i}]: depends_on is only valid on a dynamic_select"),
            );
            check(
                s.fields.is_empty(),
                format!("settings[{i}]: fields are only valid on an object_list"),
            );
        }
    }
}

fn validate_object_list_default(
    i: usize,
    s: &SettingContribution,
    items: &[toml::Value],
    check: &mut impl FnMut(bool, String),
) {
    let id_key = s.item_id_key.as_deref().unwrap_or("_id");
    for (k, item) in items.iter().enumerate() {
        let Some(table) = item.as_table() else {
            check(false, format!("settings[{i}].default[{k}] must be a table"));
            continue;
        };
        match table.get(id_key).and_then(|v| v.as_str()) {
            Some(v) if !v.trim().is_empty() => {}
            _ => check(
                false,
                format!("settings[{i}].default[{k}] must carry a non-empty {id_key:?} id"),
            ),
        }
        for key in table.keys() {
            check(
                key == id_key || s.fields.iter().any(|f| &f.key == key),
                format!("settings[{i}].default[{k}] has undeclared key {key:?}"),
            );
        }
        for f in &s.fields {
            match table.get(&f.key) {
                None => check(
                    !f.required,
                    format!(
                        "settings[{i}].default[{k}] is missing required field {:?}",
                        f.key
                    ),
                ),
                Some(v) => {
                    let type_ok = match f.value_type {
                        ObjectFieldType::String
                        | ObjectFieldType::Select
                        | ObjectFieldType::DynamicSelect
                        | ObjectFieldType::Cron => v.is_str(),
                        ObjectFieldType::Bool => v.as_bool().is_some(),
                        ObjectFieldType::Integer => v.as_integer().is_some(),
                        ObjectFieldType::DynamicMultiSelect => {
                            v.as_array().is_some_and(|a| a.iter().all(|e| e.is_str()))
                        }
                    };
                    check(
                        type_ok,
                        format!(
                            "settings[{i}].default[{k}].{} does not match type {:?}",
                            f.key, f.value_type
                        ),
                    );
                    let empty_required = match f.value_type {
                        ObjectFieldType::DynamicMultiSelect => {
                            v.as_array().is_none_or(|a| a.is_empty())
                        }
                        _ => v.as_str().map(|s| s.trim().is_empty()).unwrap_or(false),
                    };
                    if f.required && empty_required {
                        check(
                            false,
                            format!("settings[{i}].default[{k}].{} is required but empty", f.key),
                        );
                    }
                    if f.value_type == ObjectFieldType::Integer {
                        if let Some(iv) = v.as_integer() {
                            if let Some(lo) = f.min {
                                check(
                                    iv >= lo,
                                    format!(
                                        "settings[{i}].default[{k}].{} {iv} is below min {lo}",
                                        f.key
                                    ),
                                );
                            }
                            if let Some(hi) = f.max {
                                check(
                                    iv <= hi,
                                    format!(
                                        "settings[{i}].default[{k}].{} {iv} is above max {hi}",
                                        f.key
                                    ),
                                );
                            }
                        }
                    }
                    if f.value_type == ObjectFieldType::Select && !f.options.is_empty() {
                        if let Some(sv) = v.as_str() {
                            check(
                                f.options.iter().any(|o| o == sv),
                                format!(
                                    "settings[{i}].default[{k}].{} {sv:?} is not one of the options",
                                    f.key
                                ),
                            );
                        }
                    }
                }
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SettingType {
    /// Free text, rendered as a text input.
    #[default]
    String,
    #[serde(alias = "boolean")]
    Bool,
    /// Integer, rendered as a number input (bounded by `min`/`max`).
    Integer,
    /// Closed set of strings, rendered as a select over `options`.
    Select,
    DynamicSelect,
    /// A repeatable list of structured items described by `fields` (API v9).
    ObjectList,
    /// A cron expression, rendered as a validated text field (API v9).
    Cron,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ThemeContribution {
    /// Name shown in the theme picker; must not collide with a builtin.
    pub name: String,
    /// Theme TOML path, relative to the plugin directory.
    pub path: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StatusContribution {
    /// Stable identifier the host addresses this segment by.
    pub id: String,
    /// Human-readable text shown in the status surface.
    #[serde(default)]
    pub label: String,
}

pub const MAX_SCREENSHOTS: usize = 8;

const SCREENSHOT_EXTENSIONS: [&str; 5] = ["png", "jpg", "jpeg", "gif", "webp"];

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Screenshot {
    pub path: String,
    pub alt: String,
    /// Optional human-visible caption shown beneath the image.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub caption: String,
}

pub fn screenshot_path_ok(path: &str) -> bool {
    if path.is_empty() || path.len() > 512 {
        return false;
    }
    if path.starts_with('/') || path.contains(':') || path.contains('\\') {
        return false;
    }
    if path.chars().any(char::is_control) {
        return false;
    }
    if path.split('/').any(|seg| seg == ".." || seg.is_empty()) {
        return false;
    }
    match path.rsplit('.').next() {
        Some(ext) if ext != path => {
            SCREENSHOT_EXTENSIONS.contains(&ext.to_ascii_lowercase().as_str())
        }
        _ => false,
    }
}

pub fn lucide_icon_name_ok(name: &str) -> bool {
    if name.is_empty() || name.len() > 80 {
        return false;
    }
    name.split('-').all(|seg| {
        !seg.is_empty()
            && seg
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum UiSlot {
    /// A segment in the dashboard status/top bar (global).
    StatusBar,
    /// A badge on a session row (per session).
    RowBadge,
    RowColumn,
    /// A named sort option over a `RowColumn`'s scalar value (global).
    SortKey,
    /// A named filter over a `RowColumn`'s scalar value (global).
    FilterFacet,
    /// A card on the dashboard overview (global).
    Card,
    Pane,
    /// An action button next to a session's ACP composer controls (per session).
    ComposerAction,
    /// A badge in a session's detail view (per session).
    DetailBadge,
    HomePane,
    Notification,
}

impl UiSlot {
    pub fn is_per_session(self) -> bool {
        matches!(
            self,
            UiSlot::RowBadge
                | UiSlot::RowColumn
                | UiSlot::Pane
                | UiSlot::ComposerAction
                | UiSlot::DetailBadge
        )
    }

    pub fn as_str(self) -> &'static str {
        match self {
            UiSlot::StatusBar => "status-bar",
            UiSlot::RowBadge => "row-badge",
            UiSlot::RowColumn => "row-column",
            UiSlot::SortKey => "sort-key",
            UiSlot::FilterFacet => "filter-facet",
            UiSlot::Card => "card",
            UiSlot::Pane => "pane",
            UiSlot::ComposerAction => "composer-action",
            UiSlot::DetailBadge => "detail-badge",
            UiSlot::HomePane => "home-pane",
            UiSlot::Notification => "notification",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UiContribution {
    pub slot: UiSlot,
    #[serde(default)]
    pub id: String,
}

/// How the plugin's worker is launched.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum RuntimeSpec {
    Command {
        /// argv; the first element is the program, the rest its arguments.
        command: Vec<String>,
        #[serde(default, skip_serializing_if = "is_false")]
        system: bool,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        build: Vec<BuildStep>,
    },
    /// A worker binary downloaded from the source repo's GitHub release assets.
    ReleaseBinary {
        asset: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        bin: Option<String>,
    },
}

/// One install/update build command for a `command` runtime.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BuildStep {
    pub command: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub platforms: Vec<String>,
}

const KNOWN_PLATFORMS: [&str; 3] = ["linux", "macos", "windows"];

/// `skip_serializing_if` predicate for a defaulted `bool` flag.
fn is_false(b: &bool) -> bool {
    !*b
}

fn looks_like_path(arg: &str) -> bool {
    arg.contains('/') || arg.contains('\\') || std::path::Path::new(arg).is_absolute()
}

#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ManifestError {
    #[error("manifest is not valid TOML: {0}")]
    Parse(#[from] toml::de::Error),
    #[error("manifest targets api_version {found} but this host supports 1..={max}; upgrade aoe")]
    UnsupportedApiVersion { found: u64, max: u32 },
    #[error("manifest is invalid:\n{}", .0.join("\n"))]
    Invalid(Vec<String>),
}

impl PluginManifest {
    /// Parse and validate an `aoe-plugin.toml` document.
    pub fn from_toml_str(input: &str) -> Result<Self, ManifestError> {
        // Check api_version first so a newer manifest reports "upgrade aoe", not "unknown field".
        if let Some(found) = toml::from_str::<toml::Value>(input)
            .ok()
            .and_then(|doc| doc.get("api_version").and_then(toml::Value::as_integer))
        {
            if found > API_VERSION as i64 {
                return Err(ManifestError::UnsupportedApiVersion {
                    found: found as u64,
                    max: API_VERSION,
                });
            }
        }
        let manifest: Self = toml::from_str(input)?;
        manifest.validate()?;
        Ok(manifest)
    }

    pub fn hash_bytes(bytes: &[u8]) -> String {
        use std::fmt::Write;
        let mut hasher = Sha256::new();
        hasher.update(bytes);
        let digest = hasher.finalize();
        let mut out = String::with_capacity(7 + digest.len() * 2);
        out.push_str("sha256:");
        for byte in digest {
            let _ = write!(out, "{byte:02x}");
        }
        out
    }

    /// Check the host semver version against `aoe_version`, if declared.
    pub fn host_compat(&self, host: &str) -> Result<(), String> {
        let Some(req) = &self.aoe_version else {
            return Ok(());
        };
        let req = semver::VersionReq::parse(req)
            .map_err(|e| format!("aoe_version {req:?} is not a valid semver requirement: {e}"))?;
        let host_version = semver::Version::parse(host)
            .map_err(|e| format!("host aoe version {host:?} is not valid semver: {e}"))?;
        if req.matches(&host_version) {
            Ok(())
        } else {
            Err(format!(
                "plugin requires aoe {req}; this host is {host_version}"
            ))
        }
    }

    pub fn validate(&self) -> Result<(), ManifestError> {
        let mut errors = Vec::new();
        let mut check = |ok: bool, msg: String| {
            if !ok {
                errors.push(msg);
            }
        };

        check(
            (1..=API_VERSION).contains(&self.api_version),
            format!(
                "api_version {} is not supported (host supports 1..={API_VERSION})",
                self.api_version
            ),
        );
        check(!self.version.is_empty(), "version must not be empty".into());
        check(!self.name.is_empty(), "name must not be empty".into());

        if let Some(RuntimeSpec::Command {
            command,
            system,
            build,
        }) = &self.runtime
        {
            check(
                !command.is_empty(),
                "runtime command must not be empty".into(),
            );
            check(
                command.iter().all(|arg| !arg.is_empty()),
                "runtime command must not contain empty arguments".into(),
            );
            if let Some(program) = command.first().filter(|a| !a.is_empty()) {
                if *system {
                    check(
                        !looks_like_path(program),
                        format!(
                            "runtime command program {program:?} has `system = true` but is a path; \
                             a system dependency must be a bare program name resolved on PATH (like \"uv\" or \"python3\")"
                        ),
                    );
                } else {
                    check(
                        looks_like_path(program) && !std::path::Path::new(program).is_absolute(),
                        format!(
                            "runtime command program {program:?} must be a plugin-relative path \
                             (containing a separator, like \".venv/bin/worker\"); set `system = true` \
                             to depend on a program from the host PATH instead"
                        ),
                    );
                }
            }
            for (i, step) in build.iter().enumerate() {
                check(
                    !step.command.is_empty(),
                    format!("runtime.build[{i}].command must not be empty"),
                );
                check(
                    step.command.iter().all(|arg| !arg.is_empty()),
                    format!("runtime.build[{i}].command must not contain empty arguments"),
                );
                for p in &step.platforms {
                    check(
                        KNOWN_PLATFORMS.contains(&p.as_str()),
                        format!(
                            "runtime.build[{i}].platforms contains unknown platform {p:?}; expected one of linux, macos, windows"
                        ),
                    );
                }
            }
        }
        if let Some(RuntimeSpec::ReleaseBinary { asset, bin }) = &self.runtime {
            check(
                !asset.is_empty(),
                "runtime release-binary asset must not be empty".into(),
            );
            check(
                bin.as_ref().map(|b| !b.is_empty()).unwrap_or(true),
                "runtime release-binary bin must not be empty".into(),
            );
        }

        let has_browser_open = self
            .capabilities
            .iter()
            .any(|c| c.as_str() == "browser_open");
        for (i, c) in self.commands.iter().enumerate() {
            check(
                !c.id.is_empty(),
                format!("commands[{i}].id must not be empty"),
            );
            if let Some(ClientAction::OpenUiLink { slot, id }) = &c.action {
                check(
                    self.api_version >= 6,
                    format!("commands[{i}].action requires api_version >= 6"),
                );
                check(
                    has_browser_open,
                    format!("commands[{i}].action needs the `browser_open` capability"),
                );
                check(
                    slot.is_per_session(),
                    format!("commands[{i}].action open-ui-link slot must be per-session"),
                );
                check(
                    self.ui
                        .iter()
                        .filter(|u| u.slot == *slot && &u.id == id)
                        .count()
                        == 1,
                    format!(
                        "commands[{i}].action must reference exactly one ui slot ({}, {id})",
                        slot.as_str()
                    ),
                );
            }
        }
        for (i, k) in self.keybinds.iter().enumerate() {
            check(
                !k.command.is_empty(),
                format!("keybinds[{i}].command must not be empty"),
            );
            check(
                !k.key.is_empty(),
                format!("keybinds[{i}].key must not be empty"),
            );
        }
        let setting_keys: std::collections::HashSet<&str> =
            self.settings.iter().map(|s| s.key.as_str()).collect();
        let agent_setting_keys: std::collections::HashSet<&str> = self
            .settings
            .iter()
            .filter(|s| s.option_source == Some(OptionSource::AcpAgents))
            .map(|s| s.key.as_str())
            .collect();
        for (i, s) in self.settings.iter().enumerate() {
            check(
                !s.key.is_empty(),
                format!("settings[{i}].key must not be empty"),
            );
            check(
                s.value_type != SettingType::Select || !s.options.is_empty(),
                format!("settings[{i}] is a select but declares no options"),
            );
            check(
                match (s.min, s.max) {
                    (Some(lo), Some(hi)) => lo <= hi,
                    _ => true,
                },
                format!("settings[{i}].min must not exceed max"),
            );
            validate_object_list_settings(i, s, &setting_keys, &agent_setting_keys, &mut check);
            if let Some(def) = &s.default {
                let type_ok = match s.value_type {
                    SettingType::String
                    | SettingType::Select
                    | SettingType::DynamicSelect
                    | SettingType::Cron => def.is_str(),
                    SettingType::Bool => def.as_bool().is_some(),
                    SettingType::Integer => def.as_integer().is_some(),
                    SettingType::ObjectList => matches!(def, toml::Value::Array(_)),
                };
                check(
                    type_ok,
                    format!(
                        "settings[{i}].default does not match type {:?}",
                        s.value_type
                    ),
                );
                if s.value_type == SettingType::Select {
                    if let (Some(d), false) = (def.as_str(), s.options.is_empty()) {
                        check(
                            s.options.iter().any(|o| o == d),
                            format!("settings[{i}].default {d:?} is not one of the options"),
                        );
                    }
                }
                if s.value_type == SettingType::Integer {
                    if let Some(v) = def.as_integer() {
                        if let Some(lo) = s.min {
                            check(
                                v >= lo,
                                format!("settings[{i}].default {v} is below min {lo}"),
                            );
                        }
                        if let Some(hi) = s.max {
                            check(
                                v <= hi,
                                format!("settings[{i}].default {v} is above max {hi}"),
                            );
                        }
                    }
                }
                if s.value_type == SettingType::ObjectList {
                    if let toml::Value::Array(items) = def {
                        validate_object_list_default(i, s, items, &mut check);
                    }
                }
            }
        }
        for (i, t) in self.themes.iter().enumerate() {
            check(
                !t.name.is_empty(),
                format!("themes[{i}].name must not be empty"),
            );
            check(
                !t.path.is_empty(),
                format!("themes[{i}].path must not be empty"),
            );
        }
        for (i, s) in self.status.iter().enumerate() {
            check(
                !s.id.is_empty(),
                format!("status[{i}].id must not be empty"),
            );
        }
        check(
            self.screenshots.len() <= MAX_SCREENSHOTS,
            format!(
                "at most {MAX_SCREENSHOTS} screenshots are allowed (got {})",
                self.screenshots.len()
            ),
        );
        for (i, s) in self.screenshots.iter().enumerate() {
            check(
                screenshot_path_ok(&s.path),
                format!(
                    "screenshots[{i}].path {:?} must be a repository-relative image path \
                     (png/jpg/jpeg/gif/webp), not a URL or an absolute/traversing path",
                    s.path
                ),
            );
            check(
                !s.alt.trim().is_empty(),
                format!("screenshots[{i}].alt must not be empty"),
            );
        }
        if let Some(icon) = &self.icon {
            check(
                lucide_icon_name_ok(icon),
                format!("icon {icon:?} must be a lucide kebab-case icon name"),
            );
        }
        if let Some(path) = &self.icon_asset {
            check(
                screenshot_path_ok(path),
                format!(
                    "icon_asset {path:?} must be a repository-relative image path \
                     (png/jpg/jpeg/gif/webp), not a URL or an absolute/traversing path"
                ),
            );
        }
        if let Some(req) = &self.aoe_version {
            check(
                semver::VersionReq::parse(req).is_ok(),
                format!("aoe_version {req:?} is not a valid semver requirement"),
            );
        }
        // Newer fields force an api_version bump so older hosts say "upgrade aoe", not "unknown field".
        if self.api_version < 4 {
            check(
                self.status.is_empty(),
                "status contributions require api_version >= 4".into(),
            );
            check(
                self.aoe_version.is_none(),
                "aoe_version requires api_version >= 4".into(),
            );
        }
        if self.api_version < 5 {
            check(
                self.screenshots.is_empty(),
                "screenshots require api_version >= 5".into(),
            );
        }
        if self.api_version < 7 {
            check(self.icon.is_none(), "icon requires api_version >= 7".into());
            check(
                self.icon_asset.is_none(),
                "icon_asset requires api_version >= 7".into(),
            );
        }
        if self.api_version < 8 {
            check(
                self.ui.iter().all(|u| u.slot != UiSlot::ComposerAction),
                "composer-action UI slots require api_version >= 8".into(),
            );
        }
        if self.api_version < 9 {
            check(
                self.settings.iter().all(|s| {
                    !matches!(
                        s.value_type,
                        SettingType::DynamicSelect | SettingType::ObjectList | SettingType::Cron
                    )
                }),
                "dynamic_select / object_list / cron settings require api_version >= 9".into(),
            );
        }
        if self.api_version < 11 {
            check(
                self.settings.iter().all(|s| {
                    s.fields
                        .iter()
                        .all(|f| f.value_type != ObjectFieldType::DynamicMultiSelect)
                }),
                "dynamic_multi_select settings fields require api_version >= 11".into(),
            );
        }
        if self.api_version < 13 {
            check(
                self.ui.iter().all(|u| u.slot != UiSlot::HomePane),
                "home-pane UI slots require api_version >= 13".into(),
            );
        }
        for key in self.setting_defaults.keys() {
            check(
                key.contains('.') && !key.starts_with('.') && !key.ends_with('.'),
                format!("setting_defaults key {key:?} must be a dotted core path like \"section.field\""),
            );
        }
        for (i, u) in self.ui.iter().enumerate() {
            check(!u.id.is_empty(), format!("ui[{i}].id must not be empty"));
        }

        if errors.is_empty() {
            Ok(())
        } else {
            Err(ManifestError::Invalid(errors))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const HEAD: &str = "id = \"a.b\"\nname = \"B\"\nversion = \"1.0.0\"\napi_version = ";

    fn object_list_toml(api_version: u32) -> String {
        format!(
            "id = \"acme.cron\"\nname = \"Cron\"\nversion = \"1.0.0\"\napi_version = {api_version}\n\n\
             [[settings]]\nkey = \"jobs\"\nlabel = \"Jobs\"\ntype = \"object_list\"\nitem_id_key = \"id\"\nmin_items = 0\nmax_items = 50\n\n\
             [[settings.fields]]\nkey = \"agent_id\"\nlabel = \"Agent\"\ntype = \"dynamic_select\"\noption_source = \"acp.agents\"\nrequired = true\n\n\
             [[settings.fields]]\nkey = \"model_id\"\nlabel = \"Model\"\ntype = \"dynamic_select\"\noption_source = \"acp.models\"\ndepends_on = [\"agent_id\"]\n\n\
             [[settings.fields]]\nkey = \"schedule\"\nlabel = \"Schedule\"\ntype = \"cron\"\nrequired = true\n"
        )
    }

    fn multi_select_toml(api_version: u32) -> String {
        format!(
            "{HEAD}{api_version}\n\n\
             [[settings]]\nkey = \"jobs\"\ntype = \"object_list\"\nitem_id_key = \"id\"\n\n\
             [[settings.fields]]\nkey = \"projects\"\ntype = \"dynamic_multi_select\"\noption_source = \"projects\"\n"
        )
    }

    fn open_ui_link_toml(api_version: u32, caps: &str, ui_slot: &str, action_slot: &str) -> String {
        format!(
            "id = \"acme.thing\"\nname = \"Thing\"\nversion = \"1.0.0\"\napi_version = {api_version}\ncapabilities = [{caps}]\n\n\
             [[ui]]\nslot = \"{ui_slot}\"\nid = \"link\"\n\n\
             [[commands]]\nid = \"open\"\ntitle = \"Open\"\n[commands.action]\nkind = \"open-ui-link\"\nslot = \"{action_slot}\"\nid = \"link\"\n"
        )
    }

    fn home_pane_toml(api_version: u32) -> String {
        format!(
            "id = \"acme.diag\"\nname = \"Diag\"\nversion = \"1.0.0\"\napi_version = {api_version}\n\n\
             [[ui]]\nslot = \"home-pane\"\nid = \"mem\"\n"
        )
    }

    fn rejects(cases: &[(&str, String, &str)]) {
        for (label, toml, needle) in cases {
            let err = PluginManifest::from_toml_str(toml).unwrap_err().to_string();
            assert!(err.contains(needle), "{label}: {err}");
        }
    }

    #[test]
    fn v9_object_list_manifest_parses_and_validates() {
        let m = PluginManifest::from_toml_str(&object_list_toml(9)).expect("v9 manifest parses");
        let jobs = &m.settings[0];
        assert_eq!(jobs.value_type, SettingType::ObjectList);
        assert_eq!(jobs.item_id_key.as_deref(), Some("id"));
        assert_eq!(jobs.fields.len(), 3);
        assert_eq!(jobs.fields[0].value_type, ObjectFieldType::DynamicSelect);
        assert_eq!(jobs.fields[0].option_source, Some(OptionSource::AcpAgents));
        assert_eq!(jobs.fields[1].depends_on, vec!["agent_id".to_string()]);
        assert_eq!(jobs.fields[2].value_type, ObjectFieldType::Cron);
    }

    #[test]
    fn dynamic_multi_select_parses_from_v11() {
        let m = PluginManifest::from_toml_str(&multi_select_toml(11)).expect("v11 parses");
        assert_eq!(
            m.settings[0].fields[0].value_type,
            ObjectFieldType::DynamicMultiSelect
        );
        assert_eq!(
            m.settings[0].fields[0].option_source,
            Some(OptionSource::Projects)
        );
    }

    #[test]
    fn newer_setting_shapes_are_gated_on_their_api_version() {
        rejects(&[
            (
                "object_list below v9",
                object_list_toml(8),
                "api_version >= 9",
            ),
            (
                "dynamic_multi_select below v11",
                multi_select_toml(10),
                "api_version >= 11",
            ),
        ]);
        let err = PluginManifest::from_toml_str(&home_pane_toml(12))
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("home-pane") && err.contains("api_version"),
            "{err}"
        );
        PluginManifest::from_toml_str(&home_pane_toml(13)).expect("home-pane parses from v13");
    }

    #[test]
    fn settings_validation_rejects_malformed_declarations() {
        rejects(&[
            (
                "dynamic_select without option_source",
                format!("{HEAD}9\n\n[[settings]]\nkey = \"agent\"\ntype = \"dynamic_select\"\n"),
                "option_source",
            ),
            (
                "dynamic_multi_select field without option_source",
                format!(
                    "{HEAD}11\n\n[[settings]]\nkey = \"jobs\"\ntype = \"object_list\"\nitem_id_key = \"id\"\n\n\
                     [[settings.fields]]\nkey = \"projects\"\ntype = \"dynamic_multi_select\"\n"
                ),
                "option_source",
            ),
            (
                "required dynamic_multi_select defaulting to an empty array",
                format!(
                    "{HEAD}11\n\n[[settings]]\nkey = \"jobs\"\ntype = \"object_list\"\nitem_id_key = \"id\"\ndefault = [ {{ id = \"x\", projects = [] }} ]\n\n\
                     [[settings.fields]]\nkey = \"projects\"\ntype = \"dynamic_multi_select\"\noption_source = \"projects\"\nrequired = true\n"
                ),
                "is required but empty",
            ),
            (
                "object_list field colliding with the item id key",
                format!(
                    "{HEAD}9\n\n[[settings]]\nkey = \"jobs\"\ntype = \"object_list\"\nitem_id_key = \"id\"\n\n\
                     [[settings.fields]]\nkey = \"id\"\ntype = \"string\"\n"
                ),
                "collides with the item id key",
            ),
            (
                "depends_on naming a setting that does not exist",
                format!(
                    "{HEAD}9\n\n[[settings]]\nkey = \"agent\"\ntype = \"dynamic_select\"\noption_source = \"acp.agents\"\n\n\
                     [[settings]]\nkey = \"model\"\ntype = \"dynamic_select\"\noption_source = \"acp.models\"\ndepends_on = [\"typo\"]\n"
                ),
                "is not a sibling setting",
            ),
            (
                "acp.models without a depends_on",
                format!(
                    "{HEAD}9\n\n[[settings]]\nkey = \"model\"\ntype = \"dynamic_select\"\noption_source = \"acp.models\"\n"
                ),
                "acp.models/acp.modes require a depends_on",
            ),
            (
                "object_list default outside an integer field's bounds",
                format!(
                    "{HEAD}9\n\n[[settings]]\nkey = \"jobs\"\ntype = \"object_list\"\nitem_id_key = \"id\"\n\
                     default = [{{ id = \"j1\", retries = 9 }}]\n\n\
                     [[settings.fields]]\nkey = \"retries\"\ntype = \"integer\"\nmin = 0\nmax = 5\n"
                ),
                "is above max 5",
            ),
            (
                "option_source on a plain type",
                format!(
                    "{HEAD}9\n\n[[settings]]\nkey = \"x\"\ntype = \"string\"\noption_source = \"projects\"\n"
                ),
                "only valid on a dynamic_select",
            ),
        ]);
    }

    #[test]
    fn top_level_dynamic_select_depends_on_agent_sibling_validates() {
        let toml = format!(
            "{HEAD}9\n\n[[settings]]\nkey = \"agent\"\ntype = \"dynamic_select\"\noption_source = \"acp.agents\"\n\n\
             [[settings]]\nkey = \"model\"\ntype = \"dynamic_select\"\noption_source = \"acp.models\"\ndepends_on = [\"agent\"]\n"
        );
        PluginManifest::from_toml_str(&toml).expect("valid dependent selects parse");
    }

    #[test]
    fn open_ui_link_action_needs_the_capability_and_one_declared_session_slot() {
        let m = PluginManifest::from_toml_str(&open_ui_link_toml(
            6,
            "\"browser_open\"",
            "row-column",
            "row-column",
        ))
        .expect("manifest parses");
        assert!(matches!(
            m.commands[0].action,
            Some(ClientAction::OpenUiLink { .. })
        ));

        rejects(&[
            (
                "without the browser_open capability",
                open_ui_link_toml(6, "", "row-column", "row-column"),
                "browser_open",
            ),
            (
                "below api_version 6",
                open_ui_link_toml(5, "\"browser_open\"", "row-column", "row-column"),
                "api_version",
            ),
            (
                "on a global slot",
                open_ui_link_toml(6, "\"browser_open\"", "status-bar", "status-bar"),
                "per-session",
            ),
            (
                "naming a slot the manifest never declares",
                open_ui_link_toml(6, "\"browser_open\"", "row-badge", "row-column"),
                "exactly one ui slot",
            ),
            (
                "naming a slot declared twice",
                "id = \"acme.thing\"\nname = \"Thing\"\nversion = \"1.0.0\"\napi_version = 6\ncapabilities = [\"browser_open\"]\n\n\
                 [[ui]]\nslot = \"row-column\"\nid = \"link\"\n\n[[ui]]\nslot = \"row-column\"\nid = \"link\"\n\n\
                 [[commands]]\nid = \"open\"\n[commands.action]\nkind = \"open-ui-link\"\nslot = \"row-column\"\nid = \"link\"\n".to_string(),
                "exactly one ui slot",
            ),
        ]);
    }

    #[test]
    fn setting_type_accepts_boolean_and_bool() {
        for spelling in ["boolean", "bool"] {
            let manifest = PluginManifest::from_toml_str(&format!(
                "{HEAD}4\n\n[[settings]]\nkey = \"flag\"\ntype = \"{spelling}\"\n"
            ))
            .expect("manifest parses");
            assert_eq!(
                manifest.settings[0].value_type,
                SettingType::Bool,
                "{spelling}"
            );
        }
    }
}
