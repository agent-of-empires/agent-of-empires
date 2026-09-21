# Container Sandbox

A sandboxed session runs its agent in its own container with the project mounted at `/workspace`, so the agent reaches your code but not the rest of your machine. Agent credentials are seeded into a private per-session store, so you do not log in again.

Docker is the default runtime; [Podman](#podman) and [Apple Container](#apple-container) work too.

## Creating one

```bash
aoe add --sandbox .                              # default image
aoe add --sandbox-image myregistry/custom:v1 .   # custom image
aoe remove <session>                             # also removes the container
aoe remove <session> --keep-container            # leave the container behind
```

In the TUI and the web wizard, tick the sandbox toggle when creating a session. The TUI shows it only when a container runtime is available, and cannot set a per-session image.

## Configuration

```toml
[sandbox]
enabled_by_default = false
default_image = "ghcr.io/agent-of-empires/aoe-sandbox:latest"
auto_cleanup = true
cpu_limit = "4"
memory_limit = "8g"
environment = ["ANTHROPIC_API_KEY"]
```

| Option | Default | Description |
|--------|---------|-------------|
| `enabled_by_default` | `false` | Auto-enable the sandbox for new sessions |
| `default_image` | `aoe-sandbox:latest` | Image to run |
| `container_runtime` | `docker` | `docker`, `podman`, or `apple_container`. Global only; a profile or repo override is ignored |
| `auto_cleanup` | `true` | Remove the container when the session is deleted |
| `cpu_limit` / `memory_limit` | (none) | Per-container limits, e.g. `"4"` and `"8g"` |
| `environment` | `[]` | Env vars for the container (`KEY` or `KEY=VALUE`, see below) |
| `volume_ignores` | `[]` | Paths excluded from the project mount (see [Volume ignores](#volume-ignores)) |
| `volume_ignores_strategy` | `"anonymous"` | `"anonymous"` or `"named"` (see [Volume ignores](#volume-ignores)) |
| `extra_volumes` | `[]` | Additional volume mounts |
| `port_mappings` | `[]` | Published ports, e.g. `"3000:3000"` |
| `mount_ssh` | `false` | Mount `~/.ssh/` read-only |
| `selinux_relabel` | `false` | Append `:z` to every bind mount (Docker and Podman only) |
| `default_terminal_mode` | `"host"` | Where the paired terminal runs: `"host"` or `"container"` |
| `privileged` | `false` | Run privileged (`--privileged`) |
| `cap_add` / `cap_drop` | `[]` | Capabilities added or dropped |
| `security_opt` | `[]` | Security options, e.g. `seccomp=unconfined` |
| `extra_run_args` | `[]` | Extra arguments passed to container run, before the image |

Docker and Podman honor every run-policy option. Apple Container honors `cap_add` / `cap_drop` and ignores `privileged` and `security_opt` with a warning; `extra_run_args` is passed through everywhere.

YOLO mode lives under `[session] yolo_mode_default`, not `[sandbox]`, since it works with or without a sandbox. See the [configuration reference](configuration.md).

### Automatic mounts

| Host path | Container path | Mode |
|-----------|----------------|------|
| Project directory | `/workspace` | RW |
| `~/.gitconfig` | `/root/.gitconfig` | RO |
| `~/.ssh/` (with `mount_ssh`) | `/root/.ssh/` | RO |
| The session's artifact dir | `/aoe/artifacts` | RW |
| `<agent config>/sandbox-v2/<instance-id>/` | the agent's config path, e.g. `/root/.claude/` | RW |
| `~/.claude/sandbox-v2/.credentials.json` | `/root/.claude/.credentials.json` | RW |

The last two are the [per-session agent store](#per-session-agent-stores) and the [shared Claude credential](#shared-credentials).

### Environment variables

Each `environment` entry is a bare `KEY` (pass the host value through) or `KEY=VALUE`. A value starting with `$` reads a host variable (`$$` escapes a literal `$`), so secrets can live in your shell profile rather than in `config.toml`; an unset variable is skipped. Keys must match `[A-Za-z_][A-Za-z0-9_]*`; anything else is dropped with a warning.

```toml
[sandbox]
environment = [
    "ANTHROPIC_API_KEY",              # pass through from the host
    "GH_TOKEN=$AOE_GH_TOKEN",         # read AOE_GH_TOKEN, inject as GH_TOKEN
    "CUSTOM_API_KEY=sk-sandbox-key",  # literal value
]
```

To mint a short-lived value on the host at launch instead, use [`host_hooks.before_start`](configuration.md#host-hooks). Host (non-sandboxed) sessions read the top-level [`environment`](configuration.md#host-environment) list instead; the two are disjoint.

### Volume ignores

`volume_ignores` keeps build output out of the project mount. An entry is either a literal path resolved against each workspace root, or a glob (`**/bin`) expanded when the session is created.

```toml
[sandbox]
volume_ignores = ["node_modules", "target", "**/bin"]
```

A glob is a point-in-time snapshot: a directory a build creates later inside the container is not shadowed, so list it literally or recreate the session. Both the TUI and the dashboard confirm this once before creating a sandboxed session whose config uses a glob.

On macOS, Docker Desktop's VirtioFS does not reliably shadow bind-mount subdirectories with anonymous volumes, so host directories such as `.venv` stay visible in the container. Set `volume_ignores_strategy = "named"` there: each path becomes a deterministic named volume inside the Docker VM, removed when the session is deleted. Apple Container has no named volumes and falls back to anonymous with a warning.

A named volume is keyed on its mount path, so moving a session's worktree starts those caches cold, and only the volumes left behind by that move are reclaimed. Volumes orphaned any other way (dropping a `volume_ignores` entry, switching back to `"anonymous"`, attaching a repo to a workspace) are left alone, because AoE cannot tell them from a cache you still use: list them with `docker volume ls -q --filter name=aoe-vi-` and remove what you recognize.

## Images

| Image | Contents |
|-------|----------|
| `ghcr.io/agent-of-empires/aoe-sandbox:latest` | Every supported agent CLI and its ACP adapter, git, ripgrep, fzf |
| `ghcr.io/agent-of-empires/aoe-dev-sandbox:latest` | The base image plus Rust, uv, Node LTS, and the GitHub CLI |

Extend either one for project dependencies:

```dockerfile
FROM ghcr.io/agent-of-empires/aoe-sandbox:latest
RUN apt-get update && apt-get install -y python3 python3-pip \
    && rm -rf /var/lib/apt/lists/*
```

Then set `[sandbox] default_image`, or pass `aoe add --sandbox-image my-sandbox:latest .`. A custom image used with the structured view must also carry the agent's ACP adapter, or the handshake fails.

## Folder trust

Claude Code, Codex, and Gemini refuse to start in a directory they have not been told to trust, and a container workspace is always new to them, so AoE pre-trusts it in the config it stages for the container. Claude Code is trusted in every sandboxed session (its prompt blocks startup); Codex and Gemini only in YOLO mode.

Trust activates the repo's own `.claude/settings.json`, including its hooks, so a pre-trusted workspace runs them unprompted, while a repo's `.mcp.json` still asks per server. For host sessions, see `session.pre_trust_agent_folders` in the [configuration reference](configuration.md#session).

For a custom agent whose wrapper points the CLI at another directory, name that host root in `session.agent_config_dir`; AoE stages a per-session child of it and mounts it at the agent's canonical container path. Do not add an `extra_volumes` entry for that path, which would shadow the managed mount (AoE warns when one does), and leave the config-dir variables AoE sets in place inside the container.

## Per-session agent stores

Each sandboxed session gets its own copy of the agent's config and history under `sandbox-v2/<instance-id>` in the agent's config directory (for example `~/.claude/sandbox-v2/<id>`), mounted at the agent's usual config path, so credentials, hooks and conversation history belong to one session.

Sessions created under the older shared-store layout move to a private store the next time they start, which makes that first start slower; the TUI shows the progress. `aoe migrate` moves every eligible session at once, and `AOE_DEFER_SANDBOX_MIGRATION=1` skips the move for one launch (a session whose container is stopped then cannot start until its store has moved). Trashed and archived sessions keep the shared store until they are started again.

### Shared credentials

Claude Code rotates its refresh token on every refresh and invalidates the old one, so a per-session copy of `.credentials.json` would log itself out as soon as another container refreshed. Every Claude Code session therefore mounts the single `sandbox-v2/.credentials.json`, and a refresh or login in any container is seen by all of them.

A file holding no credential is seeded at the next start from the freshest of the macOS Keychain entry, `~/.claude/.credentials.json`, and any copy left in the session's store. Once it holds one, the host's copy never replaces it: from the first refresh on, the sandboxes are a credential chain of their own. Claude Code empties both tokens in place when its credential fails to authenticate, which reaches every sandbox through the shared mount and makes the file eligible for seeding again. To re-seed from a new host login, stop the sandboxes, delete `sandbox-v2/.credentials.json`, and start one. Running `/logout` inside a sandbox revokes the token for all of them.

### Reclaiming stores

Permanently deleting a sandboxed session removes its store with its container. Stores stranded otherwise (a delete that kept the container, or one that failed part-way) are found by their instance id resolving in no profile:

```bash
aoe sandbox reclaim            # report what would go, and how much it frees
aoe sandbox reclaim --delete   # remove it
```

Reporting is the default because a store holds a copy of the agent's credentials. A store is kept while any runtime still has a container for it, while a runtime cannot be asked, and for fifteen minutes after its last write, since a store is seeded before the session that owns it is recorded.

## Worktrees

Git worktrees need the bare repo pattern so the container can reach the repo's git directory. See [Worktrees](worktrees.md#bare-repos).

## Podman

[Podman](https://podman.io/) is a daemonless, rootless-friendly drop-in for the Docker CLI. Install it from your distribution (`sudo dnf install podman`, `sudo apt install podman`, `sudo pacman -S podman`), check `podman info` (AoE probes engine health the same way), then:

```toml
[sandbox]
container_runtime = "podman"
```

- **Separate image store.** Seed it with `podman pull ghcr.io/agent-of-empires/aoe-sandbox:latest`, or let AoE pull on first use.
- **Rootless networking.** Ports above 1024 work; a privileged port needs rootful Podman or `sysctl net.ipv4.ip_unprivileged_port_start`.
- **`podman info` fails.** Usually uninitialized storage (`podman system reset` destroys local images and containers) or missing `/etc/subuid` and `/etc/subgid` entries for rootless mode.
- **SELinux denies bind mounts** (blank agent pane, "Permission denied", or `?????????` inside the container) because host paths keep their `user_home_t` label. Set `selinux_relabel = true` to append `:z` to every sandbox mount, or relabel by hand with `chcon -R -t container_file_t <path>` (`semanage fcontext` to make it durable).

## Apple Container

[Apple Container](https://github.com/apple/container) is a native macOS runtime built on Apple virtualization. It needs Apple silicon and macOS 26 (Tahoe) or later:

```bash
brew install container   # or the .pkg from the GitHub releases page
container system start   # may prompt to download a Linux kernel
container system status  # apiserver running, system ready
```

```toml
[sandbox]
container_runtime = "apple_container"
```

The TUI sandbox toggle uses this runtime automatically and reports an error when the `container` daemon is not running.

- **Per-VM memory.** Each container runs in its own VM and cannot release claimed host memory until it is restarted or removed.
- **No read-only mounts.** `:ro` is unsupported, so read-only volumes (including `mount_ssh`) are downgraded to read-write with a log warning. Named volumes fall back to anonymous ones.
- **Separate image store.** Pull with `container image pull ghcr.io/agent-of-empires/aoe-sandbox:latest`.

## Troubleshooting

### Container killed (OOM)

A sandboxed session exits, the container disappears, and `docker inspect <container>` reports `OOMKilled: true`. On macOS, Docker runs in a Linux VM with a fixed memory ceiling (2 GB by default) and the kernel kills containers that exceed it.

Raise the VM memory in Docker Desktop under **Settings > Resources > Advanced** (8 GB or more for coding agents), and set `[sandbox] memory_limit` so each container has an explicit allocation no larger than the VM. `docker stats --no-stream` shows the limit a running container got. On Linux there is no VM, so a limit is only needed to stop one container from taking all the host's RAM.

### Sandboxed OpenCode fails after an image upgrade

A sandboxed OpenCode session fails on boot with a drizzle or SQLite migration error (`no such column`, `CREATE TABLE ... already exists`) after the sandbox image ships a new OpenCode release whose forward-only migrations do not fit the existing database. Host sessions are unaffected.

Delete that session's sandboxed database and restart it; only the sandboxed chat history is lost, and host OpenCode state is untouched. The database lives in the session's [agent store](#per-session-agent-stores), under `~/.local/share/opencode/sandbox-v2/<instance-id>/`.

```bash
rm -f ~/.local/share/opencode/sandbox-v2/<instance-id>/opencode.db*
```
