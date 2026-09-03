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
  state.toml               # Runtime/UI bookkeeping (auto-managed, see below)
  trusted_repos.toml       # Hook trust decisions (auto-managed)
  .schema_version          # Migration tracking (auto-managed)
  profiles/
    default/
      sessions.json        # Session data
      groups.json          # Group hierarchy
      config.toml          # Profile-specific overrides
  logs/                    # Session execution logs
```

### `state.toml`

Sits alongside `config.toml` in the same app dir. It holds global-only
runtime/UI bookkeeping, such as "seen the welcome tour", the last browse
directory, sort order, and dismissed-tip/update tracking, none of which is
a user-facing setting, so it has no profile or repo layer and is never part
of the settings TUI or the web dashboard's settings schema. `GET
/api/settings` still exposes these fields under the `app_state.*` key for
backwards-compatible reads; only their on-disk home moved. That exposure is
read-only: `PATCH /api/settings` rejects writes to `app_state.*` with a 400,
because `AppStateConfig` is not a settings-schema section and the patch
validator treats it as an unknown one.

`state.toml` is machine-owned runtime bookkeeping, but it is written with the
same locked, read-modify-write guarantee as `config.toml`: both go through
`storage::locked_update`, so a concurrent writer's changes survive and two
`aoe` processes (the TUI and an `aoe serve` daemon) never lose an update. It
lives in a separate file, with its own lock, so its highest-churn writes
(every sidebar toggle, every tip dismissal) do not contend with a real
settings save on `config.toml`.

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
pre_trust_agent_folders = false
agent_status_hooks = true
smart_rename = true
smart_rename_agent = ""    # "" = use the session's own agent; e.g. "codex"
# smart_rename_model: per-agent title model, e.g. { claude = "haiku" } (see table below)
opencode_preassign_session_id = false  # pre-create opencode's session via a throwaway `opencode serve`
auto_stop_idle_secs = 0   # 0 disables; e.g. 7200 = stop after 2h idle
row_tag = "branch"       # none | auto | profile | sandbox | branch
prevent_sleep_when_active = false      # daemon only; keep the OS awake while sessions are active
prevent_sleep_idle_grace_minutes = 15  # release once every session has been idle this long (0-240)

# Per-agent structured-view defaults live under [acp], not [session].
[acp.acp_defaults.opencode]
model = "openai/gpt-5.5"
effort = "high"           # default thinking level
mode = "plan"             # default mode, applied when the agent advertises one

[acp.acp_defaults.opencode.effort_by_model]
"openai/gpt-5.5" = "high"  # overrides `effort` when this model is resolved

# Trusted global/profile hook event to AoE status overrides.
[agents.claude.status_map]
Stop = "idle"
Notification = "waiting"
```

| Option | Default | Description |
|--------|---------|-------------|
| `default_tool` | (auto-detect) | Default agent for new sessions. Falls back to the first available tool if unset or unavailable. Can be set to a custom agent name. |
| `auto_stop_idle_secs` | `0` | Seconds a plain tmux session may sit `Idle` before it is auto-stopped: its tmux session and any sandbox container are killed, leaving a restartable `Stopped` row. `0` disables it; no session is ever auto-stopped for inactivity. Idle age is measured from the later of the last transition into `Idle` and the last user interaction, and a session with an attached tmux client is always spared, so a session you are reading is never reaped. Evaluated about once a minute (by the TUI and by `aoe serve`), so the stop can lag the threshold by up to a minute. Structured view workers use the separate `acp.auto_stop_idle_secs`. See #1689 and #1690. |
| `prevent_sleep_when_active` | `false` | When enabled, the `aoe serve` daemon holds an OS assertion that prevents user-idle system sleep (the display still sleeps) while any session is active, releasing it once every session has been idle past `prevent_sleep_idle_grace_minutes`. Opt-in, daemon only: a TUI-only user without a running `aoe serve` gets no inhibition. Global toggle, not profile-overridable, since it drives a single process-wide assertion. Backed by `caffeinate -i` on macOS and `systemd-inhibit --what=idle:sleep` on Linux; hosts without those tools (or without logind) warn once and no-op. See #2733. |
| `prevent_sleep_idle_grace_minutes` | `15` | Minutes a session must stay idle before the sleep-inhibit assertion may be released. Only consulted when `prevent_sleep_when_active` is on. Range `0` to `240`; `0` releases as soon as every session leaves an active status. The grace period only begins once a session goes `Idle`, so a session that never reaches `Idle` keeps holding the assertion: `Waiting` on an unanswered prompt, or `Creating` on a container, worktree, or submodule setup that never returns, can hold sleep indefinitely. A `Starting` session is bounded by a short (~3s) launch guard and then re-resolves. |
| `row_tag` | `"branch"` | Controls the compact metadata shown next to each TUI session title: `none` shows nothing; `auto` shows the profile code only in all-profiles view; `profile` always shows the profile code; `sandbox` shows `sb` on sandboxed sessions; `branch` shows a compact worktree or workspace branch tag. |
| `yolo_mode_default` | `false` | Enable YOLO mode by default for new sessions (skip permission prompts). Works with or without sandbox. In tmux mode this passes `--dangerously-skip-permissions` to the agent CLI; in structured view it maps to ACP `bypassPermissions` (see [Structured view: Permission modes and YOLO](../structured-view/controls.md#permission-modes-and-yolo) for the adapter caveat). |
| `pre_trust_agent_folders` | `false` | Pre-trust each host session's worktree in the agent's own config so it does not open on a folder-trust prompt. Applies to Claude Code (`projects.<path>.hasTrustDialogAccepted` in `.claude.json`), Codex (`projects.<path>.trust_level`), and Gemini (a per-path entry in `trustedFolders.json`); other agents have no such prompt and are untouched. Config-location overrides (`CLAUDE_CONFIG_DIR`, `CODEX_HOME`, `GEMINI_CLI_TRUSTED_FOLDERS_PATH`) are honored. An `agent_config_dir` entry for the session's agent wins over them. Sandboxed sessions pre-trust their container workspace regardless of this setting, because that config is staged per session; this setting covers host sessions, where the record is written to your real agent config and outlives the session. `.mcp.json` servers still ask per server and the session's permission mode is unchanged, but trust is also what activates the repo's own `.claude/settings.json`: an untrusted workspace drops its `permissions.allow` rules. And since the prompt is what holds a session before startup, pre-trusting lets that file's hooks run with nobody having looked at the repo first. Enable it only for directories you would have trusted by hand. |
| `agent_status_hooks` | `true` | Install status-detection hooks into the agent's config file; see [Adding a New Agent](../development/adding-agents.md#hook-format-reference) for the per-agent file and payload. Config-dir overrides are honored: `CODEX_HOME` (Codex), `CLAUDE_CONFIG_DIR` (Claude), or `CURSOR_CONFIG_DIR` (Cursor) set in the session's profile environment or in AoE's own environment redirects hooks to that directory instead of the `~/.codex` / `~/.claude` / `~/.cursor` default. When disabled, status detection reads the pane alone. A hook write is evidence rather than the last word: the pane and the agent's terminal title are weighed against it by declared priority, so a prompt on screen outranks a `running` write, and a write nobody has refreshed for 15 minutes stops outranking anything (an agent whose turn ends on a tool result fires no terminating hook). Turning this off costs status detection only. [Session resume](session-resume.md) does not depend on it: Claude is the one agent whose hooks also capture a conversation id, and its poller falls back to Claude's own transcripts when that sidecar is absent; every other agent AoE auto-resumes is read from its own store, sidecar, or transcript. |
| `smart_rename` | `true` | Auto-rename a new structured view (ACP) session from its first turn, using the session's own agent in one-shot mode (`claude -p`, `codex exec`, `opencode run`, `gemini -p`). Runs only while the session still carries its auto-generated civilization name; a manually named session is never touched. Title only: the worktree directory is not moved, since the running agent holds it. A sandboxed session runs the one-shot inside its own container, where that agent's credentials are mounted, and defers while the container is stopped. Skipped for agents with no one-shot mode, command-overridden agents, and sandboxed sessions whose `smart_rename_agent` is a different agent (only the session agent's credentials are mounted in the container). Best-effort: a failed or timed-out call leaves the generated name and never affects the prompt. |
| `smart_rename_agent` | `""` | Agent used for one-shot utility calls (the smart-rename title and the conversation summary). Empty means use the session's own agent. Set it to a different one-shot-capable agent (`claude`, `codex`, `opencode`, `gemini`) to point those calls at a cheaper or more obedient model without changing the session's working agent. An unknown or one-shot-incapable value falls back to the session's own agent behavior. For a sandboxed session, a value resolving to any agent other than the session's own makes smart rename ineligible rather than falling back: the container mounts only the session agent's credentials. An empty value still means the session's own agent, so sandboxed sessions auto-name normally by default. |
| `smart_rename_model` | `{}` | Per-agent model for the throwaway smart-rename title one-shot, keyed by agent name (e.g. `claude = "haiku"`). A three-to-five-word title should not bill the CLI's default frontier model. An absent key uses the agent's built-in default (claude pins its cheap `haiku` alias, others the CLI default); an empty value forces the CLI default; a non-empty value pins that model via the agent's model flag (`--model` / `-m`). Ids are free-form and not validated, so an id the CLI rejects simply keeps the generated name. Does not affect the conversation summary, which always uses the CLI default. Only agents with a one-shot mode are tunable; a configured value for an agent with no model flag is ignored. The web dashboard sets the built-in-default and pinned states; the empty-string "force CLI default" state is settable via the TUI or this file. |
| `opencode_preassign_session_id` | `false` | Pre-assign opencode's session id before launch instead of capturing it afterward by polling opencode's SQLite store. When on, AoE spawns a throwaway `opencode serve`, creates the session through `POST /api/session`, then launches `opencode --session <id>`, so the id is known before the first prompt (symmetric with Claude's `--session-id`). Eliminates the post-launch capture race, at the cost of a short server boot (~2s) on each new host opencode launch. Host sessions only: a sandboxed agent cannot reach the loopback server, so containers keep polling. Best-effort: any failure falls back to the SQLite poller. |
| `agent_extra_args` | `{}` | Per-agent extra arguments appended after the binary (e.g., `{ opencode = "--port 8080" }`). |
| `agent_command_override` | `{}` | Per-agent command override replacing the binary entirely (e.g., `{ claude = "my-claude-wrapper" }`). |
| `custom_agents` | `{}` | User-defined agents: name to command mapping. Custom agent names appear in the TUI agent picker alongside built-in agents. |
| `agent_detect_as` | `{}` | Maps a custom agent to a built-in agent it inherits. Reuses that built-in's status heuristics and its [session resume](session-resume.md) behavior, and (when the built-in has an ACP adapter) makes the custom agent structured view-capable through it without an `agent_acp_cmd`; the base's adapter binary then runs instead of the wrapper (see [Running a custom agent in the structured view](#running-a-custom-agent-in-the-structured-view)). |
| `agent_acp_cmd` | `{}` | ACP launch command for a custom agent, enabling it to run in structured view (e.g., `{ "oc-superpowers" = "ocp run sp acp" }`). A custom agent with an entry here is structured view-capable; without one it stays tmux-only. Unlike `custom_agents`, the value is split into argv and run directly, with no shell. |
| `agent_config_dir` | `{}` | Config directory an agent reads instead of its built-in default, keyed by the agent name the session runs (e.g. `{ "claude-personal" = "~/.claude-personal" }` for a wrapper that exports `CLAUDE_CONFIG_DIR`). Folder-trust records go there: host sessions to the directory itself, sandboxed sessions to its `sandbox` subdirectory, which the container must see through a `sandbox.extra_volumes` mount (AoE warns when nothing mounts it). [Native MCP discovery](mcp-servers.md) reads it too, so the servers AoE reconciles against are the ones the agent loads. Wins over the agent's own config-dir env var. Global/profile only, and not read by status hooks, which keep resolving the env var. A sandboxed `pi` session with an entry publishes no conversation sidecar and falls back to store polling. |
| `acp.restrict_agents` | `false` | Restrict structured view sessions to `acp.allowed_agents`. Off leaves every registered agent available. Read from the global config only: a profile override cannot widen it, so a shared or locked-down deployment cannot be loosened by its own users. Changing the web value requires the passphrase step-up. |
| `acp.allowed_agents` | `[]` | ACP registry keys a structured view session may run while `acp.restrict_agents` is on, e.g. `["claude", "codex"]`. These are registry keys, not binary names, and each alias counts separately (allowing `claude` does not allow `claude-code`). With the restriction on, an empty list denies every agent. Governs the structured view only; a terminal session runs in a pane where any binary can be launched, so it is not constrained here. A policy change applies to new sessions immediately and to an already-running worker when it next respawns or when the daemon restarts, at which point a worker on a now-disallowed agent is terminated rather than reattached. |
| `acp.acp_defaults` | `{}` | Per-agent defaults for structured view startup (under the `[acp]` section, not `[session]`). `model` is forwarded when the worker starts; `effort` (thinking) and `mode` are applied through the agent's ACP config options (`thought_level`, `mode`) when advertised, and skipped with a warning otherwise. `effort_by_model` (a `{model = effort}` map) overrides `effort` for the resolved model. Editable per agent from the web dashboard (Structured view tab, Structured View Defaults). Example: `[acp.acp_defaults.opencode] model = "openai/gpt-5.5" effort = "high" mode = "plan"`. |
| `agents.<name>.status_map` | `{}` | Trusted global/profile-only hook event to AoE status mappings. Valid statuses are `running`, `waiting`, `idle`, and `error`. Entries apply by event name to built-in hook defaults, so duplicate event names with different matchers all receive the same status; new event names are added to the installed hooks when the agent format supports event keys. Existing hook files update on the next hook install, usually a new or restarted session. Agent processes with installed status hooks receive `AOE_PROFILE`, so hook scripts can query the resolved map with `aoe -p "$AOE_PROFILE" profile show --status-map <agent> --json`. |
| `agents.<name>.status_rules` | `[]` | Trusted global/profile-only declarative pane status rules (`[[agents.<name>.status_rules]]` array of tables). Each rule has `status` (`running`, `waiting`, `idle`, or `error`) and exactly one of `contains` (case-insensitive substring) or `regex` (Rust regex, matched as written; use `(?i)` for case-insensitive). Rules are evaluated in order against the ANSI-stripped pane snapshot; first match wins, no match reports `idle`. Rules take precedence over `agent_detect_as`, over a built-in detector of the same name, and over a status hook the agent writes. Invalid rules are skipped with a warning in the debug log. Takes effect on the next config resolve (TUI or daemon start). |

## Status Hooks

Status hooks run local shell commands when the TUI sees a session status change. They are disabled by default and are intended for personal machine behavior such as desktop notifications.

```toml
[status_hooks]
enabled = true
on_waiting = "notify-send -a aoe 'AoE: Waiting' \"$AOE_SESSION_TITLE is waiting for input\""
on_idle = "notify-send -a aoe 'AoE: Idle' \"$AOE_SESSION_TITLE is idle\""
on_error = "notify-send -u critical -a aoe 'AoE: Error' \"$AOE_SESSION_TITLE errored\""
```

| Option | Default | Description |
|--------|---------|-------------|
| `enabled` | `false` | Run configured status hook commands from the TUI. Commands fire once a status has stayed stable for a short built-in debounce (100ms), so rapid flickers don't spam hooks. |
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
- **`agent_detect_as`** (optional): Reuses a built-in agent's status detection for the custom agent, and marks the custom agent as inheriting that built-in. Without it (and without `status_rules`, below), custom agents default to `Idle`. Best for wrappers that run the *same* binary differently (SSH, scripts); for an agent whose output differs from every built-in, use `status_rules` instead. When the base agent has an ACP adapter, this mapping also lets the wrapper run in the structured view through that adapter (see [Running a custom agent in the structured view](#running-a-custom-agent-in-the-structured-view)). It also gives the wrapper the base agent's conversation capture and resume, so the wrapper's launch line carries that agent's session-id and resume flags (for Claude, `--session-id` on a fresh start and `--resume` on a restart) and its sessions continue the same conversation across a restart. A wrapper that does not pass its arguments through to the base binary should use `status_rules` instead, so no flags are added. A launcher-shaped command (`ssh host codex`, `env FOO=1 codex`) hides which word is the agent, so a base whose resume is a subcommand (Codex) emits none and starts a fresh conversation; put the agent's own binary first if you need resume.
- **`agent_acp_cmd`** (optional): ACP launch command that lets the agent run in the structured view (see below).
- **`agent_config_dir`** (optional): The config directory the wrapper points its CLI at, when that is not the built-in default (see below).
- **`default_tool`** (optional): Can point at a custom-agent name to default new sessions to it.

Custom agents are always shown as available in the picker since their command may target a remote host or wrapper. All four maps are editable in config files or the TUI settings screen; profile (and, for `agent_detect_as`, repo) values fully replace the global map, so redeclare any agents you want to keep. The Web wizard can select a configured custom agent but does not expose or edit the command strings.

#### One CLI, two accounts

A wrapper that runs the same CLI against a second login usually does it by
exporting the agent's config-dir variable, which AoE cannot see: the wrapper
sets it after AoE has already chosen which file to write. Name that directory
in `agent_config_dir` so folder-trust records and native MCP discovery land on
the config the agent will read, instead of the default one it never opens.

```toml
[session.custom_agents]
claude-personal = "claude-personal"      # a wrapper that exports CLAUDE_CONFIG_DIR

[session.agent_detect_as]
claude-personal = "claude"

[session.agent_config_dir]
claude-personal = "~/.claude-personal"
```

The value is a host path in both contexts. Host sessions use the directory
itself; sandboxed sessions use its `sandbox` subdirectory, the same split AoE
makes for the built-in agents (`~/.claude` and `~/.claude/sandbox`), so the
container needs that subdirectory mounted where the wrapper looks:

```toml
[sandbox]
extra_volumes = ["/Users/me/.claude-personal/sandbox:/root/.claude-personal:rw"]
```

Sandboxed sessions seed their workspace either way; host sessions still need
`pre_trust_agent_folders`. Only folder trust reads this setting: status hooks
resolve their config directory from the agent's own env var, so a wrapper that
exports it needs the variable in the session environment too (see
`agent_status_hooks`).

Pi is the one agent that gives something up. A sandboxed Pi publishes its
conversation id into the config directory AoE mounts itself; a directory you
name reaches the container through your own `extra_volumes` entry, at a path
AoE never sees, so such a session skips the sidecar and falls back to store
polling.

#### Status rules for custom agents

`agent_detect_as` only works when the custom agent renders the same output as the built-in it aliases. For a harness that is *similar to but not the same binary as* any built-in, declare pane status rules instead; no change to the agent is needed:

```toml
[session.custom_agents]
gjc = "gjc"

[[agents.gjc.status_rules]]
status = "waiting"
contains = "(y/n)"

[[agents.gjc.status_rules]]
status = "running"
regex = "esc to interrupt|thinking"
```

Rules are checked in order against the last screenful of ANSI-stripped pane text every status poll; the first match wins and no match reports `idle`. Put the more specific states (`waiting`, `error`) before the broad `running` matchers. When an agent has rules, they take precedence over its `agent_detect_as` alias, over a built-in detector of the same name, and over a status hook the agent writes.

#### Running a custom agent in the structured view

Give an agent an ACP launch command in `agent_acp_cmd` to run it in the structured view UI instead of tmux. The agent must speak the [Agent Client Protocol](https://agentclientprotocol.com); the command is what AoE execs to start the ACP server.

```toml
[session.custom_agents]
"oc-superpowers" = "ocp run sp"

[session.agent_acp_cmd]
"oc-superpowers" = "ocp run sp acp"
```

The `agent_acp_cmd` value is split into argv and executed directly with no shell, so for shell features wrap explicitly, e.g. `"sh -lc 'source ~/.profile && ocp run sp acp'"`. The name must match a `custom_agents` entry and cannot shadow a built-in. A custom agent with no `agent_acp_cmd` runs in the terminal view.

**Inheriting a supported agent.** If your custom agent only wraps a supported one (for example separate tools that each run Claude Code against a different profile / oauth location), you do not need to spell out `agent_acp_cmd` at all. Map the wrapper to its base with `agent_detect_as`, and it inherits the base's ACP adapter automatically:

```toml
[session.custom_agents]
"work-claude" = "CLAUDE_CONFIG_DIR=/Users/me/.claude-work claude"

[session.agent_detect_as]
"work-claude" = "claude"
```

`work-claude` is now structured view-capable through `claude-agent-acp`, and the existing terminal session's **Switch to structured view** action becomes available. **The wrapper binary itself is never executed**: structured view runs the base agent's adapter instead (inheriting its version gate and env allowlist), so it renders exactly like the base agent. Anything the wrapper does outside that CLI, such as selecting an account, gateway, or profile by setting env, does not apply. AoE logs a warning to the daemon log at spawn whenever a structured view session launches this way. Inheritance only works when the base has a built-in ACP adapter (`claude`, `codex`, `opencode`, `gemini`, `vibe`, `pi`, `omp`, `kimi`, `prime-agent`); a wrapper mapped to a terminal-only base (e.g. `cursor`) stays tmux-only.

To pass overrides to a host (non-sandboxed) structured session anyway, use the session's `extra_env` / [Host Environment](#host-environment) (for example `environment = ["CLAUDE_CONFIG_DIR=/Users/me/.claude-work"]`) or `session.inherit_host_environment`, the same as any other agent. Those are host paths and host env: they do not reach a Docker-sandboxed session, which reads only `sandbox.environment` and pins its own config dir at a container path (`AGENT_CONFIG_MOUNTS` bind-mounts it and path-valued vars like `CLAUDE_CONFIG_DIR` are stripped at the container boundary). For a sandboxed wrapper, set the container-side value in `sandbox.environment`, or give the wrapper a real ACP command with `agent_acp_cmd`. An explicit `agent_acp_cmd` still wins if you set both.

## Host Environment

```toml
environment = [
    "CLAUDE_CONFIG_DIR=/Users/me/.claude-accounts/work",
    "GH_TOKEN=$AOE_GH_TOKEN",
    "TERM",
]
```

Top-level `environment` injects env vars into every host (non-sandboxed) session spawned at global scope, in both the terminal and the structured view. Useful for pinning a Claude/Codex/Gemini config dir per profile, forwarding an API token, or otherwise scoping per-agent state without exporting variables shell-wide.

Each entry follows the same grammar as `sandbox.environment`:

Keys must match `[A-Za-z_][A-Za-z0-9_]*` (an ASCII letter or `_` first, then ASCII alphanumerics or `_`). Any other key is dropped with a warning; Docker and `Command::env` are laxer than this, so a key like `FOO-BAR` or `foo.bar` is accepted by the runtime but silently rejected here.

- **`KEY=value`**: literal value, passed through verbatim. `~` is not expanded; use an absolute path.
- **`KEY=$VAR`**: read `$VAR` from the host env at spawn time (skipped with a warning if `$VAR` is unset).
- **`KEY=$$literal`**: escape; emits `KEY=$literal`.
- **`KEY`** (bare): passthrough from the host env (skipped with a warning if unset).

In the terminal view every form resolves to a literal `KEY=value` prefix on the pane command and is therefore visible in `ps`; for secrets you want hidden from argv there, use [`sandbox.environment`](#sandbox-docker) instead. The structured view applies the same list to the agent process's environment rather than its argv, so values do not appear in `ps`. Host and sandbox sessions take disjoint code paths: a sandboxed session reads only `sandbox.environment`, an unsandboxed session reads only the top-level `environment`. Set both lists if you want a variable available regardless of how the session launches.

Profile-scoped `environment` replaces the global list entirely (matching the `sandbox.environment` override semantics).

### What host sessions inherit automatically

Independent of the `environment` list, AoE forwards a fixed set of desktop and
session vars from its own environment into every host session, in both the
terminal and the structured view: `DISPLAY`, `WAYLAND_DISPLAY`, `XAUTHORITY`,
`DBUS_SESSION_BUS_ADDRESS`, `SSH_AUTH_SOCK`, and every `XDG_*` var. Without
this, a browser an agent launches (an OIDC login, say) has no way to reach your
desktop, since tmux carries only its own narrow `update-environment` set and the
structured view starts its agent from a cleared environment.

Worth knowing what that grants: `DISPLAY` plus `XAUTHORITY` is X11 access to
your whole session, which means an agent can capture the screen and inject
input, not just open a browser window. That is the point of forwarding them, and
it has been the terminal view's behavior since #3079, but it is the tradeoff. A
sandboxed session never receives them.

To forward everything else too, rather than naming each var in `environment`:

```toml
[session]
inherit_host_environment = true
```

Every var AoE itself holds then reaches host sessions, so a `GOPATH` or
`CARGO_HOME` you exported in your shell is simply there. `AOE_*` and
`AGENT_OF_EMPIRES_*` keys are never forwarded (they are AoE's own wiring and
credentials), and `TERM` stays owned by tmux so a pane's terminal type is not
degraded. Off by default: it widens what every agent process can read, including
any API token you exported in your shell, so it is opt-in per profile.

In the terminal view the forwarded pairs ride the short-lived `tmux new-session`
invocation as `-e KEY=value`, so a secret is briefly visible in `ps` while that
command runs. That is narrower than [`environment`](#host-environment), whose
values sit in the pane command's argv for the pane's whole life, but it is the
reason to prefer `sandbox.environment` for genuine secrets.

### When AoE has no environment to forward

Both mechanisms above read AoE's *own* environment. Forwarding is a passthrough,
not a store, so AoE can only hand a session what it holds itself. If the process
that starts AoE has no `DISPLAY`, neither does your agent.

That matters when something other than your shell starts the daemon. A systemd
unit gets a near-empty environment by default, so give it your vars explicitly:

```ini
[Service]
# Either name the vars to inherit from the systemd user manager...
PassEnvironment=DISPLAY XAUTHORITY XDG_RUNTIME_DIR DBUS_SESSION_BUS_ADDRESS
# ...or load them from a file you maintain.
EnvironmentFile=%h/.config/agent-of-empires/env
```

For a user unit, run this from your graphical session to populate the manager
AoE then inherits from, then restart the unit so it picks the values up
(`import-environment` does not touch already-running units):

```bash
systemctl --user import-environment DISPLAY XAUTHORITY XDG_RUNTIME_DIR DBUS_SESSION_BUS_ADDRESS
systemctl --user restart agent-of-empires
```

That is runtime-only and lost on reboot; `~/.config/environment.d/*.conf` is the
persistent equivalent. The same principle applies to launchd, cron, and a bare
SSH `command`: the launch context owns its environment, and AoE forwards
whatever that is.

To check what a running daemon can actually forward, read its environment
directly. On Linux:

```bash
tr '\0' '\n' < /proc/$(cat ~/.config/agent-of-empires/serve.pid)/environ
```

macOS has no `/proc`, so use `ps` there:

```bash
ps eww -o command= -p "$(cat ~/.agent-of-empires/serve.pid)"
```

## Worktree

The `[worktree]` block controls automatic git worktree creation for new sessions. Common keys:

```toml
[worktree]
enabled = false                                       # auto-enable worktrees for new sessions
path_template = "../{repo-name}-worktrees/{branch}"   # template vars: {repo-name}, {branch}, {session-id}
auto_cleanup = true                                   # prompt to remove the worktree on session delete
```

See [Git Worktrees](worktrees.md) for the full key reference (`bare_repo_path_template`, `delete_branch_on_cleanup`, `init_submodules`) and template details.

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

The `[host_hooks]` block declares hooks that run on the **host** (not inside the sandbox container). Unlike `[hooks]`, which for sandboxed sessions runs inside the container, host hooks run in your host shell and can compute a value with host-only tooling and credentials, then hand only that value to the agent.

```toml
[host_hooks]
before_start   = ['echo "GH_TOKEN=$(my-mint-tool "$AOE_REPO_SLUG")"']  # sandboxed sessions
before_session = ['my-account-switcher env']                            # host sessions
```

The two fields mirror the split the static env lists already make: `before_start` serves sandboxed sessions (the dynamic counterpart of `sandbox.environment`), `before_session` serves host sessions (the counterpart of the top-level [`environment`](#host-environment)). A launch runs exactly one of them, chosen by whether the session is sandboxed, so the same key is never minted twice.

`before_start` runs each time a sandbox container comes up (on create and on restart, so short-lived values are refreshed before the agent launches). It re-mints when the container is created fresh or restarted from a stopped state (including after a Docker daemon restart leaves it stopped); attaching to an already-running container reuses the values from the last run and only backfills if none are stashed yet, so it is not re-run on every reattach. Each `KEY=VALUE` line the command prints to stdout is injected into the container environment as an **inherited** variable: the value is passed to the `docker` invocation through the process environment, never in argv, so it does not appear in `ps`. Lines that are not `KEY=VALUE` are ignored, and the hook's stdout is never logged, so it is safe to print a secret. A non-zero exit aborts bringing the container up. A line whose key is a single token but does not match `[A-Za-z_][A-Za-z0-9_]*` (e.g. `FOO-BAR=x`) is dropped with a warning; a diagnostic line whose key contains spaces (e.g. `fetching token from url=https://...`) is ignored silently.

`before_session` runs each time a **host** (non-sandboxed) session is launched, before the agent starts, and applies its `KEY=VALUE` lines to the agent's own environment. Same stdout contract as `before_start`: other lines are ignored, stdout is never logged, and a non-zero exit aborts the launch. It re-runs on every host launch, including restart and a view switch that respawns the agent, and nothing is persisted between launches, so a short-lived value is refreshed rather than replayed.

Minted pairs are applied **after** the static `environment` list, so a freshly minted value wins over a same-keyed config entry. Both views honor that: the structured view appends the pairs to the agent process's environment, and the terminal view passes them through `tmux new-session -e` while dropping any same-keyed `environment` entry, which would otherwise shadow them via the shell-assignment prefix.

On secrecy, the terminal view is better than the static `environment` list but not airtight: a static entry becomes a shell-assignment prefix on the pane command and is therefore visible in `ps` for the pane's whole life, whereas a minted value rides `tmux new-session -e` instead, so it never enters the pane command's argv. That value is still not private, though: `tmux` stores it in the session's own environment for as long as the session exists, and any client with access to the tmux server can read it back with `tmux show-environment -t <session>`, so it is only as secret as access to that tmux server. For a value that must stay out of both argv and the tmux session environment, use a sandboxed session and `before_start`, which passes values to `docker` through the process environment.

The canonical use case is resolving *which* identity a session runs as at spawn time rather than pinning it in config: an account or provider switcher prints the config dir and endpoint for the account currently selected, refreshing a rotated token in the same step.

```toml
[host_hooks]
before_session = ['my-account-switcher env --profile "$AOE_PROFILE"']
```

```text
CLAUDE_CONFIG_DIR=/Users/me/.claude-accounts/profiles/work
ANTHROPIC_BASE_URL=http://127.0.0.1:8317
```

Scope note: `before_session` applies to the **agent** launch, matching the static `environment` list. A plain tool session (the extra shell terminal in a session) is not an agent launch and does not run it.

The command's environment carries:

- **Lifecycle vars:** `AOE_SESSION_ID`, `AOE_SESSION_TITLE`, `AOE_PROJECT_PATH`, `AOE_PROFILE`, `AOE_TOOL`, `AOE_GROUP_PATH`, `AOE_SESSION_BRANCH` (worktree sessions only), and `AOE_REPO_SLUG` (the `owner/repo` of the project's `origin` remote, when it parses; useful for minting a repo-scoped credential without parsing the path yourself). In the structured view `before_session` receives the subset available at that spawn site: `AOE_SESSION_ID`, `AOE_PROFILE`, `AOE_TOOL`, and `AOE_PROJECT_PATH`.
- **The session's sandbox environment** (`before_start` only), so a per-session value reaches the hook. Set `TEST_VAR=foo` in the session's sandbox env (the new-session dialog's env list accepts `KEY=VALUE`), and the hook reads `$TEST_VAR`; a different session can set a different value. This is the per-session input channel (the host process env, e.g. `TEST_VAR=foo aoe add ...`, only varies per CLI invocation, so in the long-running TUI it would otherwise be fixed for every session). This env is resolved from the per-session list (or profile/global `sandbox.environment`) but **not** from a repo's `.agent-of-empires/config.toml`, keeping the same host/repo trust boundary as `host_hooks` itself.

The canonical use case is per-session, repo-scoped, short-lived credentials: mint a one-hour, single-repo token on the host (where the broad credential lives) and inject only the narrow token, so the minting tool and host credential never enter the container.

`host_hooks` is **global/profile only**: it is never honored from a repo's `.agent-of-empires/config.toml`, because a checked-out repository must not be able to run host commands. Declare it in your global or profile `config.toml`.

## tmux

```toml
[tmux]
status_bar = "auto"
mouse = "auto"
clipboard = "auto"
# socket_name = "aoe"
vt_live = true
```

| Option | Default | Description |
|--------|---------|-------------|
| `status_bar` | `"auto"` | Paints aoe's themed status bar (session title, branch, sandbox, detach hint) on its own sessions. `"auto"` steps aside whenever you have a tmux config at all, because the bar is a whole theme rather than one option and a half-merge of yours with aoe's would please nobody; `"enabled"` always paints it; `"disabled"` never does. Not painting it reverts aoe's session-scoped `status*` overrides so your own config governs, so `"disabled"` means "stop styling the bar", not "hide it". |
| `mouse` | `"auto"` | Sets tmux `mouse` on aoe's sessions, which is what turns a wheel scroll (or the Web dashboard's touch scroll) into tmux copy-mode scrollback. `"auto"` leaves the option untouched when your own tmux config sets `mouse`, so your `set -g mouse ...` governs, and enables it otherwise (including when your tmux config exists but never mentions `mouse`, since tmux's own default is off). `"enabled"` always turns it on; `"disabled"` always turns it off, for aoe's sessions only. |
| `clipboard` | `"auto"` | Forwards OSC 52 clipboard escape sequences from the wrapped agent (Claude Code, OpenCode, Codex, etc.) to your terminal or Web dashboard. Without this, "select to copy" inside the agent silently fails. Sets `set-clipboard on` and `allow-passthrough on` for the aoe session (the attached path), and in live-send aoe extracts OSC 52 from either the VT stream or a terminal snapshot's raw observer before pushing it to the native or browser clipboard. `"auto"` steps aside only when your own tmux config sets one of those two options, the same per-option rule as `mouse`; `"enabled"` always applies them; `"disabled"` never does. Live-send forwarding is on for `"auto"` and `"enabled"` (your tmux config cannot affect aoe's in-process transport, so `"auto"` does not defer to it here); `"disabled"` turns it off. |

Detection for the per-option modes (`mouse`, `clipboard`) reads `~/.tmux.conf`, `$XDG_CONFIG_HOME/tmux/tmux.conf`, and `~/.config/tmux/tmux.conf`, looking for a `set` / `setw` of the option. It is deliberately conservative: an option reached via `source-file`, wrapped in `if-shell`, guarded by a false `%if`, or set from inside a key binding (`bind m set -g mouse`) is not detected, and aoe applies its own value. Set the mode to `"disabled"` if you keep yours in one of those places. `/etc/tmux.conf` is not consulted; it is not your file.
| `socket_name` | unset | Run aoe's sessions on a private tmux server with this socket name (passed as `tmux -L <name>`), so your own `tmux ls` and hand-managed sessions stay separate from aoe's. Leave unset to share the default tmux server (the current behavior). Must be a bare name, not a path; a value with a `/` or `\` is ignored. Takes effect on the next aoe start. Global/profile only. |
| `vt_live` | `true` | Render native agent and tool previews from a persistent VT channel: `tmux pipe-pane` streams the pane into an in-process terminal grid, and keystrokes go back over the same socket. Terminal previews, including the Web dashboard, always use tmux's rendered `capture-pane` snapshots to avoid prompt-paint races; a raw observer retains OSC 52 clipboard forwarding without affecting rendering. Disabling this setting makes native agent and tool previews use the slower `capture-pane` / `send-keys` path. Applies in place on the next TUI capture cycle. |

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
```

| Option | Default | Description |
|--------|---------|-------------|
| `update_check_mode` | `"notify"` | One of `auto`, `notify`, `off`. See below. |

Checks hit GitHub at most once a day (a built-in server-side cache TTL); the web dashboard re-polls the cached status hourly while open.

### `update_check_mode`

- `auto`: when a new release is detected, install it silently in the background using the same tarball install path as `aoe update`. The new binary is picked up on the next launch (no mid-session restart). Only fires when the install location is writable; Homebrew installs fall through to manual `brew upgrade`.
- `notify` (default): show the TUI banner and the CLI eprintln nag. Press `Ctrl+x` on the banner to snooze for the current latest version; the banner returns automatically when a newer release ships.
- `off`: skip every check, banner, fetch, and dashboard poll. Use this on offline / restricted networks.

The TUI banner snooze is persisted to `app_state.dismissed_update_version` (in `state.toml`, see [above](#statetoml)), so dismissing on v1.5.3 keeps the banner hidden across `aoe` restarts until v1.5.4 (or later) ships. See #1140.

Configs written for older `aoe` versions used a `check_enabled` boolean and an orphaned `auto_update` field. Migration `v009` runs once on startup and rewrites `check_enabled = false` to `update_check_mode = "off"`, `check_enabled = true` (or missing) to `"notify"`, and drops `auto_update` entirely. The former `check_interval_hours`, `notify_in_cli`, and `web_poll_interval_minutes` knobs are now fixed built-ins; migration `v022` drops them from saved configs.

## Tools

The `[tools.*]` block configures dev tools tied to each agent session's working directory. Each entry has a required `command`, an optional `hotkey` in `Alt+<single-char>` format, and optional `background = true` for fire-and-forget commands that should not create a tmux tool session.

```toml
[tools.lazygit]
command = "lazygit"
hotkey = "Alt+g"

[tools.yazi]
command = "yazi"
hotkey = "Alt+f"

[tools.github]
command = "gh repo view --web"
hotkey = "Alt+o"
background = true
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

Repo config supports: `[hooks]`, `[session]`, `[sandbox]`, and `[worktree]` sections. It does not support `[tmux]`, `[sound]`, `[updates]`, `[claude]`, or `[diff]` (those are personal settings).

See [Repo Config & Hooks](repo-config.md) for details.
