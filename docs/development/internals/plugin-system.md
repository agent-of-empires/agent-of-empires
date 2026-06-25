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
  (`session.meta.{get,set,cas}`) ships with the Tier 1 host (see below).

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
release asset). Installation resolves and downloads a `release-binary` asset;
the Tier 1 host (below) launches and supervises both kinds.

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
tree hash (see below), trust, and (for a release-binary) the release tag, asset
name, and asset sha256. `lock_version` is 2; a `tree_hash`-less v1 lock still
reads (the field defaults) and is repopulated on the next install/update.

## Integrity hashing and the featured index (#2364)

`plugin::integrity::tree_hash` is a deterministic `sha256:<hex>` over a plugin's
source tree. Files are sorted by their forward-slash relative path and hashed
under a versioned header (`aoe-plugin-tree-hash-v1`) as `file\0<path>\0<len>
<content>`. `.git` is skipped (it is stripped from an installed tree); a symlink
or non-UTF-8 path is a hard error so nothing installed escapes the hash. File
mode is excluded for cross-platform determinism, and `git clone` runs with
`core.autocrlf=false` so line endings never differ by platform. The hash is
computed over the staged source **before** any release-binary worker is
injected, so an author's `aoe plugin hash <checkout>` reproduces the
install-time value; the downloaded worker stays pinned separately by the lock's
`asset_sha256`.

`plugins/featured.toml` is the curated index, compiled into the binary. Each
entry pins one vetted release per plugin id to its `{source, tree_hash}`: a
maintainer's attestation that this exact tree was reviewed. When a plugin id
appears in the index, install and update **refuse** unless the fetched source
slug (case-insensitive) and tree hash both match the pin, and a release-binary
manifest is refused outright (its worker bytes are not covered by the tree hash
yet). A featured-verified install is the one case allowed to claim a reserved
(`aoe.*` / `agent-of-empires.*`) namespace; a builtin-id collision is always
rejected. In debug builds `AOE_FEATURED_INDEX_PATH` overrides the embedded index
for tests; a release binary always uses the compiled-in index, since the curated
set is a root of trust and must not be redefinable by the environment.

Every surface (CLI `aoe plugin list` / `info`, the TUI plugin manager, the web
Plugins panel) shows a `ValidationState`: `builtin`, `featured`, `community` (an
unvetted GitHub install), or `local` (a local-directory install). `featured` is
re-derived live at load (the id is in the embedded index and the on-disk tree
hashes to the pin), not trusted from the lockfile, since that same derivation
gates the reserved-namespace lift and the lockfile is user-writable; `community`
vs `local` is derived from the install source. The lockfile records the tree
hash and the install-time `trust` as a resolved record, but the load path does
not depend on them for validation. The recompute is cheap (only ids the index
names, and a featured plugin ships no release-binary, so its installed tree
equals its source tree). The manifest-hash grant check still catches a community
plugin tampered after install.

`aoe plugin hash <dir>` prints the tree hash for a plugin directory so an author
can produce the value a maintainer pins. Run it on a clean checkout.

## Tier 0 contribution registries (#2094)

Tier 0 wires a plugin's declarative manifest contributions into the host's
registries, with no plugin code execution (that is the Tier 1 host, #2095). Four
registries consume the manifest:

### Settings

A plugin's `[[settings]]` are typed: `type` (`string` / `bool` / `integer` /
`select`), with `options`, `min`/`max`, a `default`, and `advanced`. The host
maps each to its single-source settings schema as a virtual `plugin:<id>`
section. `settings_schema::runtime_schema()` returns the static core schema plus
those sections; `GET /api/settings/schema` serves it, the server validates
PATCHes against it (`validate_patch_with`), and the TUI/web render it through the
same generic field path as core settings. The API/validation layer speaks the
flat `plugin:<id>.<key>` shape; only the merge boundary translates to the on-disk
storage path `plugins.<id>.settings.<key>` (`settings_schema::plugin`). Plugin
settings are global-only at Tier 0 (not profile-overridable). In the TUI they
render read-only under the Plugins tab; edit them from the web dashboard or
`aoe settings`.

A manifest may also declare a *default* override for a core setting via
`[setting_defaults]` (keyed by the core `section.field`).
`settings_schema::resolve` returns the effective value, its source, and the full
candidate chain; `aoe settings explain <key>` and `GET /api/settings/resolved`
surface it.

The effective value of a core key at Tier 0 is the user's value (when it differs
from the baseline default), else the core schema default. A plugin's
`setting_defaults` override is included in the candidate chain so it is
observable, but it is NOT applied at runtime yet, so it never reports as the
effective `source`: nothing layers it during real `Config` load/merge, so every
core consumer still reads the struct default. The runtime host applies these
overrides for real (#2095); until then a `plugin_default` candidate is
"declared, not yet in effect". A plugin's own setting layers stored value >
manifest default. "Highest priority" (for the candidate ordering) is
active-plugin order, builtins first.

### Themes

A plugin's `[[themes]]` (`name`, `path`) add theme TOMLs to the picker. Each
`path` is resolved under the plugin's install directory (absolute or
parent-escaping paths are rejected); precedence is builtin > user custom >
plugin, so a plugin can never shadow a builtin or a user theme.

### Keybinds

A plugin's `[[keybinds]]` resolve through a merged resolver
(`tui::home::bindings::resolve_action`): the static core table is tried first and
always shadows a plugin binding on the same chord; active plugins' keybinds are
consulted only after. A resolved plugin keybind is inspectable but not runnable
at Tier 0 (it shows a "needs the plugin runtime" notice); `aoe plugin info` lists
a plugin's keybinds and flags any chord core shadows. Execution lands with #2095.

### CLI grafting

Active plugins' `[[commands]]` are grafted onto the derived clap tree at runtime
(`cli::graft`), so they appear in `aoe --help` and parse. Core commands always
win a name conflict. Dispatch tries the core derive first; a grafted command
falls through to the plugin dispatcher, which at Tier 0 reports that running it
needs the runtime (#2095).

## Tier 1 worker host (#2095)

The worker host runs inside the `aoe serve` daemon (it is `serve`-gated, like
`aoe.web`), because the host API it exposes reads and writes the event store and
session storage the daemon owns. A TUI-only build has no host. The daemon builds
one `PluginHost` at startup, launches a worker for every active plugin that
declares a `[runtime]`, and reaps them all on shutdown
(`AppState.plugin_host`, `src/server/mod.rs`).

### Launching a worker, language-agnostically

The host, not the plugin, decides how to resolve and execute a worker.
`src/plugin/launch.rs` turns a `LoadedPlugin` into a `ResolvedLaunch { program,
args, cwd, env }`, dispatched off the `[runtime]` kind in a single `match`.
Adding a new runtime kind later is a new arm there; the supervisor and the
transport only ever see a `ResolvedLaunch`, so nothing downstream changes.

- `command`: `argv[0]` resolves on `PATH` via `which` when it is a bare name (a
  console-script entrypoint like `aoe-github-worker`, or an interpreter like
  `python3`), or relative to the plugin directory when it contains a separator
  (an in-tree script or binary), verified executable. Absolute and
  parent-traversal paths are rejected.
- `release-binary`: the per-platform binary that installation already placed in
  the plugin directory.

A missing runtime fails loudly with an actionable hint naming the program (and,
for a binary, the host `os-arch`), matching the project's error-with-hint style.
Filesystem and `PATH` probing go through a `LaunchResolver` trait so the
resolution policy is unit-tested with no real filesystem.

Builtins do not declare a `[runtime]` in this release, so `resolve_launch`
returns `Err(LaunchError::NoRuntime)` for them. The `aoe __plugin-worker`
self-exec path for a builtin worker, and the worker-side SDK, arrive with the
first builtin worker that needs them; shipping them now would be unused code.

### Transport and supervision

A worker is an executable speaking newline-delimited JSON-RPC 2.0
(`src/plugin/protocol.rs`) over its stdio: it writes one request object per line
to stdout and reads one response per line on stdin. The host is the server. Any
language that speaks this wire is a valid worker.

The worker is a child owned by the daemon, not a detached process (this is the
ACP supervision model minus its persistence half). There is no socket, no
on-disk runner record, and no reattach: a plugin worker is a stateless
transformer over a host-owned event stream, so surviving a daemon restart would
only strand it with a stale view. The daemon dies, its workers die, a fresh
daemon respawns them. What is kept from ACP: process-group reaping (a worker
that forks helpers is torn down whole), a per-worker respawn budget so a crash
loop does not spin, and a concurrency cap. The worker's stderr drains to
`<app_dir>/plugin-workers/<id>.log`.

### Capability-gated host API

Each host method maps to a capability the plugin declared and was granted; the
middleware refuses an undeclared or ungranted call before the method runs
(`src/plugin/host_api.rs`). No new capabilities are introduced; the v1 methods
reuse the existing taxonomy:

| Method | Capability |
| --- | --- |
| `events.publish` / `events.subscribe` | `runtime.worker` |
| `session.meta.get` | `session.read` |
| `session.meta.set` / `session.meta.cas` | `session.write` |
| `sessions.list` | `session.read` |

`events.*` run over a shared plugin event bus (a `plugin_host` schema on the
durable event-log substrate, `src/events/`); `subscribe { topics, after_seq }`
is a replay-after-cursor read, so a worker polls forward from the last seq it
saw. Session metadata is always read and written under the calling plugin's own
`plugin_meta[<plugin-id>]` slot: the worker sends only a `key`, never another
plugin's id, so one plugin cannot reach another's data. A `session.meta.cas`
that loses returns the current value rather than clobbering it. Writes go
through `Storage`'s cross-process lock, so the daemon picks them up on its next
session reload (eventual consistency, not a live push).

### Sandboxing

`SandboxBackend` (`src/plugin/sandbox.rs`) is the seam between a resolved launch
and the spawn. The only v1 backend is `NoSandbox`, which runs the worker as an
ordinary child. Per D8 this is honest, not complete: capability gating at the
host API boundary stops a cooperative plugin from overreaching, but a granted
worker has no OS-level isolation, so an adversarial plugin is not contained. The
capability grant prompt states this on every install. Restricted-environment,
landlock, and `sandbox-exec` backends land later behind the same trait, with no
change to the resolver or the supervisor.

## What comes next

Each deferred piece returns as its own PR once the core is proven: the Tier 0
contribution registries and the command/keybind/UI surfaces (issues 2094 and
2366), the builtin worker self-exec path and worker SDK (with the first builtin
worker that needs them), and the discovery / featured supply-chain layer with
integrity hashing (issues 2364 and 2365). Pinning a featured plugin's
release-binary asset hash in `featured.toml` (so a featured worker is attested,
not just its source) is a follow-up; today a release-binary plugin cannot be
featured.
