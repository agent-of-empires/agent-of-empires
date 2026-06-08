# Apple Containers

`aoe` supports **Apple Container** as a native macOS sandbox runtime, a lightweight alternative to Docker Desktop built on macOS virtualization. Once configured it behaves like the Docker sandbox (see [Docker Sandbox](sandbox.md)). The Apple-Container-specific differences are below.

## Install

Requires a Mac with Apple silicon running macOS 26 (Tahoe) or later and the [container](https://github.com/apple/container) CLI.

```bash
brew install container   # or download the .pkg from the GitHub releases page
container system start   # initialize and start the daemon
```

The first `container system start` may prompt to download a default Linux kernel. Verify with `container system status`, which should report the `apiserver` running and the system ready.

## Configuration

Set the runtime in `~/.agent-of-empires/config.toml`:

```toml
[sandbox]
container_runtime = "apple_container"
```

Scope it to a single profile to keep Docker as the global default:

```toml
[profiles.apple]
sandbox.container_runtime = "apple_container"
```

Use it with `aoe add --profile apple .`. The TUI **Sandbox** toggle uses this runtime automatically, and shows an error if the `container` daemon is not running.

## Apple-Container-specific gotchas

- **Per-VM memory.** Each Apple Container runs in its own dedicated VM (Docker shares one VM across containers). As of March 2026, memory ballooning is partial: a container claims only the host memory it uses (up to its limit) but cannot release it back until the container is removed or restarted.
- **No read-only mounts.** Apple Container does not support the `:ro` flag. If `mount_ssh = true` or other read-only volumes are configured, `aoe` downgrades them to read-write and warns in the logs. Named volumes are also unsupported and fall back to anonymous volumes.
- **Separate image store.** Pull the image into Apple Container's own store with `container image pull ghcr.io/agent-of-empires/aoe-sandbox:latest`.
