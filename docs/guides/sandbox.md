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
| `volume_ignores` | `[]` | Directories to exclude from the project mount via anonymous volumes |
| `volume_ignores_strategy` | `"anonymous"` | How `volume_ignores` are mounted: `"anonymous"` (default) or `"named"` (required on macOS/VirtioFS, see below) |
| `extra_volumes` | `[]` | Additional volume mounts |
| `mount_ssh` | `false` | Mount `~/.ssh/` read-only into containers |
| `default_terminal_mode` | `"host"` | Paired terminal location: `"host"` (on host machine) or `"container"` (inside Docker) |

## Volume Mounts

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

### Shared Agent Config Directories

AOE shares your host agent credentials with sandboxed containers so agents can authenticate without re-login. This works for all supported agents.

Rather than bind-mounting your actual host config directories (which would let container writes modify your host files), AOE creates a **shared sandbox directory** per agent:

1. For each agent whose host config directory exists, AOE syncs credential files into a shared sandbox directory.
2. The sandbox directory is mounted read-write into **all** containers that use that agent.
3. Containers can read credentials and write runtime state freely without affecting your host config.
4. In-container changes (e.g. permission approvals, settings tweaks) persist across sessions since all containers share the same directory.
5. Sandbox directories are **never automatically deleted** -- not even when you remove all sandboxed sessions. This is intentional: if you later create a new sandbox, your accumulated state (permission approvals, settings) is still there so you don't have to set things up again.

If an agent's config directory doesn't exist on the host (e.g. you haven't installed that agent locally), AOE still creates the sandbox directory and mounts it. This way the agent can write auth and state inside the container and have it persist across sessions.

**What gets synced:**

- **Top-level files** from each agent's config directory (auth tokens, credentials, config files). Subdirectories are skipped by default to keep the sandbox dir small.
- **Specific subdirectories** listed per agent (e.g. Claude Code's `plugins/` and `skills/` are copied recursively so extensions work inside the container).
- **Seed files** (write-once) where needed (e.g. Claude Code gets a minimal `hasCompletedOnboarding` flag to skip the first-run wizard). Seed files are only written if they don't already exist, so any changes made inside the container are preserved.

**Platform-specific authentication:**

- **Linux:** Credential files (e.g. `.credentials.json`) live directly in the agent's config directory and are synced automatically.
- **macOS:** Some agents store credentials in the macOS Keychain rather than on disk. AOE extracts these at sync time and writes them as files in the sandbox directory so the container can authenticate. For example, Claude Code OAuth tokens are extracted from the Keychain and written as `.credentials.json`. If no Keychain entry is found (e.g. you authenticate via `ANTHROPIC_API_KEY`), the sandbox dir still works -- just pass your API key via the `environment` config.

**Credential refresh:** Host credentials are re-synced every time a session starts (not just on first creation). If you re-authenticate on the host or update credentials, the next session start picks up the changes. Container-specific state (permission approvals, runtime config) is not overwritten during refresh.

**Sandbox directory location:** Each agent's shared sandbox directory lives inside that agent's own config directory as a `sandbox/` subdirectory (e.g. `~/.claude/sandbox/`). All containers share this directory.

Deleting an agent's config directory removes everything related to that agent, including the sandbox directory. To reset just the sandbox state for an agent, delete its `sandbox/` subdirectory -- it will be re-created on the next session start.

**Upgrading from named volumes:** Older versions of AOE stored agent auth in named Docker volumes (e.g. `aoe-claude-auth`). On upgrade, AOE automatically migrates data from these volumes into the sandbox directories. The old volumes are intentionally **not** deleted -- you can remove them manually once you've confirmed everything works:

```bash
docker volume rm aoe-claude-auth aoe-opencode-auth aoe-codex-auth aoe-gemini-auth aoe-vibe-auth
```

## Container Naming

Containers are named: `aoe-sandbox-{session_id_first_8_chars}`

Example: `aoe-sandbox-a1b2c3d4`

## Structured view Mode Inside the Sandbox

Agent-view sessions can run inside the sandbox container. When both are enabled, the structured view runner wraps the ACP agent in `docker exec`, so the adapter binary must exist inside the container. The published `aoe-sandbox` image bundles the npm-distributed ACP adapters for this:

- `claude-agent-acp` (`@agentclientprotocol/claude-agent-acp@^0.39.0`, floor pin)
- `codex-acp` (`@zed-industries/codex-acp`)
- `pi-acp`

Native adapters that share a binary with the underlying CLI (`opencode acp`, `gemini --acp`, `vibe-acp`) work because the CLI itself is already installed in the image. If you build a **custom sandbox image**, install the same adapters or the structured view handshake will fail with `agent did not complete the ACP initialize handshake within 30s` (the agent process exits with status 127 the moment the runner exec's it).

## How It Works

1. **Session Creation:** When you add a sandboxed session, aoe records the sandbox configuration
2. **Container Start:** When you start the session, aoe creates/starts the Docker container with appropriate volume mounts
3. **tmux + docker exec:** Host tmux runs `docker exec -it <container> <tool>` to launch the selected agent
4. **Cleanup:** When you remove the session, the container is automatically deleted


## Environment Variables

These terminal-related variables are **always** passed through for proper UI/theming:
- `TERM`, `COLORTERM`, `FORCE_COLOR`, `NO_COLOR`

Pass additional variables through containers by adding them to the `environment` list. Each entry can be:

- **`KEY`** (bare name) -- passes the host env var value into the container
- **`KEY=VALUE`** -- sets an explicit value

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

### Claude on Vertex AI

If `CLAUDE_CODE_USE_VERTEX` is set on the host (and non-empty), AOE wires up Claude+Vertex sessions automatically:

- The Vertex env vars `CLAUDE_CODE_USE_VERTEX`, `ANTHROPIC_VERTEX_PROJECT_ID`, `ANTHROPIC_VERTEX_REGION`, and `CLOUD_ML_REGION` are forwarded into the container when set.
- GCP Application Default Credentials are bind-mounted read-only at the well-known container path `/root/.config/gcloud/application_default_credentials.json`. AOE uses `$GOOGLE_APPLICATION_CREDENTIALS` if set, otherwise falls back to `~/.config/gcloud/application_default_credentials.json`. `GOOGLE_APPLICATION_CREDENTIALS` itself is not forwarded; client libraries discover the well-known path automatically.

This only triggers when the active agent is `claude`; other agents (opencode, codex, etc.) get neither the vars nor the cred mount even if the host flag is set. `ANTHROPIC_API_KEY` is not auto-forwarded; if you also want it inside the container, list it in `sandbox.environment` explicitly.

### GitHub authentication with `GH_TOKEN`

Forwarding `GH_TOKEN` (e.g. `"GH_TOKEN=$GH_TOKEN"` in `sandbox.environment`) enables both `gh` and plain `git push` to authenticate against `github.com` inside the container. AOE seeds a scoped credential helper in the sandbox gitconfig that reads the token at push time; no credential is ever written to disk.

Security notes:

- The helper only fires for `https://github.com` remotes; other hosts are unaffected.
- Any process running in the sandbox can obtain the token by invoking `git credential fill`. Prefer **fine-grained** PATs limited to the specific repositories you expect the agent to push to.
- If `GH_TOKEN` is unset at push time the helper stays silent and git falls through to its normal credential flow. Unset the env var to temporarily disable sandboxed pushes without deleting the gitconfig.

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

## Worktrees and Sandboxing

When using git worktrees with sandboxing, there's an important consideration: worktrees have a `.git` file that points back to the main repository's git directory. If this reference points outside the sandboxed directory, git operations inside the container may fail.

### The Problem

With the default worktree template (`../{repo-name}-worktrees/{branch}`):

```
/projects/
  my-repo/
    .git/                    # Main repo's git directory
    src/
  my-repo-worktrees/
    feature-branch/
      .git                   # FILE pointing to /projects/my-repo/.git/...
      src/
```

When sandboxing `feature-branch/`, the container can't access `/projects/my-repo/.git/`.

### The Solution: Bare Repo Pattern

Use the linked worktree bare repo pattern to keep everything in one directory:

```
/projects/my-repo/
  .bare/                     # Bare git repository
  .git                       # FILE: "gitdir: ./.bare"
  main/                      # Worktree (main branch)
  feature/                   # Worktree (feature branch)
```

Now when sandboxing `feature/`, the container has access to the sibling `.bare/` directory.

AOE automatically detects bare repo setups and uses `./{branch}` as the default worktree path template, keeping new worktrees as siblings.

### Quick Setup

```bash
# Convert existing repo to bare repo pattern
cd my-project
mv .git .bare
echo "gitdir: ./.bare" > .git

# Or clone fresh as bare
git clone --bare git@github.com:user/repo.git my-project/.bare
cd my-project
echo "gitdir: ./.bare" > .git
git config remote.origin.fetch "+refs/heads/*:refs/remotes/origin/*"
git fetch origin
git worktree add main main
```

See the [Workflow Guide](workflow.md) for detailed bare repo setup instructions.

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
