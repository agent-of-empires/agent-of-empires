# Writing Plugins

This guide takes you from nothing to an installed, running Agent of Empires
plugin. For the full manifest schema see the
[Plugin API Reference](../plugin-api.md); for the architecture and security model
see [Plugin System Internals](internals/plugin-system.md). For installing and
managing plugins as a user, see [Plugins](../plugins.md).

A plugin is a directory with an `aoe-plugin.toml` manifest and, optionally, a
worker: an executable the host spawns that speaks JSON-RPC 2.0 over
newline-delimited JSON on stdio, in any language. The host does not link your
code; the manifest is the contract.

## Scaffold from the template

The official starter generates a complete plugin (manifest, worker, tests, CI)
in Python, Node, or Rust:

```sh
cookiecutter gh:agent-of-empires/plugin-template
```

Pick a `runtime` when prompted. The generated project builds, passes its tests,
and answers a `status` command out of the box. The rest of this guide explains
what it generated.

## The manifest

Every plugin declares identity, what it contributes, and (if it has a worker)
how to build and launch it:

```toml
id = "dev.example.my-plugin"
name = "My Plugin"
version = "0.1.0"
api_version = 6
aoe_version = ">=1.11.0, <2.0.0"
description = "What the plugin does."

capabilities = ["runtime.worker"]

[[commands]]
id = "status"
title = "My Plugin: status"
description = "Show the status summary."

[[settings]]
key = "enabled"
label = "Enable My Plugin"
type = "boolean"
default = true

[[ui]]
slot = "pane"
id = "my_plugin_pane"
```

Pick an `id` outside the reserved `aoe.*` and `agent-of-empires.*` namespaces.
Set `api_version` to the schema version you target (currently `6`) and
`aoe_version` to the host range you have tested against. Every key is documented
in the [Plugin API Reference](../plugin-api.md).

## Capabilities

A worker requests only the runtime grants it uses. `runtime.worker` is required
to run any code; add `net`, `session.read`, `notifications`, and so on as
needed. Static contributions (commands, keybinds, themes, ui, status) need no
capability. The user is prompted to grant the exact declared set at install, and
the grant is pinned to the manifest hash, so an update that widens capabilities
must be re-approved. Keep the list honest and minimal.

## The worker

The host spawns the worker, sends one JSON-RPC request per line on stdin, and
reads one response per line on stdout. The worker exits when stdin reaches EOF.

A request, and the response your `status` handler returns:

```json
{"jsonrpc": "2.0", "id": 1, "method": "my-plugin.status", "params": {}}
{"jsonrpc": "2.0", "id": 1, "result": {"ok": true, "message": "running"}}
```

The host maps a command id to a fully namespaced method, `plugin.<id>.<command-id>`,
so the example above is abbreviated: a worker for `dev.example.my-plugin` actually
receives `plugin.dev.example.my-plugin.status`. Dispatch on the trailing segment of
`method` so either form works. Return a JSON-RPC error with code `-32601` for an
unknown method. A message with no `id` is a notification; do not respond to it.

## Build and launch

The worker entrypoint must be **plugin-relative**, never resolved on the
daemon's `PATH`. Build into `.aoe-build/`, which the host excludes from the
plugin's integrity hash, then point `command` at the built artifact:

```toml
[runtime]
kind = "command"
command = [".aoe-build/venv/bin/my-plugin-worker"]

[[runtime.build]]
command = ["python3", "-m", "venv", ".aoe-build/venv"]
platforms = ["linux", "macos"]

[[runtime.build]]
command = [".aoe-build/venv/bin/pip", "install", "."]
platforms = ["linux", "macos"]
```

Build steps run once, at install and update, in the user's interactive shell
(where `PATH` is reliable). A compiled plugin can instead ship a release asset
with `kind = "release-binary"`; see the reference.

## Install and test locally

```sh
aoe plugin install ./my-plugin     # runs the build steps, prompts for grants
aoe plugin list
aoe plugin update my-plugin         # re-runs build, re-approves changed grants
aoe plugin uninstall my-plugin
```

Drive the worker by hand before installing, to confirm the protocol:

```sh
echo '{"jsonrpc":"2.0","id":1,"method":"my-plugin.status","params":{}}' | <your-worker>
```

The starter ships a worker-contract test (it spawns the worker, sends a request,
and asserts the response) plus its CI. Keep that test green; it is the cheapest
guard on the protocol.

## Publish

Push a `vX.Y.Z` tag to cut a GitHub release (the starter's release workflow does
the rest). Users install the latest release with
`aoe plugin install gh:your-org/my-plugin`. To be listed in the Agent of Empires
featured index, which lets a plugin claim a verified namespace, open a PR adding
your release's source tree hash to the featured index in the main repository.
