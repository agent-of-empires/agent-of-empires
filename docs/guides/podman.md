# Podman

## Overview

In addition to Docker, `aoe` supports [Podman](https://podman.io/) as a sandbox runtime. Podman is a daemonless, drop-in replacement for the Docker CLI; it shares the same image format and command surface, so AoE drives it with the same code paths used for Docker.

Podman is a popular choice on Linux and in security-conscious environments because:

- It is **daemonless**, so there is no long-running root service.
- It can run **rootless**, isolating containers inside an unprivileged user namespace.
- It is shipped or easily installed in most enterprise Linux distributions without requiring Docker Desktop.

## Prerequisites

1. **Podman installed** and available on your `PATH`. Most Linux distributions package it directly:

   ```bash
   # Fedora / RHEL / CentOS Stream
   sudo dnf install podman

   # Debian / Ubuntu
   sudo apt install podman

   # Arch
   sudo pacman -S podman
   ```

2. **Local engine reachable.** AoE probes engine health with `podman info`, which returns successfully once your rootless or rootful storage is configured. If `podman info` fails on the command line, AoE will report the runtime as unavailable.

### Verify Installation

```bash
podman --version
podman info
```

`podman info` exits successfully when the local storage and configuration are healthy.

## Configuration

To switch your sandbox runtime from Docker (the default) to Podman, update `~/.config/agent-of-empires/config.toml` (Linux) or `~/.agent-of-empires/config.toml` (macOS/Windows):

```toml
[sandbox]
container_runtime = "podman"
default_image = "ghcr.io/agent-of-empires/aoe-sandbox:latest"
```

### Profile-Specific Runtime

You can scope Podman to a specific profile if you want to keep Docker as your global default:

```toml
[profiles.podman]
sandbox.container_runtime = "podman"
```

Then use it with: `aoe add --profile podman .`

You can also pick the runtime per-session in the TUI settings under **Sandbox > Container Runtime**.

## Usage

Once configured, usage is identical to the Docker sandbox. All the standard sandbox flags work unchanged:

```bash
# Create a Podman-backed sandboxed session
aoe add --sandbox .

# Launch with a specific image
aoe add --sandbox --sandbox-image my-custom-image:latest .
```

In the TUI, the **Sandbox** toggle uses whichever runtime is configured in `container_runtime`.

## Compatibility Notes

Podman is treated as a Docker-compatible runtime by AoE, which means:

- **Volume mounts** support the `:ro` read-only flag, so `mount_ssh = true` and other read-only mounts work as on Docker.
- **Anonymous volumes** are removed alongside the container when you delete a session, just like on Docker.
- **Named volumes** (e.g., shared agent auth volumes) are not affected by container removal.

A few practical differences to keep in mind:

- **Image store is separate.** Podman maintains its own local image cache. If you previously pulled images with Docker, run `podman pull ghcr.io/agent-of-empires/aoe-sandbox:latest` (or let AoE pull on first use).
- **Rootless networking.** Rootless Podman uses `slirp4netns` or `pasta` for networking by default. Published ports above 1024 work without extra configuration; binding to a privileged port (<1024) requires either rootful Podman or `sysctl net.ipv4.ip_unprivileged_port_start`.
- **Daemonless.** There is no daemon process to keep running. If you previously used `systemctl start docker`, the Podman equivalent is unnecessary.

## Troubleshooting

### `podman info` fails

If AoE reports the Podman runtime as unavailable, run `podman info` directly. Common causes:

- **Storage not initialized.** Run `podman system reset` (destroys local images and containers) or check `~/.config/containers/storage.conf`.
- **subuid/subgid not configured.** Rootless Podman requires entries in `/etc/subuid` and `/etc/subgid` for your user. Most distros configure this automatically on install.

### Image Not Found

Podman uses a local image store separate from Docker. Pull the sandbox image once to seed the cache:

```bash
podman pull ghcr.io/agent-of-empires/aoe-sandbox:latest
```

### Permission Denied on Bind Mounts

On SELinux-enforcing systems (Fedora, RHEL), the container runs as the confined `container_t` domain and is denied access to bind-mounted host paths (the credential dir, the project worktree) because they keep their `user_home_t` label. The symptom is a blank agent pane, or "Permission denied" / `?????????` even as root inside the container.

The fix is to relabel those host paths to a container-accessible type. AoE can do this for you: set

```toml
[sandbox]
selinux_relabel = true
```

which appends the `:z` (shared) SELinux relabel flag to every sandbox bind mount, so the runtime relabels the host paths to `container_file_t` at mount time. It is off by default (a no-op on non-SELinux hosts, and it modifies host labels, so it is opt-in). Only Docker and Podman honor the flag; Apple Container ignores it.

Alternatively, relabel manually with `chcon -R -t container_file_t <path>` (one-time; reverted by a later `restorecon`), or make it durable with `semanage fcontext`.
