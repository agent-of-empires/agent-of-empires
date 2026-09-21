//! Settings field definitions, built from the single-source schema.
//!
//! [`FieldDescriptor`] rows become renderable [`SettingField`]s, read and
//! written through the serialized `Config` JSON and the sparse override JSON,
//! so adding a config field never touches this file.
//!
//! Injected explicitly because they are not schema-backed: the profile
//! `Description`, the lifecycle `hooks` (kept out of the web-exposed schema,
//! being a repo-hook RCE surface), and the root-level `environment` list.
//! `custom:*` widgets keep bespoke value mapping here, keyed by widget id.

use serde_json::{json, Value};

use crate::session::config::settings_schema::{
    clear_path, merge_json, FieldDescriptor, ValidationKind, WidgetKind,
};
use crate::session::{validate_snooze_duration, Config, ProfileConfig};
use crate::sound::{validate_sound_exists, volume_from_option, volume_options, volume_to_index};
use crate::tui::styles::available_themes;

use super::SettingsScope;

/// Categories of settings, one per tab. `schema_name` matches the per-field
/// `category` the schema emits.
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
    Plugins,
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
            Self::Plugins => "Plugins",
        }
    }

    /// The `category` string the schema emits for this tab. `Hooks` has no
    /// schema fields, so its name is only for completeness.
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
            Self::Plugins => "Plugins",
        }
    }
}

/// Which lifecycle hook list a `FieldKind::Hook` row edits.
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

/// Identity and apply metadata for a settings row. `Schema` rows carry the
/// dotted `section.field` path, so apply/clear is a generic JSON-path write;
/// the rest are the injected rows plus the section divider.
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
    /// Non-interactive divider: label is the heading, description the
    /// subtitle. Navigation skips it; apply/clear no-op.
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
    /// The inherited value, set when `has_override`.
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
    /// `name=dir`, an absolute (or `~/`) config directory for an agent.
    AgentConfigDir,
    /// Host/sandbox env entry (`KEY=value` etc).
    EnvEntry,
}

impl SettingField {
    /// True when this entry is a non-interactive section divider.
    pub fn is_section_header(&self) -> bool {
        matches!(self.value, FieldValue::SectionHeader)
    }

    /// Human-readable rendering of the current value (for read-only displays).
    pub fn display_value(&self) -> String {
        value_display_string(&self.value)
    }

    /// The schema `section` this row edits, if it is a schema-backed row.
    pub fn schema_section(&self) -> Option<&str> {
        match &self.kind {
            FieldKind::Schema { section, .. } => Some(section),
            _ => None,
        }
    }

    /// Stable identifier the search overlay jumps to and input handlers
    /// special-case.
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
                    "agent_config_dir" => ListItemValidation::AgentConfigDir,
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
            // Edited as raw JSON; require an object so a typo is rejected at
            // commit instead of wiping the map.
            (
                FieldKind::Schema {
                    widget: WidgetKind::Custom { id },
                    ..
                },
                FieldValue::Text(s),
            ) if id == "acp-defaults" || id == "smart-rename-model" => {
                let trimmed = s.trim();
                if trimmed.is_empty() {
                    return Ok(());
                }
                match serde_json::from_str::<Value>(trimmed) {
                    Ok(v) if v.is_object() => Ok(()),
                    Ok(_) => Err("Must be a JSON object (agent -> value)".to_string()),
                    Err(e) => Err(format!("Invalid JSON: {e}")),
                }
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
/// [`crate::session::config::settings_schema::validate_value`] the server applies.
fn validate_field_value(kind: &ValidationKind, value: &FieldValue) -> Result<(), String> {
    let json = field_value_to_json_for_validation(value);
    // A cleared optional field projects to null, which means "unset" and is
    // always valid; a string validator would reject it. Mirrors the server's
    // null-leaf handling in `settings_schema::validate_patch`.
    if json.is_null() {
        return Ok(());
    }
    crate::session::config::settings_schema::validate_value(kind, &json).map_err(|e| e.reason)
}

/// JSON projection of a `FieldValue` for validation. Selects pass their label,
/// lists their raw entries.
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
                | "agent_config_dir"
        )
}

/// Look up a `section.field` leaf in a serialized config / override object.
fn json_at<'a>(root: &'a Value, section: &str, field: &str) -> Option<&'a Value> {
    root.get(section)?.get(field)
}

/// Build the `{section: {field: leaf}}` JSON object that writes `section.field`.
fn nested_leaf(section: &str, field: &str, leaf: Value) -> Value {
    json!({ section: { field: leaf } })
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
        // dynamic_select and cron edit as plain text in the TUI; object_list
        // renders as its raw JSON array so it stays round-trippable.
        WidgetKind::DynamicSelect { .. } | WidgetKind::Cron => {
            FieldValue::Text(current.as_str().unwrap_or("").to_string())
        }
        WidgetKind::ObjectList { .. } => {
            let text = if current.is_null() {
                "[]".to_string()
            } else {
                serde_json::to_string_pretty(current).unwrap_or_else(|_| "[]".to_string())
            };
            FieldValue::Text(text)
        }
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

/// Built-in agents with a `oneshot_flag` that are installed here, so the
/// smart-rename picker cannot offer one the one-shot would fail on. Probed in
/// one memoized batch: field rebuilds are frequent and a per-agent probe paid
/// a login shell each time. Avoids `AvailableTools::detect()`'s `Config::load`.
fn installed_oneshot_agents() -> Vec<String> {
    let defs: Vec<&crate::agents::AgentDef> = crate::agents::oneshot_capable_names()
        .into_iter()
        .filter_map(crate::agents::get_agent)
        .collect();
    let available = crate::tmux::probe_agents_available(&defs);
    defs.iter()
        .filter(|d| available.contains(d.name))
        .map(|d| d.name.to_string())
        .collect()
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
        "smart-rename-agent" => {
            let names = installed_oneshot_agents();
            let mut options = vec!["Same as session".to_string()];
            options.extend(names.iter().cloned());
            let current = current.as_str().unwrap_or("").trim();
            let selected = if current.is_empty() {
                0
            } else {
                names.iter().position(|n| n == current).map_or(0, |i| i + 1)
            };
            FieldValue::Select { selected, options }
        }
        "sound-volume" => {
            let options = volume_options();
            let selected = volume_to_index(current.as_f64().unwrap_or(1.0));
            FieldValue::Select { selected, options }
        }
        "acp-defaults" => {
            // Raw JSON inline; commit validation rejects non-objects.
            let text = if current.is_null() {
                "{}".to_string()
            } else {
                serde_json::to_string(current).unwrap_or_else(|_| "{}".to_string())
            };
            FieldValue::Text(text)
        }
        "smart-rename-model" => {
            // A per-agent {agent: model} map as raw JSON. The only surface
            // that can set the empty-string "force CLI default" value, since
            // the web widget removes a key on clear.
            let text = if current.is_null() {
                "{}".to_string()
            } else {
                serde_json::to_string(current).unwrap_or_else(|_| "{}".to_string())
            };
            FieldValue::Text(text)
        }
        // logging-targets expands into per-target rows during build.
        _ => FieldValue::Text(String::new()),
    }
}

// ---------------------------------------------------------------------------
// Writing: FieldValue -> JSON leaf
// ---------------------------------------------------------------------------

/// The JSON leaf a field writes, `None` for rows that never write.
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
        // LoggingTarget mutates the targets map via its own path.
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
        // dynamic_select and cron round-trip as text; object_list parses its
        // raw JSON back to an array, storing null on a parse failure so the
        // row surfaces a validation error rather than corrupting.
        (WidgetKind::DynamicSelect { .. } | WidgetKind::Cron, FieldValue::Text(s)) => json!(s),
        (WidgetKind::ObjectList { .. }, FieldValue::Text(s)) => {
            serde_json::from_str::<Value>(s).unwrap_or(Value::Null)
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
        ("smart-rename-agent", FieldValue::Select { selected, options }) => {
            // Index 0 is "Same as session" (empty string), the rest agent
            // names. Persist from the rendered `options`, so an installed-set
            // change between render and save cannot shift the pick.
            if *selected == 0 {
                json!("")
            } else {
                options
                    .get(*selected)
                    .map(|name| json!(name))
                    .unwrap_or_else(|| json!(""))
            }
        }
        ("sound-volume", FieldValue::Select { selected, options }) => options
            .get(*selected)
            .map(|s| json!(volume_from_option(s)))
            .unwrap_or(Value::Null),
        ("acp-defaults", FieldValue::Text(s)) => {
            let trimmed = s.trim();
            if trimmed.is_empty() {
                return json!({});
            }
            // Commit validation guarantees an object; empty map if it slips.
            match serde_json::from_str::<Value>(trimmed) {
                Ok(v) if v.is_object() => v,
                _ => json!({}),
            }
        }
        ("smart-rename-model", FieldValue::Text(s)) => {
            let trimmed = s.trim();
            if trimmed.is_empty() {
                return json!({});
            }
            // Commit validation guarantees an object; empty map if it slips.
            match serde_json::from_str::<Value>(trimmed) {
                Ok(v) if v.is_object() => v,
                _ => json!({}),
            }
        }
        _ => Value::Null,
    }
}

// ---------------------------------------------------------------------------
// Building the rows for a category
// ---------------------------------------------------------------------------

/// Build a category's fields. In Repo scope `base` is the resolved
/// global+profile config and `overrides` the repo config as a `ProfileConfig`.
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

    for desc in crate::session::config::settings_schema::runtime_schema()
        .into_iter()
        .filter(|d| d.category == category.schema_name())
        // Global-only fields (e.g. the theme) are not profile-overridable, so
        // only surface them under Global scope. Otherwise a Profile/Repo edit
        // would write an override the read path ignores, stranding the value
        // (the empire->rose-pine flip). Enforces the documented `global_only`
        // semantics that nothing else was checking.
        .filter(|d| scope == SettingsScope::Global || d.profile_overridable)
        // Same anti-stranding rule for Repo scope: a repo config may only set
        // the sections and session fields the merge path keeps (#3154), so
        // offering the rest here would write a value nothing reads.
        .filter(|d| {
            scope != SettingsScope::Repo
                || crate::session::config::repo_config::repo_may_override_field(
                    &d.section, &d.field,
                )
        })
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
    // Plugin settings (`plugin:<id>` sections) live at a different storage
    // path (`plugins.<id>.settings.<field>`) and fall back to the manifest's
    // declared default when unset; core fields read their `section.field` leaf,
    // which always exists via the struct Default.
    let current = match crate::session::config::settings_schema::section_plugin_id(&desc.section) {
        Some(id) => crate::session::config::settings_schema::plugin_storage_value(
            ctx.effective_json,
            id,
            &desc.field,
        )
        .or(desc.default.as_ref()),
        None => json_at(ctx.effective_json, &desc.section, &desc.field),
    };
    let value = value_from_json(&desc.widget, current);
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
        SettingsScope::Global => {
            // Plugin settings are global-only and persist under
            // `plugins.<id>.settings`; core fields write their `section.field`.
            match crate::session::config::settings_schema::section_plugin_id(&section) {
                Some(id) => set_config_plugin_path(global, id, &sub, leaf),
                None => set_config_path(global, &section, &sub, leaf),
            }
        }
        // Plugin fields are filtered out of Profile/Repo scope (not
        // profile_overridable), so only core fields reach here.
        SettingsScope::Profile | SettingsScope::Repo => {
            set_override_path(profile, &section, &sub, leaf)
        }
    }
}

/// Write a plugin setting leaf into `plugins.<id>.settings.<field>` of the
/// global config.
fn set_config_plugin_path(config: &mut Config, plugin_id: &str, field: &str, leaf: Value) {
    let mut j = serde_json::to_value(&*config).unwrap_or_else(|_| json!({}));
    merge_json(
        &mut j,
        &crate::session::config::settings_schema::plugin_storage_leaf(plugin_id, field, leaf),
    );
    if let Ok(updated) = serde_json::from_value(j) {
        *config = updated;
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
    merge_json(&mut j, &nested_leaf(section, field, leaf));
    if let Ok(updated) = serde_json::from_value(j) {
        *config = updated;
    }
}

/// Write `section.field = leaf` into a profile/repo override via its JSON form.
/// Always stores the value as an override; the `r` key clears it.
fn set_override_path(profile: &mut ProfileConfig, section: &str, field: &str, leaf: Value) {
    let mut j = serde_json::to_value(&*profile).unwrap_or_else(|_| json!({}));
    merge_json(&mut j, &nested_leaf(section, field, leaf));
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

    fn build(
        cat: SettingsCategory,
        scope: SettingsScope,
        profile: &ProfileConfig,
    ) -> Vec<SettingField> {
        build_fields_for_category(cat, scope, &Config::default(), profile)
    }

    fn idents(fields: &[SettingField]) -> Vec<String> {
        fields.iter().map(|f| f.ident()).collect()
    }

    /// Whether a profile override sets `section.field`, checked through the
    /// serialized form so the test survives a change of storage shape.
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
    fn clearing_optional_field_validates_as_unset() {
        // "Unset" is always allowed and must not surface as "expected a
        // string"; a set value is still checked against the schema rule.
        let mut f = SettingField {
            kind: FieldKind::Schema {
                section: "sandbox".to_string(),
                field: "memory_limit".to_string(),
                widget: WidgetKind::OptionalText { mono: false },
                validation: ValidationKind::MemoryLimit,
                profile_overridable: true,
            },
            label: "Memory Limit".to_string(),
            description: String::new(),
            value: FieldValue::OptionalText(None),
            category: SettingsCategory::Sandbox,
            has_override: false,
            inherited_display: None,
        };

        for (value, ok) in [
            (None, true),
            (Some("not-a-size"), false),
            (Some("512m"), true),
        ] {
            f.value = FieldValue::OptionalText(value.map(str::to_string));
            assert_eq!(f.validate().is_ok(), ok, "{value:?}");
        }
    }

    #[test]
    fn repo_scope_hides_what_a_repo_may_not_override() {
        let profile = ProfileConfig::default();
        let repo = idents(&build(
            SettingsCategory::Agents,
            SettingsScope::Repo,
            &profile,
        ));
        let global = idents(&build(
            SettingsCategory::Agents,
            SettingsScope::Global,
            &profile,
        ));
        for denied in [
            "session.custom_agents",
            "session.agent_command_override",
            "session.agent_extra_args",
            "session.agent_acp_cmd",
            "session.default_tool",
            "session.agent_config_dir",
        ] {
            let denied = denied.to_string();
            assert!(
                !repo.contains(&denied),
                "{denied} must not be repo-editable"
            );
            assert!(global.contains(&denied), "{denied} stays global-editable");
        }
        assert!(repo.contains(&"session.agent_detect_as".to_string()));

        // A category whose whole section is not repo-overridable yields no
        // rows at all, so a direct Repo-scope call cannot produce a field
        // whose edit would strand at save.
        for cat in [SettingsCategory::Tmux, SettingsCategory::Sound] {
            assert!(
                build(cat, SettingsScope::Repo, &profile).is_empty(),
                "{cat:?} must yield zero repo-scope fields"
            );
            assert!(
                !build(cat, SettingsScope::Profile, &profile).is_empty(),
                "{cat:?} must still have profile-scope fields"
            );
        }
    }

    #[test]
    fn raw_json_map_widgets_round_trip_and_degrade_to_an_empty_map() {
        // (widget id, a representative value, a non-object that validation
        // rejects before commit)
        let cases = [
            (
                "acp-defaults",
                json!({"opencode": {"model": "x", "effort": "high"}}),
                "[1,2]",
            ),
            (
                "smart-rename-model",
                json!({"claude": "haiku", "codex": "gpt-5"}),
                "\"oops\"",
            ),
        ];
        for (widget, current, not_an_object) in cases {
            let FieldValue::Text(text) = custom_value_from_json(widget, &current) else {
                panic!("{widget} must build a Text field");
            };
            assert_eq!(
                custom_value_to_json(widget, &FieldValue::Text(text)),
                current,
                "{widget}"
            );

            // Empty text, a null current, and a non-object all resolve to an
            // empty map rather than a corrupt leaf.
            for text in [String::new(), not_an_object.to_string()] {
                assert_eq!(
                    custom_value_to_json(widget, &FieldValue::Text(text)),
                    json!({}),
                    "{widget}"
                );
            }
            assert!(matches!(
                custom_value_from_json(widget, &Value::Null),
                FieldValue::Text(t) if t == "{}"
            ));
        }
    }

    #[test]
    fn smart_rename_agent_widget_persists_the_rendered_option() {
        // "Same as session" is index 0 and persists as the empty string,
        // whatever agents this host has installed.
        assert!(matches!(
            custom_value_from_json("smart-rename-agent", &json!("")),
            FieldValue::Select { selected: 0, ref options } if options[0] == "Same as session"
        ));
        assert!(matches!(
            custom_value_from_json("smart-rename-agent", &Value::Null),
            FieldValue::Select { selected: 0, .. }
        ));

        // A non-zero index persists the rendered option verbatim, so a change
        // in availability between render and save cannot shift the value.
        let options = vec![
            "Same as session".to_string(),
            "claude".to_string(),
            "codex".to_string(),
        ];
        for (selected, want) in [(0, ""), (2, "codex")] {
            assert_eq!(
                custom_value_to_json(
                    "smart-rename-agent",
                    &FieldValue::Select {
                        selected,
                        options: options.clone(),
                    },
                ),
                json!(want),
            );
        }
    }

    #[test]
    fn a_profile_row_shows_an_override_only_when_the_profile_sets_it() {
        // (category, ident, the profile override that sets it)
        let cases = [
            (
                SettingsCategory::Updates,
                "updates.update_check_mode",
                json!({"updates": {"update_check_mode": "off"}}),
            ),
            (
                SettingsCategory::Worktree,
                "worktree.enabled",
                json!({"worktree": {"enabled": true}}),
            ),
        ];
        for (cat, ident, override_json) in cases {
            let inherited = build(cat, SettingsScope::Profile, &ProfileConfig::default());
            assert!(!field(&inherited, ident).has_override, "{ident}");
            assert!(
                field(
                    &build(cat, SettingsScope::Profile, &profile_from(override_json)),
                    ident
                )
                .has_override,
                "{ident}"
            );
        }

        // A global change never promotes an inheriting profile row.
        let mut global = Config::default();
        global.updates.update_check_mode = crate::session::config::UpdateCheckMode::Off;
        let fields = build_fields_for_category(
            SettingsCategory::Updates,
            SettingsScope::Profile,
            &global,
            &ProfileConfig::default(),
        );
        assert!(!field(&fields, "updates.update_check_mode").has_override);

        // Global rows read the global value.
        global.worktree.enabled = true;
        let fields = build_fields_for_category(
            SettingsCategory::Worktree,
            SettingsScope::Global,
            &global,
            &ProfileConfig::default(),
        );
        assert!(matches!(
            field(&fields, "worktree.enabled").value,
            FieldValue::Bool(true)
        ));
    }

    #[test]
    fn applying_a_profile_field_keeps_the_override_even_when_it_matches_global() {
        // Only the `r` key clears an override, so re-applying a field that
        // happens to equal the global value must not start inheriting.
        let mut global = Config::default();
        let mut profile = profile_from(json!({"updates": {"update_check_mode": "off"}}));
        let f = field(
            &build(SettingsCategory::Updates, SettingsScope::Profile, &profile),
            "updates.update_check_mode",
        )
        .clone();
        apply_field_to_config(&f, SettingsScope::Profile, &mut global, &mut profile);
        assert!(has_override_path(&profile, "updates", "update_check_mode"));
    }

    #[test]
    fn status_hook_rows_read_and_apply_in_both_scopes() {
        let global_rows = build(
            SettingsCategory::StatusHooks,
            SettingsScope::Global,
            &ProfileConfig::default(),
        );
        assert!(matches!(
            &field(&global_rows, "status_hooks.on_waiting").value,
            FieldValue::OptionalText(None)
        ));

        let profile = profile_from(json!({"status_hooks": {"on_waiting": "notify-send hi"}}));
        let profile_rows = build(
            SettingsCategory::StatusHooks,
            SettingsScope::Profile,
            &profile,
        );
        let f = field(&profile_rows, "status_hooks.on_waiting");
        assert!(f.has_override);
        assert!(matches!(
            &f.value,
            FieldValue::OptionalText(Some(v)) if v == "notify-send hi"
        ));

        let mut global = Config::default();
        let mut profile = ProfileConfig::default();
        let mut f = field(&global_rows, "status_hooks.on_waiting").clone();
        f.value = FieldValue::OptionalText(Some("notify-send hi".to_string()));
        apply_field_to_config(&f, SettingsScope::Global, &mut global, &mut profile);
        assert_eq!(
            global.status_hooks.on_waiting.as_deref(),
            Some("notify-send hi")
        );
        apply_field_to_config(&f, SettingsScope::Profile, &mut global, &mut profile);
        assert!(has_override_path(&profile, "status_hooks", "on_waiting"));
    }

    #[test]
    fn default_tool_options_mirror_the_agent_registry() {
        let fields = build(
            SettingsCategory::Agents,
            SettingsScope::Global,
            &ProfileConfig::default(),
        );
        let FieldValue::Select { options, .. } = &field(&fields, "session.default_tool").value
        else {
            panic!("default tool should be a Select");
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
    fn acp_fields_split_around_the_advanced_section_marker() {
        let fields = build(
            SettingsCategory::Acp,
            SettingsScope::Global,
            &ProfileConfig::default(),
        );
        let header = fields
            .iter()
            .position(|f| matches!(f.value, FieldValue::SectionHeader))
            .expect("acp should contain an Advanced section header");
        assert_eq!(fields[header].label, "Advanced");
        for (ident, before) in [
            ("acp.default_agent", true),
            ("acp.replay_events", true),
            ("acp.node_path", true),
            ("acp.show_tool_durations", true),
            ("acp.max_concurrent_workers", false),
            ("acp.silent_orphan_grace_secs", false),
        ] {
            let pos = fields.iter().position(|f| f.ident() == ident).unwrap();
            assert_eq!(pos < header, before, "{ident}");
        }
    }

    #[test]
    fn each_session_field_lands_on_its_own_tab() {
        let profile = ProfileConfig::default();
        let tab = |cat| idents(&build(cat, SettingsScope::Global, &profile));

        // (tab, idents it must carry)
        let expected = [
            (
                SettingsCategory::Agents,
                &[
                    "session.default_tool",
                    "session.agent_extra_args",
                    "session.agent_command_override",
                    "session.custom_agents",
                    "session.agent_detect_as",
                    "session.agent_status_hooks",
                ][..],
            ),
            (
                SettingsCategory::Interaction,
                &[
                    "session.default_attach_mode",
                    "session.new_session_mode",
                    "session.click_action",
                    "session.live_send_exit_chord",
                    "session.mouse_capture",
                    "session.host_tab_title",
                    "session.show_session_colors",
                ][..],
            ),
            (SettingsCategory::Session, &["environment"][..]),
        ];
        for (cat, wanted) in expected {
            let rows = tab(cat);
            for ident in wanted {
                assert!(rows.contains(&ident.to_string()), "{cat:?} missing {ident}");
            }
        }

        // And those moved off the Session tab stay off it.
        let session = tab(SettingsCategory::Session);
        for ident in [
            "session.default_tool",
            "session.agent_extra_args",
            "session.default_attach_mode",
            "session.live_send_exit_chord",
        ] {
            assert!(!session.contains(&ident.to_string()), "{ident} moved off");
        }

        // The Hooks tab is exactly the lifecycle hooks.
        assert_eq!(
            tab(SettingsCategory::Hooks),
            vec!["hooks.on_create", "hooks.on_launch", "hooks.on_destroy"]
        );
    }
}
