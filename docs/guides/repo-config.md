# Repository Configuration & Hooks

AoE supports per-repo configuration via a `.agent-of-empires/config.toml` file in your project root. This lets you define project-specific defaults and hooks that apply to every team member using AoE on that repo.

## Getting Started

Generate a template config:

```bash
aoe init
```

This creates `.agent-of-empires/config.toml` with commented-out examples. Edit the file to enable the settings you need.

> **Migrating from `.aoe/`?** AoE still reads the legacy `.aoe/config.toml` path, but we recommend renaming it: `mv .aoe .agent-of-empires`. If both exist, `.agent-of-empires/` takes priority.

## Configuration Sections

### Hooks

Hooks run shell commands at specific points in the session lifecycle.

```toml
[hooks]
# Run once when a session is first created (failures abort creation)
on_create = ["npm install", "cp .env.example .env"]

# Run every time a session starts (failures are logged but non-fatal)
on_launch = ["npm install"]

# Run when a session is deleted, before cleanup (failures are logged but non-fatal)
on_destroy = ["docker-compose down"]
```

For single commands, you can use a plain string instead of an array:

```toml
[hooks]
on_launch = "npm install"
```

**`on_create`** runs only once, when the session is first created. If any command fails, session creation is aborted. Use this for one-time setup like installing dependencies or generating config files.

**`on_launch`** runs every time a session starts (including the first time, and every restart). Failures are logged as warnings but don't prevent the session from starting. Use this for things like ensuring dependencies are up to date.

**`on_destroy`** runs when a session is deleted, before worktree and sandbox cleanup. This lets teardown commands access resources that are still available (e.g. running containers). Failures are logged as warnings but never prevent deletion. Use this for cleanup like stopping Docker services or removing temporary resources.

For sandboxed sessions, hooks run inside the Docker container.

#### Available environment variables

Lifecycle hooks (`on_create`, `on_launch`, `on_destroy`) receive session metadata as environment variables:

| Variable | Value |
| --- | --- |
| `AOE_SESSION_ID` | Stable session identifier (e.g. `s_abc123`). |
| `AOE_SESSION_TITLE` | Session title (also the worktree branch name). |
| `AOE_PROJECT_PATH` | Absolute path to the working directory. Equals `$PWD` inside `on_create` / `on_launch`. |
| `AOE_PROFILE` | Resolved profile name. |
| `AOE_TOOL` | Agent tool name (e.g. `claude`, `codex`). |
| `AOE_GROUP_PATH` | Group path for grouped sessions; empty otherwise. |
| `AOE_SESSION_BRANCH` | Git branch name. Set only for worktree sessions. |

Quote any expansion that may contain spaces (titles often do):

```toml
[hooks]
on_create = ["port \"$AOE_SESSION_TITLE\""]
on_destroy = ["port rm \"$AOE_SESSION_TITLE\""]
```

Container hooks get the same variables (forwarded via `docker exec -e`). Status-transition hooks in `[status_hooks]` add `AOE_OLD_STATUS`, `AOE_NEW_STATUS`, and `AOE_STATUS_CHANGED_AT`.

### Session

```toml
[session]
default_tool = "opencode"   # Override the default agent for this repo
```

Any supported agent name (run `aoe add --help` to see the list).

### Sandbox

Override sandbox settings for this repo:

```toml
[sandbox]
enabled_by_default = true
default_image = "ghcr.io/agent-of-empires/aoe-dev-sandbox:latest"
environment = ["NODE_ENV", "DATABASE_URL", "CUSTOM_KEY=value"]
volume_ignores = ["node_modules", ".next", "target"]
extra_volumes = ["/data:/data:ro"]
cpu_limit = "8"
memory_limit = "16g"
auto_cleanup = true
default_terminal_mode = "host"   # "host" or "container"
```

List fields (`environment`, `volume_ignores`, `extra_volumes`, `port_mappings`) accept either an array or a single string:

```toml
[sandbox]
environment = "ANTHROPIC_API_KEY"          # single value
volume_ignores = ["node_modules", ".next"] # multiple values
```

### Worktree

Override worktree settings for this repo:

```toml
[worktree]
enabled = true
path_template = "../{repo-name}-worktrees/{branch}"
bare_repo_path_template = "./{branch}"
auto_cleanup = true
show_branch_in_tui = true
delete_branch_on_cleanup = false
```

## Hook Trust System

When AoE encounters hooks in a repo for the first time, it prompts you to review and approve them before execution. This prevents untrusted repos from running arbitrary commands.

- Trust decisions are stored globally (shared across all profiles)
- If hook commands change (e.g., someone updates `.agent-of-empires/config.toml`), AoE prompts for re-approval
- Use `--trust-hooks` with `aoe add` to skip the trust prompt (useful for CI or repos you control)

```bash
# Trust hooks automatically
aoe add --trust-hooks .
```

## Config Precedence

Settings are resolved in this order (later overrides earlier):

1. **Global config** (`~/.agent-of-empires/config.toml`)
2. **Profile config** (`~/.agent-of-empires/profiles/<name>/config.toml`)
3. **Repo config** (`.agent-of-empires/config.toml`)

Only settings that are explicitly set in the repo config override the global/profile values. Unset fields inherit from the higher-level config.

## Example: Full Repo Config

```toml
[hooks]
on_create = ["npm install", "npx prisma generate"]
on_launch = ["npm install"]
on_destroy = ["docker-compose down"]

[session]
default_tool = "claude"

[sandbox]
enabled_by_default = true
default_image = "ghcr.io/agent-of-empires/aoe-dev-sandbox:latest"
environment = ["DATABASE_URL", "REDIS_URL", "NODE_ENV=development"]
volume_ignores = ["node_modules", ".next"]

[worktree]
enabled = true
```

## Checking Into Version Control

The `.agent-of-empires/config.toml` file is meant to be committed to your repo so the entire team shares the same configuration. The hook trust system ensures that each developer explicitly approves hook commands before they run.
