# Plugin system internals

Host architecture and security boundaries. Plugin authors want [Writing plugins](../writing-plugins.md) and the [Plugin API reference](../../plugin-api.md) instead.

## Components

| Area | Source |
| --- | --- |
| Public manifest types | `aoe-plugin-api/` |
| Registry and contributions | `src/plugin/registry.rs`, `contributions.rs` |
| Install and update | `install.rs`, `fetch.rs`, `integrity.rs`, `lockfile.rs` |
| Discovery | `discover.rs`, `featured.rs`, `update_check.rs` |
| Worker launch and supervision | `launch.rs`, `host.rs`, `protocol.rs` |
| Worker RPCs | `host_api.rs`, `session_api.rs` |
| Host-rendered UI state | `ui_state.rs` |
| Automation policy | `automation_policy.rs` |

`src/plugin/mod.rs` owns a lazily loaded process-wide registry; reloading it after a config change updates the CLI, TUI, and web views from one `PluginView`. The standalone `aoe-plugin-api` crate is the source of truth for `aoe-plugin.toml`, and parsing is strict: unknown fields, unsupported API versions, invalid ids, and malformed contributions are rejected. `api_version` describes the manifest schema; `aoe_version` constrains the host application.

Per-plugin settings live under `[plugins."<id>".settings]` in `config.toml` and per-session metadata in `Instance.plugin_meta[<id>]`. Both survive disable and reinstall.

## Installation and trust

Sources are local paths or GitHub references. Installation stages content in the app directory, validates the manifest and host compatibility, runs declared build steps, then atomically places the plugin under `plugins/<id>/`. Build outputs go under `.aoe-build/`, excluded from source integrity. The lockfile pins installed source identity, resolved revision, content hash, manifest hash, and grants, and an update that changes capabilities requires approval. Featured plugins additionally match the repository's pinned source hash and may claim reserved namespaces.

Every worker needs `runtime.worker`; other capabilities gate host RPCs and are checked before dispatch. A plugin reaches only its own settings and metadata unless granted more. Capability checks are an API boundary, not an OS sandbox: workers run as ordinary child processes through `NoSandbox`, so only trusted plugin code should be installed.

## Contributions

Static manifest contributions are collected from active plugins: settings join the single settings schema and keep plugin ownership, themes resolve within the plugin directory, commands and keybindings are namespaced with core names taking precedence, status definitions provide host-rendered labels, and the `pane`, `home-pane`, and `composer-action` slots render typed worker state.

No plugin code runs in a frontend. Workers publish typed state, the host stores the latest generation, and the web or daemon-connected TUI renders it; generation ids stop a stopped worker's late cleanup from deleting a newer worker's state. Pane actions return to the worker as JSON-RPC notifications.

## Worker host

The worker host exists only in `aoe serve`, where session storage and the event store are available. For each active plugin with a runtime it resolves the entrypoint, launches one child process, and speaks newline-delimited JSON-RPC 2.0 over stdio. Command runtimes resolve plugin-relative executables inside the install tree, bare interpreter names may resolve on `PATH`, and release-binary runtimes use the platform asset the installer placed; build steps follow the same policy.

Workers are tied to the daemon lifetime, with no reattach socket or durable runner record. Supervision reaps the whole process group, limits concurrency, and stops retrying after three crashes in 60 seconds; disabling and re-enabling a plugin clears its crash tombstone. Worker stderr goes under `<app_dir>/plugin-workers/`. `PluginHost::reconcile` is the idempotent lifecycle entrypoint used at daemon startup and after enable or disable, and local CLI and TUI toggles route through a running daemon when possible so workers change immediately.

## Host APIs and events

The host exposes capability-checked RPCs for the plugin event bus, private session metadata, plugin settings, session listing, UI state, ACP capability discovery, and plugin-created sessions; the methods and payloads are in the [Plugin API reference](../../plugin-api.md). The event bus uses the protocol-neutral store in `src/events/` with its own schema and replay cursor. Session writes take the storage lock and become visible to other processes on their next reload. Each caller's plugin id comes from its host context, never from request parameters.

## Unattended plugin sessions

Workers may create structured-view sessions and send turns, but the host keeps the security decisions:

- `session.create` and `session.prompt` grant the basic operations, while `session.unattended` is separately required for a host-classified bypass or auto-write mode, and unknown approval modes classify as unattended;
- repository hooks still require repository trust, which a plugin cannot grant;
- turns may target only sessions the calling plugin created;
- idempotency keys are scoped to the plugin and session lifetime;
- per-plugin create, active-session, and turn limits survive daemon restarts in a private audit ledger;
- disabling the plugin stops its worker and its automation.

The create RPC accepts structured fields only: no raw agent arguments, no arbitrary environment variables, no trust bypass.

## Discovery and updates

Discovery merges the featured index with exact GitHub sources the user configured; search does not crawl GitHub. Update checks compare the installed lockfile with the resolved source or release and never mutate installation state. Auto-update uses the same staging, validation, build, integrity, and grant path as an explicit update, so changes needing approval stay pending. Plugin screenshots and identity assets are validated as relative image paths; installed icons are served from canonicalized paths inside the install tree, and marketplace assets resolve against the pinned GitHub source.
