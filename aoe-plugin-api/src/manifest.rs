use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::{CapabilityId, PluginId, API_VERSION};

/// Parsed `aoe-plugin.toml`.
///
/// Identity (`id`, `name`, `version`, `api_version`, `description`) plus the
/// contribution sections a plugin declares. The contribution sections are
/// defined here but consumed by later issues: the settings registry (#2094),
/// the runtime host (#2095), and the command/keybind/UI surfaces (#2366). This
/// host parses and validates them; it does not yet act on them.
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

    /// Screenshots / animated GIFs the plugin ships to illustrate itself in the
    /// marketplace and detail views. Each `path` is repository-relative;
    /// presentation only, granting nothing. Requires `api_version >= 5`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub screenshots: Vec<Screenshot>,

    /// Lucide kebab-case icon name (e.g. `"git-branch"`) used as the plugin's
    /// identity glyph where a raster asset isn't available or hasn't loaded.
    /// Only syntax-checked here; an unknown name resolves to the host's
    /// generic fallback icon client-side rather than failing to parse.
    /// Requires `api_version >= 7`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub icon: Option<String>,

    /// Repository-relative raster image used as the plugin's identity icon,
    /// shown in place of `icon` wherever the surface can render an image.
    /// Same path rules as `screenshots` (see [`screenshot_path_ok`]).
    /// Requires `api_version >= 7`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub icon_asset: Option<String>,

    /// Resource/effect capabilities the plugin requests. Static contributions
    /// below are NOT listed here; only runtime resource access is. The user
    /// grants these once at install (community plugins); builtins are
    /// auto-granted. See [`crate::capability`].
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub capabilities: Vec<CapabilityId>,

    /// Commands the plugin contributes (palette / CLI). Consumed by #2366.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub commands: Vec<CommandContribution>,

    /// Keybinds the plugin contributes. Consumed by #2366.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub keybinds: Vec<KeybindContribution>,

    /// Settings the plugin declares. Each is a typed field the host renders in
    /// the settings surfaces (TUI / web) and persists under
    /// `[plugins."<id>".settings]`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub settings: Vec<SettingContribution>,

    /// Default overrides the plugin applies to *core* settings, keyed by the
    /// core canonical path (`"theme.idle_decay_minutes"`). Resolution layers a
    /// user value over the highest-priority active plugin override over the core
    /// schema default; see the host's settings resolution (#2094). A plugin
    /// cannot override another plugin's settings at Tier 0.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub setting_defaults: BTreeMap<String, toml::Value>,

    /// Color themes the plugin ships. Each `path` is a theme TOML relative to
    /// the plugin's install directory; the host adds them to the theme picker
    /// (#2094).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub themes: Vec<ThemeContribution>,

    /// Status segments the plugin contributes. Each is a labelled id the host
    /// renders in a status surface; consumed by the status reference plugin
    /// (#2096). Requires `api_version >= 4`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub status: Vec<StatusContribution>,

    /// UI slots the plugin renders into. Consumed by #2366.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub ui: Vec<UiContribution>,

    /// The worker entrypoint. Defined here so installation can fetch a
    /// release-binary worker; actually launching it is #2095.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub runtime: Option<RuntimeSpec>,

    /// Optional range of aoe (host app) versions this plugin version supports,
    /// as a semver requirement like `">=0.10, <0.12"`. Distinct from
    /// `api_version` (the manifest schema version): `api_version` gates the
    /// manifest shape, `aoe_version` gates the host's app behaviour. The host
    /// refuses to install and skips loading a plugin when its running version
    /// is outside this range. Absent means no constraint. Requires
    /// `api_version >= 4`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub aoe_version: Option<String>,
}

/// A command the plugin contributes. The host namespaces it as
/// `plugin.<plugin-id>.<id>`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CommandContribution {
    pub id: String,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub description: String,
    /// How invoking the command behaves. Absent means a fire-and-forget worker
    /// notification (deferred). When present, the host surface executes the
    /// action directly, synchronously inside the user's gesture, so it works on
    /// a remote web dashboard where an async round-trip would be popup-blocked.
    /// Requires `api_version >= 6` and the `browser_open` capability.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub action: Option<ClientAction>,
}

/// A client-executed command action: the host surface (web dashboard, TUI) runs
/// it directly rather than forwarding to the worker. Tagged by `kind` so future
/// action shapes extend the set without breaking existing manifests.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum ClientAction {
    /// Open the `href` carried by this plugin's own `(slot, id)` UI-state entry
    /// for the active session, in the user's browser. The href is read from the
    /// snapshot the surface already holds, so no worker call is made; the slot
    /// must be per-session.
    OpenUiLink { slot: UiSlot, id: String },
}

/// A keybind the plugin contributes, binding a key chord to a command.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KeybindContribution {
    /// Command id this binds to (a plugin command or a core command).
    pub command: String,
    /// Key chord, e.g. `Ctrl+K`. Parsed by the consuming surface (#2366).
    pub key: String,
}

/// A setting the plugin declares. The host renders it on every settings surface
/// and persists its value under `[plugins."<id>".settings.<key>]`. The fields
/// map directly onto the host's settings schema (widget + validation) without
/// this crate depending on host types.
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
    /// The plugin's declared default (the "owning manifest default" layer in
    /// settings resolution). Absent means the type's zero value.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default: Option<toml::Value>,
    /// Group under an "Advanced" fold on the settings surfaces.
    #[serde(default)]
    pub advanced: bool,
    /// Host option source for a `dynamic_select` (API v9). Ignored for other
    /// types; the host resolves the choices, so the plugin never ships them.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub option_source: Option<OptionSource>,
    /// Sibling setting keys whose values parameterize a `dynamic_select`'s
    /// option source (API v9), e.g. an `acp.models` select depends on the
    /// `acp.agents` select. Empty for an independent source.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub depends_on: Vec<String>,
    /// Nested per-item fields of an `object_list` (API v9). Non-recursive: an
    /// item field cannot itself be an object list. Ignored for other types.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub fields: Vec<ObjectFieldContribution>,
    /// The item field that holds each `object_list` row's stable id (API v9).
    /// Defaults to `_id` (host-generated) when omitted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub item_id_key: Option<String>,
    /// Inclusive item-count bounds for an `object_list` (API v9).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_items: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_items: Option<u32>,
}

/// A host option source a `dynamic_select` draws its choices from (API v9).
/// The host resolves the choices from its own state; the plugin only names
/// the source and any dependencies. `acp.models` / `acp.modes` require the
/// selected agent as their dependency.
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

/// One nested field of an `object_list` item (API v9). A restricted,
/// non-recursive mirror of [`SettingContribution`]: its type cannot be
/// `object_list`, so an object list is at most one level deep.
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

/// The type of an `object_list` item field. Deliberately excludes
/// `object_list`, which keeps object lists one level deep in both the Rust
/// types and the serialized schema.
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
    Cron,
}

/// Validate the API v9 structured-setting shape of one contribution: that a
/// `dynamic_select` names a source, an `object_list` declares well-formed
/// non-recursive fields, and no other type carries structured-only
/// properties. Split out of `validate` to keep that method readable.
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
            // Every depends_on entry must name a distinct sibling setting other
            // than itself; a typo would otherwise install cleanly and leave the
            // select permanently unresolved. Mirrors the object_list item-field
            // checks below for the top-level dynamic_select path.
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
            // acp.models / acp.modes are meaningless without the agent they
            // scope to, so require a dependency on an acp.agents sibling setting.
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
            // Sibling fields that resolve the selected agent; acp.models /
            // acp.modes depend on one of these.
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
                check(
                    (f.value_type == ObjectFieldType::DynamicSelect) == f.option_source.is_some(),
                    format!(
                        "settings[{i}].fields[{j}]: option_source is required for and exclusive to dynamic_select"
                    ),
                );
                check(
                    f.value_type == ObjectFieldType::DynamicSelect || f.depends_on.is_empty(),
                    format!(
                        "settings[{i}].fields[{j}]: depends_on is only valid on a dynamic_select"
                    ),
                );
                // Every depends_on entry must name a distinct sibling field
                // other than itself; a typo would otherwise install cleanly and
                // leave the select permanently unresolved.
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
                // acp.models / acp.modes are meaningless without the agent they
                // scope to, so require a dependency on an acp.agents sibling.
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

/// Validate an `object_list` setting's declared default against its own item
/// schema (#2897): each element must be a table keyed only by the id key and
/// declared fields, carry a non-empty id, satisfy required fields, and match
/// each field's declared type. Without this, an author's malformed default
/// reaches the UI and cannot be saved unchanged.
// ponytail: cron *expression* syntax is validated server-side at store time,
// not re-implemented here to avoid a second copy of the croner dialect.
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
                    };
                    check(
                        type_ok,
                        format!(
                            "settings[{i}].default[{k}].{} does not match type {:?}",
                            f.key, f.value_type
                        ),
                    );
                    if f.required && v.as_str().map(|s| s.trim().is_empty()).unwrap_or(false) {
                        check(
                            false,
                            format!("settings[{i}].default[{k}].{} is required but empty", f.key),
                        );
                    }
                    // Integer field bounds, mirroring the top-level integer
                    // default check so an out-of-range object-field default is
                    // rejected at manifest parse rather than at store time.
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

/// The type of a plugin setting value. One declaration drives both the widget
/// the surfaces render and the validation the server enforces.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SettingType {
    /// Free text, rendered as a text input.
    #[default]
    String,
    /// On/off, rendered as a toggle. Accepts `boolean` too: it is the natural
    /// spelling next to `integer`, and shipped plugins use it.
    #[serde(alias = "boolean")]
    Bool,
    /// Integer, rendered as a number input (bounded by `min`/`max`).
    Integer,
    /// Closed set of strings, rendered as a select over `options`.
    Select,
    /// A select whose choices the host resolves from an `option_source`
    /// (API v9), optionally parameterized by `depends_on` siblings.
    DynamicSelect,
    /// A repeatable list of structured items described by `fields` (API v9).
    /// Rendered with add/remove/reorder; one level deep.
    ObjectList,
    /// A cron expression, rendered as a validated text field (API v9).
    Cron,
}

/// A color theme the plugin ships. `path` is a theme TOML relative to the
/// plugin's install directory.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ThemeContribution {
    /// Name shown in the theme picker; must not collide with a builtin.
    pub name: String,
    /// Theme TOML path, relative to the plugin directory.
    pub path: String,
}

/// A status segment the plugin contributes. The host namespaces it by plugin
/// id and renders `label` in a status surface; consumed by #2096.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StatusContribution {
    /// Stable identifier the host addresses this segment by.
    pub id: String,
    /// Human-readable text shown in the status surface.
    #[serde(default)]
    pub label: String,
}

/// Maximum screenshots a manifest may declare. A cap keeps the detail modal
/// usable and the manifest from ballooning; the lenient detail parser truncates
/// to the same bound.
pub const MAX_SCREENSHOTS: usize = 8;

/// Image extensions a screenshot `path` may use. The host renders each in an
/// `<img>`, so this is the raster/animated set a browser shows inline; SVG is
/// deliberately excluded (it can embed external references).
const SCREENSHOT_EXTENSIONS: [&str; 5] = ["png", "jpg", "jpeg", "gif", "webp"];

/// A screenshot or animated GIF a plugin ships to illustrate itself. `path` is
/// repository-relative (resolved against the plugin's source repo by the detail
/// endpoint); absolute URLs are rejected so opening a detail modal cannot issue
/// author-chosen third-party requests.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Screenshot {
    /// Repository-relative path to a PNG/JPEG/GIF/WebP asset in the plugin's
    /// source repo. Not a URL: no scheme, no leading separator, no `..`.
    pub path: String,
    /// Accessible description of the image. Required; screenshots are content,
    /// not decoration.
    pub alt: String,
    /// Optional human-visible caption shown beneath the image.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub caption: String,
}

/// Whether `path` is a clean repository-relative image path usable as a
/// screenshot: relative (no URL scheme, no drive letter, no leading separator),
/// no `..` traversal, no empty components, no control characters, bounded
/// length, and an allowed image extension. Shared by the strict validator and
/// the lenient detail parser so both agree on what resolves.
pub fn screenshot_path_ok(path: &str) -> bool {
    if path.is_empty() || path.len() > 512 {
        return false;
    }
    // A colon rejects both URL schemes (`https:`) and Windows drive letters
    // (`C:`); a leading slash rejects absolute paths. Screenshot paths are
    // repository paths, so they must use `/`, never `\`: a backslash would
    // survive into the resolved raw URL percent-encoded and 404, so reject it
    // here to fail fast for the author rather than render a broken image.
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
        // No extension (or a leading-dot dotfile with no extension).
        _ => false,
    }
}

/// Whether `name` is a syntactically valid lucide icon name: lowercase
/// ASCII kebab-case, non-empty segments, bounded length. Checked here so a
/// malformed name fails at parse time with an actionable message; whether the
/// name actually exists in lucide's icon set is the client's problem alone
/// (an unknown name degrades to the host's generic fallback icon rather than
/// failing to parse), so this crate does not depend on lucide's registry.
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

/// A host-rendered UI slot a plugin may push state into (#2366). A closed set,
/// unlike the open-string capabilities: the host must know how to render each
/// slot, so an unknown slot is unrenderable and rejected at parse time rather
/// than carried forward. The worker pushes typed state into a declared slot
/// over the `ui.state.*` host RPCs; the host renders it (the dashboard runs no
/// plugin code).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum UiSlot {
    /// A segment in the dashboard status/top bar (global).
    StatusBar,
    /// A badge on a session row (per session).
    RowBadge,
    /// A text column on a session row, carrying optional sort/filter scalars
    /// (per session).
    RowColumn,
    /// A named sort option over a `RowColumn`'s scalar value (global).
    SortKey,
    /// A named filter over a `RowColumn`'s scalar value (global).
    FilterFacet,
    /// A card on the dashboard overview (global).
    Card,
    /// A dockable tool-window pane in a session's view (per session). The host
    /// renders it in the right or bottom dock per the entry's `default_location`.
    Pane,
    /// An action button next to a session's ACP composer controls (per session).
    /// The host renders the button and forwards clicks to the plugin worker;
    /// optional draft edits are applied by the host surface, not by plugin JS.
    ComposerAction,
    /// A badge in a session's detail view (per session).
    DetailBadge,
    /// A transient notification, pushed via `ui.notify` (gated by the
    /// `notifications` capability rather than a slot declaration).
    Notification,
}

impl UiSlot {
    /// Whether entries in this slot are scoped to a single session (and so must
    /// carry a `session_id`), versus global to the dashboard.
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

    /// The kebab-case wire name, matching the serde representation. Handy for
    /// display (install prompt, plugin info) without round-tripping through
    /// serde.
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
            UiSlot::Notification => "notification",
        }
    }
}

/// A UI contribution: the plugin declares it may fill `slot` with entries
/// addressed by `id`. The host gates `ui.state.set`/`ui.state.remove` on the
/// `(slot, id)` pair being declared here.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UiContribution {
    pub slot: UiSlot,
    #[serde(default)]
    pub id: String,
}

/// How the plugin's worker is launched. Defined here; executed by #2095.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum RuntimeSpec {
    /// A worker launched by running a command from the plugin directory (a
    /// script, an interpreter invocation, or an in-tree binary).
    Command {
        /// argv; the first element is the program, the rest its arguments.
        ///
        /// The program (`argv[0]`) must be plugin-relative (a path containing a
        /// separator, like `.venv/bin/worker`, resolved inside the install
        /// directory), unless `system` is set. A plugin-relative entrypoint is
        /// PATH-independent: the daemon's PATH never decides whether the worker
        /// launches. Validation rejects a bare program name here so the
        /// PATH-independent shape is the default and a PATH dependency is a
        /// conscious opt-in (`system = true`).
        command: Vec<String>,
        /// Opt in to resolving `command`'s program (`argv[0]`) on the host PATH
        /// at launch, instead of in the plugin directory. Set this only when the
        /// worker genuinely depends on a system tool (`uv run worker`,
        /// `python3 -m pkg`): it makes the daemon's PATH a launch dependency,
        /// which is the fragility a plugin-relative entrypoint avoids. With
        /// `system` set, `argv[0]` must be a bare program name, not a path.
        #[serde(default, skip_serializing_if = "is_false")]
        system: bool,
        /// Ordered build steps the host runs once at install and update,
        /// inside the installed plugin directory, before the plugin is
        /// registered (e.g. create a venv, `pip install`, `npm ci`). They run
        /// in the user's interactive shell at install time, where PATH is
        /// reliable, so an interpreted worker can produce a self-contained
        /// in-tree environment and then launch via a plugin-relative
        /// `command`, never depending on the daemon's PATH. Builds run in the
        /// final directory, not a staging tree, because tools like Python
        /// venvs embed absolute paths and are not relocatable.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        build: Vec<BuildStep>,
    },
    /// A worker binary downloaded from the source repo's GitHub release assets.
    /// Installation resolves the asset for the host platform, downloads it, and
    /// places the binary in the plugin directory.
    ReleaseBinary {
        /// Asset name template. `${os}`, `${arch}`, and `${target}` are
        /// substituted with the host's values before matching the release.
        asset: String,
        /// Executable to run after extraction (the path within an archive). The
        /// downloaded asset itself when omitted (a raw, non-archive binary).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        bin: Option<String>,
    },
}

/// One install/update build command for a `command` runtime.
///
/// `command` is argv (program then arguments), resolved with the same policy
/// as the launch `command`: a bare name on the install-time PATH, a
/// separator-bearing path relative to the plugin directory, an absolute path
/// rejected. `platforms`, when non-empty, restricts the step to host OSes
/// matching `std::env::consts::OS` (`linux`, `macos`, `windows`); an empty
/// `platforms` runs on every platform.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BuildStep {
    pub command: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub platforms: Vec<String>,
}

/// Host OS names a build step's `platforms` may name. These match
/// `std::env::consts::OS`; a typo is rejected at parse rather than silently
/// skipping the step on every platform.
const KNOWN_PLATFORMS: [&str; 3] = ["linux", "macos", "windows"];

/// `skip_serializing_if` predicate for a defaulted `bool` flag.
fn is_false(b: &bool) -> bool {
    !*b
}

/// Whether `arg` reads as a path (carries a separator, or is absolute) rather
/// than a bare program name. The same classification the launch-time resolver
/// applies to `argv[0]`, lifted here so validation rejects a misshapen worker
/// entrypoint before install rather than at the first launch.
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
        // Pre-parse api_version permissively first. A manifest targeting a
        // newer host may introduce fields this host's strict schema does not
        // know, so a plain `toml::from_str::<Self>` would fail with a confusing
        // "unknown field" error. Surfacing the version mismatch first tells the
        // author the real problem (upgrade aoe).
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

    /// sha256 over the raw `aoe-plugin.toml` bytes as installed, formatted
    /// `sha256:<hex>`. A capability grant is pinned to this; an update whose
    /// manifest bytes (hence possibly its capability set) change re-prompts.
    /// Hashing the raw bytes, not a reserialized struct, avoids depending on
    /// serializer canonicalization.
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

    /// Check the running host (aoe app) version against the manifest's declared
    /// `aoe_version` range. `host` is a semver version string (the host's
    /// `CARGO_PKG_VERSION`). No declared range means no constraint. Returns an
    /// actionable message when the host is outside the range, so install can
    /// refuse and load can skip with a reason. The range is re-parsed here
    /// rather than cached because [`validate`] already gated its syntax, so a
    /// loaded manifest's range is known-valid.
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

    /// Structural validation; collects every problem instead of stopping at
    /// the first so a plugin author sees the full list in one pass.
    ///
    /// Capability strings are deliberately not validated here: they are open
    /// strings (forward-compatible), and the host rejects an unknown one at
    /// install rather than at parse, so a manifest targeting a newer host still
    /// parses on an older one.
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
            // The worker entrypoint must be plugin-relative so the daemon's PATH
            // never decides whether the worker launches; depending on a system
            // tool is a conscious opt-in (`system = true`), not a fallback from
            // a name that happens not to be on PATH. Enforce the two shapes are
            // mutually exclusive: relative path by default, bare name with
            // `system`.
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

        // Contribution sections declare required identifiers; an empty one would
        // install and persist a malformed manifest, so reject it here rather
        // than push the cleanup onto the later consumers (#2094 / #2095 / #2366).
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
        // Sibling setting keys for top-level dynamic_select depends_on
        // validation, and the subset whose source resolves the selected agent
        // (acp.models / acp.modes depend on one of these).
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
            // A declared default must match the value type, so an author learns
            // of a type mismatch at parse time rather than at render/store time.
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
                        // Check each bound independently so a single-sided range
                        // (only min, or only max) still rejects an out-of-range
                        // default.
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
        // `aoe_version` is the host-app compatibility range, gated by the host
        // at install and load; reject a malformed requirement at parse so the
        // author learns of it before publishing rather than at a user's install.
        if let Some(req) = &self.aoe_version {
            check(
                semver::VersionReq::parse(req).is_ok(),
                format!("aoe_version {req:?} is not a valid semver requirement"),
            );
        }
        // `status` and `aoe_version` are api_version 4 fields. A manifest using
        // them while declaring an older api_version would parse fine on this
        // host but fail with a confusing "unknown field" on a pre-4 host (which
        // never reaches the "upgrade aoe" path because the declared version is
        // not newer). Force the bump so older hosts emit the right message.
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
        // `screenshots` is an api_version 5 field; force the bump for the same
        // reason as the api_version 4 fields above, so a pre-5 host emits the
        // "upgrade aoe" path rather than a confusing "unknown field" error.
        if self.api_version < 5 {
            check(
                self.screenshots.is_empty(),
                "screenshots require api_version >= 5".into(),
            );
        }
        // `icon` and `icon_asset` are api_version 7 fields; same reasoning as
        // the gates above.
        if self.api_version < 7 {
            check(self.icon.is_none(), "icon requires api_version >= 7".into());
            check(
                self.icon_asset.is_none(),
                "icon_asset requires api_version >= 7".into(),
            );
        }
        // `composer-action` is an api_version 8 slot; force the bump for the
        // same reason as the gates above.
        if self.api_version < 8 {
            check(
                self.ui.iter().all(|u| u.slot != UiSlot::ComposerAction),
                "composer-action UI slots require api_version >= 8".into(),
            );
        }
        // `dynamic_select`, `object_list`, and `cron` settings types are
        // api_version 9; same reasoning as the gates above.
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
        for key in self.setting_defaults.keys() {
            check(
                key.contains('.') && !key.starts_with('.') && !key.ends_with('.'),
                format!("setting_defaults key {key:?} must be a dotted core path like \"section.field\""),
            );
        }
        for (i, u) in self.ui.iter().enumerate() {
            // `slot` is a typed enum, so an unknown slot is already a parse
            // error; only the addressing `id` needs checking. A UI entry is
            // pushed and gated by its `(slot, id)` pair, so an empty id leaves
            // it unaddressable.
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

    /// A v9 manifest declaring an object_list of dynamic_selects plus a cron
    /// field, the shape the cron plugin (PR2) ships.
    fn object_list_toml(api_version: u32) -> String {
        format!(
            "id = \"acme.cron\"\nname = \"Cron\"\nversion = \"1.0.0\"\napi_version = {api_version}\n\n\
             [[settings]]\nkey = \"jobs\"\nlabel = \"Jobs\"\ntype = \"object_list\"\nitem_id_key = \"id\"\nmin_items = 0\nmax_items = 50\n\n\
             [[settings.fields]]\nkey = \"agent_id\"\nlabel = \"Agent\"\ntype = \"dynamic_select\"\noption_source = \"acp.agents\"\nrequired = true\n\n\
             [[settings.fields]]\nkey = \"model_id\"\nlabel = \"Model\"\ntype = \"dynamic_select\"\noption_source = \"acp.models\"\ndepends_on = [\"agent_id\"]\n\n\
             [[settings.fields]]\nkey = \"schedule\"\nlabel = \"Schedule\"\ntype = \"cron\"\nrequired = true\n"
        )
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
    fn v9_settings_types_rejected_below_v9() {
        let err = PluginManifest::from_toml_str(&object_list_toml(8))
            .unwrap_err()
            .to_string();
        assert!(err.contains("api_version >= 9"), "{err}");
    }

    #[test]
    fn dynamic_select_requires_option_source() {
        let toml = "id = \"a.b\"\nname = \"B\"\nversion = \"1.0.0\"\napi_version = 9\n\n\
             [[settings]]\nkey = \"agent\"\ntype = \"dynamic_select\"\n";
        let err = PluginManifest::from_toml_str(toml).unwrap_err().to_string();
        assert!(err.contains("option_source"), "{err}");
    }

    #[test]
    fn object_list_field_key_cannot_collide_with_id_key() {
        let toml = "id = \"a.b\"\nname = \"B\"\nversion = \"1.0.0\"\napi_version = 9\n\n\
             [[settings]]\nkey = \"jobs\"\ntype = \"object_list\"\nitem_id_key = \"id\"\n\n\
             [[settings.fields]]\nkey = \"id\"\ntype = \"string\"\n";
        let err = PluginManifest::from_toml_str(toml).unwrap_err().to_string();
        assert!(err.contains("collides with the item id key"), "{err}");
    }

    #[test]
    fn top_level_dynamic_select_depends_on_must_name_a_sibling() {
        let toml = "id = \"a.b\"\nname = \"B\"\nversion = \"1.0.0\"\napi_version = 9\n\n\
             [[settings]]\nkey = \"agent\"\ntype = \"dynamic_select\"\noption_source = \"acp.agents\"\n\n\
             [[settings]]\nkey = \"model\"\ntype = \"dynamic_select\"\noption_source = \"acp.models\"\ndepends_on = [\"typo\"]\n";
        let err = PluginManifest::from_toml_str(toml).unwrap_err().to_string();
        assert!(err.contains("is not a sibling setting"), "{err}");
    }

    #[test]
    fn top_level_acp_models_requires_agent_dependency() {
        let toml = "id = \"a.b\"\nname = \"B\"\nversion = \"1.0.0\"\napi_version = 9\n\n\
             [[settings]]\nkey = \"model\"\ntype = \"dynamic_select\"\noption_source = \"acp.models\"\n";
        let err = PluginManifest::from_toml_str(toml).unwrap_err().to_string();
        assert!(
            err.contains("acp.models/acp.modes require a depends_on"),
            "{err}"
        );
    }

    #[test]
    fn top_level_dynamic_select_depends_on_agent_sibling_validates() {
        let toml = "id = \"a.b\"\nname = \"B\"\nversion = \"1.0.0\"\napi_version = 9\n\n\
             [[settings]]\nkey = \"agent\"\ntype = \"dynamic_select\"\noption_source = \"acp.agents\"\n\n\
             [[settings]]\nkey = \"model\"\ntype = \"dynamic_select\"\noption_source = \"acp.models\"\ndepends_on = [\"agent\"]\n";
        PluginManifest::from_toml_str(toml).expect("valid dependent selects parse");
    }

    #[test]
    fn object_list_default_integer_field_respects_bounds() {
        let toml = "id = \"a.b\"\nname = \"B\"\nversion = \"1.0.0\"\napi_version = 9\n\n\
             [[settings]]\nkey = \"jobs\"\ntype = \"object_list\"\nitem_id_key = \"id\"\n\
             default = [{ id = \"j1\", retries = 9 }]\n\n\
             [[settings.fields]]\nkey = \"retries\"\ntype = \"integer\"\nmin = 0\nmax = 5\n";
        let err = PluginManifest::from_toml_str(toml).unwrap_err().to_string();
        assert!(err.contains("is above max 5"), "{err}");
    }

    #[test]
    fn option_source_rejected_on_plain_types() {
        let toml = "id = \"a.b\"\nname = \"B\"\nversion = \"1.0.0\"\napi_version = 9\n\n\
             [[settings]]\nkey = \"x\"\ntype = \"string\"\noption_source = \"projects\"\n";
        let err = PluginManifest::from_toml_str(toml).unwrap_err().to_string();
        assert!(err.contains("only valid on a dynamic_select"), "{err}");
    }

    fn open_ui_link_toml(api_version: u32, caps: &str, ui_slot: &str, action_slot: &str) -> String {
        format!(
            "id = \"acme.thing\"\nname = \"Thing\"\nversion = \"1.0.0\"\napi_version = {api_version}\ncapabilities = [{caps}]\n\n\
             [[ui]]\nslot = \"{ui_slot}\"\nid = \"link\"\n\n\
             [[commands]]\nid = \"open\"\ntitle = \"Open\"\n[commands.action]\nkind = \"open-ui-link\"\nslot = \"{action_slot}\"\nid = \"link\"\n"
        )
    }

    #[test]
    fn open_ui_link_action_valid() {
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
    }

    #[test]
    fn open_ui_link_requires_capability() {
        let err =
            PluginManifest::from_toml_str(&open_ui_link_toml(6, "", "row-column", "row-column"))
                .unwrap_err()
                .to_string();
        assert!(err.contains("browser_open"), "{err}");
    }

    #[test]
    fn open_ui_link_requires_api_version_6() {
        let err = PluginManifest::from_toml_str(&open_ui_link_toml(
            5,
            "\"browser_open\"",
            "row-column",
            "row-column",
        ))
        .unwrap_err()
        .to_string();
        assert!(err.contains("api_version"), "{err}");
    }

    #[test]
    fn open_ui_link_rejects_global_slot() {
        let err = PluginManifest::from_toml_str(&open_ui_link_toml(
            6,
            "\"browser_open\"",
            "status-bar",
            "status-bar",
        ))
        .unwrap_err()
        .to_string();
        assert!(err.contains("per-session"), "{err}");
    }

    #[test]
    fn open_ui_link_requires_declared_slot() {
        // Declares a row-badge ui slot but the action points at row-column.
        let err = PluginManifest::from_toml_str(&open_ui_link_toml(
            6,
            "\"browser_open\"",
            "row-badge",
            "row-column",
        ))
        .unwrap_err()
        .to_string();
        assert!(err.contains("exactly one ui slot"), "{err}");
    }

    #[test]
    fn open_ui_link_rejects_duplicate_slot() {
        // Two ui contributions declare the same per-session (slot, id), so the
        // action's target is ambiguous and must be rejected.
        let err = PluginManifest::from_toml_str(
            "id = \"acme.thing\"\nname = \"Thing\"\nversion = \"1.0.0\"\napi_version = 6\ncapabilities = [\"browser_open\"]\n\n\
             [[ui]]\nslot = \"row-column\"\nid = \"link\"\n\n[[ui]]\nslot = \"row-column\"\nid = \"link\"\n\n\
             [[commands]]\nid = \"open\"\n[commands.action]\nkind = \"open-ui-link\"\nslot = \"row-column\"\nid = \"link\"\n",
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("exactly one ui slot"), "{err}");
    }

    #[test]
    fn setting_type_accepts_boolean_and_bool() {
        // `boolean` is the natural spelling next to `integer`, and shipped
        // plugins (plugin-github) use it; both must parse to Bool.
        for spelling in ["boolean", "bool"] {
            let manifest = PluginManifest::from_toml_str(&format!(
                "id = \"acme.thing\"\nname = \"Thing\"\nversion = \"1.0.0\"\napi_version = 4\n\n[[settings]]\nkey = \"flag\"\ntype = \"{spelling}\"\n"
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
