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

    /// UI slots the plugin renders into. Consumed by #2366.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub ui: Vec<UiContribution>,

    /// The worker entrypoint. Defined here so installation can fetch a
    /// release-binary worker; actually launching it is #2095.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub runtime: Option<RuntimeSpec>,
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
}

/// The type of a plugin setting value. One declaration drives both the widget
/// the surfaces render and the validation the server enforces.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SettingType {
    /// Free text, rendered as a text input.
    #[default]
    String,
    /// On/off, rendered as a toggle.
    Bool,
    /// Integer, rendered as a number input (bounded by `min`/`max`).
    Integer,
    /// Closed set of strings, rendered as a select over `options`.
    Select,
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
    /// A panel in a session's detail view (per session).
    DetailPanel,
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
            UiSlot::RowBadge | UiSlot::RowColumn | UiSlot::DetailPanel | UiSlot::DetailBadge
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
            UiSlot::DetailPanel => "detail-panel",
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
        for (i, c) in self.commands.iter().enumerate() {
            check(
                !c.id.is_empty(),
                format!("commands[{i}].id must not be empty"),
            );
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
            // A declared default must match the value type, so an author learns
            // of a type mismatch at parse time rather than at render/store time.
            if let Some(def) = &s.default {
                let type_ok = match s.value_type {
                    SettingType::String | SettingType::Select => def.is_str(),
                    SettingType::Bool => def.as_bool().is_some(),
                    SettingType::Integer => def.as_integer().is_some(),
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
