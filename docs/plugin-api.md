# Plugin API Reference

The field-by-field reference for `aoe-plugin.toml`, the manifest every Agent of
Empires plugin ships. The schema lives in the `aoe-plugin-api` crate
(`PluginManifest`) and is the source of truth; this page documents it for plugin
authors. The host parses the manifest strictly (unknown keys are rejected), so
every key here maps to a schema field.

For a guided introduction see [Writing Plugins](development/writing-plugins.md).
To scaffold a working plugin, use the starter template:

```sh
cookiecutter gh:agent-of-empires/plugin-template
```

## Versioning

A manifest carries two independent version axes.

| Key | Meaning |
|---|---|
| `api_version` | The manifest *schema* version. The current schema is `10`. The host rejects a manifest whose `api_version` is newer than it supports. Bump it as you adopt newer sections (see below). |
| `aoe_version` | A semver requirement on the *host app* version, e.g. `">=1.11.0, <2.0.0"`. The host refuses to install, and skips loading, a plugin whose requirement excludes the running version. Optional; requires `api_version >= 4`. |

Schema additions by `api_version`: `2` added contributions (commands, keybinds, settings, ui), `3` added the `pane` UI slot, `4` added `status` and `aoe_version`, `5` added `screenshots`, `6` added a command `action`, `7` added identity icons, `8` added the `composer-action` UI slot, `9` added session-driving worker RPCs (see [Session-driving RPCs](#session-driving-rpcs)), plugin-private storage, and the `dynamic_select` / `object_list` / `cron` settings widgets, `10` added the `tool-card-badge` UI slot.

## Top-level fields

```toml
id = "dev.example.my-plugin"
name = "My Plugin"
version = "0.1.0"
api_version = 8
aoe_version = ">=1.11.0, <2.0.0"
description = "What the plugin does."
capabilities = ["runtime.worker"]
```

| Key | Type | Required | Notes |
|---|---|---|---|
| `id` | string | yes | Plugin id (see [Plugin id](#plugin-id)). Namespaces config, events, and action names. |
| `name` | string | yes | Human-readable display name. |
| `version` | string | yes | Semantic version of the plugin. |
| `api_version` | integer | yes | Manifest schema version, `1` to `10`. |
| `description` | string | no | Shown in plugin listings. Defaults to empty. |
| `aoe_version` | string | no | Host-app semver requirement. Requires `api_version >= 4`. |
| `capabilities` | array of string | no | Runtime grants the worker needs (see [Capabilities](#capabilities)). Static contributions need none. |
| `screenshots` | array | no | Up to 8. Requires `api_version >= 5`. See [Screenshots](#screenshots). |
| `setting_defaults` | table | no | Overrides for core host settings, keyed by canonical path (e.g. `"theme.idle_decay_minutes"`). Resolution is user value, then plugin override, then core default. |

## Plugin id

A dotted, lowercase ASCII identifier such as `dev.example.review-helper`. Each
dot-separated segment starts with a lowercase letter and may contain digits and
hyphens; the whole id is at most 64 bytes. The `aoe.*` and `agent-of-empires.*`
namespaces are reserved for bundled and officially featured plugins; a community
install cannot claim them.

## Capabilities

Capabilities gate runtime resource access. They are prompted once at install and
pinned to the manifest hash; an update that widens them must be re-approved.
Declare only what the worker uses. Static contributions (commands, keybinds,
themes, ui, status) need no capability.

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

### Command action

```toml
[commands.action]
kind = "open-ui-link"
slot = "row-badge"
id = "my_badge"
```

The only `kind` is `open-ui-link`: it opens the `href` from the plugin's own
`(slot, id)` UI-state entry in the browser, with no worker round-trip. The
`(slot, id)` pair must match a declared `[[ui]]` entry on a per-session slot.

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

Plugin-declared settings, rendered on the TUI and web settings surfaces and
stored under `[plugins."<id>".settings]`. The worker reads them via the
`config.get` host RPC.

```toml
[[settings]]
key = "refresh_secs"
label = "Refresh interval (seconds)"
description = "How often the worker polls."
type = "integer"
default = 120
min = 0
max = 86400
advanced = true
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
| `cron` | Validated 5-field cron expression text field (`api_version >= 9`). |
| `object_list` | A repeatable list of structured items described by `fields` (`api_version >= 9`). |

### Dynamic selects (`api_version >= 9`)

A `dynamic_select` renders a dropdown whose options the **host** resolves at
render time, so the plugin never ships a hardcoded list that could drift from
the host's real agents, models, or projects. Set `option_source` to one of:

| `option_source` | Choices |
|---|---|
| `acp.agents` | ACP-capable agents the host knows. |
| `acp.models` | Models the selected agent advertised. Needs the agent via `depends_on`. |
| `acp.modes` | Approval modes the selected agent advertised. Needs the agent via `depends_on`. |
| `projects` | Registered projects (value is the project path). |
| `groups` | Existing session group paths. |

`depends_on` names sibling keys whose current values parameterize the source;
`acp.models` and `acp.modes` require the selected agent. Saved ids are
advisory: the host revalidates them when a session is actually created, so a
model that later disappears from the catalog surfaces as an error at creation,
not silently at save.

### Object lists (`api_version >= 9`)

An `object_list` is a repeatable list of structured records (for example, a
cron plugin's schedule entries), stored on disk as a TOML array of tables under
`[[plugins."<id>".settings.<key>]]`. It is **one level deep**: each item field
is declared in `fields` and cannot itself be an `object_list`. Every item
carries a stable id under `item_id_key` (host-generated on add, never changed on
edit or reorder) so a worker can track an entry across edits.

```toml
[[settings]]
key = "jobs"
label = "Scheduled jobs"
type = "object_list"
item_id_key = "id"
max_items = 50

[[settings.fields]]
key = "agent_id"
label = "Agent"
type = "dynamic_select"
option_source = "acp.agents"
required = true

[[settings.fields]]
key = "model_id"
label = "Model"
type = "dynamic_select"
option_source = "acp.models"
depends_on = ["agent_id"]

[[settings.fields]]
key = "schedule"
label = "Schedule"
type = "cron"
required = true
```

Each item field takes the same `key` / `label` / `description` / `type` /
`options` / `min` / `max` / `default` / `option_source` / `depends_on` keys as a
top-level setting, plus `required` (the item must carry a non-empty value). An
item field's `type` cannot be `object_list`.

## Session-driving RPCs

With `api_version >= 9` a worker can discover ACP capabilities and create
host-owned structured sessions, the primitives an automation plugin (for
example a scheduler) needs. These are worker RPCs, not manifest keys; the host
enforces a strict security model around them.

| Method | Capability | Purpose |
|---|---|---|
| `acp.capabilities.get` | `acp.capabilities.read` | List agents and their advertised models / modes (never launches an agent). |
| `sessions.create` | `session.create` (+ `session.prompt` for an initial turn, + `session.unattended` for an unattended mode) | Create a structured session, optionally with an initial turn and a plugin-scoped idempotency key. |
| `sessions.turn.send` | `session.prompt` | Deliver a turn to a session **this plugin created**. |
| `plugin.storage.get` / `set` / `cas` / `remove` | `runtime.worker` | Plugin-private durable key/value storage (see [Plugin storage](#plugin-storage)). |

**Approval-mode classification.** The plugin proposes a `mode_id`; the **host**
decides its security class, never the plugin. A mode is *interactive* (omitted /
adapter default), *guarded* (a reviewed read-only or plan preset), or
*unattended* (a bypass or auto-write mode, and every mode the host does not
recognize, which fail closed to unattended). An unattended mode requires the
distinct `session.unattended` grant on top of `session.create`.

**Repository trust is enforced regardless of grants.** A session against a
repository whose hooks need approval is refused even with `session.unattended`;
a plugin cannot pre-approve repository trust. See
[Unattended sessions](development/internals/plugin-system.md#unattended-plugin-sessions)
for the full model.

**Ownership.** `sessions.turn.send` only reaches a session the calling plugin
created; a plugin cannot deliver turns to a user's or another plugin's session.

**Idempotency.** `sessions.create` accepts an `idempotency_key` scoped to the
plugin: retrying with the same key and payload returns the existing session
(`created: false`); a different payload under the same key is a conflict.

**Limits.** Per plugin: 20 session creates per hour, 5 active plugin-created
sessions, 120 turns per hour. Exceeding a limit returns a `rate_limited` /
`concurrency_limited` error. Disabling the plugin stops all of its automation.

**Settings-change events.** After a settings write the host sends the plugin's
worker a `plugin.settings.changed` notification carrying `{ revision,
changed_keys }`; the worker re-reads the affected values via `config.get`
(whose response includes the current `revision`). Polling `config.get` remains
a fallback for a worker that was down when the write landed.

## Plugin storage

A worker has a host-backed, private key/value store, namespaced by its plugin
id, that survives daemon and worker restarts (it is not the install directory,
which an upgrade can replace). No capability beyond `runtime.worker` is needed:
a plugin can only reach its own namespace.

| Method | Params | Returns |
|---|---|---|
| `plugin.storage.get` | `{ key }` | `{ value }` (null if absent) |
| `plugin.storage.set` | `{ key, value }` | `{}` |
| `plugin.storage.cas` | `{ key, expected, value }` | `{ swapped, current }` |
| `plugin.storage.remove` | `{ key }` | `{ removed }` |

Quotas per plugin: 64 keys, 256-byte keys, 64 KiB values. `cas` (compare-and-swap)
enables safe concurrent updates: the write applies only when the stored value
equals `expected`.

## UI slots

Declares the host-rendered slots the worker pushes state into via the
`ui.state.set` host RPC.

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
| `pane` | per-session | A dockable tool-window pane (requires `api_version >= 3`). |
| `composer-action` | per-session | A button beside the ACP composer controls (requires `api_version >= 8`). |
| `tool-card-badge` | per-session | A pill on a transcript MCP or skill tool-call card, matched by target (requires `api_version >= 10`). |
| `notification` | n/a | A transient notification pushed via `ui.notify`; gated by the `notifications` capability, not a slot declaration. |

### Composer action payload

A `composer-action` entry renders a host-owned button in the web dashboard ACP
composer. The worker pushes it with `ui.state.set`:

```json
{
  "label": "Dictate",
  "method": "dictation.start",
  "icon": "mic",
  "tooltip": "Start dictation",
  "tone": "info",
  "disabled": false
}
```

`label` and `method` are required. On click, the dashboard POSTs `method` to
`/api/plugins/{id}/action` with the active `session_id`. When the plugin has
`composer.read`, the forwarded params include:

```json
{
  "composer": {
    "text": "current draft",
    "selection_start": 0,
    "selection_end": 5
  }
}
```

Without `composer.read`, the server strips that snapshot before forwarding the
action to the worker.

To mutate the draft, include a `draft_operation` in the pushed payload. This
requires `composer.write`.

```json
{
  "label": "Dictate",
  "method": "dictation.start",
  "draft_operation": {
    "kind": "insert-text",
    "id": "transcript-1",
    "text": "Hello from dictation."
  }
}
```

`kind` is `insert-text`, `replace-selection`, or `set-text`. `id` must be stable
and non-empty; the web dashboard applies each operation id once so a persistent
UI-state entry cannot replay the edit on every poll.

### Tool-card badge payload

A `tool-card-badge` entry attaches provenance pills to transcript tool-call
cards. Declare one slot id per session and push a single entry whose `items`
list carries every badge you want; the host matches each `item` to a card by its
`target`. `target.kind` is `mcp` or `skill` and `target.name` is the raw MCP
server name or skill name (matched exactly, not canonicalized), since an MCP
server and a skill can share a name. Requires `api_version >= 10`.

```json
{
  "items": [
    { "target": { "kind": "mcp", "name": "github" }, "text": "Company", "tone": "info", "icon": "building-2" },
    { "target": { "kind": "skill", "name": "deploy" }, "text": "Verified" }
  ]
}
```

Each item needs `text` or `icon` (a badge with neither renders nothing) and a
non-empty target name; `tone` and `tooltip` are optional. Empty `items: []`
clears the plugin's badges. Rendered in the web dashboard; the native TUI ignores
this slot for now.

## Status

Status segments the plugin contributes, consumed by the status surface. Requires
`api_version >= 4`.

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

Up to 8 marketplace screenshots, shown in the plugin detail view. Requires
`api_version >= 5`.

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

The worker the host spawns and supervises. Omit it for a static, metadata-only
plugin. Two kinds.

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

Build into `.aoe-build/` (the host's build-output directory); the host excludes
it from the plugin tree hash, so a venv, `node_modules`, or `target/` there does
not break integrity verification.

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
