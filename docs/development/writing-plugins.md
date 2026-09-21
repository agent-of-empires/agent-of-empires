# Writing Plugins

From nothing to an installed, running plugin. The manifest schema is in the [Plugin API Reference](../plugin-api.md); the architecture and security model are in [Plugin System Internals](internals/plugin-system.md); installing and managing plugins as a user is [Plugins](../plugins.md).

A plugin is a directory with an `aoe-plugin.toml` manifest and, optionally, a worker: an executable the host spawns that speaks JSON-RPC 2.0 over newline-delimited JSON on stdio, in any language. The host does not link your code; the manifest is the contract.

## Scaffold from the template

```sh
cookiecutter gh:agent-of-empires/plugin-template
```

The starter generates a complete plugin (manifest, worker, tests, CI) in Python, Node, or Rust. It builds, passes its tests, and answers a `status` command out of the box; the rest of this guide explains what it generated.

## The manifest

Every plugin declares identity, what it contributes, and, with a worker, how to build and launch it:

```toml
id = "dev.example.my-plugin"
name = "My Plugin"
version = "0.1.0"
api_version = 13
aoe_version = ">=1.11.0, <2.0.0"
description = "What the plugin does."

capabilities = ["runtime.worker"]

[[commands]]
id = "status"
title = "My Plugin: status"

[[settings]]
key = "enabled"
label = "Enable My Plugin"
type = "boolean"
default = true

[[ui]]
slot = "pane"
id = "my_plugin_pane"
```

Pick an `id` outside the reserved `aoe.*` and `agent-of-empires.*` namespaces, set `api_version` to the schema version you target, and set `aoe_version` to the host range you have tested.

A worker requests only the grants it uses. `runtime.worker` is required to run any code at all; add `net`, `session.read`, `notifications`, and so on as needed. Static contributions (commands, keybinds, themes, ui, status) need no capability. The user grants the exact declared set at install, pinned to the manifest hash, so an update that widens capabilities must be re-approved. Keep the list honest and minimal.

## The worker

The host spawns the worker, sends one JSON-RPC request per line on stdin, and reads one response per line on stdout; the worker exits when stdin reaches EOF.

```json
{"jsonrpc": "2.0", "id": 1, "method": "my-plugin.status", "params": {}}
{"jsonrpc": "2.0", "id": 1, "result": {"ok": true, "message": "running"}}
```

The host maps a command id to a fully namespaced method, `plugin.<id>.<command-id>`, so a worker for `dev.example.my-plugin` actually receives `plugin.dev.example.my-plugin.status`. Dispatch on the trailing segment so either form works, return a JSON-RPC error with code `-32601` for an unknown method, and never respond to a message with no `id`.

## Build and launch

The entrypoint must be **plugin-relative**, never resolved on the daemon's `PATH`. Build into `.aoe-build/`, which the host excludes from the plugin's integrity hash, then point `command` at the artifact:

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

Build steps run once, at install and update, in the user's interactive shell, where `PATH` is reliable. A compiled plugin can instead ship a release asset with `kind = "release-binary"`.

## Install and test locally

```sh
aoe plugin install ./my-plugin     # runs the build steps, prompts for grants
aoe plugin update my-plugin        # re-runs the build, re-approves changed grants
aoe plugin uninstall my-plugin
```

Drive the worker by hand before installing, to confirm the protocol:

```sh
echo '{"jsonrpc":"2.0","id":1,"method":"my-plugin.status","params":{}}' | <your-worker>
```

The starter ships a worker-contract test (spawn the worker, send a request, assert the response) plus its CI. Keep it green; it is the cheapest guard on the protocol.

## Publish

Push a `vX.Y.Z` tag to cut a GitHub release, and users install it with `aoe plugin install gh:your-org/my-plugin`. To be listed in the featured index, which lets a plugin claim a verified namespace, open a PR adding your release's source tree hash to that index in the main repository.
