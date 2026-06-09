# Docker Sandbox: Quick Reference

## Overview

Docker sandboxing runs your AI coding agents (Claude Code, OpenCode, Mistral Vibe, Hermes, Codex CLI, Gemini CLI, Antigravity CLI, Cursor CLI, Copilot CLI, Pi, Kiro CLI, Qwen Code) inside isolated Docker containers while maintaining access to your project files and credentials.

> **Linux users:** AoE also supports [Podman](podman.md) as a daemonless, rootless-friendly alternative to Docker.
>
> **macOS users:** AoE also supports [Apple Containers](apple-containers.md) as a native alternative to Docker Desktop.

**Key Features:**
- One container per session
- Shared authentication across containers (no re-auth needed)
- Automatic container lifecycle management
- Full project access via volume mounts

Agent credentials are shared into containers automatically, so agents authenticate without re-login. For how this works, see [Sandbox internals](../development/internals/sandbox.md).

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

## Available Images

AOE provides two official sandbox images:

| Image | Description |
|-------|-------------|
| `ghcr.io/agent-of-empires/aoe-sandbox:latest` | Base image with Claude Code, OpenCode, Mistral Vibe, Hermes, Codex CLI, Gemini CLI, Cursor CLI, Copilot CLI, Pi, Kiro CLI, Qwen Code, git, ripgrep, fzf |
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

> Building a custom image and using structured view? Install the ACP adapters too, or the handshake fails. See [Sandbox internals](../development/internals/sandbox.md).

## Worktrees and Sandboxing

Git worktrees need the bare repo pattern so the container can reach the repo's git directory. See the [Workflow Guide](workflow.md).

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
