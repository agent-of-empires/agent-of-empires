# Docker Sandbox: Quick Reference

## Overview

Docker sandboxing runs your AI coding agents (Claude Code, OpenCode, Mistral Vibe, Hermes, Codex CLI, Gemini CLI, Antigravity CLI, Cursor CLI, Copilot CLI, Pi, Oh My Pi (OMP), Kiro CLI, Qwen Code, Kimi Code, Prime Agent) inside isolated Docker containers while maintaining access to your project files and credentials.

> **Linux users:** AoE also supports [Podman](podman.md) as a daemonless, rootless-friendly alternative to Docker.
>
> **macOS users:** AoE also supports [Apple Containers](apple-containers.md) as a native alternative to Docker Desktop.

**Key Features:**
- One container per session
- Shared authentication across containers (no re-auth needed)
- Automatic container lifecycle management
- Full project access via volume mounts

Agent credentials are seeded from the host config into a private per-session
sandbox directory, so agents authenticate without re-login. Containers do not
share a writable agent store.

## CLI vs TUI Behavior

| Feature | CLI | TUI |
|---------|-----|-----|
| Enable sandbox | `--sandbox` flag | Checkbox toggle |
| Custom image | `--sandbox-image <image>` | Not supported |
| Container cleanup | Automatic on remove | Automatic on remove |
| Keep container | `--keep-container` flag | Not supported |

## One-Liner Commands

```bash
# Create sandboxed session
aoe add --sandbox .

# Create sandboxed session with custom image
aoe add --sandbox-image myregistry/custom:v1 .

# Create and launch sandboxed session
aoe add --sandbox -l .

# Remove session (auto-cleans container)
aoe remove <session>

# Remove session but keep container
aoe remove <session> --keep-container
```


**Note:** In the TUI, the sandbox checkbox only appears when Docker is available on your system.

## Default Configuration

```toml
[sandbox]
enabled_by_default = false
default_image = "ghcr.io/agent-of-empires/aoe-sandbox:latest"
auto_cleanup = true
cpu_limit = "4"
memory_limit = "8g"
environment = ["ANTHROPIC_API_KEY"]
```

> **Note:** YOLO mode (skip permission prompts) is now configured under `[session]` instead of `[sandbox]`, since it works with or without Docker sandboxing. See `[session] yolo_mode_default` in the [configuration guide](configuration.md).

## Configuration Options

| Option | Default | Description |
|--------|---------|-------------|
| `enabled_by_default` | `false` | Auto-enable sandbox for new sessions |
| `default_image` | `ghcr.io/agent-of-empires/aoe-sandbox:latest` | Docker image to use |
| `auto_cleanup` | `true` | Remove containers when sessions are deleted |
| `cpu_limit` | (none) | CPU limit (e.g., "4") |
| `memory_limit` | (none) | Memory limit (e.g., "8g") |
| `environment` | `[]` | Env vars for containers (bare KEY or KEY=VALUE, see below) |
| `volume_ignores` | `[]` | Directory paths to exclude from the project mount via anonymous volumes. Literal paths or glob patterns expanded at create time (see below) |
| `volume_ignores_strategy` | `"anonymous"` | How `volume_ignores` are mounted: `"anonymous"` (default) or `"named"` (required on macOS/VirtioFS, see below) |
| `extra_volumes` | `[]` | Additional volume mounts |
| `mount_ssh` | `false` | Mount `~/.ssh/` read-only into containers |
| `default_terminal_mode` | `"host"` | Paired terminal location: `"host"` (on host machine) or `"container"` (inside Docker) |
| `privileged` | `false` | Run the container in privileged mode (`--privileged`) |
| `cap_add` | `[]` | Capabilities granted on top of the runtime default (`--cap-add`) |
| `cap_drop` | `[]` | Capabilities removed from the runtime default (`--cap-drop`) |
| `security_opt` | `[]` | Security options (`--security-opt`, e.g. `seccomp=unconfined`) |
| `extra_run_args` | `[]` | Extra arguments passed to container run, before the image |

### Run Policy

Configure container privileges, capabilities, and security options:

```toml
[sandbox]
# Privileged mode
privileged = true

# Capabilities
cap_add = ["SYS_ADMIN"]
cap_drop = ["NET_RAW"]

# Security options
security_opt = ["seccomp=unconfined"]

# Additional arguments passed to container run
extra_run_args = ["--device", "/dev/fuse"]
```

Docker and Podman support all run-policy options. Apple Container supports `cap_add` and `cap_drop`; `privileged` and `security_opt` are ignored with a warning. `extra_run_args` is passed through on all runtimes.

## Volume Mounts

### Volume Ignores: Literal Paths and Glob Patterns

`volume_ignores` entries can be literal directory paths or glob patterns:

- A **literal path** (e.g. `node_modules`, `target`, `src/MyApp/bin`) is resolved relative to each mounted workspace root and mounted unconditionally. It need not exist yet; the anonymous volume shadows it once the directory is created.
- A **glob pattern** (containing `*`, `?`, `[`, or `]`, e.g. `**/bin`, `**/obj`) is expanded against the workspace filesystem when the session is created, and one ignore mount is created per matching directory.

```toml
[sandbox]
volume_ignores = ["node_modules", "target", "**/bin", "**/obj"]
```

> **Glob expansion is a point-in-time snapshot.** Docker needs concrete mount paths when the container starts, so a glob is expanded only against the directories that exist at create time. A `bin/` that a build creates *later*, inside the container, is **not** shadowed. Re-create the session to pick up new matches, or list the path literally if you know it ahead of time. The native TUI and the web dashboard show a one-time confirmation explaining this before creating a sandbox session whose config has a glob entry.

### Volume Ignores Strategy (macOS/VirtioFS)

By default, `volume_ignores` paths are mounted as **anonymous volumes** (`volume_ignores_strategy = "anonymous"`). This works on Linux, but on macOS with Docker Desktop's VirtioFS, anonymous volumes may not reliably shadow bind-mount subdirectories, causing host-side directories like `.venv` or `node_modules` to remain visible inside the container.

To fix this on macOS, set `volume_ignores_strategy = "named"`. This mounts each `volume_ignores` path as a **deterministic named Docker/Podman volume** stored entirely inside the Docker VM, bypassing VirtioFS. Named volumes are explicitly removed when the session is deleted.

A volume's name is derived from its mount path, so moving a session's worktree changes it. The recreated container starts from an empty cache for the paths that moved, and the volumes those paths left behind are removed the next time the session starts.

Moving a worktree therefore costs the cache in both directions: move it back and the volumes from the first location are already gone, so that side rebuilds cold too.

The reclaim covers the paths that moved with the worktree, and nothing else. A volume orphaned any other way is left alone, because AoE cannot tell it apart from a cache you are still using: dropping an entry from `volume_ignores`, switching back to `"anonymous"`, or attaching a repo to a multi-repo workspace all strand a volume without a move it can recognize. Neither is anything already orphaned, including the leftovers from a move the session has since restarted after, so a volume that has been sitting there stays. To find what is left over, list them with `docker volume ls -q --filter name=aoe-vi-` and remove what you recognize; `docker volume prune` skips named volumes unless you pass `-a`.

```toml
[sandbox]
volume_ignores = ["node_modules", ".venv", "target"]
volume_ignores_strategy = "named"
```

> Named volumes are not supported on Apple Container. Setting `"named"` on Apple Container falls back to anonymous volume behavior with a warning.

### Automatic Mounts

| Host Path | Container Path | Mode | Purpose |
|-----------|----------------|------|---------|
| Project directory | `/workspace` | RW | Your code |
| `~/.gitconfig` | `/root/.gitconfig` | RO | Git config |
| `~/.ssh/` | `/root/.ssh/` | RO | SSH keys |
| `~/.config/opencode/` | `/root/.config/opencode/` | RO | OpenCode config |

## Environment Variables

Pass variables through containers by adding them to the `environment` list. Each entry can be:

- **`KEY`** (bare name) passes the host env var value into the container
- **`KEY=VALUE`** sets an explicit value

Keys must match `[A-Za-z_][A-Za-z0-9_]*` (an ASCII letter or `_` first, then ASCII alphanumerics or `_`). Any other key is dropped with a warning; Docker accepts laxer keys than this, so `FOO-BAR=x` or `foo.bar=x` is accepted by the runtime but silently rejected here.

```toml
[sandbox]
environment = [
    "ANTHROPIC_API_KEY",                # pass through from host
    "OPENAI_API_KEY",                   # pass through from host
    "GH_TOKEN=$AOE_GH_TOKEN",          # read AOE_GH_TOKEN from host, inject as GH_TOKEN
    "CUSTOM_API_KEY=sk-sandbox-key",    # literal value
]
```

For `KEY=VALUE` entries, values starting with `$` read from a host env var. This lets you store secrets in your shell profile rather than in the AOE config file:

```bash
# In your .bashrc / .zshrc
export AOE_GH_TOKEN="ghp_sandbox_scoped_token"
```

If the referenced host env var is not set, the entry is silently skipped.

To use a literal value starting with `$`, double it: `$$LITERAL` is injected as `$LITERAL`.

## Folder Trust

Claude Code, Codex, and Gemini each refuse to start in a directory they have
not been told to trust. A container workspace is always a new directory to
them, so AoE pre-trusts it in the agent's staged config before the session
starts. Claude Code is trusted in every sandboxed session because its prompt
blocks startup; Codex and Gemini are trusted only in YOLO mode, where their
prompts guard approvals the user has already opted out of.

A repo that ships an `.mcp.json` still asks per server before any of them run.
Trust does activate the repo's own `.claude/settings.json`, whose
`permissions.allow` rules an untrusted workspace drops, and since the prompt is
what holds a session before startup, a pre-trusted workspace runs that file's
hooks unprompted.

AoE seeds the config it stages for the container. For a custom agent whose
wrapper points the CLI at another directory, set that host root in
`session.agent_config_dir`. AoE stages a private per-session child and mounts it
at the agent's canonical container config path. Remove any
`sandbox.extra_volumes` entry for that path because it would shadow AoE's
managed mount.

To pre-trust worktrees for host sessions too, see `session.pre_trust_agent_folders`
in the [configuration guide](configuration.md).

## Available Images

AOE provides two official sandbox images:

| Image | Description |
|-------|-------------|
| `ghcr.io/agent-of-empires/aoe-sandbox:latest` | Base image with Claude Code, OpenCode, Mistral Vibe, Hermes, Codex CLI, Gemini CLI, Cursor CLI, Copilot CLI, Pi, Oh My Pi (OMP), Kiro CLI, Qwen Code, Kimi Code, Prime Agent, git, ripgrep, fzf |
| `ghcr.io/agent-of-empires/aoe-dev-sandbox:latest` | Extended image with additional dev tools |

### Dev Sandbox Tools

The dev sandbox (`aoe-dev-sandbox`) includes everything in the base image plus:

- **Rust** (rustup, cargo, rustc)
- **uv** (fast Python package manager)
- **Node.js LTS** (via nvm, with npm and npx)
- **GitHub CLI** (gh)

To use the dev sandbox:

```bash
# Per-session
aoe add --sandbox-image ghcr.io/agent-of-empires/aoe-dev-sandbox:latest .

# Or set as default in ~/.agent-of-empires/config.toml
[sandbox]
default_image = "ghcr.io/agent-of-empires/aoe-dev-sandbox:latest"
```

## Custom Docker Images

The default sandbox image includes all supported agents, git, and basic development tools. For projects requiring additional dependencies beyond what the dev sandbox provides, you can extend either base image.

### Step 1: Create a Dockerfile

Create a `Dockerfile` in your project (or a shared location):

```dockerfile
FROM ghcr.io/agent-of-empires/aoe-sandbox:latest

# Example: Add Python for a data science project
RUN apt-get update && apt-get install -y \
    python3 \
    python3-pip \
    python3-venv \
    && rm -rf /var/lib/apt/lists/*

# Install Python packages
RUN pip3 install --break-system-packages \
    pandas \
    numpy \
    requests
```

### Step 2: Build Your Image

```bash
# Build locally
docker build -t my-sandbox:latest .

# Or build and push to a registry
docker build -t ghcr.io/yourusername/my-sandbox:latest .
docker push ghcr.io/yourusername/my-sandbox:latest
```

### Step 3: Configure AOE to Use Your Image

**Option A: Set as default for all sessions**

Add to `~/.agent-of-empires/config.toml`:

```toml
[sandbox]
default_image = "my-sandbox:latest"
# Or with registry:
# default_image = "ghcr.io/yourusername/my-sandbox:latest"
```

**Option B: Use per-session via CLI**

```bash
aoe add --sandbox-image my-sandbox:latest .
```

> Building a custom image and using structured view? Install the agent's ACP
> adapter in the image too, or the handshake fails.

## Per-session agent stores

Each sandboxed session gets its own agent store on the host: a copy of the
agent's config and history under `sandbox-v2/<instance-id>` inside the agent's config directory
(for example `~/.claude/sandbox-v2/<id>`). The container mounts that copy at
the agent's usual config path, so credentials, hooks and conversation history
belong to one session and `aoe` can resume the right conversation.

Sessions created before this layout shared one agent store per agent (for
example `~/.claude/sandbox`). Each one moves when you start it: AoE copies the
shared store into that session's private directory, removes its stopped
container so the next launch mounts the copy, and deletes the shared store once
every session that used it has moved (the private copies are the data from then
on). A large store takes a while, so the first start of a session is slower
than usual. A session whose container is still running is skipped and moved on
a later start, after it stops. Trashed and archived sessions stay on the shared
store. Starting one moves it; restoring or unarchiving alone does not, so run
`aoe migrate` afterwards if you want it moved before its next start.

To move every eligible session at once instead of paying for each at its next
start:

```bash
aoe migrate
```

This skips trashed and archived sessions, which keep their shared store until
one of them is started or brought back.

`AOE_DEFER_SANDBOX_MIGRATION=1` skips the move for that launch. A session whose
container is still running carries on unaffected, on the shared store. One
whose container is stopped cannot start until its store has moved, so drop the
variable or run `aoe migrate` before launching it.

## Worktrees and Sandboxing

Git worktrees need the bare repo pattern so the container can reach the repo's git directory. See [Worktrees](worktrees.md#bare-repos).

## Troubleshooting

### Container killed due to memory (OOM)

**Symptoms:** Your sandboxed session exits unexpectedly, the container disappears, or you see "Killed" in the output. Running `docker inspect <container>` shows `OOMKilled: true`.

**Cause:** On macOS (and Windows), Docker runs inside a Linux VM with a fixed memory ceiling. Docker Desktop defaults to 2 GB for the entire VM. If a container tries to use more memory than the VM has available, the Linux OOM killer terminates it. This commonly happens with AI coding agents that load large language model contexts or process big codebases.

**Fix:**

1. **Increase Docker Desktop VM memory:**
   Open Docker Desktop, go to **Settings > Resources > Advanced**, increase the **Memory** slider (8 GB+ recommended for AI coding agents), then click **Apply & Restart**.

2. **Set a per-container memory limit** in your AOE config (`~/.agent-of-empires/config.toml`) so containers have an explicit allocation rather than competing for the VM's total memory:

   ```toml
   [sandbox]
   memory_limit = "8g"
   ```

   The per-container limit must be less than or equal to the Docker Desktop VM memory. If you set `memory_limit = "8g"` but your VM only has 4 GB, the container will still be OOM-killed.

3. **Verify the fix:** Start a new session and check the container's limit:

   ```bash
   docker stats --no-stream
   ```

   The `MEM LIMIT` column should reflect your configured value.

**Note:** On Linux, Docker runs natively without a VM, so the memory ceiling is your host's physical RAM. You typically only need `memory_limit` on Linux to prevent a single container from consuming all system memory.

### Sandboxed OpenCode fails to start after a container-image upgrade

**Symptoms:** After bumping the sandbox container image, launching or resuming a sandboxed OpenCode session fails on boot with a drizzle or SQLite migration error (e.g. `no such column`, `CREATE TABLE ... already exists`, `Failed to run the query`). Non-sandboxed OpenCode sessions on the same host are unaffected.

**Cause:** The sandboxed OpenCode SQLite database at `~/.local/share/opencode/sandbox/opencode.db{,-wal,-shm}` persists across sessions so that `aoe resume` keeps `ses_*` identity working across kills and daemon restarts (see #2605). OpenCode manages its schema with drizzle, and drizzle migrations are forward-only. If the new image ships an OpenCode release whose forward migrations are not compatible with the pre-existing DB, OpenCode aborts on boot. This is the same failure class users hit on standalone OpenCode CLI upgrades (see upstream anomalyco/opencode#31119 and anomalyco/opencode#16678); the sandbox-image bump is just another version-change vector.

**Fix:** Delete the sandboxed OpenCode DB and restart the session. Only the sandboxed OpenCode chat history is lost; host OpenCode state (outside the `sandbox/` subdir) is untouched.

```bash
rm -f ~/.local/share/opencode/sandbox/opencode.db*
```

**Note:** This is a rare event tied to breaking schema changes in OpenCode releases; most upgrades migrate cleanly. If you hit it repeatedly on the same image bump, please file an issue upstream at [anomalyco/opencode](https://github.com/anomalyco/opencode) with the migration error output.
