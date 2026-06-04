# Apple Containers

## Overview

In addition to Docker, `aoe` supports **Apple Container** as a native sandbox runtime for macOS. Apple Containers provide a lightweight, high-performance alternative to Docker Desktop by leveraging macOS's native virtualization and containerization capabilities.

## Prerequisites

1.  **System Requirements:** You need a Mac with Apple silicon running macOS 26 (Tahoe) or later.
2.  **Apple Container CLI:** You must have the [container](https://github.com/apple/container) command-line tool installed and the daemon running.

### Installation

You can download a macOS installation `.pkg` from the [official GitHub releases page](https://github.com/apple/container/releases) or use Homebrew:

```bash
brew install container
```

### Initial Setup

After installation, you must initialize and start the background service:

```bash
# Initialize and start the daemon
container system start
```

*Note: The first time you run this, it may prompt you to download a default Linux kernel or set up initial system resources.*

### Verify Installation

Once the service is started, verify that the system is ready:

```bash
# Check version
container --version

# Check daemon status
container system status
```

Expected output for `system status` should show that the `apiserver` is running and the system is ready.

## Configuration

To switch your sandbox runtime from Docker (default) to Apple Container, update your `~/.agent-of-empires/config.toml`:

```toml
[sandbox]
container_runtime = "apple_container"
default_image = "ghcr.io/agent-of-empires/aoe-sandbox:latest"
```

### Profile-Specific Runtime

You can also create a specific profile for Apple Container if you want to switch between runtimes easily:

```toml
[profiles.apple]
sandbox.container_runtime = "apple_container"
```

Then use it with: `aoe add --profile apple .`

## Usage

Once configured, usage is identical to the Docker sandbox.

### CLI Commands

```bash
# Create an Apple Container sandboxed session
aoe add --sandbox .

# Launch with a specific image
aoe add --sandbox --sandbox-image my-custom-image:latest .
```

### TUI Integration

In the TUI, the **Sandbox** toggle will automatically use the Apple Container runtime if it is configured as your `container_runtime`.

> **Note:** The TUI will show an error if you have `container_runtime = "apple_container"` set but the `container` daemon is not running.

## Key Differences from Docker Sandbox

While `aoe` abstracts most differences, there are a few technical variations to keep in mind when using Apple Container:

### Container Memory Usage

Unlike Docker, which runs all containers in a single shared VM, each Apple Container runs in its own dedicated VM.

As of March 2026, **memory ballooning** is partially supported. This means a container will only claim the amount of host memory it actually uses (up to its limit), but it cannot currently release that memory back to the host until the container is removed or restarted.

### Volume Mounts

Apple Container does not currently support the `:ro` (read-only) flag for volume mounts. If you have `mount_ssh = true` or other read-only volumes configured, `aoe` will automatically downgrade them to read-write mounts and issue a warning in the logs.

## Troubleshooting

### Daemon Not Found

If you see `Container runtime daemon is not running`, ensure the Apple Container service is active:

```bash
container system status
```

### Image Not Found

Apple Container uses its own local image store, separate from Docker Desktop. If you have an image in Docker, you must pull or build it again for Apple Container:

```bash
container image pull ghcr.io/agent-of-empires/aoe-sandbox:latest
```
