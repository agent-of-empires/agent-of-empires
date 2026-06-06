# Podman

`aoe` supports [Podman](https://podman.io/) as a sandbox runtime. Podman is a daemonless, rootless-friendly drop-in for the Docker CLI, so once configured it behaves like the Docker sandbox (see [Docker Sandbox](sandbox.md)). The Podman-specific differences are below.

## Install

Most Linux distributions package Podman directly, e.g. `sudo dnf install podman` (Fedora/RHEL), `sudo apt install podman` (Debian/Ubuntu), or `sudo pacman -S podman` (Arch). Verify with `podman info`; AoE probes engine health the same way and reports the runtime as unavailable if that command fails.

## Configuration

Set the runtime in `~/.config/agent-of-empires/config.toml` (Linux) or `~/.agent-of-empires/config.toml` (macOS/Windows):

```toml
[sandbox]
container_runtime = "podman"
```

Scope it to a single profile to keep Docker as the global default:

```toml
[profiles.podman]
sandbox.container_runtime = "podman"
```

Use it with `aoe add --profile podman .`, or pick the runtime per-session in the TUI under **Sandbox > Container Runtime**.

## Podman-specific gotchas

- **Separate image store.** Podman maintains its own local image cache. Seed it once with `podman pull ghcr.io/agent-of-empires/aoe-sandbox:latest`, or let AoE pull on first use.
- **Rootless networking.** Published ports above 1024 work out of the box; binding a privileged port (<1024) requires rootful Podman or `sysctl net.ipv4.ip_unprivileged_port_start`.
- **`podman info` fails.** Run it directly to diagnose. Common causes: uninitialized storage (`podman system reset` destroys local images/containers) or missing `/etc/subuid`/`/etc/subgid` entries for rootless mode (usually configured on install).

### SELinux: permission denied on bind mounts

On SELinux-enforcing systems (Fedora, RHEL), the container is denied access to bind-mounted host paths because they keep their `user_home_t` label. The symptom is a blank agent pane or "Permission denied" / `?????????` even as root inside the container.

Fix it by relabeling the host paths. AoE can do this for you:

```toml
[sandbox]
selinux_relabel = true
```

This appends the `:z` SELinux relabel flag to every sandbox bind mount. It is off by default (it modifies host labels), and only Docker and Podman honor it. Alternatively, relabel manually with `chcon -R -t container_file_t <path>` (reverted by a later `restorecon`), or make it durable with `semanage fcontext`.
