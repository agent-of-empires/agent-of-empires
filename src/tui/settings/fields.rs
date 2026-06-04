//! Settings field definitions, built from the single-source schema.
//!
//! Every configurable field is declared once on its `Config` sub-struct via
//! `#[derive(SettingsSection)]` (see `aoe-settings-derive`). This module turns
//! the resulting [`FieldDescriptor`] list into renderable [`SettingField`]
//! rows and applies edits back, instead of hand-wiring a `build_*_fields` /
//! `apply_field_*` pair per field. Reads and writes go through the serialized
//! `Config` JSON and the sparse profile/repo override JSON, so adding a config
//! field never touches this file.
//!
//! A handful of rows are not schema-backed and are injected explicitly:
//! - the profile `Description` (profile-only, no global counterpart),
//! - the lifecycle `hooks` (a repo-hook RCE surface, deliberately kept out of
//!   the web-exposed schema), and
//! - the host `environment` list (a root-level `Config` field).
//!
//! The five `custom:*` widgets (theme picker, default-tool picker, sound mode
//! and volume, per-target logging matrix) keep bespoke value mapping here,
//! keyed by the widget id from the schema.

use serde_json::{json, Value};

use crate::session::settings_schema::{
    clear_path, merge_json, schema, FieldDescriptor, ValidationKind, WidgetKind,
};
use crate::session::{validate_snooze_duration, Config, ProfileConfig};
use crate::sound::{
    validate_sound_exists, volume_from_option, volume_options, volume_to_index, SoundMode,
};
use crate::tui::styles::available_themes;

use super::SettingsScope;

/// Categories of settings. Each maps to a tab in the left-hand panel. The
/// string returned by [`SettingsCategory::schema_name`] is matched against the
/// per-field `category` emitted by the schema.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettingsCategory {
    Theme,
    Updates,
    Telemetry,
    Worktree,
    Sandbox,
    Tmux,
    Session,
    Agents,
    Interaction,
    Sound,
    StatusHooks,
    Hooks,
    Web,
    Acp,
    Diff,
    Logging,
}

impl SettingsCategory {
    pub fn label(&self) -> &'static str {
        match self {
            Self::Theme => "Theme",
            Self::Updates => "Updates",
            Self::Telemetry => "Telemetry",
            Self::Worktree => "Worktree",
            Self::Sandbox => "Sandbox",
            Self::Tmux => "Tmux",
            Self::Session => "Session",
            Self::Agents => "Agents",
            Self::Interaction => "Interaction",
            Self::Sound => "Sound",
            Self::StatusHooks => "Status Hooks",
            Self::Hooks => "Lifecycle Hooks",
            Self::Web => "Web",
            Self::Acp => "Acp",
            Self::Diff => "Diff",
            Self::Logging => "Logging",
        }
    }

    /// The `category` string the schema emits for fields in this tab.
    /// `Hooks` has no schema fields (lifecycle hooks are injected, not
    /// schema-backed), so its name is only used for completeness.
    fn schema_name(&self) -> &'static str {
        match self {
            Self::Theme => "Theme",
            Self::Updates => "Updates",
            Self::Worktree => "Worktree",
            Self::Sandbox => "Sandbox",
            Self::Tmux => "Tmux",
            Self::Session => "Session",
            Self::Agents => "Agents",
            Self::Interaction => "Interaction",
            Self::Sound => "Sound",
            Self::StatusHooks => "Status Hooks",
            Self::Hooks => "Lifecycle Hooks",
            Self::Web => "Web",
            Self::Acp => "Acp",
            Self::Diff => "Diff",
            Self::Telemetry => "Telemetry",
            Self::Logging => "Logging",
        }
    }
}

/// Which lifecycle hook list a [`FieldKind::Hook`] row edits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HookField {
    OnCreate,
    OnLaunch,
    OnDestroy,
}

impl HookField {
    fn field(&self) -> &'static str {
        match self {
            Self::OnCreate => "on_create",
            Self::OnLaunch => "on_launch",
            Self::OnDestroy => "on_destroy",
        }
    }
}

/// Identity + apply metadata for a settings row.
///
/// `Schema` rows carry the dotted `section.field` path plus the widget and
/// validation pulled straight from the descriptor, so apply/clear is a generic
/// JSON-path write. The remaining variants are the non-schema rows the TUI
/// injects (see the module docs) plus the section divider.
#[derive(Debug, Clone)]
pub enum FieldKind {
    Schema {
        section: String,
        field: String,
        widget: WidgetKind,
        validation: ValidationKind,
        profile_overridable: bool,
    },
    /// Profile-only description (no global counterpart to inherit).
    ProfileDescription,
    /// Lifecycle hook list (`config.hooks.*`); not in the schema.
    Hook(HookField),
    /// Host environment list (`Config.environment`, root-level).
    HostEnvironment,
    /// One per-target logging-level row, carrying the index into
    /// [`crate::logging::KNOWN_SUB_TARGETS`].
    LoggingTarget(usize),
    /// Non-interactive divider rendered as a styled heading.
    SectionMarker,
}

/// Value types for settings fields.
#[derive(Debug, Clone)]
pub enum FieldValue {
    Bool(bool),
    Text(String),
    Number(u64),
    Select {
        selected: usize,
        options: Vec<String>,
    },
    List(Vec<String>),
    OptionalText(Option<String>),
    /// Non-interactive section divider. The label carries the heading text and
    /// the description the optional subtitle. Navigation skips it; apply/clear
    /// no-op.
    SectionHeader,
}

/// A setting field with metadata.
#[derive(Debug, Clone)]
pub struct SettingField {
    pub kind: FieldKind,
    pub label: String,
    pub description: String,
    pub value: FieldValue,
    pub category: SettingsCategory,
    /// Whether this field has a profile/repo override.
    pub has_override: bool,
    /// Display of the inherited (global/base) value; set when `has_override`.
    pub inherited_display: Option<String>,
}

/// Which list-entry grammar a `List` row enforces while editing items.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ListItemValidation {
    None,
    /// `agent_name=value`, where `agent_name` is a known agent.
    AgentKeyValue,
    /// `name=command`, where `name` does not collide with a built-in agent.
    CustomAgent,
    /// `name=builtin`, where `builtin` is a known agent.
    DetectAs,
    /// `name=command`, an ACP launch command split into argv (acp).
    AcpCmd,
    /// Host/sandbox env entry (`KEY=value` etc).
    EnvEntry,
}

impl SettingField {
    /// True when this entry is a non-interactive section divider.
    pub fn is_section_header(&self) -> bool {
        matches!(self.value, FieldValue::SectionHeader)
    }

    /// Stable identifier used by the search overlay to relocate the cursor
    /// after a jump, and by input handlers that special-case a specific field.
    pub fn ident(&self) -> String {
        match &self.kind {
            FieldKind::Schema { section, field, .. } => format!("{section}.{field}"),
            FieldKind::ProfileDescription => "__profile.description".to_string(),
            FieldKind::Hook(h) => format!("hooks.{}", h.field()),
            FieldKind::HostEnvironment => "environment".to_string(),
            FieldKind::LoggingTarget(i) => format!("logging.targets.{i}"),
            FieldKind::SectionMarker => format!("__section.{}", self.label),
        }
    }

    /// The sandbox custom-instruction field opens a multiline dialog on Enter.
    pub fn is_custom_instruction(&self) -> bool {
        matches!(
            &self.kind,
            FieldKind::Schema { section, field, .. }
                if section == "sandbox" && field == "custom_instruction"
        )
    }

    /// The theme picker live-previews on edit.
    pub fn is_theme_name(&self) -> bool {
        matches!(
            &self.kind,
            FieldKind::Schema { widget: WidgetKind::Custom { id }, .. } if id == "theme-name"
        )
    }

    /// Grammar enforced on each entry while editing a list field.
    pub fn list_item_validation(&self) -> ListItemValidation {
        match &self.kind {
            FieldKind::Schema { section, field, .. } if section == "session" => {
                match field.as_str() {
                    "agent_extra_args" | "agent_command_override" => {
                        ListItemValidation::AgentKeyValue
                    }
                    "custom_agents" => ListItemValidation::CustomAgent,
                    "agent_detect_as" => ListItemValidation::DetectAs,
                    "agent_acp_cmd" => ListItemValidation::AcpCmd,
                    _ => ListItemValidation::None,
                }
            }
            FieldKind::Schema { section, field, .. }
                if section == "sandbox" && field == "environment" =>
            {
                ListItemValidation::EnvEntry
            }
            _ => ListItemValidation::None,
        }
    }

    pub fn validate(&self) -> Result<(), String> {
        match (&self.kind, &self.value) {
            // Snooze has a domain-specific message; defer to its validator.
            (FieldKind::Schema { section, field, .. }, FieldValue::Number(n))
                if section == "session" && field == "snooze_duration_minutes" =>
            {
                validate_snooze_duration(*n)
            }
            // Sound files must exist on disk if named.
            (FieldKind::Schema { section, .. }, FieldValue::OptionalText(Some(name)))
                if section == "sound" =>
            {
                if !name.is_empty() {
                    validate_sound_exists(name)?;
                }
                Ok(())
            }
            // Everything else: enforce the schema's server-authoritative rule.
            (FieldKind::Schema { validation, .. }, value) => {
                validate_field_value(validation, value)
            }
            _ => Ok(()),
        }
    }
}

/// Validate a `FieldValue` against the schema rule, reusing the same
/// [`crate::session::settings_schema::validate_value`] the server applies.
fn validate_field_value(kind: &ValidationKind, value: &FieldValue) -> Result<(), String> {
    let json = field_value_to_json_for_validation(value);
    crate::session::settings_schema::validate_value(kind, &json).map_err(|e| e.reason)
}

/// Best-effort JSON projection of a `FieldValue` for validation. Selects pass
/// their label (validation rules never gate select fields), lists pass their
/// raw string entries.
fn field_value_to_json_for_validation(value: &FieldValue) -> Value {
    match value {
        FieldValue::Bool(b) => json!(b),
        FieldValue::Text(s) => json!(s),
        FieldValue::Number(n) => json!(n),
        FieldValue::OptionalText(v) => match v {
            Some(s) => json!(s),
            None => Value::Null,
        },
        FieldValue::Select { selected, options } => {
            json!(options.get(*selected).cloned().unwrap_or_default())
        }
        FieldValue::List(items) => json!(items),
        FieldValue::SectionHeader => Value::Null,
    }
}

/// Convert a `FieldValue` to a human-readable display string.
fn value_display_string(value: &FieldValue) -> String {
    match value {
        FieldValue::Bool(v) => if *v { "on" } else { "off" }.to_string(),
        FieldValue::Text(v) => {
            if v.is_empty() {
                "(empty)".to_string()
            } else {
                v.clone()
            }
        }
        FieldValue::Number(v) => v.to_string(),
        FieldValue::Select { selected, options } => {
            options.get(*selected).cloned().unwrap_or_default()
        }
        FieldValue::List(items) => format!("[{} items]", items.len()),
        FieldValue::OptionalText(v) => v.clone().unwrap_or_else(|| "(empty)".to_string()),
        FieldValue::SectionHeader => String::new(),
    }
}

/// Parse `key=value` strings into a JSON object (for map-backed list fields).
fn parse_key_value_object(items: &[String]) -> Value {
    let mut map = serde_json::Map::new();
    for item in items {
        if let Some((k, v)) = item.split_once('=') {
            map.insert(k.to_string(), json!(v));
        }
    }
    Value::Object(map)
}

/// The `session` map fields rendered as `key=value` lists. Their JSON is
/// an object, not an array, so apply must reconstruct the object form.
fn is_map_list(section: &str, field: &str) -> bool {
    section == "session"
        && matches!(
            field,
            "agent_extra_args"
                | "agent_command_override"
                | "custom_agents"
                | "agent_detect_as"
                | "agent_acp_cmd"
        )
}

/// Look up a `section.field` leaf in a serialized config / override object.
fn json_at<'a>(root: &'a Value, section: &str, field: &str) -> Option<&'a Value> {
    root.get(section)?.get(field)
}

// ---------------------------------------------------------------------------
// Reading: JSON value -> FieldValue
// ---------------------------------------------------------------------------

/// Build a `FieldValue` for a schema field from its current effective JSON.
fn value_from_json(widget: &WidgetKind, current: Option<&Value>) -> FieldValue {
    let null = Value::Null;
    let current = current.unwrap_or(&null);
    match widget {
        WidgetKind::Toggle => FieldValue::Bool(current.as_bool().unwrap_or(false)),
        WidgetKind::Text { .. } => FieldValue::Text(current.as_str().unwrap_or("").to_string()),
        WidgetKind::OptionalText { .. } => {
            FieldValue::OptionalText(current.as_str().map(|s| s.to_string()))
        }
        WidgetKind::Number { .. } | WidgetKind::Slider { .. } => {
            FieldValue::Number(current.as_u64().unwrap_or(0))
        }
        WidgetKind::Select { options } => {
            let labels: Vec<String> = options.iter().map(|o| o.label.clone()).collect();
            let selected = current
                .as_str()
                .and_then(|cur| options.iter().position(|o| o.value == cur))
                .unwrap_or(0);
            FieldValue::Select {
                selected,
                options: labels,
            }
        }
        WidgetKind::List => FieldValue::List(json_to_list(current)),
        WidgetKind::Custom { id } => custom_value_from_json(id, current),
    }
}

/// Convert a JSON value into the list-of-strings the List widget renders.
/// Objects become sorted `key=value` rows; arrays become their string entries.
fn json_to_list(current: &Value) -> Vec<String> {
    match current {
        Value::Object(map) => {
            let mut items: Vec<String> = map
                .iter()
                .map(|(k, v)| format!("{}={}", k, v.as_str().unwrap_or_default()))
                .collect();
            items.sort();
            items
        }
        Value::Array(arr) => arr
            .iter()
            .map(|v| v.as_str().unwrap_or_default().to_string())
            .collect(),
        _ => Vec::new(),
    }
}

/// Build a `FieldValue` for a `custom:*` widget from its current JSON.
fn custom_value_from_json(id: &str, current: &Value) -> FieldValue {
    match id {
        "theme-name" => {
            let options = available_themes();
            let name = current.as_str().unwrap_or("");
            let selected = options.iter().position(|o| o == name).unwrap_or(0);
            FieldValue::Select { selected, options }
        }
        "default-tool" => {
            let mut options = vec!["Auto (first available)".to_string()];
            options.extend(crate::agents::agent_names().iter().map(|n| n.to_string()));
            let selected = crate::agents::settings_index_from_name(current.as_str());
            FieldValue::Select { selected, options }
        }
        "sound-mode" => {
            let mode: SoundMode =
                serde_json::from_value(current.clone()).unwrap_or(SoundMode::Random);
            let selected = match mode {
                SoundMode::Random => 0,
                SoundMode::Specific(_) => 1,
            };
            FieldValue::Select {
                selected,
                options: vec!["Random".to_string(), "Specific".to_string()],
            }
        }
        "sound-volume" => {
            let options = volume_options();
            let selected = volume_to_index(current.as_f64().unwrap_or(1.0));
            FieldValue::Select { selected, options }
        }
        // logging-targets is expanded into per-target rows during build and
        // never lands here.
        _ => FieldValue::Text(String::new()),
    }
}

// ---------------------------------------------------------------------------
// Writing: FieldValue -> JSON leaf
// ---------------------------------------------------------------------------

/// Compute the JSON leaf a field's value writes into config / override JSON.
/// Returns `None` for rows that never write (section markers).
fn field_value_to_json(kind: &FieldKind, value: &FieldValue) -> Option<Value> {
    match (kind, value) {
        (FieldKind::SectionMarker, _) => None,
        (FieldKind::ProfileDescription, FieldValue::OptionalText(v)) => Some(
            v.as_ref()
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .map(Value::from)
                .unwrap_or(Value::Null),
        ),
        (FieldKind::Hook(_) | FieldKind::HostEnvironment, FieldValue::List(items)) => {
            Some(json!(items))
        }
        (
            FieldKind::Schema {
                widget,
                section,
                field,
                ..
            },
            value,
        ) => Some(schema_value_to_json(widget, section, field, value)),
        // LoggingTarget is applied via a dedicated path (mutates the targets
        // map), not as a single leaf.
        _ => None,
    }
}

/// Convert a schema field's `FieldValue` to its JSON leaf.
fn schema_value_to_json(
    widget: &WidgetKind,
    section: &str,
    field: &str,
    value: &FieldValue,
) -> Value {
    match (widget, value) {
        (WidgetKind::Toggle, FieldValue::Bool(b)) => json!(b),
        (WidgetKind::Text { .. }, FieldValue::Text(s)) => json!(s),
        (WidgetKind::OptionalText { .. }, FieldValue::OptionalText(Some(s))) => json!(s),
        (WidgetKind::OptionalText { .. }, FieldValue::OptionalText(None)) => Value::Null,
        (WidgetKind::Number { .. } | WidgetKind::Slider { .. }, FieldValue::Number(n)) => {
            json!(n)
        }
        (WidgetKind::Select { options }, FieldValue::Select { selected, .. }) => options
            .get(*selected)
            .map(|o| json!(o.value))
            .unwrap_or(Value::Null),
        (WidgetKind::List, FieldValue::List(items)) => {
            if is_map_list(section, field) {
                parse_key_value_object(items)
            } else {
                json!(items)
            }
        }
        (WidgetKind::Custom { id }, value) => custom_value_to_json(id, value),
        _ => Value::Null,
    }
}

/// Convert a `custom:*` widget's `FieldValue` back to its JSON leaf.
fn custom_value_to_json(id: &str, value: &FieldValue) -> Value {
    match (id, value) {
        ("theme-name", FieldValue::Select { selected, options }) => {
            json!(options.get(*selected).cloned().unwrap_or_default())
        }
        ("default-tool", FieldValue::Select { selected, .. }) => {
            match crate::agents::name_from_settings_index(*selected) {
                Some(name) => json!(name),
                None => Value::Null,
            }
        }
        ("sound-mode", FieldValue::Select { selected, .. }) => {
            let mode = if *selected == 1 {
                SoundMode::Specific(String::new())
            } else {
                SoundMode::Random
            };
            serde_json::to_value(mode).unwrap_or(Value::Null)
        }
        ("sound-volume", FieldValue::Select { selected, options }) => options
            .get(*selected)
            .map(|s| json!(volume_from_option(s)))
            .unwrap_or(Value::Null),
        _ => Value::Null,
    }
}

// ---------------------------------------------------------------------------
// Building the rows for a category
// ---------------------------------------------------------------------------

/// Build fields for a category based on scope and current config values.
///
/// For Repo scope, `base` should be the resolved (global+profile merged)
/// config and `overrides` the repo config converted to a `ProfileConfig`.
pub fn build_fields_for_category(
    category: SettingsCategory,
    scope: SettingsScope,
    base: &Config,
    overrides: &ProfileConfig,
) -> Vec<SettingField> {
    let base_json = serde_json::to_value(base).unwrap_or_else(|_| json!({}));
    let over_json = serde_json::to_value(overrides).unwrap_or_else(|_| json!({}));
    let effective_json = match scope {
        SettingsScope::Global => base_json.clone(),
        SettingsScope::Profile | SettingsScope::Repo => {
            let mut merged = base_json.clone();
            merge_json(&mut merged, &over_json);
            merged
        }
    };

    let ctx = BuildCtx {
        scope,
        base_json: &base_json,
        over_json: &over_json,
        effective_json: &effective_json,
    };

    // Lifecycle hooks are the entire Hooks tab (not schema-backed).
    if category == SettingsCategory::Hooks {
        return build_hook_rows(&ctx);
    }

    let mut primary: Vec<SettingField> = Vec::new();
    let mut advanced: Vec<SettingField> = Vec::new();

    // Profile description sits at the very top of the Theme tab, profile-only.
    if category == SettingsCategory::Theme && scope == SettingsScope::Profile {
        primary.push(SettingField {
            kind: FieldKind::ProfileDescription,
            label: "Description".to_string(),
            description:
                "Short, human-readable description of what this profile does. Shown as helper \
                 text under the profile name in the new-session picker (TUI + web)."
                    .to_string(),
            value: FieldValue::OptionalText(overrides.description.clone()),
            category,
            has_override: overrides.description.is_some(),
            inherited_display: None,
        });
    }

    for desc in schema()
        .into_iter()
        .filter(|d| d.category == category.schema_name())
    {
        // The per-target logging matrix expands one descriptor into N rows.
        if matches!(&desc.widget, WidgetKind::Custom { id } if id == "logging-targets") {
            primary.extend(build_logging_target_rows(category, &ctx));
            continue;
        }
        let row = build_schema_row(category, &desc, &ctx);
        if desc.advanced {
            advanced.push(row);
        } else {
            primary.push(row);
        }
    }

    // Host environment list lives in the Session tab (root-level Config field).
    if category == SettingsCategory::Session {
        primary.push(build_host_environment_row(&ctx));
    }

    if advanced.is_empty() {
        primary
    } else {
        primary.push(SettingField {
            kind: FieldKind::SectionMarker,
            label: "Advanced".to_string(),
            description:
                "Operational tuning, rarely needed after first setup. Adjust only if you've read \
                 the description and know what you're changing."
                    .to_string(),
            value: FieldValue::SectionHeader,
            category,
            has_override: false,
            inherited_display: None,
        });
        primary.extend(advanced);
        primary
    }
}

/// Shared inputs for building rows in one category/scope pass.
struct BuildCtx<'a> {
    scope: SettingsScope,
    base_json: &'a Value,
    over_json: &'a Value,
    effective_json: &'a Value,
}

impl BuildCtx<'_> {
    /// Whether the profile/repo override sets `section.field` (Profile/Repo
    /// scope only).
    fn has_override(&self, section: &str, field: &str) -> bool {
        matches!(self.scope, SettingsScope::Profile | SettingsScope::Repo)
            && json_at(self.over_json, section, field).is_some()
    }
}

/// Build a single schema-backed row.
fn build_schema_row(
    category: SettingsCategory,
    desc: &FieldDescriptor,
    ctx: &BuildCtx,
) -> SettingField {
    let value = value_from_json(
        &desc.widget,
        json_at(ctx.effective_json, &desc.section, &desc.field),
    );
    let has_override = desc.profile_overridable && ctx.has_override(&desc.section, &desc.field);
    let inherited_display = if has_override {
        let base_value = value_from_json(
            &desc.widget,
            json_at(ctx.base_json, &desc.section, &desc.field),
        );
        Some(value_display_string(&base_value))
    } else {
        None
    };
    SettingField {
        kind: FieldKind::Schema {
            section: desc.section.clone(),
            field: desc.field.clone(),
            widget: desc.widget.clone(),
            validation: desc.validation.clone(),
            profile_overridable: desc.profile_overridable,
        },
        label: desc.label.clone(),
        description: desc.description.clone(),
        value,
        category,
        has_override,
        inherited_display,
    }
}

/// The three lifecycle-hook rows (the whole Hooks tab).
fn build_hook_rows(ctx: &BuildCtx) -> Vec<SettingField> {
    let specs = [
        (
            HookField::OnCreate,
            "On Create",
            "Commands run once when a session is first created. Runs inside sandbox when enabled.",
        ),
        (
            HookField::OnLaunch,
            "On Launch",
            "Commands run every time a session starts. Runs inside sandbox when enabled.",
        ),
        (
            HookField::OnDestroy,
            "On Destroy",
            "Commands run when a session is deleted, before cleanup. Use for teardown (e.g. docker-compose down).",
        ),
    ];
    specs
        .into_iter()
        .map(|(hook, label, desc)| {
            let field = hook.field();
            let value = FieldValue::List(json_to_list(
                json_at(ctx.effective_json, "hooks", field).unwrap_or(&Value::Null),
            ));
            let has_override = ctx.has_override("hooks", field);
            let inherited_display = if has_override {
                Some(value_display_string(&FieldValue::List(json_to_list(
                    json_at(ctx.base_json, "hooks", field).unwrap_or(&Value::Null),
                ))))
            } else {
                None
            };
            SettingField {
                kind: FieldKind::Hook(hook),
                label: label.to_string(),
                description: desc.to_string(),
                value,
                category: SettingsCategory::Hooks,
                has_override,
                inherited_display,
            }
        })
        .collect()
}

/// The host environment list row (root-level `Config.environment`).
fn build_host_environment_row(ctx: &BuildCtx) -> SettingField {
    let value = FieldValue::List(json_to_list(
        ctx.effective_json
            .get("environment")
            .unwrap_or(&Value::Null),
    ));
    let has_override = matches!(ctx.scope, SettingsScope::Profile | SettingsScope::Repo)
        && ctx.over_json.get("environment").is_some();
    let inherited_display = if has_override {
        Some(value_display_string(&FieldValue::List(json_to_list(
            ctx.base_json.get("environment").unwrap_or(&Value::Null),
        ))))
    } else {
        None
    };
    SettingField {
        kind: FieldKind::HostEnvironment,
        label: "Host Environment".to_string(),
        description: "Env vars injected into the host command line: KEY=value (literal), KEY=$VAR \
             (passthrough from host), KEY=$$literal (escape a leading $), or bare KEY \
             (passthrough). All forms resolve to a literal `KEY=value` arg in the spawned \
             process, visible in `ps`; for secrets you want hidden from argv, configure \
             Sandbox > Sandbox Environment instead. Profile value replaces the global list."
            .to_string(),
        value,
        category: SettingsCategory::Session,
        has_override,
        inherited_display,
    }
}

const LOG_LEVEL_OVERRIDE_OPTIONS: &[&str] =
    &["(default)", "trace", "debug", "info", "warn", "error"];

/// Expand the per-target logging matrix into one Select row per known target.
fn build_logging_target_rows(category: SettingsCategory, ctx: &BuildCtx) -> Vec<SettingField> {
    let targets = ctx
        .effective_json
        .get("logging")
        .and_then(|l| l.get("targets"));
    crate::logging::KNOWN_SUB_TARGETS
        .iter()
        .enumerate()
        .map(|(i, target)| {
            let current = targets
                .and_then(|t| t.get(*target))
                .and_then(|v| v.as_str())
                .unwrap_or("(default)");
            let selected = LOG_LEVEL_OVERRIDE_OPTIONS
                .iter()
                .position(|o| *o == current)
                .unwrap_or(0);
            SettingField {
                kind: FieldKind::LoggingTarget(i),
                label: target.to_string(),
                description: "Per-target override; (default) inherits the baseline.".to_string(),
                value: FieldValue::Select {
                    selected,
                    options: LOG_LEVEL_OVERRIDE_OPTIONS
                        .iter()
                        .map(|s| s.to_string())
                        .collect(),
                },
                category,
                has_override: false,
                inherited_display: None,
            }
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Applying an edited value back to the config / override
// ---------------------------------------------------------------------------

/// Apply the current field value back to the configs.
pub fn apply_field_to_config(
    field: &SettingField,
    scope: SettingsScope,
    global: &mut Config,
    profile: &mut ProfileConfig,
) {
    // Per-target logging is global-only and mutates the targets map directly.
    if let FieldKind::LoggingTarget(idx) = field.kind {
        apply_logging_target(global, idx, &field.value);
        return;
    }

    // The profile description is stored on the override struct, not the
    // merged config, and only in profile/repo scope.
    if matches!(field.kind, FieldKind::ProfileDescription) {
        if matches!(scope, SettingsScope::Profile | SettingsScope::Repo) {
            if let FieldValue::OptionalText(v) = &field.value {
                profile.description = v
                    .as_ref()
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty());
            }
        }
        return;
    }

    let Some(leaf) = field_value_to_json(&field.kind, &field.value) else {
        return;
    };
    let (section, sub) = match &field.kind {
        FieldKind::Schema { section, field, .. } => (section.clone(), field.clone()),
        FieldKind::Hook(h) => ("hooks".to_string(), h.field().to_string()),
        FieldKind::HostEnvironment => {
            return apply_root_field(scope, global, profile, "environment", leaf)
        }
        _ => return,
    };

    match scope {
        SettingsScope::Global => set_config_path(global, &section, &sub, leaf),
        SettingsScope::Profile | SettingsScope::Repo => {
            set_override_path(profile, &section, &sub, leaf)
        }
    }
}

/// Apply a root-level (non-sectioned) config field such as `environment`.
fn apply_root_field(
    scope: SettingsScope,
    global: &mut Config,
    profile: &mut ProfileConfig,
    field: &str,
    leaf: Value,
) {
    match scope {
        SettingsScope::Global => {
            let mut j = serde_json::to_value(&*global).unwrap_or_else(|_| json!({}));
            if let Value::Object(map) = &mut j {
                map.insert(field.to_string(), leaf);
            }
            if let Ok(updated) = serde_json::from_value(j) {
                *global = updated;
            }
        }
        SettingsScope::Profile | SettingsScope::Repo => {
            let mut j = serde_json::to_value(&*profile).unwrap_or_else(|_| json!({}));
            if let Value::Object(map) = &mut j {
                map.insert(field.to_string(), leaf);
            }
            if let Ok(updated) = serde_json::from_value(j) {
                *profile = updated;
            }
        }
    }
}

/// Mutate one per-target logging level on the global config.
fn apply_logging_target(global: &mut Config, idx: usize, value: &FieldValue) {
    let Some(target) = crate::logging::KNOWN_SUB_TARGETS.get(idx) else {
        return;
    };
    if let FieldValue::Select { selected, options } = value {
        let level = options.get(*selected).cloned().unwrap_or_default();
        if level.is_empty() || level == "(default)" {
            global.logging.targets.remove(*target);
        } else {
            global.logging.targets.insert(target.to_string(), level);
        }
    }
}

/// Write `section.field = leaf` into a typed `Config` via its JSON form.
fn set_config_path(config: &mut Config, section: &str, field: &str, leaf: Value) {
    let mut j = serde_json::to_value(&*config).unwrap_or_else(|_| json!({}));
    merge_json(&mut j, &json!({ section: { field: leaf } }));
    if let Ok(updated) = serde_json::from_value(j) {
        *config = updated;
    }
}

/// Write `section.field = leaf` into a profile/repo override via its JSON form.
/// Always stores the value as an override; the `r` key clears it.
fn set_override_path(profile: &mut ProfileConfig, section: &str, field: &str, leaf: Value) {
    let mut j = serde_json::to_value(&*profile).unwrap_or_else(|_| json!({}));
    merge_json(&mut j, &json!({ section: { field: leaf } }));
    if let Ok(updated) = serde_json::from_value(j) {
        *profile = updated;
    }
}

/// Clear a profile/repo override, reverting the field to inherit the base.
/// Returns the value display the caller may want (e.g. theme re-preview).
pub fn clear_override(field: &SettingField, profile: &mut ProfileConfig) {
    match &field.kind {
        FieldKind::ProfileDescription => {
            profile.description = None;
        }
        FieldKind::HostEnvironment => {
            let mut j = serde_json::to_value(&*profile).unwrap_or_else(|_| json!({}));
            if let Value::Object(map) = &mut j {
                map.remove("environment");
            }
            if let Ok(updated) = serde_json::from_value(j) {
                *profile = updated;
            }
        }
        FieldKind::Hook(h) => clear_override_path(profile, "hooks", h.field()),
        FieldKind::Schema {
            section,
            field,
            profile_overridable,
            ..
        } => {
            if *profile_overridable {
                clear_override_path(profile, section, field);
            }
        }
        // Logging is global-only and section markers are non-interactive.
        FieldKind::LoggingTarget(_) | FieldKind::SectionMarker => {}
    }
}

fn clear_override_path(profile: &mut ProfileConfig, section: &str, field: &str) {
    let mut j = serde_json::to_value(&*profile).unwrap_or_else(|_| json!({}));
    clear_path(&mut j, section, field);
    if let Ok(updated) = serde_json::from_value(j) {
        *profile = updated;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Find a built field by its stable identity (`section.field`).
    fn field<'a>(fields: &'a [SettingField], ident: &str) -> &'a SettingField {
        fields
            .iter()
            .find(|f| f.ident() == ident)
            .unwrap_or_else(|| panic!("missing field {ident}"))
    }

    /// Whether a profile override sets `section.field`, checked via the
    /// serialized (storage-agnostic) form so the test survives the sparse-JSON
    /// storage flip.
    fn has_override_path(profile: &ProfileConfig, section: &str, field: &str) -> bool {
        serde_json::to_value(profile)
            .ok()
            .and_then(|v| v.get(section).and_then(|s| s.get(field)).cloned())
            .is_some()
    }

    fn profile_from(value: serde_json::Value) -> ProfileConfig {
        serde_json::from_value(value).expect("profile override deserializes")
    }

    #[test]
    fn profile_field_inherits_after_global_change() {
        let mut global = Config::default();
        let profile = ProfileConfig::default();

        let fields = build_fields_for_category(
            SettingsCategory::Updates,
            SettingsScope::Profile,
            &global,
            &profile,
        );
        assert!(!field(&fields, "updates.update_check_mode").has_override);

        // Changing the global value must not promote the profile to "override".
        global.updates.update_check_mode = crate::session::config::UpdateCheckMode::Off;
        let fields = build_fields_for_category(
            SettingsCategory::Updates,
            SettingsScope::Profile,
            &global,
            &profile,
        );
        assert!(
            !field(&fields, "updates.update_check_mode").has_override,
            "profile should inherit, not show an override, after a global change"
        );
    }

    #[test]
    fn profile_field_shows_override_after_profile_change() {
        let global = Config::default();
        let profile = profile_from(json!({"updates": {"update_check_mode": "off"}}));
        let fields = build_fields_for_category(
            SettingsCategory::Updates,
            SettingsScope::Profile,
            &global,
            &profile,
        );
        assert!(field(&fields, "updates.update_check_mode").has_override);
    }

    #[test]
    fn default_tool_options_include_all_registered_agents() {
        let global = Config::default();
        let profile = ProfileConfig::default();
        let fields = build_fields_for_category(
            SettingsCategory::Agents,
            SettingsScope::Global,
            &global,
            &profile,
        );
        let options = match &field(&fields, "session.default_tool").value {
            FieldValue::Select { options, .. } => options.clone(),
            other => panic!("default tool should be a Select, got {other:?}"),
        };
        // Skip the leading "Auto" entry; the rest must mirror the registry.
        let tool_options: Vec<&str> = options.iter().skip(1).map(|s| s.as_str()).collect();
        let agent_names = crate::agents::agent_names();
        for name in &agent_names {
            assert!(tool_options.contains(name), "missing agent {name}");
        }
        for option in &tool_options {
            assert!(agent_names.contains(option), "unknown agent {option}");
        }
    }

    #[test]
    fn apply_to_profile_always_stores_override_even_when_matching_global() {
        // Toggling a profile field back to the global value must keep the
        // override (the 'r' key is the only way to clear it), so the profile
        // does not silently start inheriting again.
        let mut global = Config::default();
        let mut profile = profile_from(json!({"updates": {"update_check_mode": "off"}}));
        let fields = build_fields_for_category(
            SettingsCategory::Updates,
            SettingsScope::Profile,
            &global,
            &profile,
        );
        // Re-apply the field as-is.
        let f = field(&fields, "updates.update_check_mode").clone();
        apply_field_to_config(&f, SettingsScope::Profile, &mut global, &mut profile);
        assert!(
            has_override_path(&profile, "updates", "update_check_mode"),
            "override must be preserved after re-apply"
        );
    }

    #[test]
    fn worktree_enabled_reads_global_value() {
        let mut global = Config::default();
        global.worktree.enabled = true;
        let profile = ProfileConfig::default();
        let fields = build_fields_for_category(
            SettingsCategory::Worktree,
            SettingsScope::Global,
            &global,
            &profile,
        );
        assert!(matches!(
            field(&fields, "worktree.enabled").value,
            FieldValue::Bool(true)
        ));
    }

    #[test]
    fn worktree_enabled_profile_override() {
        let global = Config::default();
        let profile = profile_from(json!({"worktree": {"enabled": true}}));
        let fields = build_fields_for_category(
            SettingsCategory::Worktree,
            SettingsScope::Profile,
            &global,
            &profile,
        );
        let f = field(&fields, "worktree.enabled");
        assert!(f.has_override);
        assert!(matches!(f.value, FieldValue::Bool(true)));
    }

    #[test]
    fn status_hook_debounce_reads_default_and_override() {
        let global = Config::default();
        let default_debounce = global.status_hooks.debounce_ms;

        let fields = build_fields_for_category(
            SettingsCategory::StatusHooks,
            SettingsScope::Global,
            &global,
            &ProfileConfig::default(),
        );
        assert!(matches!(
            field(&fields, "status_hooks.debounce_ms").value,
            FieldValue::Number(n) if n == default_debounce
        ));

        let profile = profile_from(json!({"status_hooks": {"debounce_ms": 500}}));
        let fields = build_fields_for_category(
            SettingsCategory::StatusHooks,
            SettingsScope::Profile,
            &global,
            &profile,
        );
        let f = field(&fields, "status_hooks.debounce_ms");
        assert!(f.has_override);
        assert!(matches!(f.value, FieldValue::Number(500)));
        assert_eq!(
            f.inherited_display.as_deref(),
            Some(default_debounce.to_string().as_str())
        );
    }

    #[test]
    fn status_hook_debounce_applies_global_and_profile() {
        let mut global = Config::default();
        let mut profile = ProfileConfig::default();
        let mut f = field(
            &build_fields_for_category(
                SettingsCategory::StatusHooks,
                SettingsScope::Global,
                &global,
                &profile,
            ),
            "status_hooks.debounce_ms",
        )
        .clone();
        f.value = FieldValue::Number(250);

        apply_field_to_config(&f, SettingsScope::Global, &mut global, &mut profile);
        assert_eq!(global.status_hooks.debounce_ms, 250);

        apply_field_to_config(&f, SettingsScope::Profile, &mut global, &mut profile);
        assert!(has_override_path(&profile, "status_hooks", "debounce_ms"));
    }

    #[test]
    fn acp_fields_have_advanced_section_marker() {
        let global = Config::default();
        let profile = ProfileConfig::default();
        let fields = build_fields_for_category(
            SettingsCategory::Acp,
            SettingsScope::Global,
            &global,
            &profile,
        );
        let header_idx = fields
            .iter()
            .position(|f| matches!(f.value, FieldValue::SectionHeader))
            .expect("acp should contain an Advanced section header");
        assert_eq!(fields[header_idx].label, "Advanced");
        for ident in [
            "acp.default_agent",
            "acp.replay_events",
            "acp.node_path",
            "acp.show_tool_durations",
        ] {
            let pos = fields.iter().position(|f| f.ident() == ident).unwrap();
            assert!(pos < header_idx, "{ident} must precede the Advanced header");
        }
        for ident in [
            "acp.max_concurrent_workers",
            "acp.max_concurrent_resumes",
            "acp.queue_drain_mode",
            "acp.replay_bytes",
            "acp.force_end_turn_threshold_secs",
            "acp.silent_orphan_grace_secs",
            "acp.silent_orphan_fast_grace_secs",
        ] {
            let pos = fields.iter().position(|f| f.ident() == ident).unwrap();
            assert!(pos > header_idx, "{ident} must follow the Advanced header");
        }
    }

    #[test]
    fn session_split_routes_fields_to_their_tabs() {
        let global = Config::default();
        let profile = ProfileConfig::default();
        let idents = |cat| -> Vec<String> {
            build_fields_for_category(cat, SettingsScope::Global, &global, &profile)
                .iter()
                .map(|f| f.ident())
                .collect()
        };

        let agents = idents(SettingsCategory::Agents);
        for ident in [
            "session.default_tool",
            "session.agent_extra_args",
            "session.agent_command_override",
            "session.custom_agents",
            "session.agent_detect_as",
            "session.agent_status_hooks",
        ] {
            assert!(
                agents.contains(&ident.to_string()),
                "Agents missing {ident}"
            );
        }

        let interaction = idents(SettingsCategory::Interaction);
        for ident in [
            "session.default_attach_mode",
            "session.new_session_attach_mode",
            "session.click_action",
            "session.live_send_exit_chord",
            "session.mouse_capture",
        ] {
            assert!(
                interaction.contains(&ident.to_string()),
                "Interaction missing {ident}"
            );
        }

        let session = idents(SettingsCategory::Session);
        for ident in [
            "session.default_tool",
            "session.agent_extra_args",
            "session.default_attach_mode",
            "session.live_send_exit_chord",
        ] {
            assert!(
                !session.contains(&ident.to_string()),
                "{ident} should have moved out of the Session tab"
            );
        }
    }

    #[test]
    fn host_environment_row_present_in_session_tab() {
        let global = Config::default();
        let profile = ProfileConfig::default();
        let fields = build_fields_for_category(
            SettingsCategory::Session,
            SettingsScope::Global,
            &global,
            &profile,
        );
        assert!(fields.iter().any(|f| f.ident() == "environment"));
    }

    #[test]
    fn lifecycle_hooks_are_the_hooks_tab() {
        let global = Config::default();
        let profile = ProfileConfig::default();
        let fields = build_fields_for_category(
            SettingsCategory::Hooks,
            SettingsScope::Global,
            &global,
            &profile,
        );
        let idents: Vec<String> = fields.iter().map(|f| f.ident()).collect();
        assert_eq!(
            idents,
            vec!["hooks.on_create", "hooks.on_launch", "hooks.on_destroy"]
        );
    }
}
