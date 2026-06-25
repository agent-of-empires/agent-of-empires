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

## Shared substrate

Two neutral modules hold the protocol-agnostic plumbing that both `src/acp/`
and the future plugin host build on, so the host never depends on ACP (the
dependency arrow runs consumer -> substrate):

- `src/process/worker.rs`: worker-subprocess plumbing, process-group
  signalling (terminate/kill/reap), pid liveness, the runner self-inspection
  state machine, and the `<dir>/<id>.{json,sock,log,restart}` path builders.
  The consumer supplies the base directory and a pid extractor for record
  inspection.
- `src/events/`: a durable event-log storage core, a topic-keyed SQLite seq
  log with retention, keyset scans, seq bookkeeping, and attachment blobs over
  opaque JSON payloads. The consumer holds the `Connection` and owns its
  payload type and replay semantics. `acp::event_store::EventStore` is the
  first consumer (`Schema::new("acp")` keeps the existing `acp_events` tables,
  so no migration).

## Contribution schema (#2093)

`PluginManifest` extends past identity to the contribution sections a plugin
declares: `capabilities`, `commands`, `keybinds`, `settings`, `ui`, and a
`runtime` worker entrypoint. These are the sections the first external plugin
declares; they are defined in `aoe-plugin-api` and parsed/validated by the
host, but consumed by later issues (the settings registry in #2094, the runtime
host in #2095, the command/keybind/UI surfaces in #2366). `api_version` is
bumped to 2; an `api_version` 1 manifest still loads. Unknown top-level keys
remain a hard parse error (`deny_unknown_fields`).

The `themes`, `status`, and `panes` sections are deferred until a consumer
exists, so no schema lands in core ahead of one (#2386). With
`deny_unknown_fields`, a manifest declaring `[[themes]]`, `[[status]]`, or
`[[panes]]` is a hard parse error today.

The `runtime` section is one of two kinds: `command` (an argv launched from the
plugin directory) or `release-binary` (a compiled worker shipped as a GitHub
release asset). Only installation acts on `release-binary` today (it downloads
the asset); launching either worker is #2095.

## Capabilities and grants (#2093)

Static contributions are not capabilities; a theme or a command needs no
approval. A capability gates runtime access to a resource that can affect user
data, host state, the OS, or the network. The v1 set
(`aoe_plugin_api::KNOWN_CAPABILITIES`): `runtime.worker`, `session.read`,
`session.write`, `config.read`, `config.write`, `process.spawn`, `net`,
`fs.read`, `fs.write`, `clipboard.read`, `clipboard.write`, `notifications`. A
plugin's own declared settings need no `config.*`; that gates host/global or
other-plugin config.

Capabilities are open strings (`CapabilityId`), so a follow-up can add one
without an `api_version` bump. An unknown capability still parses (forward
compatibility) but is rejected at install (`unsupported capability; upgrade
aoe`), never silently granted.

A grant (`PluginConfig.grant`, in `config.toml`) records the capabilities the
user approved and is pinned to the `sha256` of the installed manifest bytes
(`PluginManifest::hash_bytes`). The registry treats a community plugin as
active only when enabled AND the grant covers the installed manifest (same hash,
all declared capabilities present). A changed manifest, hence a changed hash or
capability set, invalidates the grant: the plugin stays installed but inactive
(`needs_reapproval`) until `aoe plugin update` re-prompts and re-approves.
Builtins are first-party, auto-granted, and never store a grant.

## External install, trust, and the lockfile (#2093)

`aoe plugin install <source>` installs an external plugin under
`<app_dir>/plugins/<id>/`; `aoe plugin` stays reserved for management (D4), so
there is no web install path. A source is a `gh:owner/repo[@ref]` slug or a
local directory (`src/plugin/source.rs`).

`src/plugin/fetch.rs` stages a plugin before install. A GitHub source is
`git clone`d (shallow when possible, a full clone plus checkout for a commit
ref), the exact commit is resolved, and `.git` is stripped; the clone base
defaults to `https://github.com` and is overridable via `AOE_GITHUB_CLONE_BASE`
(a GitHub Enterprise host, or a local `file://` base in tests). A local source
is copied (minus `.git` and symlinks). When the manifest declares a
`release-binary` runtime, the matching release asset for the host platform
(`${os}`/`${arch}`/`${version}` in the asset template) is downloaded via the
GitHub client and unpacked (raw or `.tar.gz`) into the tree, made executable.
The staging tree lives under the plugins dir so the final move into place is an
atomic same-filesystem rename.

Trust is host-assigned (`TrustLevel`): `builtin` (compiled in, auto-granted) or
`community` (external, capabilities gated). An external plugin whose id sits in
a reserved namespace (`aoe.*` / `agent-of-empires.*`, lifted only by featured
verification in #2364) or collides with a builtin is rejected at install and
skipped at load.

`plugins.lock` (`<app_dir>/plugins.lock`, TOML, keyed by id, deterministic and
timestamp-free like `Cargo.lock`) records each external plugin's resolved
identity: source slug, requested ref, resolved commit, version, manifest hash,
trust, and (for a release-binary) the release tag, asset name, and asset
sha256.

## What comes next

Each deferred piece returns as its own PR once the core is proven: the
contribution registries and the JSON-RPC worker runtime and event bus built on
the substrate above (issues 2094, 2095, and 2366), and the discovery / featured
supply-chain layer with integrity hashing (issues 2364 and 2365).
