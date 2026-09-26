# Configuration Reference

Settings resolve in layers, each overriding the one before it, field by field:

1. **Global**: `~/.agent-of-empires/config.toml`
2. **Profile**: `~/.agent-of-empires/profiles/<name>/config.toml`
3. **Repo**: `.agent-of-empires/config.toml` in the project root

Unset fields inherit from the layer above. List fields replace rather than extend. Everything below is also editable from the TUI settings screen (`s`) and, unless noted, from the web dashboard.

Global-only settings use the global config. On upgrade, the default profile's values for them move there, and other profiles' values are removed. `PATCH /api/profiles/<name>/settings` rejects global-only fields with HTTP 400; use `PATCH /api/settings` instead.

A project registry entry can also override `worktree.enabled` and `session.smart_rename` for that project, from the web Projects view or the TUI add-project form. This override wins over all three layers. It lives in your own registry (`projects.json`), not the repo, so it does not weaken the `repo = "deny"` policy on either field.

## File locations

| Platform | Global config |
|----------|--------------|
| Linux | `$XDG_CONFIG_HOME/agent-of-empires/config.toml` (default `~/.config/agent-of-empires/`) |
| macOS | `~/.agent-of-empires/config.toml`, or the XDG path when `XDG_CONFIG_HOME` is set or that directory already exists |

On macOS nothing is moved for you: an existing `~/.agent-of-empires/` keeps being used even after you set `XDG_CONFIG_HOME`.

```
~/.agent-of-empires/
  config.toml              # Global configuration
  state.toml               # Runtime/UI bookkeeping (auto-managed)
  trusted_repos.toml       # Hook trust decisions (auto-managed)
  .schema_version          # Migration tracking (auto-managed)
  profiles/default/
    sessions.json          # Session data
    groups.json            # Group hierarchy
    config.toml            # Profile overrides
  logs/
```

`state.toml` holds global-only UI bookkeeping (tour seen, last browse directory, sort order, dismissed tips and updates). It is not a setting: it has no profile or repo layer and no TUI or web control. `GET /api/settings` still reports these under `app_state.*`, but `PATCH` rejects writes to them.

## Environment variables

| Variable | Description |
|----------|-------------|
| `AGENT_OF_EMPIRES_PROFILE` | Default profile |
| `AOE_LOG_LEVEL` | File log level: `trace`, `debug`, `info`, `warn`, `error` |
| `AGENT_OF_EMPIRES_DEBUG` | Legacy alias for `AOE_LOG_LEVEL=debug` |
| `AOE_DEFER_SANDBOX_MIGRATION` | Start without moving sandboxed sessions' agent stores; retried later or by `aoe migrate`. See [Per-session agent stores](sandbox.md#per-session-agent-stores) |

## Theme

```toml
[theme]
name = "default"          # see the picker or `aoe theme list`
color_mode = "truecolor"  # truecolor | palette (TUI only)
```

`name` applies to both the TUI and the web dashboard. Builtins: `default`, `empire`, `phosphor`, `tokyo-night-storm`, `catppuccin-latte`, `dracula`, `rose-pine`, `deep-ocean`. `color_mode = "palette"` downsamples the TUI to xterm-256 for transports that mangle 24-bit color (some `mosh` setups); the web dashboard is always truecolor.

Custom themes are TOML files in `~/.agent-of-empires/themes/`, listed in the picker under their filename:

```bash
aoe theme export empire   # writes ~/.agent-of-empires/themes/custom-empire.toml
aoe theme list
aoe theme dir
```

Every field is optional. Missing colors fall back to the Empire baseline, while an omitted `appearance` or `[syntax].shiki_theme` is derived from the theme's background luminance. `appearance = "dark" | "light"` and `[syntax].shiki_theme` (any id [Shiki bundles](https://shiki.style/themes)) drive the dashboard's surface ramp and code highlighting.

## Session

```toml
[session]
default_tool = "claude"
yolo_mode_default = false
agent_status_hooks = true
smart_rename = true
auto_stop_idle_secs = 0   # 0 disables; e.g. 7200 = stop after 2h idle
row_tag = "branch"        # none | auto | profile | sandbox | branch
sidebar_position = "left" # left | right; TUI session list
```

| Option | Default | Description |
|--------|---------|-------------|
| `default_tool` | (auto-detect) | Default agent for new sessions, including a custom agent name. Falls back to the first available tool. |
| `yolo_mode_default` | `false` | Start sessions with permission prompts skipped. In the terminal view this passes `--dangerously-skip-permissions`; in the structured view it maps to ACP `bypassPermissions` (see [Modes and YOLO](../structured-view/controls.md#permission-modes-and-yolo)). |
| `auto_stop_idle_secs` | `0` | Seconds a plain tmux session may sit `Idle` before its tmux session and container are killed, leaving a restartable `Stopped` row. Idle age counts from the later of the last `Idle` transition and the last interaction, a session with an attached tmux client is spared, and the check runs about once a minute. Structured view workers use `acp.auto_stop_idle_secs`. |
| `prevent_sleep_when_active` | `false` | Have the `aoe serve` daemon hold an OS sleep inhibitor (`caffeinate -i`, `systemd-inhibit`) while any session is active. Global only; a TUI without a daemon gets nothing. |
| `prevent_sleep_idle_grace_minutes` | `15` | Minutes (0 to 240) every session must stay idle before the inhibitor is released. A session that never reaches `Idle` (`Waiting` on a prompt, `Creating` forever) holds it indefinitely. |
| `session_id_poller_max_threads` | `50` | Ceiling on concurrent session-id pollers per process. Past the ceiling, an overflow session's id is not refreshed until it gets a poller; starting one is retried on a 5 s to 60 s backoff. Global only, applied at process start. |
| `row_tag` | `"branch"` | Metadata next to a TUI session title: `none`, `auto` (profile code in all-profiles view), `profile`, `sandbox`, or `branch`. |
| `sidebar_position` | `"left"` | TUI session sidebar position: `left` or `right`. Global only. Narrow terminals keep the stacked layout. |
| `tie_workdir_to_name` | `true` | Keep a managed worktree session's directory named after its title. See [Worktrees](worktrees.md#naming). |
| `pre_trust_agent_folders` | `false` | Pre-trust each host session's worktree in the agent's own config (Claude Code, Codex, Gemini) so it does not open on a folder-trust prompt. Config-dir overrides are honored, and an `agent_config_dir` entry wins over them. Trust also activates the repo's `.claude/settings.json`, hooks included, so enable it only for directories you would have trusted by hand. Sandboxed sessions always pre-trust their own staged config. |
| `agent_status_hooks` | `true` | Install status-detection hooks into the agent's config; see [Adding a New Agent](../development/adding-agents.md#hook-format-reference). Disabling it leaves status to pane reading but keeps identity hooks used for native resume. |
| `opencode_preassign_session_id` | `false` | Pre-assign OpenCode's native session id before a host launch (about two seconds per session) so resume captures it. Unsupported for sandboxed OpenCode. |
| `smart_rename` | `true` | Auto-rename a still-default-named structured session from its first turn, using the session's agent in one-shot mode. Title only; a session you named is never touched. Skipped for agents with no one-shot mode and command-overridden agents. Overridable per project. |
| `smart_rename_agent` | `""` | Agent used for one-shot utility calls (the rename title and the conversation summary). Empty means the session's own agent. A sandboxed session only mounts its own agent's credentials, so a different value makes it ineligible instead of falling back. |
| `smart_rename_model` | `{}` | Per-agent model for the rename one-shot, e.g. `{ claude = "haiku" }`. An absent key uses the agent's built-in default, an empty value forces the CLI default, and any other value is passed to the agent's model flag. |
| `inherit_host_environment` | `false` | Forward AoE's whole environment to host sessions. See [Host environment](#host-environment). |
| `agent_extra_args` | `{}` | Per-agent arguments appended after the binary, e.g. `{ opencode = "--port 8080" }`. Ignored for structured view sessions. |
| `agent_command_override` | `{}` | Per-agent command replacing the binary. Managed resume and fork validate the actual native command and store; opaque wrappers require an explicit execution contract. See [execution identity and wrappers](session-resume.md#execution-identity-and-wrappers). |
| `custom_agents` | `{}` | User-defined agents (name to command). See [Custom agents](#custom-agents). |
| `agent_detect_as` | `{}` | Built-in status detection and ACP adapter inheritance for a custom agent. This is not authority for terminal resume or fork. |
| `agent_execution_as` | `{}` | Explicit native-agent contract for an opaque terminal wrapper. Requires `agent_config_dir` naming its store. Trusted global/profile configuration only; repository overrides are refused. |
| `agent_acp_cmd` | `{}` | ACP launch command that makes a custom agent structured-view capable, e.g. `{ "oc-superpowers" = "ocp run sp acp" }`. Split into argv and run with no shell. |
| `agent_config_dir` | `{}` | Config directory an agent reads instead of its built-in default, keyed by agent name. Wins over the agent's config-dir environment variable. Two names pointing at the same agent are two accounts of it; a restart that swaps between them carries the conversation across, see [Session Resume](session-resume.md#swapping-the-engine-on-a-restart). Global/profile only. |
| `agents.<name>.status_map` | `{}` | Trusted hook-event to status mapping (`running`, `waiting`, `idle`, `error`), applied on the next hook install. Hooks receive `AOE_PROFILE`, so a script can read the resolved map with `aoe -p "$AOE_PROFILE" profile show --status-map <agent> --json`. Global/profile only. |
| `agents.<name>.status_rules` | `[]` | Declarative pane status rules. See [Status rules](#status-rules-for-custom-agents). Global/profile only. |

Per-agent structured view defaults live under `[acp]`, not `[session]`:

| Option | Default | Description |
|--------|---------|-------------|
| `acp.offer_structured_in_new_session` | `false` | Offer the structured view in the TUI: the Structured toggle in the new-session dialog, and switching a terminal session into it. Off, every TUI-created session is a terminal. The web dashboard always offers it. |
| `acp.default_new_session_view` | `auto` | View the new-session dialog starts on when the agent supports the structured view: `auto`, `structured`, or `terminal`. `auto` keeps each surface's default, structured on the web dashboard and terminal in the TUI. The dialog's toggle still wins per session. |
| `acp.acp_defaults.<agent>` | `{}` | `model`, `effort` (thinking level), `mode`, and an `effort_by_model` map applied when a worker starts. `effort` and `mode` go through the agent's ACP config options when advertised and are skipped with a warning otherwise. A model chosen at creation wins over `model`, unless `pin_model = true` makes it a pin: creation then refuses any other model (`kind = "model_pinned"`) and the dashboard's picker collapses to it. The entry is keyed by the agent the session spawns as, so a custom agent reads its `agent_detect_as` base's entry. |
| `acp.restrict_agents` | `false` | Restrict structured view sessions to `acp.allowed_agents`. Read from the global config only, so a profile cannot widen it; changing it from the web needs the passphrase step-up. |
| `acp.allowed_agents` | `[]` | Registry keys allowed while the restriction is on, e.g. `["claude", "codex"]`. Each alias counts separately, and an empty list denies every agent. A worker on a now-disallowed agent is terminated at its next respawn. |
| `acp.rate_limit_auto_resume` | `false` | Respawn a worker parked on a provider rate limit once the reported reset passes. See [Rate-limit recovery](../structured-view/troubleshooting.md#rate-limits-and-agent-hand-off). |

The rest of `[acp]` tunes the structured view globally; see [Structured View Internals](../development/internals/structured-view.md#global-tuning-acp).

### Recovering hook-based status detection

AoE writes per-session status into `/tmp/aoe-hooks-<euid>/`. When that directory cannot be initialized (another user squatted the path, a permissive umask or ACL widened it, a symlink was planted, or `/tmp` was reaped mid-run), AoE logs the resolved path with a recovery hint and falls back to reading the pane, which costs latency but no functionality. Check it with `ls -ldn /tmp/aoe-hooks-$(id -u)`, which should be `drwx------` and owned by you. Removing the directory (`rm -rf /tmp/aoe-hooks-$(id -u)`) and starting a session recreates it, a wrong ACL clears with `setfacl -b`, and a path owned by another user has to be removed by them or by root, since `/tmp` is sticky. `aoe uninstall` removes the hooks from every agent settings file and tears the directory down, leaving status detection on pane content alone.

## Status hooks

Local shell commands run by the TUI on a status change, for desktop notifications and similar personal automation. Off by default, and global/profile only because they run arbitrary commands.

```toml
[status_hooks]
enabled = true
on_waiting = "notify-send -a aoe 'AoE: Waiting' \"$AOE_SESSION_TITLE is waiting for input\""
on_error = "notify-send -u critical -a aoe 'AoE: Error' \"$AOE_SESSION_TITLE errored\""
```

`on_starting`, `on_running`, `on_waiting`, `on_idle`, and `on_error` fire on that transition; `on_change` fires on every transition, after the status-specific command. A status must hold for a 100 ms debounce before a hook runs. Commands run in the session's project directory, are best-effort, and never block status updates or sounds.

Each command receives `AOE_SESSION_ID`, `AOE_SESSION_TITLE`, `AOE_PROJECT_PATH`, `AOE_PROFILE`, `AOE_TOOL`, `AOE_GROUP_PATH`, `AOE_OLD_STATUS`, `AOE_NEW_STATUS`, and `AOE_STATUS_CHANGED_AT`.

## Custom agents

Custom agents name commands AoE cannot detect as a built-in binary: SSH wrappers, local scripts, or a CLI pointed at a second account. Configure them once, then pick the name in the TUI picker, `aoe add --tool <name>`, or the web wizard.

```toml
[session]
default_tool = "lenovo-claude"
custom_agents = { "lenovo-claude" = "ssh -t lenovo claude" }
agent_detect_as = { "lenovo-claude" = "claude" }
```

- **`custom_agents`** maps a display name to the command AoE runs in a tmux pane.
- **`agent_detect_as`** reuses built-in status detection and ACP adapter inheritance. It never proves which native CLI owns a terminal conversation.
- **`agent_execution_as`** declares the native agent actually invoked by an opaque wrapper, together with its `agent_config_dir` store contract. This is required for managed resume and fork through such a wrapper.
  Bare, non-path wrappers can receive Default automatic resume flags and pane-scoped capture without this contract. Cleared launches selected by a failed-resume marker or disabled `auto_resume_on_restart` start fresh, mint and pass fresh-session flags where supported, and capture the new pane-scoped ID; explicit resume and fork remain unavailable. See [Execution identity and wrappers](session-resume.md#execution-identity-and-wrappers).
- **`agent_acp_cmd`** gives the agent its own ACP command (see below).
- **`agent_config_dir`** names the config directory the wrapper points its CLI at (see below).

Custom agents remain selectable even for remote or unmanaged commands. Profile maps replace global maps, so redeclare entries you want to keep. `agent_execution_as` and `agent_config_dir` are trusted global/profile declarations; repository overrides are refused. The web wizard can select a custom agent but never edits these command strings.

### One CLI, two accounts

A wrapper that runs the same CLI against a second login usually exports the agent's config-dir variable, which AoE cannot see: the wrapper sets it after AoE has chosen which file to write. Name the directory in `agent_config_dir` so folder-trust records and [native MCP discovery](mcp-servers.md) land on the config the agent actually reads, for as long as the conversation has not recorded a store of its own.

```toml
[session.custom_agents]
claude-personal = "claude-personal"      # a wrapper that exports CLAUDE_CONFIG_DIR

[session.agent_detect_as]
claude-personal = "claude"

[session.agent_execution_as]
claude-personal = "claude"

[session.agent_config_dir]
claude-personal = "~/.claude-personal"
```

The value is a host path. Host sessions use the directory itself; each sandboxed session gets a private `sandbox-v2/<instance-id>` child mounted at the agent's canonical container config path, so do not mount that tree through `sandbox.extra_volumes` and keep the config-dir variables AoE sets inside the container. A host Claude conversation that already recorded its store keeps resuming there, so repointing an entry moves new sessions only, the folder-trust record still lands in the directory named here, and a host terminal launch logs a warning naming both stores; see [Native Session Resume](session-resume.md#swapping-the-engine-on-a-restart) for the account swap that carries a conversation across.

### Status rules for custom agents

`agent_detect_as` only fits a wrapper whose output matches the built-in it aliases. For a harness that merely resembles one, declare pane rules instead:

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

Each rule sets `status` (`running`, `waiting`, `idle`, `error`) and exactly one of `contains` (case-insensitive substring) or `regex` (Rust regex; use `(?i)` for case-insensitive). Rules are matched in order against the ANSI-stripped pane; first match wins and no match reports `idle`, so put the specific states before broad ones. Rules outrank `agent_detect_as`, a built-in detector of the same name, and a status hook the agent writes. Invalid rules are skipped with a warning and take effect on the next config resolve.

### Running a custom agent in the structured view

Give the agent an ACP launch command; it must speak the [Agent Client Protocol](https://agentclientprotocol.com).

```toml
[session.custom_agents]
"oc-superpowers" = "ocp run sp"

[session.agent_acp_cmd]
"oc-superpowers" = "ocp run sp acp"
```

The value is split into argv and executed with no shell, so wrap explicitly for shell features (`sh -lc '...'`). The name must match a `custom_agents` entry and cannot shadow a built-in.

A wrapper around a supported agent needs no `agent_acp_cmd` at all: map it with `agent_detect_as` and it inherits the base agent's ACP adapter, which also lights up **Switch to structured view** on an existing terminal session. **The wrapper binary is never executed** in that case, so anything it does outside the CLI (selecting an account, gateway, or profile by setting env) does not apply. Inheritance works only for bases with a built-in adapter (`claude`, `codex`, `opencode`, `gemini`, `vibe`, `pi`, `omp`, `kimi`, `prime-agent`).

To pass those overrides to a host structured session anyway, use the session's `extra_env` or [`environment`](#host-environment). A Docker-sandboxed session reads only `sandbox.environment` and pins its own config dir at a container path, so set the container-side value there, or give the wrapper a real `agent_acp_cmd`. An explicit `agent_acp_cmd` wins if you set both.

## Agent command overrides

An override replaces the command AoE launches for an agent, which is how you run it with fixed options, through a script, or under a sandbox such as [nono](https://github.com/always-further/nono/).

```toml
[session.agent_command_override]
opencode = "nono run --profile opencode-dev --allow-cwd -- opencode"
```

Set the same thing in the TUI under **Agents**, using `<agent>=<cmd>`, or per session with `aoe add --cmd-override <CMD>`. Overrides are evaluated per-session first, then profile, then global (a repo config cannot set one).

A configured override also applies to plain `aoe add --cmd <agent>`, and the on-PATH check validates the resolved override binary, so a session works when only the wrapper is installed. Native conversation resume survives an override only when the command starts with the built-in's exact binary token, or is a single bare token, and contains no shell control syntax; see [session resume](session-resume.md).

The web wizard previews the resolved command under **More options**, including the ACP registry args a structured view session adds (`opencode acp`). Extra args are ignored for structured view sessions, so change the command override instead.

An override runs through your `$SHELL`, falling back to `bash` when `$SHELL` is unset or non-POSIX (`fish`, `nu`, `pwsh`). If your wrapper is a function or abbreviation in a non-POSIX shell, write it as a bash script or spell the command out here.

## Host environment

```toml
environment = [
    "CLAUDE_CONFIG_DIR=/Users/me/.claude-accounts/work",
    "GH_TOKEN=$AOE_GH_TOKEN",
    "TERM",
]
```

The top-level `environment` list injects variables into every host (non-sandboxed) session, in both views. Entries follow the same grammar as [`sandbox.environment`](sandbox.md#environment-variables): `KEY=value` literal, `KEY=$VAR` read from AoE's environment at spawn, `KEY=$$literal` escape, and a bare `KEY` passthrough. Keys must match `[A-Za-z_][A-Za-z0-9_]*`; anything else is dropped with a warning.

In the terminal view each pair becomes a shell-assignment prefix on the pane command and is therefore visible in `ps`; the structured view applies the list to the agent process's environment instead. For genuine secrets prefer `sandbox.environment` or a [host hook](#host-hooks). Host and sandboxed sessions read disjoint lists, so set both if a variable must be present either way. A profile's `environment` replaces the global list.

### What host sessions inherit automatically

Independent of that list, AoE forwards `DISPLAY`, `WAYLAND_DISPLAY`, `XAUTHORITY`, `DBUS_SESSION_BUS_ADDRESS`, `SSH_AUTH_SOCK`, and every `XDG_*` variable into host sessions, so a browser the agent launches (an OIDC login) can reach your desktop. Worth knowing what that grants: `DISPLAY` plus `XAUTHORITY` is X11 access to your whole session, which includes screen capture and input injection. A sandboxed session never receives them.

To forward everything else too:

```toml
[session]
inherit_host_environment = true
```

Every variable AoE holds then reaches host sessions, except `AOE_*` and `AGENT_OF_EMPIRES_*` (its own wiring) and `TERM` (owned by tmux). Off by default, since it widens what every agent can read, including tokens exported in your shell. In the terminal view the pairs ride `tmux new-session -e`, so a secret is briefly visible in `ps` while that command runs.

### When AoE has no environment to forward

Forwarding is a passthrough, not a store: AoE can only hand a session what its own process holds. A daemon started by systemd, launchd, cron, or an SSH command inherits that launcher's environment, not your shell's.

```ini
[Service]
PassEnvironment=DISPLAY XAUTHORITY XDG_RUNTIME_DIR DBUS_SESSION_BUS_ADDRESS
EnvironmentFile=%h/.config/agent-of-empires/env
```

For a user unit, populate the manager from your graphical session and restart the unit (`import-environment` does not touch running units); make it permanent with `~/.config/environment.d/*.conf`.

```bash
systemctl --user import-environment DISPLAY XAUTHORITY XDG_RUNTIME_DIR DBUS_SESSION_BUS_ADDRESS
systemctl --user restart agent-of-empires
```

To see what a running daemon can forward, read its environment: `tr '\0' '\n' < /proc/$(cat ~/.config/agent-of-empires/serve.pid)/environ` on Linux, `ps eww -o command= -p "$(cat ~/.agent-of-empires/serve.pid)"` on macOS.

## Host hooks

`[host_hooks]` commands run on the **host**, unlike `[hooks]`, which for a sandboxed session runs inside the container. They compute a value with host-only tooling and credentials and hand only that value to the agent: the canonical case is minting a short-lived, repo-scoped token, or resolving which account a session runs as at spawn time.

```toml
[host_hooks]
before_start   = ['echo "GH_TOKEN=$(my-mint-tool "$AOE_REPO_SLUG")"']  # sandboxed sessions
before_session = ['my-account-switcher env --profile "$AOE_PROFILE"']  # host sessions
```

A launch runs exactly one of them, chosen by whether the session is sandboxed. `before_start` runs when a container is created or restarted (attaching to a running container reuses the last values); `before_session` runs on every host agent launch, including a restart or a view switch, and persists nothing. A plain tool session is not an agent launch and runs neither.

Each `KEY=VALUE` line the command prints to stdout becomes an environment variable for the agent; other lines are ignored, stdout is never logged (so it is safe to print a secret), and a non-zero exit aborts the launch. A key that is a single token but not a valid identifier is dropped with a warning. Minted pairs are applied after the static `environment` list, so they win over a same-keyed entry.

For `before_start` the values reach `docker` through the process environment, never argv. In the terminal view a minted value rides `tmux new-session -e`, so it stays out of the pane command's argv, but tmux keeps it in the session environment where any client of that tmux server can read it back with `tmux show-environment`. A value that must stay out of both belongs in a sandboxed session with `before_start`.

The command's environment carries the lifecycle variables (`AOE_SESSION_ID`, `AOE_SESSION_TITLE`, `AOE_PROJECT_PATH`, `AOE_PROFILE`, `AOE_TOOL`, `AOE_GROUP_PATH`, `AOE_SESSION_BRANCH` on worktree sessions, and `AOE_REPO_SLUG`, the `owner/repo` of the project's `origin` remote). In the structured view `before_session` receives the subset available at that spawn site: `AOE_SESSION_ID`, `AOE_PROFILE`, `AOE_TOOL`, `AOE_PROJECT_PATH`. `before_start` additionally receives the session's sandbox environment, which is the per-session input channel: set `TEST_VAR=foo` in the new-session dialog's env list and the hook reads `$TEST_VAR`. That env is resolved from the session, profile, or global `sandbox.environment`, never from a repo config.

`host_hooks` is **global/profile only**: a checked-out repository must not be able to run host commands.

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
| `status_bar` | `"auto"` | Paint aoe's themed status bar (title, branch, sandbox, detach hint) on its own sessions. The bar is a whole theme, so `"auto"` steps aside whenever you have a tmux config at all. `"disabled"` reverts aoe's session-scoped `status*` overrides, so your own config governs. See [tmux status bar](tmux-status-bar.md). |
| `mouse` | `"auto"` | Set tmux `mouse` on aoe's sessions, which is what turns a wheel or touch scroll into copy-mode scrollback. `"auto"` defers only when your tmux config sets `mouse` itself, and enables it otherwise. |
| `clipboard` | `"auto"` | Forward the agent's OSC 52 clipboard writes (`set-clipboard on`, `allow-passthrough on`) to your terminal or the dashboard. Without it, "select to copy" inside an agent silently fails. Same per-option `"auto"` as `mouse`; live-send forwarding stays on for `"auto"` and `"enabled"`. |
| `socket_name` | unset | Run aoe's sessions on a private tmux server (`tmux -L <name>`), so your own `tmux ls` stays separate. Bare name only, applied at the next aoe start. Global only. |
| `vt_live` | `true` | Render agent previews and the dashboard's agent terminal from a persistent VT channel instead of `capture-pane` polling. See [the VT live transport](live-mode.md#the-vt-live-transport). |

Per-option detection reads `~/.tmux.conf`, `$XDG_CONFIG_HOME/tmux/tmux.conf`, and `~/.config/tmux/tmux.conf` for a `set` / `setw` of the option. It is deliberately conservative: an option reached through `source-file`, `if-shell`, a false `%if`, or a key binding is not detected, so set the mode to `"disabled"` if you keep yours in one of those places. `/etc/tmux.conf` is not consulted.

## Diff

```toml
[diff]
default_branch = "main"   # auto-detected when unset
context_lines = 3
split_view = false        # side-by-side instead of unified
```

## Updates

```toml
[updates]
update_check_mode = "notify"   # auto | notify | off
```

- `auto`: install a new release silently in the background through the same tarball path as `aoe update`, picked up on the next launch. Only when the install location is writable; Homebrew installs fall through to `brew upgrade`.
- `notify` (default): show the TUI banner and the CLI nag. `Ctrl+x` snoozes the banner until a newer release ships.
- `off`: no check, banner, or dashboard poll. Use it on offline networks.

Checks hit GitHub at most once a day; the dashboard re-polls the cached status hourly while open.

## Tools

`[tools.*]` defines dev tools tied to each session's working directory. Each entry takes a `command`, an optional `hotkey` (`Alt+<char>`), and optional `background = true` for fire-and-forget commands.

```toml
[tools.lazygit]
command = "lazygit"
hotkey = "Alt+g"
```

See [Tool Sessions](tool-sessions.md) for the full reference.

## Worktree and sandbox

```toml
[worktree]
enabled = false                                       # auto-enable for new sessions
path_template = "../{repo-name}-worktrees/{branch}"   # {repo-name}, {branch}, {session-id}
auto_cleanup = true

[sandbox]
enabled_by_default = false
default_image = "ghcr.io/agent-of-empires/aoe-sandbox:latest"
environment = ["GH_TOKEN=$AOE_GH_TOKEN"]
```

[Git Worktrees](worktrees.md) and [Container Sandbox](sandbox.md) document the remaining keys in each section.

## Profiles

Profiles are separate workspaces with their own sessions, groups, and overrides of profile-overridable settings.

```bash
aoe                        # "default" profile
aoe -p work                # the "work" profile
aoe profile create client-xyz
aoe profile list
aoe profile default work
```

## Repo config

Per-repo settings live in `.agent-of-empires/config.toml`; `aoe init` writes a template. A repo may set `[hooks]`, parts of `[session]`, `[sandbox]`, and `[worktree]`, and nothing else. See [Repo Config & Hooks](repo-config.md) for which keys are accepted and why the rest are not.
