# Configuration Reference

AoE uses a layered configuration system. Settings are resolved in this order:

1. **Global config**: `~/.agent-of-empires/config.toml` (or `~/.config/agent-of-empires/config.toml` on Linux)
2. **Profile config**: `~/.agent-of-empires/profiles/<name>/config.toml`
3. **Repo config**: `.agent-of-empires/config.toml` in the project root

Later layers override earlier ones. Only explicitly set fields override; unset fields inherit from the previous layer.

All settings below can also be edited from the TUI settings screen (press `s` or access via the menu).

## File Locations

| Platform | Global Config |
|----------|--------------|
| Linux | `$XDG_CONFIG_HOME/agent-of-empires/config.toml` (defaults to `~/.config/agent-of-empires/`) |
| macOS | `~/.agent-of-empires/config.toml` by default, or `$XDG_CONFIG_HOME/agent-of-empires/config.toml` when you opt into the XDG layout (see below) |

On macOS, AoE reads from `$XDG_CONFIG_HOME/agent-of-empires/` (e.g. `~/.config/agent-of-empires/`) when you set `XDG_CONFIG_HOME`, or whenever that directory already exists, so a dotfile manager like chezmoi can share one config path with Linux. Otherwise it uses `~/.agent-of-empires/`. Nothing is moved automatically: an existing `~/.agent-of-empires/` keeps being used even after you set `XDG_CONFIG_HOME`, until you relocate it yourself.

```
~/.agent-of-empires/
  config.toml              # Global configuration
  trusted_repos.toml       # Hook trust decisions (auto-managed)
  .schema_version          # Migration tracking (auto-managed)
  profiles/
    default/
      sessions.json        # Session data
      groups.json          # Group hierarchy
      config.toml          # Profile-specific overrides
  logs/                    # Session execution logs
```

## Environment Variables

| Variable | Description |
|----------|-------------|
| `AGENT_OF_EMPIRES_PROFILE` | Default profile to use |
| `AGENT_OF_EMPIRES_DEBUG` | Enable debug logging to `debug.log` in app data dir (`1` to enable). Legacy alias for `AOE_LOG_LEVEL=debug`. |
| `AOE_LOG_LEVEL` | File log level: `trace`, `debug`, `info`, `warn`, `error`. |

## Theme

```toml
[theme]
name = "default"   # default, empire, phosphor, tokyo-night-storm, catppuccin-latte, dracula, rose-pine, deep-ocean
color_mode = "truecolor"   # truecolor | palette (TUI only)
```

| Option | Default | Description |
|--------|---------|-------------|
| `name` | `"default"` | Color theme. Applies to **both the TUI and the web dashboard**. Available builtins: `default` (neutral zinc/amber), `empire` (warm navy/copper), `phosphor` (green), `tokyo-night-storm` (dark blue/purple), `catppuccin-latte` (light pastel), `dracula` (dark purple/pink), `rose-pine` (dark muted purple/pink), `deep-ocean` (Material Theme Deep Ocean, dark navy/cyan). Custom TOML themes in `~/.agent-of-empires/themes/*.toml` also appear in the picker. An empty `name` resolves to `default`. |
| `color_mode` | `"truecolor"` | TUI only. `palette` downsamples to xterm-256 for transports that mangle 24-bit RGB (e.g. some `mosh` setups). The web dashboard always renders truecolor. |

### Custom themes

Drop a TOML file in `~/.agent-of-empires/themes/<name>.toml` (or `$XDG_CONFIG_HOME/agent-of-empires/themes/` on Linux). The file appears in the theme picker under its filename stem. Export a builtin as a starting point:

```bash
aoe theme export empire             # writes ~/.agent-of-empires/themes/custom-empire.toml
aoe theme export dracula -o my.toml # writes to my.toml
aoe theme list                      # show all available themes
aoe theme dir                       # print the custom themes directory
```

The schema is flat and every field is optional. Missing color fields fall back to the Empire baseline; an omitted `appearance` or `[syntax].shiki_theme` is derived from the theme's background luminance rather than copied from Empire. Color fields cover background, borders, text, status semantics, diff colors, branch/sandbox chips, and accent. `appearance = "dark" | "light"` and `[syntax].shiki_theme` control the web dashboard's surface ramp and code-block syntax theme.

## Session

```toml
[session]
default_tool = "claude"   # any supported agent name
yolo_mode_default = false
agent_status_hooks = true
smart_rename = true
smart_rename_agent = ""    # "" = use the session's own agent; e.g. "codex"
auto_stop_idle_secs = 0   # 0 disables; e.g. 7200 = stop after 2h idle

[session.acp_defaults.opencode]
model = "openai/gpt-5.5"
effort = "high"
```

| Option | Default | Description |
|--------|---------|-------------|
| `default_tool` | (auto-detect) | Default agent for new sessions. Falls back to the first available tool if unset or unavailable. Can be set to a custom agent name. |
| `auto_stop_idle_secs` | `0` | Seconds a plain tmux session may sit `Idle` before it is auto-stopped: its tmux session and any sandbox container are killed, leaving a restartable `Stopped` row. `0` disables it; no session is ever auto-stopped for inactivity. Idle age is measured from the later of the last transition into `Idle` and the last user interaction, and a session with an attached tmux client is always spared, so a session you are reading is never reaped. Evaluated about once a minute (by the TUI and by `aoe serve`), so the stop can lag the threshold by up to a minute. Structured view workers use the separate `acp.auto_stop_idle_secs`. See #1689 and #1690. |
| `yolo_mode_default` | `false` | Enable YOLO mode by default for new sessions (skip permission prompts). Works with or without sandbox. In tmux mode this passes `--dangerously-skip-permissions` to the agent CLI; in structured view it maps to ACP `bypassPermissions` (see [Structured view: Permission modes and YOLO](../structured-view/controls.md#permission-modes-and-yolo) for the adapter caveat). |
| `agent_status_hooks` | `true` | Install status-detection hooks into the agent's config file. Codex uses the `[hooks]` table in its resolved `config.toml` (typically `~/.codex/config.toml`); other JSON-based agents use their settings JSON. Config-dir overrides are honored: `CODEX_HOME` (Codex), `CLAUDE_CONFIG_DIR` (Claude), or `CURSOR_CONFIG_DIR` (Cursor) set in the session's profile environment or in AoE's own environment redirects hooks to that directory instead of the `~/.codex` / `~/.claude` / `~/.cursor` default. When disabled, status detection falls back to tmux pane content parsing. Codex is hook-first, but known hook gaps are reconciled from pane content. |
| `smart_rename` | `true` | Auto-rename a new structured view (ACP) session from its first message, using the session's own agent in one-shot mode (`claude -p`, `codex exec`, `opencode run`, `gemini -p`). Runs only while the session still carries its auto-generated civilization name; a manually named session is never touched. Title only: the worktree directory is not moved, since the running agent holds it. Skipped for sandboxed sessions (a host one-shot lacks the container's auth), agents with no one-shot mode, and command-overridden agents. Best-effort: a failed or timed-out call leaves the generated name and never affects the prompt. |
| `smart_rename_agent` | `""` | Agent used for the one-shot smart-rename title call. Empty means use the session's own agent. Set it to a different one-shot-capable agent (`claude`, `codex`, `opencode`, `gemini`) to point rename at a cheaper or more obedient title model without changing the session's working agent. An unknown or one-shot-incapable value leaves the generated name. |
| `agent_extra_args` | `{}` | Per-agent extra arguments appended after the binary (e.g., `{ opencode = "--port 8080" }`). |
| `agent_command_override` | `{}` | Per-agent command override replacing the binary entirely (e.g., `{ claude = "my-claude-wrapper" }`). |
| `custom_agents` | `{}` | User-defined agents: name to command mapping. Custom agent names appear in the TUI agent picker alongside built-in agents. |
| `agent_detect_as` | `{}` | Status detection mapping: maps an agent name to a built-in agent whose status heuristics should be used. |
| `agent_acp_cmd` | `{}` | ACP launch command for a custom agent, enabling it to run in structured view (e.g., `{ "oc-superpowers" = "ocp run sp acp" }`). A custom agent with an entry here is structured view-capable; without one it stays tmux-only. Unlike `custom_agents`, the value is split into argv and run directly, with no shell. |
| `acp_defaults` | `{}` | Per-agent defaults for structured view startup. `model` is forwarded when the worker starts; `effort` is applied through the agent's ACP `thought_level` config option when advertised. Example: `[session.acp_defaults.opencode] model = "openai/gpt-5.5" effort = "high"`. |

For Codex, AoE preserves existing `[hooks.state]` trust data and writes `~/.codex/config.toml` through `config.toml.lock` plus an atomic replace. This keeps repeated or concurrent AoE launches from duplicating hook blocks or leaving partial TOML.

## Status Hooks

Status hooks run local shell commands when the TUI sees a session status change. They are disabled by default and are intended for personal machine behavior such as desktop notifications.

```toml
[status_hooks]
enabled = true
debounce_ms = 100
on_waiting = "notify-send -a aoe 'AoE: Waiting' \"$AOE_SESSION_TITLE is waiting for input\""
on_idle = "notify-send -a aoe 'AoE: Idle' \"$AOE_SESSION_TITLE is idle\""
on_error = "notify-send -u critical -a aoe 'AoE: Error' \"$AOE_SESSION_TITLE errored\""
```

| Option | Default | Description |
|--------|---------|-------------|
| `enabled` | `false` | Run configured status hook commands from the TUI. |
| `debounce_ms` | `100` | Wait this many milliseconds for a status to remain stable before running commands. Set to `0` to run hooks immediately. |
| `on_starting` | unset | Command run when a session enters `Starting`. |
| `on_running` | unset | Command run when a session enters `Running`. |
| `on_waiting` | unset | Command run when a session enters `Waiting`. |
| `on_idle` | unset | Command run when a session enters `Idle`. |
| `on_error` | unset | Command run when a session enters `Error`. |
| `on_change` | unset | Command run on every status change after the status-specific command. |

Commands run in the session project directory and receive context through environment variables:

| Variable | Description |
|----------|-------------|
| `AOE_SESSION_ID` | Session UUID |
| `AOE_SESSION_TITLE` | Session title |
| `AOE_PROJECT_PATH` | Session working directory |
| `AOE_PROFILE` | Active profile |
| `AOE_TOOL` | Agent name |
| `AOE_GROUP_PATH` | Group hierarchy path |
| `AOE_OLD_STATUS` / `AOE_NEW_STATUS` | Status before/after the transition |
| `AOE_STATUS_CHANGED_AT` | Transition timestamp |

When both a status-specific hook and `on_change` fire for the same transition, AoE runs them sequentially (status-specific first). Hook commands are best-effort, non-blocking, and never block status updates or sound playback. They are configurable in global and profile settings only, not repo config, because they run arbitrary local commands.

### Custom Agents

Custom agents let you name commands for agents that AoE cannot detect as built-in binaries, such as SSH wrappers, local scripts, or remote Claude sessions. Configure them once in `custom_agents`, then select the configured name from the TUI picker, `aoe add --tool <name>`, or the Web session wizard.

```toml
[session]
default_tool = "lenovo-claude"
custom_agents = { "lenovo-claude" = "ssh -t lenovo claude" }
agent_detect_as = { "lenovo-claude" = "claude" }
```

- **`custom_agents`**: Maps a display name to the shell command AoE runs in a tmux pane when that agent is selected. Names appear in the TUI picker alongside built-ins like `claude`, `opencode`, and `codex`, and work with `aoe add --tool <name>`.
- **`agent_detect_as`** (optional): Reuses a built-in agent's status detection for the custom agent. Without it, custom agents default to `Idle`.
- **`agent_acp_cmd`** (optional): ACP launch command that lets the agent run in the structured view (see below).
- **`default_tool`** (optional): Can point at a custom-agent name to default new sessions to it.

Custom agents are always shown as available in the picker since their command may target a remote host or wrapper. All three maps are editable in config files or the TUI settings screen and support profile/repo overrides; profile/repo values fully replace the global map (redeclare any agents you want to keep). The Web wizard can select a configured custom agent but does not expose or edit the command strings.

#### Running a custom agent in the structured view

Give an agent an ACP launch command in `agent_acp_cmd` to run it in the structured view UI instead of tmux. The agent must speak the [Agent Client Protocol](https://agentclientprotocol.com); the command is what AoE execs to start the ACP server.

```toml
[session.custom_agents]
"oc-superpowers" = "ocp run sp"

[session.agent_acp_cmd]
"oc-superpowers" = "ocp run sp acp"
```

The `agent_acp_cmd` value is split into argv and executed directly with no shell, so for shell features wrap explicitly, e.g. `"sh -lc 'source ~/.profile && ocp run sp acp'"`. The name must match a `custom_agents` entry and cannot shadow a built-in. A custom agent with no `agent_acp_cmd` runs in the terminal view.

## Host Environment

```toml
environment = [
    "CLAUDE_CONFIG_DIR=/Users/me/.claude-accounts/work",
    "GH_TOKEN=$AOE_GH_TOKEN",
    "TERM",
]
```

Top-level `environment` injects env vars into the host command line for every session spawned at global scope. Useful for pinning a Claude/Codex/Gemini config dir per profile, forwarding an API token, or otherwise scoping per-agent state without exporting variables shell-wide.

Each entry follows the same grammar as `sandbox.environment`:

- **`KEY=value`**: literal value, passed through verbatim. `~` is not expanded; use an absolute path.
- **`KEY=$VAR`**: read `$VAR` from the host env at spawn time (skipped with a warning if `$VAR` is unset).
- **`KEY=$$literal`**: escape; emits `KEY=$literal`.
- **`KEY`** (bare): passthrough from the host env (skipped with a warning if unset).

All forms resolve to a literal `KEY=value` argument on the spawned process and are therefore visible in `ps`. For secrets you want hidden from argv, use [`sandbox.environment`](#sandbox-docker) instead. Host and sandbox sessions take disjoint code paths: a sandboxed session reads only `sandbox.environment`, an unsandboxed session reads only the top-level `environment`. Set both lists if you want a variable available regardless of how the session launches.

Profile-scoped `environment` replaces the global list entirely (matching the `sandbox.environment` override semantics).

## Worktree

The `[worktree]` block controls automatic git worktree creation for new sessions. Common keys:

```toml
[worktree]
enabled = false                                       # auto-enable worktrees for new sessions
path_template = "../{repo-name}-worktrees/{branch}"   # template vars: {repo-name}, {branch}, {session-id}
auto_cleanup = true                                   # prompt to remove the worktree on session delete
```

See [Git Worktrees](worktrees.md) for the full key reference (`bare_repo_path_template`, `show_branch_in_tui`, `delete_branch_on_cleanup`, `init_submodules`) and template details.

## Sandbox (Docker)

The `[sandbox]` block configures Docker sandboxing for sessions. Common keys:

```toml
[sandbox]
enabled_by_default = false                                    # auto-enable sandbox for new sessions
default_image = "ghcr.io/agent-of-empires/aoe-sandbox:latest" # container image
environment = ["GH_TOKEN=$AOE_GH_TOKEN"]                      # env vars forwarded into the container
```

See [Docker Sandbox](sandbox.md) for the full key reference (`cpu_limit`, `memory_limit`, `port_mappings`, `extra_volumes`, `volume_ignores`, `volume_ignores_strategy`, `auto_cleanup`, `default_terminal_mode`), the `environment` grammar, and credential handling. For env vars on host (non-sandboxed) sessions, use [Host Environment](#host-environment) instead; the two lists are disjoint.

## Host Hooks

The `[host_hooks]` block declares hooks that run on the **host** (not inside the sandbox container). Unlike `[hooks]`, which for sandboxed sessions runs inside the container, host hooks run in your host shell and can compute a value with host-only tooling and credentials, then hand only that value to the container.

```toml
[host_hooks]
before_start = ['echo "GH_TOKEN=$(my-mint-tool "$AOE_REPO_SLUG")"']
```

`before_start` runs each time a sandbox container comes up (on create and on restart, so short-lived values are refreshed before the agent launches). It re-mints when the container is created fresh or restarted from a stopped state (including after a Docker daemon restart leaves it stopped); attaching to an already-running container reuses the values from the last run and only backfills if none are stashed yet, so it is not re-run on every reattach. Each `KEY=VALUE` line the command prints to stdout is injected into the container environment as an **inherited** variable: the value is passed to the `docker` invocation through the process environment, never in argv, so it does not appear in `ps`. Lines that are not `KEY=VALUE` are ignored, and the hook's stdout is never logged, so it is safe to print a secret. A non-zero exit aborts bringing the container up.

The command's environment carries:

- **Lifecycle vars:** `AOE_SESSION_ID`, `AOE_SESSION_TITLE`, `AOE_PROJECT_PATH`, `AOE_PROFILE`, `AOE_TOOL`, `AOE_GROUP_PATH`, `AOE_SESSION_BRANCH` (worktree sessions only), and `AOE_REPO_SLUG` (the `owner/repo` of the project's `origin` remote, when it parses; useful for minting a repo-scoped credential without parsing the path yourself).
- **The session's sandbox environment**, so a per-session value reaches the hook. Set `TEST_VAR=foo` in the session's sandbox env (the new-session dialog's env list accepts `KEY=VALUE`), and the hook reads `$TEST_VAR`; a different session can set a different value. This is the per-session input channel (the host process env, e.g. `TEST_VAR=foo aoe add ...`, only varies per CLI invocation, so in the long-running TUI it would otherwise be fixed for every session). This env is resolved from the per-session list (or profile/global `sandbox.environment`) but **not** from a repo's `.agent-of-empires/config.toml`, keeping the same host/repo trust boundary as `host_hooks` itself.

The canonical use case is per-session, repo-scoped, short-lived credentials: mint a one-hour, single-repo token on the host (where the broad credential lives) and inject only the narrow token, so the minting tool and host credential never enter the container.

`host_hooks` is **global/profile only**: it is never honored from a repo's `.agent-of-empires/config.toml`, because a checked-out repository must not be able to run host commands. Declare it in your global or profile `config.toml`.

## tmux

```toml
[tmux]
status_bar = "auto"
mouse = "auto"
clipboard = "auto"
```

| Option | Default | Description |
|--------|---------|-------------|
| `status_bar` | `"auto"` | `"auto"`: apply if no `~/.tmux.conf`; `"enabled"`: always apply; `"disabled"`: never apply |
| `mouse` | `"auto"` | Same modes as `status_bar`. Controls mouse support in aoe tmux sessions. |
| `clipboard` | `"auto"` | Same modes. Forwards OSC 52 clipboard escape sequences from the wrapped agent (Claude Code, OpenCode, Codex, etc.) through tmux to your terminal. Without this, "select to copy" inside the agent silently fails. Sets `set-clipboard on` and `allow-passthrough on` on the aoe tmux session. |

## Diff

```toml
[diff]
default_branch = "main"
context_lines = 3
```

| Option | Default | Description |
|--------|---------|-------------|
| `default_branch` | (auto-detect) | Base branch for diffs |
| `context_lines` | `3` | Lines of context around changes |

## Updates

```toml
[updates]
update_check_mode = "notify"
check_interval_hours = 24
notify_in_cli = true
web_poll_interval_minutes = 60
```

| Option | Default | Description |
|--------|---------|-------------|
| `update_check_mode` | `"notify"` | One of `auto`, `notify`, `off`. See below. |
| `check_interval_hours` | `24` | Hours between GitHub checks (server-side cache TTL) |
| `notify_in_cli` | `true` | Show the `aoe` CLI eprintln nag when a new version is available; only fires while `update_check_mode = "notify"` |
| `web_poll_interval_minutes` | `60` | How often the web dashboard re-polls `/api/system/update-status` while open (min 5) |

### `update_check_mode`

- `auto`: when a new release is detected, install it silently in the background using the same tarball install path as `aoe update`. The new binary is picked up on the next launch (no mid-session restart). Only fires when the install location is writable; Homebrew installs fall through to manual `brew upgrade`.
- `notify` (default): show the TUI banner and, if `notify_in_cli = true`, the CLI eprintln nag. Press `Ctrl+x` on the banner to snooze for the current latest version; the banner returns automatically when a newer release ships.
- `off`: skip every check, banner, fetch, and dashboard poll. Use this on offline / restricted networks.

The TUI banner snooze is persisted to `app_state.dismissed_update_version`, so dismissing on v1.5.3 keeps the banner hidden across `aoe` restarts until v1.5.4 (or later) ships. See #1140.

Configs written for older `aoe` versions used a `check_enabled` boolean and an orphaned `auto_update` field. Migration `v009` runs once on startup and rewrites `check_enabled = false` to `update_check_mode = "off"`, `check_enabled = true` (or missing) to `"notify"`, and drops `auto_update` entirely.

## Tools

The `[tools.*]` block configures persistent dev tool sessions (lazygit, yazi, tig, etc.) tied to each agent session's working directory. Each entry has a required `command` and an optional `hotkey` in `Alt+<single-char>` format.

```toml
[tools.lazygit]
command = "lazygit"
hotkey = "Alt+g"

[tools.yazi]
command = "yazi"
hotkey = "Alt+f"
```

See [Tool Sessions](tool-sessions.md) for the full reference, hotkey rules, and lifecycle.

## Profiles

Profiles provide separate workspaces with their own sessions and groups. Each profile can override any of the settings above.

```bash
aoe                 # Uses "default" profile
aoe -p work         # Uses "work" profile
aoe profile create client-xyz
aoe profile list
aoe profile default work   # Set "work" as default
```

Profile overrides go in `~/.agent-of-empires/profiles/<name>/config.toml` and use the same format as the global config.

## Repo Config

Per-repo settings go in `.agent-of-empires/config.toml` at your project root. Run `aoe init` to generate a template.

Repo config supports: `[hooks]`, `[session]`, `[sandbox]`, and `[worktree]` sections. It does not support `[tmux]`, `[updates]`, `[claude]`, or `[diff]` (those are personal settings).

See [Repo Config & Hooks](repo-config.md) for details.
