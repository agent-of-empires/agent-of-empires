# Plugin API Reference

The field-by-field reference for `aoe-plugin.toml`, the manifest every Agent of Empires plugin ships. The schema lives in the `aoe-plugin-api` crate (`PluginManifest`) and is the source of truth; the host parses it strictly, so unknown keys are rejected and every key here maps to a schema field. For a guided introduction, see [Writing Plugins](development/writing-plugins.md).

## Versioning

A manifest carries two independent version axes.

| Key | Meaning |
|---|---|
| `api_version` | The manifest *schema* version. The current schema is `13`. The host rejects a manifest whose `api_version` is newer than it supports. |
| `aoe_version` | A semver requirement on the *host app* version, e.g. `">=1.11.0, <2.0.0"`. The host refuses to install, and skips loading, a plugin whose requirement excludes the running version. Optional; requires `api_version >= 4`. |

Each key below notes the `api_version` it needs. Target the newest schema your plugin uses, and set `aoe_version` to the host range you have tested.

## Top-level fields

```toml
id = "dev.example.my-plugin"
name = "My Plugin"
version = "0.1.0"
api_version = 13
aoe_version = ">=1.11.0, <2.0.0"
description = "What the plugin does."
capabilities = ["runtime.worker"]
```

| Key | Type | Required | Notes |
|---|---|---|---|
| `id` | string | yes | Plugin id (see [Plugin id](#plugin-id)). Namespaces config, events, and action names. |
| `name` | string | yes | Human-readable display name. |
| `version` | string | yes | Semantic version of the plugin. |
| `api_version` | integer | yes | Manifest schema version, `1` to `13`. |
| `description` | string | no | Shown in plugin listings. Defaults to empty. |
| `aoe_version` | string | no | Host-app semver requirement. Requires `api_version >= 4`. |
| `capabilities` | array of string | no | Runtime grants the worker needs (see [Capabilities](#capabilities)). Static contributions need none. |
| `screenshots` | array | no | Up to 8. Requires `api_version >= 5`. See [Screenshots](#screenshots). |
| `setting_defaults` | table | no | Overrides for core host settings, keyed by canonical path (e.g. `"theme.idle_decay_minutes"`). Resolution is user value, then plugin override, then core default. |

## Plugin id

A dotted, lowercase ASCII identifier such as `dev.example.review-helper`. Each dot-separated segment starts with a lowercase letter and may contain digits and hyphens; the whole id is at most 64 bytes. The `aoe.*` and `agent-of-empires.*` namespaces are reserved for bundled and officially featured plugins; a community install cannot claim them.

## Capabilities

Capabilities gate runtime resource access. They are prompted once at install and pinned to the manifest hash, so an update that widens them must be re-approved. Declare only what the worker uses; static contributions (commands, keybinds, themes, ui, status) need none.

| Capability | Grants |
|---|---|
| `runtime.worker` | Running any plugin code at all (host RPCs the worker initiates). Any worker needs this. |
| `session.read` | Reading the attached session. |
| `session.write` | Mutating the attached session. |
| `config.read` | Reading host or other-plugin configuration (not the plugin's own settings). |
| `config.write` | Writing host or other-plugin configuration. |
| `process.spawn` | Spawning processes beyond the plugin's own worker. |
| `net` | Outbound network access. |
| `fs.read` | Filesystem reads outside the plugin directory. |
| `fs.write` | Filesystem writes outside the plugin directory. |
| `clipboard.read` | Reading the clipboard. |
| `clipboard.write` | Writing the clipboard. |
| `notifications` | Posting desktop / TUI notifications. |
| `browser_open` | Opening a URL in the user's browser from a command `action`. |
| `composer.read` | Reading a click-scoped snapshot of the active ACP composer draft from a `composer-action`. |
| `composer.write` | Publishing a host-validated draft edit from a `composer-action` UI-state payload. |
| `acp.capabilities.read` | Discovering available agents and their advertised models/modes via `acp.capabilities.get` (`api_version >= 9`). |
| `acp.capabilities.probe` | Triggering a handshake-only catalog probe via `acp.capabilities.probe`: the host spawns the agent adapter, runs initialize + `session/new` (no prompt turn, so no tokens), records the advertised models/modes/thought-levels, and tears it down. Distinct from `acp.capabilities.read` because it spawns a real process (`api_version >= 11`). |
| `session.create` | Creating a host-owned structured session via `sessions.create` (`api_version >= 9`). |
| `session.prompt` | Delivering a turn to a session the plugin created via `sessions.turn.send`, and the initial turn on `sessions.create` (`api_version >= 9`). |
| `session.unattended` | Creating a session in a host-classified *unattended* approval mode. A distinct, high-severity grant, never implied by `session.create` or `session.prompt` (`api_version >= 9`). See [Session-driving RPCs](#session-driving-rpcs). |

A capability this host version does not recognize is rejected, not granted.

## Commands

Palette and CLI entries, namespaced by the host as `plugin.<id>.<command-id>`.

```toml
[[commands]]
id = "status"
title = "My Plugin: status"
description = "Show the status summary."
```

| Key | Type | Required | Notes |
|---|---|---|---|
| `id` | string | yes | Command id. Empty is unaddressable. |
| `title` | string | no | Display name. |
| `description` | string | no | Help text. |
| `action` | table | no | A client-executed action. Requires `api_version >= 6` and the `browser_open` capability. |

A command `action` is a client-executed action instead of a worker call. The only `kind` is `open-ui-link`, which opens the `href` from the plugin's own `(slot, id)` UI-state entry in the browser, with no worker round-trip; that pair must match a declared `[[ui]]` entry on a per-session slot.

```toml
[commands.action]
kind = "open-ui-link"
slot = "row-badge"
id = "my_badge"
```

## Keybinds

```toml
[[keybinds]]
command = "status"
key = "Ctrl+Shift+G"
```

| Key | Type | Required | Notes |
|---|---|---|---|
| `command` | string | yes | Target command id (a plugin or core command). |
| `key` | string | yes | Key chord, e.g. `Ctrl+Shift+G`. Core bindings win a collision. |

## Settings

Plugin-declared settings, rendered on the TUI and web settings surfaces and stored under `[plugins."<id>".settings]`. The worker reads them via the `config.get` host RPC.

```toml
[[settings]]
key = "refresh_secs"
label = "Refresh interval (seconds)"
type = "integer"
default = 120
min = 0
max = 86400
```

| Key | Type | Required | Notes |
|---|---|---|---|
| `key` | string | yes | Setting key, stored under the plugin's settings table. |
| `label` | string | no | Display label. |
| `description` | string | no | Help text. |
| `type` | string | no | Value type (see below). Defaults to `string`. |
| `options` | array of string | no | Allowed values for `select`; ignored otherwise. |
| `min` / `max` | integer | no | Inclusive bounds for `integer`; ignored otherwise. |
| `default` | any | no | Declared default. Must match `type`. Absent means the type's zero value. |
| `advanced` | bool | no | Group under the Advanced fold. Defaults to `false`. |
| `multiline` | bool | no | Render a `string` field as a multi-line textarea; ignored for other types (`api_version >= 11`). |
| `option_source` | string | no | Host source for a `dynamic_select` (`api_version >= 9`). |
| `depends_on` | array of string | no | Sibling keys whose values parameterize a `dynamic_select` (`api_version >= 9`). |
| `fields` | array | no | Item fields of an `object_list` (`api_version >= 9`). |
| `item_id_key` | string | no | Item field holding each `object_list` row's stable id; defaults to `_id` (host-generated) (`api_version >= 9`). |
| `min_items` / `max_items` | integer | no | Inclusive item-count bounds for an `object_list` (`api_version >= 9`). |

Setting types:

| `type` | Widget |
|---|---|
| `string` | Text input (default). |
| `bool` (or `boolean`) | Toggle. |
| `integer` | Number input, bounded by `min` / `max`. |
| `select` | Dropdown over a non-empty `options` array. |
| `dynamic_select` | Dropdown whose choices the host resolves from `option_source` (`api_version >= 9`). |
| `dynamic_multi_select` | Multi-select (checkbox list) whose choices the host resolves from `option_source`; the stored value is an array of chosen values. Object-list item fields only (`api_version >= 11`). |
| `cron` | Validated 5-field cron expression text field (`api_version >= 9`). |
| `object_list` | A repeatable list of structured items described by `fields` (`api_version >= 9`). |

### Dynamic selects (`api_version >= 9`)

A `dynamic_select`'s options are resolved by the **host** at render time, so the plugin never ships a list that could drift from the host's real agents, models, or projects. Set `option_source` to one of:

| `option_source` | Choices |
|---|---|
| `acp.agents` | ACP-capable agents the host knows *and whose adapter is installed on this host*. Uninstalled harnesses are not offered. |
| `acp.models` | Models the selected agent advertised. Needs the agent via `depends_on`. |
| `acp.modes` | Approval modes the selected agent advertised. Needs the agent via `depends_on`. |
| `projects` | Registered projects (value is the project path). |
| `groups` | Existing session group paths. |

`depends_on` names sibling keys whose values parameterize the source, which `acp.models` and `acp.modes` require. When that agent's catalog has never been discovered, resolving them runs a one-shot handshake probe (see `acp.capabilities.probe`), so the picker self-fills on first open. Saved ids are advisory: the host revalidates at session creation, so a model that later disappears surfaces as an error then rather than silently at save.

### Object lists (`api_version >= 9`)

An `object_list` is a repeatable list of structured records (a scheduler's entries, say), stored as a TOML array of tables under `[[plugins."<id>".settings.<key>]]`. It is **one level deep**: item fields are declared in `fields` and cannot themselves be an `object_list`. Every item carries a stable id under `item_id_key`, host-generated on add and never changed on edit or reorder, so a worker can track an entry across edits.

```toml
[[settings]]
key = "jobs"
type = "object_list"
item_id_key = "id"
max_items = 50

[[settings.fields]]
key = "agent_id"
type = "dynamic_select"
option_source = "acp.agents"
required = true

[[settings.fields]]
key = "schedule"
type = "cron"
required = true
```

An item field takes the same keys as a top-level setting (`key`, `label`, `description`, `type`, `options`, `min`, `max`, `default`, `multiline`, `option_source`, `depends_on`) plus `required`. It may be a `dynamic_multi_select` (`api_version >= 11`), whose stored value is an array of the chosen option values.

## Session-driving RPCs

With `api_version >= 9` a worker can discover ACP capabilities and create host-owned structured sessions, the primitives an automation plugin (for example a scheduler) needs. These are worker RPCs, not manifest keys; the host enforces a strict security model around them.

| Method | Capability | Purpose |
|---|---|---|
| `acp.capabilities.get` | `acp.capabilities.read` | List agents and their advertised models / modes / thought-levels (never launches an agent; a never-run agent reports `catalog_status: undiscovered` with empty lists). |
| `acp.capabilities.probe` | `acp.capabilities.probe` | Populate the catalog for one agent (optional `agent_id`; otherwise every undiscovered registry agent) via a handshake-only probe, then return the same shape as `acp.capabilities.get`. Spawns the adapter and runs initialize + `session/new` with **no prompt turn** (no tokens); each probe degrades to a no-op on failure. `api_version >= 11`. |
| `sessions.create` | `session.create` (+ `session.prompt` for an initial turn, + `session.unattended` for an unattended mode) | Create a structured session, optionally with an initial turn and a plugin-scoped idempotency key. |
| `sessions.turn.send` | `session.prompt` | Deliver a turn to a session **this plugin created**. |
| `plugin.storage.get` / `set` / `cas` / `remove` | `runtime.worker` | Plugin-private durable key/value storage (see [Plugin storage](#plugin-storage)). |

**Project selection (`api_version >= 11`).** `sessions.create` takes an optional `project_path` (the trust-checked primary repo) and `extra_project_paths` (the other repos of a multi-repo session). Omitting `project_path` creates a **scratch** session: a throwaway directory with no repository, so extras alongside it are refused. Every path is canonicalized and existence-checked host-side, fail-closed and capped per call.

**Sandbox (`api_version >= 11`).** `sandbox: true` runs the session in the host's container sandbox, on the host's own configured image. It only narrows what the agent can reach, so it needs no grant beyond `session.create`. The create fails synchronously when no runtime is installed, but the container starts asynchronously, so image-pull problems surface on the session later.

**Approval-mode classification.** The plugin proposes a `mode_id`; the **host** decides its security class. A mode is *interactive* (omitted, adapter default), *guarded* (a reviewed read-only or plan preset), or *unattended* (a bypass or auto-write mode, plus every mode the host does not recognize, which fail closed). An unattended mode requires `session.unattended` on top of `session.create`.

**Repository trust holds regardless of grants.** A session against a repository whose hooks need approval is refused even with `session.unattended`; a plugin cannot pre-approve trust. See [Unattended sessions](development/internals/plugin-system.md#unattended-plugin-sessions).

**Ownership.** `sessions.turn.send` reaches only a session the calling plugin created.

**Busy sessions.** A turn aimed at an agent already running a non-steerable turn (or cancelling, or compacting) is refused with a retryable `agent_busy` rather than dropped. A stopped or dormant session is not busy: the host wakes it the way a user prompt does, closes any turn the previous worker left open, resumes the worker, and waits.

**Idempotency.** `sessions.create` takes a plugin-scoped `idempotency_key`: retrying with the same key and payload returns the existing session (`created: false`), while a different payload under that key is a conflict.

**Limits.** Per plugin: 20 creates per hour, 5 active plugin-created sessions, 120 turns per hour, reported as `rate_limited` or `concurrency_limited`. Disabling the plugin stops all of its automation.

**Settings-change events.** After a settings write the host notifies the worker with `plugin.settings.changed` carrying `{ revision, changed_keys }`; the worker re-reads those values with `config.get`, whose response carries the current `revision`. Polling that method is the fallback for a worker that was down.

## Plugin storage

A worker has a host-backed private key/value store, namespaced by its plugin id, that survives daemon and worker restarts (unlike the install directory, which an upgrade can replace). It needs no capability beyond `runtime.worker`, since a plugin can only reach its own namespace.

| Method | Params | Returns |
|---|---|---|
| `plugin.storage.get` | `{ key }` | `{ value }` (null if absent) |
| `plugin.storage.set` | `{ key, value }` | `{}` |
| `plugin.storage.cas` | `{ key, expected, value }` | `{ swapped, current }` |
| `plugin.storage.remove` | `{ key }` | `{ removed }` |

Quotas per plugin: 64 keys, 256-byte keys, 64 KiB values. `cas` (compare-and-swap) enables safe concurrent updates: the write applies only when the stored value equals `expected`.

## UI slots

Declares the host-rendered slots the worker pushes state into via the `ui.state.set` host RPC.

```toml
[[ui]]
slot = "pane"
id = "my_pane"
```

| Key | Type | Required | Notes |
|---|---|---|---|
| `slot` | string | yes | One of the slot names below. Unknown slots are rejected. |
| `id` | string | no | Addressing id for `(slot, id)` state pushes. Required to be non-empty when a command `action` targets it. |

| Slot | Scope | Renders |
|---|---|---|
| `status-bar` | global | A segment in the dashboard status bar. |
| `card` | global | A card on the dashboard overview. |
| `sort-key` | global | A named sort option over a `row-column` value. |
| `filter-facet` | global | A named filter over a `row-column` value. |
| `row-badge` | per-session | A badge on the session row. |
| `row-column` | per-session | A text column on the session row. |
| `detail-badge` | per-session | A badge in the session detail view. |
| `pane` | per-session | A dockable tool-window pane (requires `api_version >= 3`). See [Pane payload](#pane-payload). |
| `home-pane` | global | A host-wide docked pane on the dashboard overview and the structured-view pane overlay, carrying the same block vocabulary as `pane` but session-less (requires `api_version >= 13`). Several plugins' home panes stack in snapshot order. |
| `composer-action` | per-session | A button beside the ACP composer controls (requires `api_version >= 8`). |
| `notification` | n/a | A transient notification pushed via `ui.notify`; gated by the `notifications` capability, not a slot declaration. |

### Pane payload

A `pane` entry renders a dockable tool-window, pushed with `ui.state.set`:

```json
{
  "title": "GitHub",
  "default_location": "right",
  "icon": "git-branch",
  "blocks": [{ "kind": "heading", "text": "GitHub" }],
  "footer": { "text": "refreshed 12:07", "value": "blocked", "tone": "danger", "icon": "refresh-cw" }
}
```

| Key | Type | Notes |
|---|---|---|
| `title` | string | Shown on the dock tab. |
| `body` | string | The simple form: plain text, used only when `blocks` is absent. |
| `blocks` | array | The block list (below). Takes precedence over `body`. |
| `default_location` | string | `right` or `bottom`. The dock it first opens in; the user can move it after. |
| `icon` | string | Lucide name for the activity-bar / dock-tab icon. A manifest `icon_asset` outranks it. |
| `footer` | table | A status line pinned below the scrolling block list: `text` left, tone-colored `value` right, plus an optional `icon`. Requires `api_version >= 12`. |

The payload is capped at 64 KiB. Everything but `blocks` is validated strictly; `blocks` is opaque JSON, and **each surface renders the kinds it knows and drops the rest.** That forward-compatibility contract cuts both ways: a new kind needs no host change, but an older host renders nothing for it, so a pane that depends on a newer kind should say so with `api_version` and `aoe_version`.

#### Block kinds

| `kind` | Required | Optional |
|---|---|---|
| `heading` | `text` | |
| `note` | `text` | `tone` |
| `divider` | | |
| `row` | one of `label` / `value` / `prefix` / `icon` / `avatar` | `sublabel`, `tone`, `value_tone`, `color`, `href`, `tooltip`, `mono`, `selected`, `badges`, `method`, `params` |
| `section` | | `title`, `children`, `value`, `value_tone`, `badges`, `icon`, `tone`, `boxed`, `scroll`, `collapsible`, `collapsed` |
| `callout` | one of `title` / `detail` | `icon`, `tone`, `color`, `actions` |
| `bar` | `segments` | `caption` |
| `sparkline` | `values` | `max`, `tone`, `bands`, `caption` (requires `api_version >= 13`) |
| `columns` | `children` | |
| `action` | `label`, plus one of `method` / `href` / `disabled` | `icon`, `tone`, `tooltip`, `variant` |
| `comment` | one of `author` / `body` | `path`, `line`, `resolved`, `href` |

`tone` is one of `neutral` / `info` / `success` / `warn` / `danger`. `color` is a validated `#rgb` / `#rrggbb` literal for a hue no tone names (a merged PR's purple); anything else is ignored.

**`row`** lays out at most two lines: `prefix` (mono, tone-tinted) and `label` lead the first with `value` pinned right; `sublabel` leads the second with `badges` (`{ text?, icon?, tone?, tooltip? }`) pinned right. `value_tone` colors the trailing token independently of the row, and `mono` monospaces the row's text. A `method` makes the row body a button firing that worker method, and an `href` alongside it becomes a separate trailing link-out; with `href` alone the whole row is the link. `selected` marks the row as the pane's current subject.

**`section`** groups `children`, with a right-pinned `value` summary or `badges` in its header. `boxed` draws a bordered card, `scroll` caps the body height so a long list scrolls inside the section, and `collapsible` folds it via a native `<details>` (`collapsed` sets the initial state).

**`callout`** is a tone-bordered verdict card: glyph, `title`, `detail` paragraph, and full-width `actions`. Use it for the one thing the pane is telling the user, and a `section` for a list.

**`bar`** stacks `segments` (`{ value, tone?, color?, label? }`) proportionally; segments without a positive `value` are dropped and a bar left with nothing renders nothing. **`sparkline`** plots `values` (oldest first) as a history line, with `max` fixing the top of the scale so a series does not auto-scale each refresh and `bands` (`{ at, tone }` thresholds) recoloring each sample by the highest band it reaches. Both take a `caption` beneath.

**`columns`** lays its `children` out in equal fractions, and a single child spans the full width, so eliding one card collapses the row cleanly.

**`action`** forwards `method` to the worker (see [Pane actions](#pane-actions)); with `href` and no `method` it is a link-out button. `disabled` renders it inert and non-navigating, and `variant: "primary"` gives the brand-filled treatment.

#### Pane actions

Clicking an `action` block, or a `row` carrying a `method`, POSTs to `/api/plugins/{id}/action` with `{ method, params, session_id }`. `params` is the block's own `params` object, forwarded verbatim, so one method can serve every row in a list:

```json
{ "kind": "row", "label": "warn when daemon is stale", "prefix": "#3231",
  "method": "github.select_pr", "params": { "pr": "o/r#3231" } }
```

The host merges in the authoritative `session_id` (a plugin cannot spoof it) and delivers the call as a **fire-and-forget JSON-RPC notification**: no reply, no return value. The worker does its work and re-pushes its UI state, and the clicked control spins until the plugin's UI revision moves, with a 15s timeout. Actions are read-write-mode only and are not passphrase gated, so treat every method as reachable by anyone who can use the dashboard.

The TUI renders panes read-only: it draws the text of every kind (dropping icons, hrefs, and tooltips, and stacking `columns`) but cannot fire an action, so `action` blocks appear as inert `[action] <label>` labels.

### Composer action payload

A `composer-action` entry renders a host-owned button in the dashboard's ACP composer, pushed with `ui.state.set`. `label` and `method` are required; `icon`, `tooltip`, `tone`, and `disabled` are optional.

```json
{ "label": "Dictate", "method": "dictation.start", "icon": "mic" }
```

On click the dashboard POSTs `method` to `/api/plugins/{id}/action` with the active `session_id`. With `composer.read` the forwarded params also carry `{ "composer": { "text", "selection_start", "selection_end" } }`, a click-scoped snapshot of the draft; without it the server strips that snapshot before forwarding.

To mutate the draft, include a `draft_operation` in the pushed payload, which requires `composer.write`:

```json
{
  "label": "Dictate",
  "method": "dictation.start",
  "draft_operation": { "kind": "insert-text", "id": "transcript-1", "text": "Hello." }
}
```

`kind` is `insert-text`, `replace-selection`, or `set-text`. `id` must be stable and non-empty: the dashboard applies each operation id once, so a persistent UI-state entry cannot replay the edit on every poll.

## Status

Status segments the plugin contributes, consumed by the status surface. Requires `api_version >= 4`.

```toml
[[status]]
id = "pr_state"
label = "PR state"
```

| Key | Type | Required | Notes |
|---|---|---|---|
| `id` | string | yes | Stable segment id. |
| `label` | string | no | Human-readable text. |

## Themes

```toml
[[themes]]
name = "My Theme"
path = "themes/my-theme.toml"
```

| Key | Type | Required | Notes |
|---|---|---|---|
| `name` | string | yes | Theme name in the picker. Must not collide with a builtin. |
| `path` | string | yes | Theme TOML path, relative to the plugin directory. |

## Screenshots

Up to 8 marketplace screenshots, shown in the plugin detail view. Requires `api_version >= 5`.

```toml
[[screenshots]]
path = "assets/screenshots/overview.png"
alt = "The plugin's pane showing live status."
caption = "Live status in the pane."
```

| Key | Type | Required | Notes |
|---|---|---|---|
| `path` | string | yes | Repository-relative image path. No URL scheme, no leading separator, no `..`; must be PNG, JPEG, GIF, or WebP. |
| `alt` | string | yes | Accessible description; non-empty. |
| `caption` | string | no | Caption shown beneath the image. |

## Runtime

The worker the host spawns and supervises, in one of two kinds. Omit it for a static, metadata-only plugin.

### Command

The host runs the build steps at install or update, then launches `command`.

```toml
[runtime]
kind = "command"
command = [".aoe-build/venv/bin/my-plugin-worker"]

[[runtime.build]]
command = ["python3", "-m", "venv", ".aoe-build/venv"]
platforms = ["linux", "macos"]
```

| Key | Type | Required | Notes |
|---|---|---|---|
| `command` | array of string | yes | argv. Plugin-relative by default (must contain a path separator, never absolute) so the daemon's `PATH` never decides whether the worker launches. With `system = true` it must instead be a bare program name resolved on `PATH`. |
| `system` | bool | no | Resolve `command[0]` on the host `PATH` (for genuine system tools only). Defaults to `false`. |
| `build` | array | no | Ordered build steps, run once at install or update inside the plugin directory, in the user's interactive shell. |

Build into `.aoe-build/` (the host's build-output directory); the host excludes it from the plugin tree hash, so a venv, `node_modules`, or `target/` there does not break integrity verification.

#### Build step

| Key | Type | Required | Notes |
|---|---|---|---|
| `command` | array of string | yes | argv, same resolution policy as the launch `command`. |
| `platforms` | array of string | no | Restrict to OS names: `linux`, `macos`, `windows`. Empty runs on all. |

### Release binary

The host downloads a release asset instead of building from source.

```toml
[runtime]
kind = "release-binary"
asset = "my-plugin-${target}.tar.gz"
bin = "my-plugin-worker"
```

| Key | Type | Required | Notes |
|---|---|---|---|
| `asset` | string | yes | Asset-name template; `${os}`, `${arch}`, `${target}` are substituted before matching the release. |
| `bin` | string | no | Executable path inside the extracted archive. Omit to run the downloaded asset directly (a raw, non-archive binary). |
