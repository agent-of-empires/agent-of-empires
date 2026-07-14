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
| `api_version` | The manifest *schema* version. The current schema is `8`. The host rejects a manifest whose `api_version` is newer than it supports. Bump it as you adopt newer sections (see below). |
| `aoe_version` | A semver requirement on the *host app* version, e.g. `">=1.11.0, <2.0.0"`. The host refuses to install, and skips loading, a plugin whose requirement excludes the running version. Optional; requires `api_version >= 4`. |

Schema additions by `api_version`: `2` added contributions (commands, keybinds, settings, ui), `3` added the `pane` UI slot, `4` added `status` and `aoe_version`, `5` added `screenshots`, `6` added a command `action`, `7` added identity icons, `8` added the `composer-action` UI slot.

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
| `api_version` | integer | yes | Manifest schema version, `1` to `8`. |
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

Setting types:

| `type` | Widget |
|---|---|
| `string` | Text input (default). |
| `bool` (or `boolean`) | Toggle. |
| `integer` | Number input, bounded by `min` / `max`. |
| `select` | Dropdown over a non-empty `options` array. |

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
