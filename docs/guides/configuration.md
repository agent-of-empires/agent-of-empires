# Configuration Reference

AoE uses a layered configuration system. Settings are resolved in this order:

1. **Global config** -- `~/.agent-of-empires/config.toml` (or `~/.config/agent-of-empires/config.toml` on Linux)
2. **Profile config** -- `~/.agent-of-empires/profiles/<name>/config.toml`
3. **Repo config** -- `.agent-of-empires/config.toml` in the project root

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
| `AOE_LOG_LEVEL` | File log level: `trace`, `debug`, `info`, `warn`, `error`. Applies to `agent_of_empires`, `structured view`, and `terminal` targets. |
| `AOE_ACP_TRACE` | Add the ACP framework's raw JSON-RPC firehose to `debug.log` (`1` to enable). Very chatty; useful for chasing schema mismatches. |
| `AOE_TERMINAL_TRACE` | Add per-message byte tracing for the web terminal WebSocket relay to `debug.log` (`1` to enable). Bumps the `terminal` target to `trace`, surfacing every PTY read/write and every WS send/recv. Spammy under load (a busy claude session emits thousands of frames/min); use only when chasing terminal disconnect bugs. |

### Terminal latency instrumentation

The web dashboard can attribute keystroke-to-echo lag (the gap between pressing a key and seeing the character) to network versus server and PTY hops. Append `?debug=terminal-timing` to the dashboard URL. This is a measurement aid only; it changes no behavior and is entirely inert without the flag, so normal sessions pay nothing.

When the flag is set, the terminal:

- Measures **Idle-TTFB**: when you type after a quiet moment, it stamps the keystroke and resolves on the next inbound frame, recording both socket arrival and xterm render completion. It does not match echoed bytes (shells, TUIs, autosuggestions and password prompts make the echo unrelated to what you typed), so the number reflects the real key-to-screen path without parsing the echo.
- Sends a small control-channel ping every 500ms that the server bounces back without touching the PTY. This is the **WS control RTT**: network plus WebSocket transit with no PTY in the loop. The server includes its own recv-to-send duration so no clock sync is needed.
- Logs a rolling p50/p95 summary to the browser console every 10s, including the active renderer (`webgl` or `dom`).

Call `window.__aoeTiming.dump()` in the browser console to pull the raw samples as JSON for offline analysis. Interpretation: if WS control RTT is close to Idle-TTFB, the network dominates; if Idle-TTFB is well above WS control RTT, the server, PTY, tmux, or agent echo path dominates; if socket arrival is fast but render is slow, the renderer or main thread dominates.

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

### Builtin themes

Each builtin theme is a TOML file under `themes/builtin/` in the repo, embedded into the binary at build time. The schema matches the custom-theme format below, plus two optional metadata fields:

- `appearance = "dark" | "light"` declares the surface polarity. Drives the web dashboard's surface-ramp derivation (lighten vs darken from background) and selects the fallback Shiki syntax theme. If omitted, the server classifies from background luminance.
- `[syntax].shiki_theme = "..."` names the bundled Shiki theme the web dashboard loads for code blocks. If omitted, falls back by appearance (`github-dark` / `github-light`).

### Custom themes

Drop a TOML file in `~/.agent-of-empires/themes/<name>.toml` (or `$XDG_CONFIG_HOME/agent-of-empires/themes/` on Linux). The file appears in the theme picker under its filename stem.

Export a builtin as a starting point:

```bash
aoe theme export empire             # writes ~/.agent-of-empires/themes/custom-empire.toml
aoe theme export dracula -o my.toml # writes to my.toml
aoe theme list                      # show all available themes
aoe theme dir                       # print the custom themes directory
```

The schema is flat; every field is optional. Missing fields in a partial custom TOML fall back to Empire's hex values (the canonical baseline `Theme` struct defaults); fully unknown theme names at lookup time fall back to the `default` builtin. The 24 color fields cover background, borders, text, status semantics, diff colors, branch/sandbox chips, and accent. Add the optional `appearance` and `[syntax]` table to control the web surface.

## Session

```toml
[session]
default_tool = "claude"   # any supported agent name
yolo_mode_default = false
agent_status_hooks = true
auto_stop_idle_secs = 0   # 0 disables; e.g. 7200 = stop after 2h idle

[session.acp_defaults.opencode]
model = "openai/gpt-5.5"
effort = "high"
```

| Option | Default | Description |
|--------|---------|-------------|
| `default_tool` | (auto-detect) | Default agent for new sessions. Falls back to the first available tool if unset or unavailable. Can be set to a custom agent name. |
| `auto_stop_idle_secs` | `0` | Seconds a plain tmux session may sit `Idle` before it is auto-stopped: its tmux session and any sandbox container are killed, leaving a restartable `Stopped` row. `0` disables it; no session is ever auto-stopped for inactivity. Idle age is measured from the later of the last transition into `Idle` and the last user interaction, and a session with an attached tmux client is always spared, so a session you are reading is never reaped. Evaluated about once a minute (by the TUI and by `aoe serve`), so the stop can lag the threshold by up to a minute. Structured view workers use the separate `acp.auto_stop_idle_secs`. See #1689 and #1690. |
| `yolo_mode_default` | `false` | Enable YOLO mode by default for new sessions (skip permission prompts). Works with or without sandbox. In tmux mode this passes `--dangerously-skip-permissions` to the agent CLI; in structured view it maps to ACP `bypassPermissions` (see [Structured view: Permission modes and YOLO](../structured-view.md#permission-modes-and-yolo) for the adapter caveat). |
| `agent_status_hooks` | `true` | Install status-detection hooks into the agent's config file. Codex uses the `[hooks]` table in `~/.codex/config.toml`; other JSON-based agents use their settings JSON. When disabled, status detection falls back to tmux pane content parsing. Codex is hook-first, but known hook gaps are reconciled from pane content. |
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

Commands run in the session project directory and receive context through environment variables: `AOE_SESSION_ID`, `AOE_SESSION_TITLE`, `AOE_PROJECT_PATH`, `AOE_PROFILE`, `AOE_TOOL`, `AOE_GROUP_PATH`, `AOE_OLD_STATUS`, `AOE_NEW_STATUS`, and `AOE_STATUS_CHANGED_AT`. When both a status-specific hook and `on_change` are configured for the same transition, AoE runs them sequentially in one background worker, with the status-specific command first.

Hook commands are best-effort and non-blocking. Failures are logged at `warn` under `hooks.status_hooks` and never block status updates or sound playback. While you are attached to a tmux session from the TUI, AoE keeps polling eligible sessions so status hook commands still fire during long attaches. Status hooks are configurable in global and profile settings, not repo config, because they run arbitrary local commands.

### Custom Agents

Custom agents let you name commands for agents that AoE cannot detect as built-in binaries, such as SSH wrappers, local scripts, or remote Claude sessions. Configure them once in `custom_agents`, then select the configured name from the TUI picker, `aoe add --tool <name>`, or the Web session wizard.

```toml
[session]
default_tool = "lenovo-claude"
custom_agents = { "lenovo-claude" = "ssh -t lenovo claude" }
agent_detect_as = { "lenovo-claude" = "claude" }
```

- **`custom_agents`**: Maps a display name to the command AoE runs when that agent is selected. Custom-agent names are configured in config files or the TUI settings screen, and they appear alongside built-in agents like `claude`, `opencode`, and `codex`.
- **`agent_detect_as`** (optional): Maps a custom agent to a built-in agent's status detection. Without this, custom agents default to `Idle` status. Setting `"lenovo-claude" = "claude"` reuses Claude's Running/Waiting/Idle detection heuristics for the remote session.
- **`agent_acp_cmd`** (optional): The ACP launch command that lets a custom agent run in the structured view UI instead of the tmux terminal. See the Structured view subsection below.
- **`default_tool`** (optional): Can point at a custom-agent name so new sessions default to that configured agent.

Custom agents are always shown as available in the TUI picker because their command may target a remote host or wrapper script instead of a local binary. From the CLI, use `aoe add --tool <name>` to create a session with a configured custom agent by name. The selected custom agent still uses the command from `custom_agents`; browser or CLI input is not treated as a raw command.

The Web session wizard can select configured custom agents and submit the selected name to the server. For security, the Web UI does not expose custom-agent command strings, does not expose `agent_detect_as` or `agent_acp_cmd` values, and does not edit any of these maps. Edit those fields through config files or the TUI settings screen instead.

All three fields are editable from the TUI settings screen and support profile/repo-level overrides.

#### Running a custom agent in the structured view

By default a custom agent runs in the tmux terminal. To run it in the structured view UI, give it an ACP launch command in `agent_acp_cmd`. The agent must speak the [Agent Client Protocol](https://agentclientprotocol.com); the command is what AoE execs to start the ACP server.

```toml
[session.custom_agents]
"oc-superpowers" = "ocp run sp"

[session.agent_acp_cmd]
"oc-superpowers" = "ocp run sp acp"
```

Selecting `oc-superpowers` in the web wizard creates an structured-view session, and `aoe add --tool oc-superpowers` launches the ACP command. A custom agent with no `agent_acp_cmd` runs in the terminal view; if you force the structured view with `--agent oc-superpowers` and it has no ACP command, `aoe add` errors instead of silently launching the bundled fallback agent.

Note the difference from `custom_agents`: the `custom_agents` value is a shell command run in a tmux pane, while the `agent_acp_cmd` value is split into argv with shell-word rules and executed directly, with no shell. For shell features, wrap explicitly, e.g. `"sh -lc 'source ~/.profile && ocp run sp acp'"`. The agent name must match a `custom_agents` entry so it appears in the picker, and it cannot shadow a built-in agent name.

> **Note:** Profile and repo-level overrides fully replace the global value rather than merging with it. A profile that defines `custom_agents` replaces the entire global set, so you must redeclare any global agents you want to keep in that profile.

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

- **`KEY=value`** -- literal value, passed through verbatim. `~` is not expanded; use an absolute path.
- **`KEY=$VAR`** -- read `$VAR` from the host env at spawn time (skipped with a warning if `$VAR` is unset).
- **`KEY=$$literal`** -- escape; emits `KEY=$literal`.
- **`KEY`** (bare) -- passthrough from the host env (skipped with a warning if unset).

All forms resolve to a literal `KEY=value` argument on the spawned process and are therefore visible in `ps`. For secrets you want hidden from argv, use [`sandbox.environment`](#sandbox-docker) instead. Host and sandbox sessions take disjoint code paths: a sandboxed session reads only `sandbox.environment`, an unsandboxed session reads only the top-level `environment`. Set both lists if you want a variable available regardless of how the session launches.

Profile-scoped `environment` replaces the global list entirely (matching the `sandbox.environment` override semantics).

## Worktree

```toml
[worktree]
enabled = false
path_template = "../{repo-name}-worktrees/{branch}"
bare_repo_path_template = "./{branch}"
auto_cleanup = true
show_branch_in_tui = true
delete_branch_on_cleanup = false
init_submodules = true
```

| Option | Default | Description |
|--------|---------|-------------|
| `enabled` | `false` | Auto-enable worktree creation for new TUI sessions |
| `path_template` | `../{repo-name}-worktrees/{branch}` | Path template for worktrees in regular repos |
| `bare_repo_path_template` | `./{branch}` | Path template for worktrees in bare repos |
| `auto_cleanup` | `true` | Prompt to remove worktree when deleting a session |
| `show_branch_in_tui` | `true` | Display branch name in the TUI session list |
| `delete_branch_on_cleanup` | `false` | Also delete the git branch when removing a worktree |
| `init_submodules` | `true` | Run `git submodule update --init --recursive` after creating a worktree; set to `false` (or pass `--no-submodules` to `aoe add`) to skip submodule init for repos with large or deeply-nested submodule trees |

**Template variables:**

| Variable | Description |
|----------|-------------|
| `{repo-name}` | Repository folder name |
| `{branch}` | Branch name (slashes converted to hyphens) |
| `{session-id}` | First 8 characters of session UUID |

## Sandbox (Docker)

```toml
[sandbox]
enabled_by_default = false
default_image = "ghcr.io/agent-of-empires/aoe-sandbox:latest"
cpu_limit = "4"
memory_limit = "8g"
port_mappings = ["3000:3000", "5432:5432"]
environment = ["ANTHROPIC_API_KEY", "OPENAI_API_KEY", "GH_TOKEN=$AOE_GH_TOKEN"]
extra_volumes = []
volume_ignores = ["node_modules", "target"]
auto_cleanup = true
default_terminal_mode = "host"
```

| Option | Default | Description |
|--------|---------|-------------|
| `enabled_by_default` | `false` | Auto-enable sandbox for new sessions |
| `default_image` | `ghcr.io/agent-of-empires/aoe-sandbox:latest` | Docker image for containers |
| `cpu_limit` | (none) | CPU limit (e.g., `"4"`) |
| `memory_limit` | (none) | Memory limit (e.g., `"8g"`) |
| `port_mappings` | `[]` | Host-to-container port mappings (e.g., `["3000:3000"]`) |
| `environment` | `["TERM", "COLORTERM", "FORCE_COLOR", "NO_COLOR"]` | Env vars for containers (see below) |
| `extra_volumes` | `[]` | Additional Docker volume mounts |
| `volume_ignores` | `[]` | Directories to exclude from the project mount via anonymous volumes |
| `volume_ignores_strategy` | `"anonymous"` | Volume mounting strategy: `"anonymous"` (default) or `"named"` (required on macOS/VirtioFS to reliably shadow bind-mount subdirectories; named volumes are explicitly removed on session delete) |
| `auto_cleanup` | `true` | Remove containers when sessions are deleted |
| `default_terminal_mode` | `"host"` | Paired terminal location: `"host"` or `"container"` |

### environment entries

Each entry in the `environment` list can be:
- **`KEY`** (bare name) -- passes the host env var value into the container
- **`KEY=VALUE`** -- sets an explicit value; if VALUE starts with `$`, it reads from a host env var (e.g., `GH_TOKEN=$AOE_GH_TOKEN`). Use `$$` for a literal `$`.

Bare `KEY` and `KEY=$VAR` entries use Docker's `-e KEY` (key-only) form so the value stays out of argv. For env vars on **host (non-sandboxed) sessions**, see [Host Environment](#host-environment) instead. The two lists live on disjoint code paths: a sandboxed session reads only `sandbox.environment`, an unsandboxed session reads only top-level `environment`.

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

Repo config supports: `[hooks]`, `[session]`, `[sandbox]`, and `[worktree]` sections. It does not support `[tmux]`, `[updates]`, `[claude]`, or `[diff]` -- those are personal settings.

See [Repo Config & Hooks](repo-config.md) for details.
