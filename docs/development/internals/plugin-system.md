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

### Screenshots

A plugin may declare up to eight `[[screenshots]]` to illustrate itself in the
dashboard marketplace and detail modal (requires `api_version >= 5`):

```toml
[[screenshots]]
path = "docs/screenshots/dashboard.png"  # repository-relative; not a URL
alt = "Dashboard card the plugin contributes"  # required, for accessibility
caption = "Live status card."  # optional, shown beneath the image
```

`path` must be a repository-relative image (`png`/`jpg`/`jpeg`/`gif`/`webp`):
absolute URLs, absolute or `..`-traversing paths, and other extensions are
rejected. The plugin detail endpoint resolves each path against the source
repo's `raw.githubusercontent.com` (honoring the installed ref, else `HEAD`),
so the browser fetches the asset directly; AoE never proxies image bytes. The
endpoint silently drops any screenshot whose path or alt text fails validation
rather than failing the whole detail response, so one bad entry omits only that
image. The assets must be committed and pushed to the source repo before they
render; local plugins do not support screenshots yet (future work beyond
#2484).

### Identity icon

A plugin may declare a static identity icon, shown in the installed Settings >
Plugins list, the detail modal, and its activity-bar button / dock tab
(requires `api_version >= 7`):

```toml
icon = "git-branch"                 # a lucide kebab-case icon name
icon_asset = "assets/icon.png"      # repository-relative raster image
```

`icon` is only syntax-checked (lowercase kebab-case); whether the name exists
in lucide's icon set is the web client's problem, and an unknown name falls
back to a generic icon rather than failing to parse. `icon_asset` follows the
exact same path rules as `screenshots.path`; lucide ships no brand/logo icons
(the motivating case: the GitHub plugin cannot render its own mark through
`icon` alone), so `icon_asset` is how a plugin ships one. When both are set,
`icon_asset` wins wherever an image can render, falling back to `icon` if the
asset fails to load.

`icon_asset` wins unconditionally in the activity bar / dock tab too, even
over a pane's own per-pane runtime icon (the `icon` field a plugin's worker
pushes on its `pane` UI-state payload, see below): a plugin's real logo is a
stronger identity signal than a lucide glyph a worker chose before this field
existed. Below `icon_asset`, the precedence is per-pane runtime icon, then the
manifest `icon`, then the host's generic fallback. `PaneIcon`
(`web/src/components/PaneIcon.tsx`) and `PluginIdentityIcon`
(`web/src/components/settings/PluginIdentityIcon.tsx`) share the same
reset-on-URL-change fallback behavior via `useAssetFailed`
(`web/src/lib/pluginUi.ts`).

For an installed (not-yet-uninstalled) plugin, `icon_asset` is served from its
own install directory via `GET /api/plugins/{id}/icon`, which re-validates the
path and canonicalizes it against the install directory before reading, so a
plugin cannot declare a path that escapes its own tree. For a `gh:` source not
yet installed, the plugin detail endpoint resolves it to
`raw.githubusercontent.com` exactly like screenshots. A builtin (no install
directory) or a plugin with no `icon_asset` renders `icon` or the generic
fallback instead.

## Registry

`src/plugin/registry.rs` owns the in-process registry.

- `BUILTINS` is a static slice of `BuiltinPlugin`, each embedding its manifest
  TOML via `include_str!`. The `aoe.web` marker is gated on the `serve` cargo
  feature, so it is present in every dashboard/release build and absent from a
  TUI-only build. `default-plugins` (on by default) reserves the on-by-default
  slot for bundled plugins that do not require the dashboard. CI runs a
  `--no-default-features` leg (`.github/workflows/tests.yml`) that builds with
  every default plugin compiled out and runs the e2e suite, so a default plugin
  cannot silently grow an undeclared core dependency: core must still create,
  attach to, and destroy a tmux + worktree session from CLI and TUI with all
  default plugins disabled (invariant 1 of #268).
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
registry, writes `[plugins."<id>"].enabled` through the normal `update_config`
path, and reloads the registry. The three surfaces are thin twins over it:

- CLI: `aoe plugin enable|disable` (`src/cli/plugin.rs`).
- TUI: the command-palette / settings-tab plugin manager
  (`src/tui/dialogs/plugin_manager.rs`); the settings tab stages the change and
  persists it on the normal settings save (and best-effort nudges a running
  daemon per changed id afterwards).
- Web: `POST /api/plugins/{id}/enabled`, gated on read-write mode and (when
  login is enabled) an elevated session (`src/server/api/plugins.rs`).

The CLI and the palette-opened TUI manager route through
`install::set_enabled_live`: when a local daemon is running (serve builds;
discovered via the serve state files), the toggle POSTs the daemon endpoint so
its worker host reconciles live, exactly like the web toggle; with no daemon,
or when the daemon refuses (read-only, unreachable), the change is written
locally and the surface says so. A TUI-only build always writes locally.

The one behavior wired to a plugin's state today: `aoe serve` refuses to start
while `aoe.web` is disabled (`src/cli/serve.rs`).

## Persisted plugin state (#2091)

Two storage slots hold plugin data on disk ahead of the APIs that read and
write them, so the later API PRs (#2094, #2095) stay focused on behavior:

- **Per-plugin settings.** `PluginConfig.settings` (`src/session/config.rs`) is
  an opaque `toml::Table` persisted as `[plugins."<id>".settings]` in
  `config.toml`. It is kept schema-free on purpose: values survive on disk even
  while the plugin is disabled, and the typed schema that validates and renders
  them arrived with the Tier 0 settings registry (#2094). `enabled` is declared
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
host in #2095, the command/keybind/UI surfaces in #2366). `api_version` is now
8 (bumped to 2 for the contribution sections, 3 when the `detail-panel` slot
became the dockable `pane` slot, 4 for the `status` section and the
`aoe_version` field, 5 for screenshots, 6 for command actions, 7 for identity
icons, and 8 for the `composer-action` slot); an older `api_version` manifest
still loads as long as it targets no newer field. Unknown top-level keys remain
a hard parse error
(`deny_unknown_fields`).

The `themes` section ships and is consumed by the theme registry (#2094); the
`status` section (`id`, `label`) is parsed and validated here, its consumer is
the status reference plugin (#2096). `panes` is not a manifest section: panes
ship as a `ui` slot of kind `pane` (#2432), so `[[panes]]` stays a hard parse
error. `status` and `aoe_version` (below) require `api_version >= 4`: under
`deny_unknown_fields` a pre-4 host would otherwise report a bare "unknown
field" instead of the "upgrade aoe" message, so the host gates them behind the
bump.

`aoe_version` is an optional semver requirement (`">=0.10, <0.12"`) naming
which aoe (host app) versions this plugin version supports. It is distinct from
`api_version`: `api_version` gates the manifest schema shape, `aoe_version`
gates the host's app behaviour. The host refuses to install or update a plugin
whose range excludes the running aoe, and skips loading one (into the
registry's load errors) rather than bailing, so an aoe upgrade cannot brick
startup. Builtins are exempt (they ship with aoe). An absent range means no
constraint.

The `runtime` section is one of two kinds: `command` (an argv launched from the
plugin directory) or `release-binary` (a compiled worker shipped as a GitHub
release asset). Installation resolves and downloads a `release-binary` asset;
the Tier 1 host (below) launches and supervises both kinds.

A `command` runtime may declare ordered `[[runtime.build]]` steps, run once at
install and update inside the installed plugin directory before the plugin is
registered. This is how an interpreted worker sets itself up (create a venv,
`pip install`, `npm ci`), so it can then launch via a plugin-relative
`command` that never depends on the daemon's PATH:

```toml
[runtime]
kind = "command"
command = [".aoe-build/venv/bin/aoe-github-worker"]   # plugin-relative

[[runtime.build]]
command = ["python3", "-m", "venv", ".aoe-build/venv"]

[[runtime.build]]
command = [".aoe-build/venv/bin/pip", "install", "."]
platforms = ["linux", "macos"]              # optional; omitted runs everywhere
```

Build output must go under the reserved `.aoe-build/` directory, never directly
into the source tree. That directory is excluded from `tree_hash` (see below), so
a build that mutates the install tree (a venv, `node_modules`, compiled
artifacts) does not change the source hash and a featured plugin still
re-derives `Featured` at load. A source tree that ships `.aoe-build` is refused
at install.

Each step's argv resolves through the host's argv resolver (bare name on PATH,
separator path relative to the plugin dir, absolute rejected), evaluated just
before the step runs so `.aoe-build/venv/bin/pip` resolves once the prior step
created it.
Build steps are free to name bare PATH programs (`python3`, `node`, `uv`): they
run in the user's interactive shell where PATH is reliable, which is exactly why
the worker entrypoint, launched later by the daemon, is not. A step's optional `platforms` (`linux` / `macos` / `windows`)
restricts it to matching hosts. Builds run with cwd set to the plugin dir, with
stdin closed and stdout/stderr going to the operation log: the user's terminal
for a CLI install, or a per-job log file for a dashboard install (see below).

Why install time and the final dir, not launch or staging: `aoe plugin
install` runs in the user's interactive shell, where `python3` / `node` / `uv`
are reliably on PATH; the daemon that later launches the worker is not. And a
Python venv is not relocatable (console-script shebangs and `pyvenv.cfg` embed
absolute paths), so the build runs in the final `<plugins_dir>/<id>`, never in
a staging tree that is then renamed. A failed build aborts the install with no
trace; a failed update restores the prior version from a backup, and a leftover
backup from an interrupted update is recovered on the next install/update.

Web lifecycle jobs: the dashboard (Settings -> Plugins) installs, updates, and
uninstalls a `gh:` plugin without a terminal. The same disclosure the CLI
prompts for (capabilities, build commands, UI slots, unverified-source warning)
is returned as structured data and approved in a modal; the host then runs the
operation as a job whose progress and build output stream to a per-job log file
under `<plugins_dir>/jobs/<job_id>.log`, which the dashboard tails. Every update
(in the web modal and the TUI plugin manager) first shows a changelog between
the installed and target version, release notes when the refs bracket release
tags, otherwise commit subjects from the GitHub compare endpoint
(`plugin::changelog`, assembled best-effort in `preview_update` only). A safe
version bump no longer auto-applies: it opens the same review surface so the
changelog is seen before the update runs. One
lifecycle mutation runs at a time (config and lockfile writes are not
concurrency-safe; a second start is rejected). One PATH caveat: a dashboard
build runs with the daemon's environment, not the user's interactive shell, so a
build step that names a bare PATH program present only in the interactive shell
can succeed from `aoe plugin install` yet fail from a dashboard install. Local
(non-`gh:`) installs stay CLI-only.

The worker entrypoint (`command`'s `argv[0]`) must be plugin-relative: a path
containing a separator (`.venv/bin/worker`), resolved inside the install
directory. The host enforces this at manifest validation, so the
PATH-independent shape is the default and a bare program name is rejected rather
than silently resolved against whatever PATH the daemon happens to have. An
absolute path is rejected in every mode (it pins a host path).

A worker that genuinely depends on a system tool, for example `command = ["uv",
"run", "worker"]`, opts into that PATH dependency explicitly with `system =
true`:

```toml
[runtime]
kind = "command"
command = ["uv", "run", "worker"]
system = true   # argv[0] is a bare PATH program, resolved at launch
```

`system = true` requires a bare program name (a path is contradictory and
rejected) and moves resolution to launch time against the daemon's PATH. It is
the conscious "I accept the daemon must have this tool" choice, not a fallback a
manifest falls into by naming a program that happens not to be on PATH. Because
its program is resolved at launch, a `system` worker is also not PATH-checked at
install (the install shell's PATH is not the daemon's), so it installs even when
the tool is absent from the install environment.

Two trust notes for build steps. They run as the user, unsandboxed, before any
capability gate (the same honest D8 model as the worker, just earlier), so a
plugin with build steps always prompts at install, even when it requests no
capabilities, and discloses the commands verbatim; `--yes` consents to both.
And a build that runs `pip install` pulls dependency bytes the source tree hash
does not attest; a featured plugin should pin them (for example a hash-locked
`requirements.txt`). First-class dependency and release-binary attestation are
deferred.

## Capabilities and grants (#2093)

Static contributions are not capabilities; a theme or a command needs no
approval. A capability gates runtime access to a resource that can affect user
data, host state, the OS, or the network. The v1 set
(`aoe_plugin_api::KNOWN_CAPABILITIES`): `runtime.worker`, `session.read`,
`session.write`, `config.read`, `config.write`, `process.spawn`, `net`,
`fs.read`, `fs.write`, `clipboard.read`, `clipboard.write`, `notifications`,
`browser_open`, `composer.read`, and `composer.write`. A plugin's own declared
settings need no `config.*`; that gates host/global or other-plugin config.

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
`<app_dir>/plugins/<id>/`. A source is a `gh:owner/repo[@ref]` slug or a
local directory (`src/plugin/source.rs`). The web dashboard and the TUI
manager install `gh:` sources through the preview/apply consent flow (see Web
lifecycle jobs below); local-directory installs stay CLI-only.

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
<content>`. `.git` and the reserved `.aoe-build/` build-output directory are skipped
(the first is stripped from an installed tree, the second is generated into it by
build steps and is not part of the source); a symlink or non-UTF-8 path outside
those is a hard error so nothing installed escapes the hash. Skipping
`.aoe-build` is what lets a build-mutating plugin (a venv holds ~thousands of
files and a `python3` symlink) re-derive the same hash at load that the author's
`aoe plugin hash` of a clean checkout produced; a fixed reserved name keeps the
exclusion out of attacker control, where a manifest-declared list a tampered
manifest could widen would not. A source that ships `.aoe-build` is refused at
fetch, so the excluded subtree can never hide vetted source. File
mode is excluded for cross-platform determinism, and `git clone` runs with
`core.autocrlf=false` so line endings never differ by platform. The hash is
computed over the staged source **before** any release-binary worker is
injected, so an author's `aoe plugin hash <checkout>` reproduces the
install-time value; the downloaded worker stays pinned separately by the lock's
`asset_sha256`.

`plugins/featured.toml` is the curated index, compiled into the binary. Each
entry holds a plugin's `source` slug and a `version -> tree_hash` map of vetted
releases: a maintainer's attestation that each listed tree was reviewed. When a
plugin id appears in the index, install and update **refuse** if the fetched
source slug (case-insensitive) does not match, or if the manifest ships a
release-binary worker (its bytes are not covered by the tree hash yet). The tree
hash is then checked against the entry's set of vetted hashes: a match is
featured-verified. An id-in-index install at a hash that is **not** in the set is
an unvetted version, not a tamper-refuse: it installs as a non-featured plugin
(community for a GitHub install), so a maintainer can vet a new release by
appending its hash without un-verifying older ones. The reserved-namespace gate
is unchanged, so an unvetted version of a reserved-namespace plugin is still
refused (only a vetted release lifts that gate). A featured-verified install is
the one case allowed to claim a reserved (`aoe.*` / `agent-of-empires.*`)
namespace; a builtin-id collision is always rejected. To ship a new release, run
`aoe plugin hash` against the new tag and add a `"<version>" = "sha256:..."`
entry inside the entry's `versions` map alongside the existing ones. In debug
builds `AOE_FEATURED_INDEX_PATH` overrides the embedded
index for tests; a release binary always uses the compiled-in index, since the
curated set is a root of trust and must not be redefinable by the environment.

Every surface (CLI `aoe plugin list` / `info`, the TUI plugin manager, the web
Plugins panel) shows a `ValidationState`: `builtin`, `featured`, `community` (an
unvetted GitHub install), or `local` (a local-directory install). `featured` is
re-derived live at load (the id is in the embedded index and the on-disk tree
hashes to the pin), not trusted from the lockfile, since that same derivation
gates the reserved-namespace lift and the lockfile is user-writable; `community`
vs `local` is derived from the install source. The lockfile records the tree
hash and the install-time `trust` as a resolved record, but the load path does
not depend on them for validation. The recompute is cheap (only ids the index
names, and a featured plugin ships no release-binary, so its installed source
equals its pinned source; build output under `.aoe-build` is excluded, so a
build-mutating plugin re-derives the same hash) and is done live on every load
from the on-disk tree,
never from a cache: a metadata-keyed cache could be forged to return a stale
vetted hash for a tampered tree, so the verified decision always re-hashes
content. The manifest-hash grant check still catches a community plugin tampered
after install.

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
settings are global-only at Tier 0 (not profile-overridable). In the TUI the
settings Plugins tab is a master-detail split: the manager list on top (sized
to its rows), and the selected plugin's settings as normal editable rows
beneath it, through the same generic schema field path every other category
uses. Moving the list selection swaps the detail pane, Tab moves the sub-focus
between the panes, and a settings-search jump to a plugin field selects that
plugin's row. While the manager captures input (discovery, a consent or
progress popup) it takes the whole pane.

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
consulted only after. `aoe plugin info` lists a plugin's keybinds and flags any
chord core shadows. The home view resolves a plugin keybind but has no per-session
plugin snapshot, so it still shows a "needs the plugin runtime" notice; the
structured view executes them (see "Command execution" below).

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

- `command`: `argv[0]` resolves on `PATH` via `which` when it is a bare name (an
  interpreter or system tool like `python3` / `uv`), or relative to the plugin
  directory when it contains a separator (an in-tree script or binary, for
  example a build-produced `.venv/bin/worker`), verified executable. Absolute
  and parent-traversal paths are rejected. The same policy resolves each
  `[[runtime.build]]` step at install time; a plugin's own entrypoint should be
  plugin-relative so the daemon's PATH never decides whether it launches.
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

### Recovery and observability

Launch runs through one idempotent `PluginHost::reconcile`: it starts a worker
for every active runtime plugin that has none and tears down any worker whose
plugin is no longer active. `start()` at daemon boot is that reconcile plus a
WARN for each load error and each enabled-but-inactive runtime plugin, so a boot
that launches zero workers names the reason (a stale grant, a host-version
mismatch) in `debug.log` instead of going silent. The web enable/disable handler
(`POST /api/plugins/{id}/enabled`) calls reconcile too, so toggling a plugin
launches or stops its worker live, without a daemon restart.

When a worker exhausts its respawn budget the host records an in-memory crash
tombstone and surfaces a dashboard notification. A tombstoned plugin is not
revived by an unrelated plugin's reconcile; disabling it clears the tombstone,
so a disable then enable is a clean retry. A daemon restart also clears it. The
CLI `aoe plugin enable|disable` and the TUI manager toggle route through the
running daemon when one is up (`set_enabled_live`, above), so a disable/enable
recycle from any surface is a clean retry; when no daemon is reachable, or the
daemon request fails (read-only, auth, server error), the toggle falls back to
a local config write that a later daemon start picks up.

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
| `config.get` | `runtime.worker` |

`events.*` run over a shared plugin event bus (a `plugin_host` schema on the
durable event-log substrate, `src/events/`); `subscribe { topics, after_seq }`
is a replay-after-cursor read, so a worker polls forward from the last seq it
saw. Session metadata is always read and written under the calling plugin's own
`plugin_meta[<plugin-id>]` slot: the worker sends only a `key`, never another
plugin's id, so one plugin cannot reach another's data. A `session.meta.cas`
that loses returns the current value rather than clobbering it. Writes go
through `Storage`'s cross-process lock, so the daemon picks them up on its next
session reload (eventual consistency, not a live push).

`sessions.list` returns one entry per session with `id`, `title`,
`project_path`, `tool`, `status` (the run-state), and two inactivity flags:
`archived` (the session is archived) and `snoozed` (it has a snooze deadline
still in the future; a past deadline reports `false`). A worker that should
ignore dormant sessions, for example to avoid spending API quota on them, can
check these flags itself.

The call also takes an optional `exclude` param: an array of state names to
drop server-side, from `archived`, `snoozed`, and `trashed`. A missing or empty
`exclude` returns every session (so an older worker is unaffected); a value
outside that set, or a non-string entry, is an `INVALID_PARAMS` error rather
than a silently ignored filter. This lets a worker skip trashed sessions
(pending deletion, never surfaced to the user) without enumerating them, which
matters because a workspace with many trashed sessions would otherwise push
per-session UI state for all of them.

`config.get { key }` returns the value at `plugins.<plugin-id>.settings.<key>`
for the calling plugin's own id, so a worker reads back the settings the user
edited on the TUI/web surfaces, falling back to its own default when the key is
unset (the call returns null). The id is the caller's own, never a request
parameter, so a plugin can only read its own table. Reading one's own declared
settings needs no `config.*` capability: `config.read` / `config.write` gate
host/global or other-plugin configuration, which no host method exposes yet, so
`config.get` rides on `runtime.worker` like `events.*`.

### Sandboxing

`SandboxBackend` (`src/plugin/sandbox.rs`) is the seam between a resolved launch
and the spawn. The only v1 backend is `NoSandbox`, which runs the worker as an
ordinary child. Per D8 this is honest, not complete: capability gating at the
host API boundary stops a cooperative plugin from overreaching, but a granted
worker has no OS-level isolation, so an adversarial plugin is not contained. The
capability grant prompt states this on every install. Restricted-environment,
landlock, and `sandbox-exec` backends land later behind the same trait, with no
change to the resolver or the supervisor.

## UI extension points (#2366)

A plugin worker pushes typed UI state to the host over capability-gated RPCs;
the **host** renders every slot, on the web dashboard and (the
terminal-applicable subset) in the daemon-connected TUI. No plugin code runs in
either surface and the render path never awaits a worker: the host keeps an
in-memory snapshot each surface reads synchronously (see Delivery below for what
the TUI renders).

The slots are a closed `UiSlot` set (`aoe-plugin-api`), kebab-case on the
wire: `status-bar`, `row-badge`, `row-column`, `sort-key`, `filter-facet`,
`card`, `pane`, `composer-action`, `detail-badge`, `notification`. A plugin
declares the `(slot, id)` pairs it may fill in its manifest `[[ui]]` section;
an unknown slot is a hard parse error (the host must know how to render each).

A UI contribution is not a capability and needs no grant, but the slots a
plugin declares are disclosed so the user knows it modifies the dashboard
before trusting it: the `aoe plugin install` prompt lists them alongside the
requested capabilities, and they show in `aoe plugin info`, the TUI plugin
manager, and the web Plugins panel (via `PluginView.ui_contributions`).

### RPCs (`src/plugin/host_api.rs`)

- `ui.state.set { slot, id, session_id?, payload }` and
  `ui.state.remove { slot, id, session_id? }`. Gated by `runtime.worker` **and**
  the `(slot, id)` being declared in the manifest: no dedicated `ui` capability
  is introduced. The `payload` is validated against the slot's typed shape and
  stored normalized; an unknown field or bad tone is rejected. Per-session slots
  (`row-badge`, `row-column`, `pane`, `composer-action`, `detail-badge`) require a
  `session_id`; global slots must not carry one. The text-based slots
  (`status-bar`, `row-badge`, `detail-badge`) accept optional `icon` (a lucide
  icon name in kebab-case, e.g. `git-pull-request-arrow`; the client maps it
  through an allowlist, an unknown name renders nothing) and `href` (when set,
  the badge renders as a link that opens in a new tab; only `http`/`https` URLs
  are followed).
- `ui.notify { tone, title, body?, session_id? }`. Gated by the existing
  `notifications` capability (not a slot declaration). Returns a monotonic
  `seq`.
- `ui.open_url { url, session_id?, title? }`. Gated by the `browser_open`
  capability. For a URL computed in the worker rather than sitting in a
  `(slot, id)` UI-state entry (which an `open-ui-link` command reads directly).
  Delivered as a notification carrying the `href`: the native TUI opens it on
  first display, the web renders a click-to-open toast (an async push cannot
  `window.open` without the popup blocker). The URL must be `http`/`https`.
- `composer-action` payloads have the shape
  `{ label, method, icon?, tone?, tooltip?, disabled?, draft_operation? }`.
  The dashboard renders the button in the ACP composer and POSTs the named
  `method` to the same `/api/plugins/{id}/action` endpoint used by pane
  actions, including the active session id. If the plugin declares
  `composer.read`, the request params also include a click-scoped composer
  snapshot (`text`, `selection_start`, `selection_end`); otherwise the server
  strips that snapshot before forwarding. A `draft_operation`
  (`insert-text`, `replace-selection`, or `set-text`) requests a host-mediated
  edit to the current draft and requires `composer.write`. The web applies each
  operation once per operation id so a persistent UI-state entry cannot replay
  on every poll.

#### Richer payloads: `row-badge` items and the `pane` block list

Two slots carry more than a single value, so one entry (one declared
`(slot, id)`) can render a list:

- `row-badge` also accepts `items: BadgeItem[]` where
  `BadgeItem = { text?, icon?, tone?, href?, tooltip? }`. Each item renders as a
  compact, tone-tinted icon (falling back to `text`), linked when `href` is a
  safe URL. The single `{ text, tone, tooltip, icon, href }` form still works.
  An empty `items: []` clears the row.
- `pane` also accepts `blocks: Block[]`, an ordered list of typed
  blocks. The web renderer knows these kinds: `heading { text }`,
  `row { label, value?, sublabel?, icon?, tone?, color?, href? }`,
  `note { text, tone? }`, `divider {}`,
  `section { title?, children: Block[], collapsible?, collapsed? }` (nested
  blocks; `collapsible` wraps the section in a native `<details>` the user can
  fold, and `collapsed` starts it folded, default open),
  `comment { author, body, path?, line?, resolved?, href? }` (a read-only PR
  review comment: author, optional file:line, a wrapped body excerpt, and an
  unresolved/resolved marker), and
  `action { label, method, icon? }` (a button that forwards `method` to the
  plugin's worker, see below). A block's optional `color` is a validated hex
  literal (`#rgb`/`#rrggbb`, normalized; no CSS names, `rgb()`, `var()`, or
  `url()`, so it can never carry arbitrary CSS) that tints the block's
  icon/value where a semantic `tone` cannot name the hue, e.g. a merged PR's
  purple. The simple `{ title, body }` form still works
  when `blocks` is absent. A `pane`
  also takes an optional `default_location` (`right` | `bottom`) choosing the
  dock it first opens in; the user can move it between docks afterward, and an
  optional `icon` (any lucide icon name, kebab-case) for its activity-bar
  button, falling back to a generic plugin icon. The host renders each `pane` as
  a dockable tool-window (activity-bar toggle, move, close) alongside the
  built-in diff and terminal panes. Each pane's body scrolls, and a long
  `comment` body is clamped with a "more"/"less" toggle, so a full PR comment
  list stays browsable.

  A pane entry gets a larger payload budget than the other slots: its normalized
  JSON may be up to 64KB, against 8KB for every other slot (`status-bar`,
  `row-badge`, `row-column`, `card`, `detail-badge`), so a plugin can push a full
  comment list in one pane entry without truncating to fit.

**Block parsing is forward-compatible by design.** The host stores `blocks` as
opaque JSON (`Vec<Value>`); it validates only that the payload envelope is
well-formed, not the block kinds. The web renderer draws the kinds it knows and
silently ignores any unknown `kind` or unknown field within a block. So a plugin
can add a field to an existing kind, or push a brand new kind, without any host
change: an older host simply renders what it understands and drops the rest.
This is deliberate, the GitHub plugin's pane keeps growing (PR state today,
review/CI/timelines later) and must not require lockstep host releases.

**Pane actions (host to worker).** An `action` block is a button. When clicked,
the dashboard POSTs `/api/plugins/{id}/action { method, params? }`; the host
writes that JSON-RPC method to the worker's stdin as a notification (no id, so
no reply) via `PluginHost::notify_worker`. The worker runs the method (e.g.
`github.refresh`) and re-pushes its UI state, which the next `ui-state` poll
renders. The plugin names the `method` in its own block, and the worker is the
trust boundary: it acts only on methods it implements and ignores the rest (the
honest-plugin model). The endpoint is gated on read-write mode only, not on
passphrase elevation: a pane action mutates no host-managed state (config,
registry, grants, lockfile) and grants no new host capability, so it does not
warrant the step-up the way enable/disable does (the worker's own behavior may
still have plugin-defined side effects). If an action ever needs elevation,
make it opt-in per action rather than blanket-gating every action.

### Command execution

A `[[commands]]` entry either carries a client `action` or does not, and that
splits how invoking it (from the cmd+k palette or a keybind) behaves.

- **Client action (`open-ui-link { slot, id }`, requires `browser_open`).** The
  surface executes it directly, no worker round-trip: it reads the `href` from
  the command's own `(slot, id)` per-session UI-state entry and opens it. Web
  and TUI both resolve links the same way (`resolveCommandLinks` /
  `UiSnapshot::links_for`): each `items[]` href, else the top-level `href`,
  `http`/`https` only. The palette lists one entry per link, so a multi-repo
  workspace with several open PRs becomes several entries. A keybind cannot
  disambiguate several links, so it shows a small numbered picker (`1`-`9` to
  open); a single link opens directly.
- **No action (fire-and-forget worker command).** The command is dispatched to
  the worker as a fixed `plugin.command.invoke { command, session_id }`
  notification. The web POSTs `/api/plugins/commands/{fqid}/invoke { session_id }`;
  the TUI structured view calls the same endpoint over the daemon. Unlike
  `/action` (an arbitrary caller-named method), the host resolves the command
  from the registry and requires that it exists, carries no client action, and
  names a live session before dispatching. The worker acts on commands it knows
  and ignores the rest.

The native TUI executor lives in the structured view (which holds the
per-session plugin snapshot): a plugin chord it does not otherwise consume runs
the command, opening a link, showing the numbered picker, or POSTing the invoke
endpoint. It resolves chords against the command list the daemon serves at `GET
/api/plugins/commands` (polled alongside the UI snapshot), not the TUI's own
local registry, so a session on a remote daemon drives plugins installed only
there. All TUI browser opens go through `tui::open_url`, which honors
`AOE_OPEN_URL_TO` (a file it appends URLs to instead of launching a browser) so
a live-daemon e2e can assert the resolved URL.

### Store and lifecycle (`src/plugin/ui_state.rs`)

State is in-memory and dies with the daemon, like the rest of the Tier 1 host.
Each worker spawn takes a *generation*; a plugin's entries are cleared when its
worker exits, guarded by the generation so a late write or an instant respawn
cannot resurrect or clobber the live worker's state. Notifications ride a
separate bounded ring and survive a worker exit (a plugin that posts then
crashes still reaches the browser). Per-plugin quotas bound memory.

### Delivery

`GET /api/plugins/ui-state` returns the full snapshot (entries grouped nowhere,
plus the notification ring); it is small and bounded, so there is no
incremental cursor. The dashboard polls it on the same cadence as
`/api/sessions` and renders per-session entries only for sessions present in
the live list. Notifications surface as toasts, deduped by `seq`.

`sort-key` and `filter-facet` render in the dashboard sidebar (#2401): each
global `sort-key` is an extra option in the sort picker that orders rows by the
referenced `row-column`'s `sort_value` (best value per direction at the
workspace and group level, unvalued rows sink), and each `filter-facet` is a
facet control that filters rows by the referenced `row-column`'s
`filter_values` (AND across facets, OR within one). Both selections are
client-side and ephemeral: they read the already-fetched scalars, run no plugin
code, and are not persisted, so a daemon restart falls back to the built-in
sort.

The native **structured-view** TUI (`aoe acp attach`, the remote-home picker)
polls the same endpoint on a 3-second cadence and renders the slots a terminal
can show: global `status-bar` segments and the open session's `detail-badge`
entries, tone-colored, in its status line, plus `notification`s as toasts
(deduped by `seq`, queued so a burst shows one at a time). It renders text and
tone only; `icon`, `tooltip`, and `href` are dropped, and `card`, `pane`,
`row-badge`, `row-column`, `sort-key`, and `filter-facet` have no structured-view
surface. The standalone home screen reads local session storage and has no
daemon link, so it renders no plugin slots; rendering there is a follow-up
(#2402).

## Discovery and update checks (#2365)

Discovery and update checks are **explicit actions, never background work** (the
one exception is the opt-in auto-update sweep below). Both are repo/source level
and reuse the existing install trust model rather than weakening it.

### Discovery

`plugin::discover::discover(query)` runs one GitHub search over the `aoe-plugin`
topic (`topic:aoe-plugin fork:false archived:false`, plus an optional free-text
term) and badges each result by matching the repo slug against the featured
index source slugs and the installed plugins' sources (case-insensitive). It does
**not** fetch each repo's `aoe-plugin.toml`: cloning N search results to read a
manifest would be an N+1 blowup against the unauthenticated search rate limit. So
a result is "a GitHub repository tagged `aoe-plugin`", and a `featured` badge
means "a curated source slug", not "the current tree matches the pin". Results
rank featured-first then by stars (#2105 will add popularity ranking). Install
stays the trust boundary: `aoe plugin install` fetches the manifest, prompts for
capabilities, and enforces the featured pin.

Each result also carries `source_avatar_url`, the repo owner's GitHub avatar
(`github.com/{owner}.png`), derived from the search response's `full_name` with
no extra request. This is a source-identity affordance, not the plugin's own
`icon`/`icon_asset` (unknown until a manifest fetch, which discovery
deliberately never does); the dashboard renders it as a separate, distinctly
styled avatar rather than through the plugin identity icon component.

Surfaces: `aoe plugin discover [query]`, the TUI plugin manager `d` key (`/`
edits the free-text search term), and the dashboard "Search GitHub" button
(`GET /api/plugins/discover?q=`). Both in-app surfaces install a result through
the preview/apply consent flow: the dashboard marketplace's Install button and
the TUI discover list's Enter open the same structured disclosure
(`preview_install`) before anything runs, and each result still shows the
copyable `aoe plugin install gh:owner/repo` command for users who prefer the
terminal.

Unauthenticated GitHub search is rate limited (about 10 requests/minute/IP); the
client maps a 403/429 to a `RateLimited` error so each surface reports it plainly
rather than as a generic failure.

`GET /api/plugins/details?source=gh:owner/repo` backs the dashboard's detail
modal (opened from a discovery result or an installed-plugin row). It reads the
plugin's `aoe-plugin.toml` via the GitHub contents API (no clone) and lists the
repo's release tags as the available versions. The manifest is parsed leniently
(unknown and future keys ignored, `api_version` not range-checked), so a plugin
targeting a newer host than the one installed still renders; a missing or
unparseable manifest is reported in `manifest_error` while the release tags still
load.

### Update checks

`plugin::update_check::outdated()` checks every installed external plugin against
its `plugins.lock` entry. A GitHub source compares the locked `resolved_commit`
to `git ls-remote <clone_url> <ref|HEAD>` (no clone, no REST rate limit; honors
`AOE_GITHUB_CLONE_BASE`, and an annotated tag's peeled `^{}` target wins). A local
source re-hashes its source directory with `integrity::tree_hash` and compares to
the locked `tree_hash`. Builtins are skipped; a missing lock entry, absent `git`,
or dead remote is reported per-plugin, never silently treated as up to date. A
commit-pinned install is never "outdated". Limitation: a `release-binary` plugin
whose release asset is replaced without a source-commit change is not detected
(ls-remote only sees the source tree).

Surfaces: `aoe plugin outdated`, the TUI plugin manager `c` key, and
`GET /api/plugins/updates`. The web endpoint is separate from the always-on
`GET /api/plugins` list so a settings render never blocks on git or the network;
the dashboard paints update-available badges only after the user clicks "Check
for updates".

### Auto-update sweep

The opt-in `updates.auto_update_plugins` setting (off by default) runs a sweep at
TUI and `aoe serve` startup (`plugin::auto_update::spawn_if_enabled`), spawned
non-blocking so a slow remote never delays startup. It applies only **clean**
updates, those that need no new consent; any version that changes the capability
set, build steps, or UI slots is skipped and left for a manual `aoe plugin
update` so the new grant is reviewed (`install::ConsentMode::CleanOnlyNonInteractive`).
A background sweep therefore never grants new capabilities, runs a changed build
step unattended, or deactivates a working plugin. Applied updates take effect on
the next launch / daemon restart.

## Unattended plugin sessions (#2897)

With API v9 a worker can create host-owned structured sessions and deliver
turns to them (`sessions.create`, `sessions.turn.send`), the primitives an
automation plugin such as a scheduler needs. Because this lets plugin code
start agents and send prompts with no user present, the host enforces a
defense-in-depth model; the plugin proposes, the host disposes.

**Capability layering.** `session.create` and `session.prompt` gate creating a
session and delivering a turn. `session.unattended` is a *separate,
high-severity* grant, shown as its own install-consent line and never implied
by the other two: it is required only when the requested approval mode is
host-classified as unattended.

**Host-owned approval classification.** The plugin supplies a `mode_id`; the
host, not the plugin, assigns its security class. `classify_mode` (in
`src/plugin/automation_policy.rs`) resolves: an omitted mode is *interactive*
(the adapter default that prompts); a mode in the host's trusted table that
preserves approvals (a read-only or plan preset) is *guarded*; an adapter
bypass id or auto-write mode (for example claude `bypassPermissions` or
`acceptEdits`) is *unattended*; and any mode the host does not recognize is
*unattended* (fail closed). Only an unattended class requires
`session.unattended`. The plugin can never self-label a mode as safe, and the
option catalog (what a mode is *available*) is deliberately not treated as a
statement of what a mode is *allowed* to do.

**Repository trust is independent of install grants.** A session against a
repository whose hooks need approval is refused at spawn even when
`session.unattended` was granted, fail closed. Plugin-created sessions force
`trust_hooks = false`, so a plugin can never pre-approve a repository's hooks;
that remains a runtime, per-repository user decision. The path is canonicalized
immediately before the trust check to close symlink/substitution races.

**Ownership.** A turn from `sessions.turn.send` reaches only a session whose
`created_by_plugin` matches the caller. The check runs in `SessionService`
before any side effect (no wake, resume, publish, or forward for a denied
caller), so no transport can bypass it; a plugin cannot adopt or write to a
user's or another plugin's session. A plugin-delivered turn also re-asserts the
session's persisted mode before publishing, and withholds the prompt if that
fails, so a turn never runs under an unconfirmed approval posture.

**Idempotency.** `sessions.create` takes an `idempotency_key` scoped to the
plugin, persisted on the session record. A retry with the same key and payload
returns the existing session (`created: false`); a different payload under the
same key is a conflict. Retention equals the session's lifetime: archive,
snooze, and trash keep deduplicating; a hard delete releases the key.

**Rate, concurrency, audit, kill switch.** Per plugin the host allows 20
creates/hour, 5 active (non-trashed) plugin-created sessions, and 120
turns/hour, backed by a private audit ledger (a dedicated `events` schema in
`plugin_events.db`, unreachable from worker RPCs) so the rolling windows
survive daemon restarts. Every admission and denial is audited (plugin id,
operation, agent, mode, session id, decision) without recording prompt text.
Disabling the plugin (the existing per-plugin enable toggle) tears down its
worker and stops all of its automation; there is no plugin-provided bypass flag
(`allow_untrusted`, raw ACP argv, arbitrary env are not accepted).

**No plugin bypass surface.** `sessions.create` accepts only a structured view,
an agent/model/mode, a project path, an optional initial turn, and an
idempotency key. It rejects unknown fields at decode, so a payload cannot
smuggle a host-side knob.

## What comes next

Each deferred piece returns as its own PR once the core is proven: the Tier 0
contribution registries (issue 2094), the UI extension points (issue 2366,
above), the builtin worker self-exec path and worker SDK (with the first
builtin worker that needs them); the integrity-hashing / featured supply-chain
layer landed in #2364 and the discovery / update-check layer in #2365 (both
above). Rendering plugin slots in the standalone (non-daemon) home screen is
still a follow-up (the structured-view TUI already renders the
terminal-applicable subset, #2402). Pinning a featured plugin's
release-binary asset hash in `featured.toml` (so a featured worker is attested,
not just its source) is a follow-up; today a release-binary plugin cannot be
featured. Popularity-based discovery ranking and a `release-binary` asset-drift
update check are tracked in #2105 and remain out of scope here.
