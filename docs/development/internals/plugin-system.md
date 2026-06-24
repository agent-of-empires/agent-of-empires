# Plugin System Internals

Code-level design for the plugin system (issue #268). This first release ships
only the minimal core: a registry that loads compiled-in first-party plugin
manifests and exposes each one's enabled/disabled state to every surface (CLI,
TUI, web). Contribution registries (settings, keybinds, themes, commands,
status detection, UI slots, panes), the subprocess JSON-RPC worker runtime, the
capability model, external installation, and the supply-chain/trust machinery
are intentionally deferred to follow-up PRs and are not present in the tree yet.

## Manifest schema

`aoe-plugin-api` is the standalone crate that defines the manifest a plugin
ships in `aoe-plugin.toml`. The core schema is just identity:

- `id` (`PluginId`, a validated dotted-lowercase namespace, e.g. `aoe.web`),
- `name`, `version`, `api_version`, and an optional `description`.

`PluginManifest::from_toml_str` pre-checks `api_version` permissively (so a
manifest targeting a newer host reports "upgrade aoe" rather than a confusing
unknown-field error), then parses strictly (`deny_unknown_fields`, so a
contribution section from a future schema is a hard error today) and validates
(`api_version` in range, non-empty `name`/`version`). `API_VERSION` is the
schema/host version this crate understands.

## Registry

`src/plugin/registry.rs` owns the in-process registry.

- `BUILTINS` is a static slice of `BuiltinPlugin`, each embedding its manifest
  TOML via `include_str!`. The `aoe.web` marker is gated on the `serve` cargo
  feature, so it is present in every dashboard/release build and absent from a
  TUI-only build. `default-plugins` (on by default) reserves the on-by-default
  slot for bundled plugins that do not require the dashboard.
- `PluginRegistry::load(config)` parses every builtin manifest, resolves each
  plugin's enabled flag from `[plugins."<id>"]` in `config.toml` (default
  enabled), and collects any parse errors as non-fatal `load_errors`.
- `LoadedPlugin { manifest, enabled }` exposes `id()`, `active()`, and `view()`.

`src/plugin/mod.rs` holds the process-wide `REGISTRY` (an
`RwLock<Option<Arc<PluginRegistry>>>`); `registry()` loads it lazily from the
global config and `reload_registry()` rebuilds it after an enable/disable.

## View model

`src/plugin/view.rs` defines `PluginView { id, name, version, description,
enabled, builtin }`, a `Serialize` struct built straight off `LoadedPlugin`. The
CLI, the TUI plugin manager, and the web dashboard all render from the same
view, so plugin fields are never re-derived per surface.

## Enable/disable

`src/plugin/install::set_enabled(id, enabled)` validates the id against the
registry, writes `[plugins."<id>"].enabled` through the normal `save_config`
path, and reloads the registry. The three surfaces are thin twins over it:

- CLI: `aoe plugin enable|disable` (`src/cli/plugin.rs`).
- TUI: the command-palette / settings-tab plugin manager
  (`src/tui/dialogs/plugin_manager.rs`); the settings tab stages the change and
  persists it on the normal settings save.
- Web: `POST /api/plugins/{id}/enabled`, gated on read-write mode and (when
  login is enabled) an elevated session (`src/server/api/plugins.rs`).

The one behavior wired to a plugin's state today: `aoe serve` refuses to start
while `aoe.web` is disabled (`src/cli/serve.rs`).

## Persisted plugin state (#2091)

Two storage slots hold plugin data on disk ahead of the APIs that read and
write them, so the later API PRs (#2094, #2095) stay focused on behavior:

- **Per-plugin settings.** `PluginConfig.settings` (`src/session/config.rs`) is
  an opaque `toml::Table` persisted as `[plugins."<id>".settings]` in
  `config.toml`. It is kept schema-free on purpose: values survive on disk even
  while the plugin is disabled, and the typed schema that validates and renders
  them arrives with the Tier 0 settings registry (#2094). `enabled` is declared
  before `settings` so the scalar reads above the nested table; the toml
  serializer emits scalars before subtables regardless, so the order is for
  readability. An empty table is omitted.
- **Per-session plugin data.** `Instance.plugin_meta`
  (`src/session/instance.rs`) is a `BTreeMap<String, serde_json::Value>` keyed
  by plugin id, persisted per session in `sessions.json`. Each plugin owns only
  its own slot; data for an uninstalled plugin is retained (cheap, and
  reinstalling restores it). The read/write/cas host API over it
  (`session.meta.{get,set,cas}`) lands with the Tier 1 host (#2095).

Both fields are additive (`#[serde(default, skip_serializing_if = ...)]`):
absent in older on-disk rows, so they deserialize to empty and need no data
migration.

## What comes next

Each deferred piece returns as its own PR once the core is proven: the
contribution schema and registries, the JSON-RPC worker runtime and event bus,
the capability model, external (GitHub/local) installation, and the
discovery/featured supply-chain layer.
