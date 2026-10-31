# Configuration Reference

Settings resolve in layers, each overriding the one before it, field by field:

1. **Global**: `~/.agent-of-empires/config.toml`
2. **Profile**: `~/.agent-of-empires/profiles/<name>/config.toml`
3. **Repo**: `.agent-of-empires/config.toml` in the project root

Unset fields inherit from the layer above. List fields replace rather than extend. Everything below is also editable from the TUI settings screen (`s`) and, unless noted, from the web dashboard.

Over the web API, `GET /api/settings` returns the settings as they apply to the profile the server serves (or `?profile=<name>`): the profile's overrides over the global config. `PATCH /api/settings` saves each field to the layer it belongs in, that profile for fields it can override and the global config for the rest, so a save always lands where the read looks. Add `?layer=machine` to either to read or write the global config alone.

Global-only settings use the global config. On upgrade, values from the default profile move there and copies in other profiles are removed. The profile settings API rejects global-only fields; use `PATCH /api/settings` instead.

A project registry entry can override `worktree.enabled`, `session.smart_rename` and `sandbox.enabled_by_default` for that project from the web Projects view or the TUI add-project form. Project overrides win over all three configuration layers. They live in the user's `projects.json`, not the repository, so they do not weaken these fields' `repo = "deny"` policy.

## File locations

| Platform | Global config |
|----------|--------------|
| Linux | `$XDG_CONFIG_HOME/agent-of-empires/config.toml` (default `~/.config/agent-of-empires/`) |
| macOS | `~/.agent-of-empires/config.toml`, or the XDG path under the precedence below |

`XDG_CONFIG_HOME` must be absolute; empty or relative values use `~/.config` as the XDG base. On macOS, AoE first uses an existing XDG app directory, then an existing `~/.agent-of-empires/`. If neither exists, an absolute `XDG_CONFIG_HOME` selects the XDG path; otherwise AoE uses `~/.agent-of-empires/`. Nothing moves automatically.

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

### `state.toml`

`state.toml` sits alongside `config.toml` and holds global-only runtime/UI bookkeeping, such as the welcome tour, last browse directory, sort order, and dismissed tips and updates. It has no profile or repo layer and is not part of the settings schema. `GET /api/settings` exposes `app_state.*` for read-only compatibility; `PATCH /api/settings` rejects those fields with HTTP 400.

It also holds the one-time [agent hook approval](#agent-hook-approval), set by `aoe hooks approve` or accepted in the TUI, and hand-editable.

Updates use a locked read-modify-write operation, preserving concurrent changes. Its separate lock keeps bookkeeping writes from contending with configuration saves.

## Environment Variables

| Variable | Description |
|----------|-------------|
| `AGENT_OF_EMPIRES_PROFILE` | Default profile |
| `AOE_LOG_LEVEL` | File log level for AoE's own targets: `trace`, `debug`, `info`, `warn`, `error`. Dependency crates stay at `warn`. One-shot CLI commands always log to the same file at the configured level once the app directory exists; `[logging].output = "stdout"` applies to a foreground `aoe serve` only |
| `AGENT_OF_EMPIRES_DEBUG` | Legacy alias for `AOE_LOG_LEVEL=debug` |
| `AOE_DEFER_SANDBOX_MIGRATION` | Start without moving sandboxed sessions' agent stores; retried later or by `aoe migrate`. See [Per-session agent stores](sandbox.md#per-session-agent-stores) |

## Theme

```toml
[theme]
name = "default"          # see the picker or `aoe theme list`
color_mode = "truecolor"  # truecolor | palette (TUI only)
```

| Option | Default | Description |
|--------|---------|-------------|
| `name` | `"default"` | Theme for the TUI and web dashboard. Builtins: `default`, `empire`, `phosphor`, `tokyo-night-storm`, `catppuccin-latte`, `dracula`, `rose-pine`, `deep-ocean`. Custom TOML themes appear in the picker. Empty means `default`. |
| `color_mode` | `"truecolor"` | TUI only. `palette` downsamples to xterm-256 for transports that mangle 24-bit RGB (e.g. some `mosh` setups). The web dashboard always renders truecolor. |

Custom themes are TOML files in `<app_dir>/themes/`, listed in the picker under their filename stem. Run `aoe theme dir` for the active directory; see [File Locations](#file-locations) for Linux and macOS resolution.

```bash
aoe theme export empire   # writes custom-empire.toml in the active themes directory
aoe theme list
aoe theme dir
```

Every field is optional. Missing colors fall back to the Empire baseline, except `unread` and `favorite`, which inherit the theme's own `accent`. An omitted `appearance` or `[syntax].shiki_theme` is derived from the theme's background luminance. `appearance = "dark" | "light"` and `[syntax].shiki_theme` (any id [Shiki bundles](https://shiki.style/themes)) drive the dashboard's surface ramp and code highlighting. An unknown `shiki_theme` falls back to `github-dark` or `github-light` by appearance.

## Session

```toml
[session]
default_tool = "claude"
yolo_mode_default = false
agent_status_hooks = true
smart_rename = true
name_agent_session = false
auto_stop_idle_secs = 0   # 0 disables; e.g. 7200 = stop after 2h idle
row_tag = "branch"        # none | auto | profile | sandbox | branch
sidebar_position = "left" # left | right; TUI session list
```

After your 31st session creation, a one-time tip explains how to hide the shortcut bar.
It waits until the home screen is free, respects **Show tips**, and never changes the setting.
You can browse tips from **Ctrl+K > Show tips** even with the bar hidden.

| Option | Default | Description |
|--------|---------|-------------|
| `default_tool` | (auto-detect) | Default agent for new sessions. Falls back to the first available tool if unset or unavailable. Can be set to a custom agent name. |
| `auto_stop_idle_secs` | `0` | Seconds a plain tmux session may stay `Idle` before its tmux session and sandbox container are stopped, leaving a restartable `Stopped` row. `0` disables it. Idle age starts at the later of the last `Idle` transition and last interaction; an attached tmux client spares the session. The Daemon checks about once a minute. Structured workers use `acp.auto_stop_idle_secs`. |
| `startup_recovery_wake` | `false` | Send `restart_wake_message` after automatic recovery of a missing terminal agent pane at Daemon startup, including after a reboot. Off still restores the pane, honoring native resume settings, without sending a new prompt. Explicit CLI, TUI, and API restarts keep their existing behavior. Global and profile settings only; repository overrides cannot enable it. |
| `restart_wake_message` | `"wake up: pick up what you were doing"` | Wake text for explicit restarts and opted-in startup recovery. An empty string disables the message even when `startup_recovery_wake` is on, without disabling recovery or native resume. Custom text is sent unchanged. |
| `prevent_sleep_when_active` | `false` | Have the Daemon inhibit user-idle system sleep while sessions are active; the display can sleep. Global only. Uses `caffeinate -i` or `systemd-inhibit --what=idle:sleep`; missing tools or logind warn once and skip inhibition. |
| `prevent_sleep_idle_grace_minutes` | `15` | Minutes a session must stay idle before the sleep-inhibit assertion may be released. Only consulted when `prevent_sleep_when_active` is on. Range `0` to `240`; `0` releases as soon as every session leaves an active status. The grace period only begins once a session goes `Idle`, so a session that never reaches `Idle` keeps holding the assertion: `Waiting` on an unanswered prompt, or `Creating` on a container, worktree, or submodule setup that never returns, can hold sleep indefinitely. A `Starting` session is bounded by a short (~3s) launch guard and then re-resolves. |
| `session_id_poller_max_threads` | `50` | Ceiling on concurrent native-session-ID pollers per process. Overflow IDs stop refreshing until a poller is available, with a 5 s to 60 s retry backoff. `0` keeps the default. Global only; applied when the Daemon starts. |
| `row_tag` | `"branch"` | Controls the compact metadata shown next to each TUI session title: `none` shows nothing; `auto` shows the profile code only in all-profiles view; `profile` always shows the profile code; `sandbox` shows `sb` on sandboxed sessions; `branch` shows a compact worktree or workspace branch tag. |
| `show_activity_age` | `true` | Show the age column at the right edge of each TUI session row: time since the agent stopped on `Idle` rows, time since last access on `Unknown` rows, and remaining snooze time under the Attention sort. |
| `sidebar_position` | `"left"` | Places the TUI session list on the left or right. Narrow terminals keep the list above the preview. Global only. |
| `show_shortcut_bar` | `true` | Show the TUI's bottom shortcut row, including the LIVE banner. Global only, under Settings > Global > Interaction. Hiding it gives that row to the main view; shortcuts, `?` help, dialog controls, and temporary notifications still work. |
| `preview_link_prefixes` | `[]` | Link targets beyond http and https that a click in the preview may open, as case-insensitive `scheme://authority/` prefixes, for example `["vscode://file/"]` to open file links in VS Code. Entries not in that form, including a missing `/` after the authority, are ignored, and `file:`, `javascript:`, `data:` and `vbscript:` never open. Unlisted targets are neither opened nor passed to the terminal; a new entry applies to links printed after it. Global only and not editable from the web dashboard; under Settings > Global > Interaction (advanced). |
| `tie_workdir_to_name` | `true` | Keep a managed worktree session's directory named after its title. See [Worktrees](worktrees.md#naming). |
| `yolo_mode_default` | `false` | Enable YOLO mode by default for new sessions (skip permission prompts). Works with or without sandbox. In tmux mode this passes `--dangerously-skip-permissions` to the agent CLI; in structured view it maps to ACP `bypassPermissions` (see [Structured view: Permission modes and YOLO](../structured-view/controls.md#permission-modes-and-yolo) for the adapter caveat). |
| `pre_trust_agent_folders` | `false` | Pre-trust host worktrees in Claude Code, Codex and Gemini config. Config-dir overrides are honored; `agent_config_dir` wins. The record outlives the session. Trust activates the repo's `.claude/settings.json`, including hooks and permission rules, so enable it only for directories you would trust by hand. MCP servers still ask separately and permission mode is unchanged. Sandboxed sessions pre-trust their staged config. |
| `agent_status_hooks` | `true` | Install status-detection hooks into the agent's config file; see [Agent hook approval](#agent-hook-approval) for the approval that gates the write and [Adding a New Agent](../development/adding-agents.md#hook-format-reference) for the per-agent file and payload. Config-dir overrides are honored. When disabled, status detection reads the pane alone, but any authoritative identity hooks declared for native resume remain installed. A hook write is evidence rather than the last word: pane state and terminal title are weighed against it by declared priority. |
| `opencode_preassign_session_id` | `false` | Pre-assign OpenCode's native session id before a new host launch. AoE starts a short-lived `opencode serve`, creates the session through `POST /api/session`, then launches `opencode --session <id>`. This avoids guessing from OpenCode's shared SQLite store, at the cost of about two seconds per new session. When disabled or unavailable, automatic capture stays off; an explicitly pinned exact id still resumes. Sandboxed OpenCode automatic capture is unsupported. |
| `smart_rename` | `true` | Auto-rename a default-named session after its first completed turn. Structured sessions use their first-turn transcript; terminal sessions use the pane after a Running-to-Idle transition. A manually named session is never overwritten. Only the title changes, not the worktree directory. Sandboxed sessions run the one-shot inside their own running container; unsupported agents, command overrides, or a different sandbox rename agent prevent naming. A failed or timed-out call leaves the generated name. Daemon-owned naming jobs finish before daemon shutdown releases ownership. |
| `smart_rename_agent` | `""` | Agent for smart-name and conversation-summary one-shots. Empty selects the session's own agent. A named agent must support one-shot mode; unknown or unsupported agents are rejected rather than replaced. Sandboxed sessions must use their own agent because only its credentials are mounted. |
| `smart_rename_model` | `{}` | Per-agent rename model, e.g. `{ claude = "haiku" }`. Missing uses the agent's built-in default; empty forces its CLI default; another value passes its model flag. Invalid IDs leave the generated name. Applies only to one-shot agents with a model flag, not conversation summaries. The web dashboard sets default or pinned models; use the TUI or file for an empty value. |
| `name_agent_session` | `false` | For a session whose title was typed in the TUI's New Session dialog, give its title at the first launch (including a rename in AoE before then) to the agent as its own session name, so it shows in the agent's own apps (Claude's `--name`). For a structured session that is its first terminal launch. Skipped for a generated or suggested title, a title starting with `-`, a launch that resumes a conversation, sandboxed sessions, a launch whose command, arguments or `PATH` AoE cannot attest (including your own `-n`/`--name`), and when the agent's `--help` does not list the flag. |
| `inherit_host_environment` | `false` | Forward AoE's whole environment to host sessions. See [Host environment](#host-environment). |
| `agent_extra_args` | `{}` | Per-agent arguments appended after the binary, e.g. `{ opencode = "--port 8080" }`. Ignored for structured view sessions. |
| `agent_command_override` | `{}` | Per-agent command replacing the binary. Managed resume and fork validate the actual native command and store; opaque wrappers require an explicit execution contract. See [execution identity and wrappers](session-resume.md#execution-identity-and-wrappers). |
| `custom_agents` | `{}` | User-defined agents (name to command). See [Custom agents](#custom-agents). |
| `agent_detect_as` | `{}` | Built-in status detection and ACP adapter inheritance for a custom agent. This does not prove native execution or authorize terminal resume or fork. |
| `agent_execution_as` | `{}` | Explicit native-agent contract for an opaque terminal wrapper. Requires `agent_config_dir` naming its store. Trusted global/profile configuration only; repository overrides are refused. |
| `agent_acp_cmd` | `{}` | ACP launch command for a custom agent, enabling it to run in structured view (for example, `{ "oc-superpowers" = "ocp run sp acp" }`). A custom agent with an entry here is structured view-capable; without one it stays tmux-only. Unlike `custom_agents`, the value is split into argv and run directly, with no shell. |
| `agent_config_dir` | `{}` | Agent config directory, keyed by agent name. Host sessions use it directly; sandboxed sessions use a private `sandbox-v2/<instance-id>` child at the canonical container path. Hooks and [native MCP discovery](mcp-servers.md) use it, and it wins over config-dir environment variables and host hooks. A recorded Claude conversation store outranks it; changing accounts or recreating a sandbox follows the [resume store rules](session-resume.md#swapping-the-engine-on-a-restart). Global/profile only. |
Per-agent structured view defaults live under `[acp]`, not `[session]`:

| Option | Default | Description |
|--------|---------|-------------|
| `acp.structured_view` | `structured` | `off`, `terminal`, or `structured`. `off` hides the new-session structured toggle and terminal-to-structured switch in both UIs. The other values offer and preselect that view; per-session choice wins. The CLI defaults to terminal; `--structured-view` and `--agent` still opt in with `off`. Structured sessions run the ACP adapter, not Claude's Remote Control app. |
| `acp.acp_defaults.<agent>` | `{}` | `model`, `effort` (thinking level), `mode`, and an `effort_by_model` map applied when a worker starts. `effort` and `mode` go through the agent's ACP config options when advertised and are skipped with a warning otherwise. A model chosen at creation wins over `model`, unless `pin_model = true` makes it a pin: creation then refuses any other model (`kind = "model_pinned"`) and the dashboard's picker collapses to it. The entry is keyed by the agent the session spawns as, so a custom agent reads its `agent_detect_as` base's entry. |
| `acp.restrict_agents` | `false` | Restrict structured view sessions to `acp.allowed_agents`. Read from the global config only, so a profile cannot widen it; changing it from the web needs the passphrase step-up. |
| `acp.allowed_agents` | `[]` | Registry keys allowed while the restriction is on, e.g. `["claude", "codex"]`. Each alias counts separately, and an empty list denies every agent. A worker on a now-disallowed agent is terminated at its next respawn. |
| `acp.rate_limit_auto_resume` | `false` | Respawn a worker parked on a provider rate limit once the reported reset passes. See [Rate-limit recovery](../structured-view/troubleshooting.md#rate-limits-and-agent-hand-off). |

Agent status overrides use `[agents.<name>]`:

| Option | Default | Description |
|--------|---------|-------------|
| `agents.<name>.status_map` | `{}` | Trusted global/profile-only hook event to AoE status mappings. Valid statuses are `running`, `waiting`, `idle`, and `error`. Entries apply by event name to built-in hook defaults, so duplicate event names with different matchers all receive the same status; new event names are added to the installed hooks when the agent format supports event keys. Existing hook files update on the next hook install, usually a new or restarted session. Agent processes with installed status hooks receive `AOE_PROFILE`, so hook scripts can query the resolved map with `aoe -p "$AOE_PROFILE" profile show --status-map <agent> --json`. |
| `agents.<name>.status_rules` | `[]` | Trusted global/profile-only declarative pane status rules (`[[agents.<name>.status_rules]]` array of tables). Each rule has `status` (`running`, `waiting`, `idle`, or `error`) and exactly one of `contains` (case-insensitive substring) or `regex` (Rust regex, matched as written; use `(?i)` for case-insensitive). Rules are evaluated in order against the ANSI-stripped pane snapshot; first match wins, no match reports `idle`. Rules take precedence over `agent_detect_as`, over a built-in detector of the same name, and over a status hook the agent writes. Invalid rules are skipped with a warning in the debug log. Takes effect on the next config resolve (TUI or daemon start). |

AoE writes per-session status into `/tmp/aoe-hooks-<euid>/`. If that directory cannot be initialized, AoE logs the resolved path and falls back to pane detection. Check it with `ls -ldn /tmp/aoe-hooks-$(id -u)`; it must be owned by you with mode `0700`.

Stop the daemon and any running AoE TUIs before repairing or removing the directory. Remove an unwanted ACL with `setfacl -b /tmp/aoe-hooks-$(id -u)` where available, then restore mode `0700`. If the path must be recreated, remove it with `rm -rf /tmp/aoe-hooks-$(id -u)`; a path owned by another user requires that user or root because `/tmp` is sticky. Restart AoE and the affected sessions after repair. Initialization failures and opened directory descriptors are cached for each process's lifetime; starting a new session alone does not reset them.

`aoe uninstall` removes the hooks from agent settings files and tears down the directory, leaving status detection on pane content alone.

## Status hooks

Status hooks run local shell commands when the Daemon publishes a session status change, including while no TUI is open or a TUI is attached to tmux. They are disabled by default and are intended for personal machine behavior such as desktop notifications. The initial snapshot does not replay existing statuses.

```toml
[status_hooks]
enabled = true
on_waiting = "notify-send -a aoe 'AoE: Waiting' \"$AOE_SESSION_TITLE is waiting for input\""
on_error = "notify-send -u critical -a aoe 'AoE: Error' \"$AOE_SESSION_TITLE errored\""
```

| Option | Default | Description |
|--------|---------|-------------|
| `enabled` | `false` | Run configured status hook commands from the Daemon. Commands fire once a status has stayed stable for a short built-in debounce (100ms), so rapid flickers do not spam hooks. |
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
| `AOE_PROFILE` | Session profile |
| `AOE_TOOL` | Agent name |
| `AOE_GROUP_PATH` | Group hierarchy path |
| `AOE_OLD_STATUS` / `AOE_NEW_STATUS` | Status before/after the transition |
| `AOE_STATUS_CHANGED_AT` | Timestamp when the Daemon observed the published transition |

When both a status-specific hook and `on_change` fire, AoE runs them sequentially (status-specific first), outside session locks. Each command has a 30-second timeout and owns its process group; remaining group members are killed when it exits. Stopping the daemon cancels pending hooks and terminates running hook groups. Disabling or changing the profile hook configuration, removing a session, or moving it to another profile invalidates its debounced transition. Hooks use the session profile's effective global and profile settings, never repo config, because they run arbitrary local commands.

## Agent hook approval

`agent_status_hooks` (above) makes AoE write hook entries into the agent's own config, which lives under your home directory unless `agent_config_dir`, the profile `environment`, or a config-dir variable exported in the launching shell moves it, so status comes from the agent reporting it rather than from reading its pane. That writes into files you own and runs a command whenever the agent fires a hook, so it is gated behind a one-time approval. The TUI offers it as a dialog when you create a session that writes to the host; `aoe hooks approve` is the same approval for a launch with no TUI, and `aoe hooks status` reports the current answer alongside the files and hook events the effective profile resolves. That list is a disclosure, not a manifest: a launch that routes through a native store, merges into a selected agent, or targets a selected or recorded Claude conversation store resolves its own target at launch time.

The approval is per installation and is not bound to a profile, so every profile and every agent resolves its own paths under it. One agent's hooks also change launcher state: installing Kiro hooks may run `kiro-cli agent set-default aoe-hooks`, which Kiro keeps as its persistent default, so it affects later Kiro sessions including ones outside AoE. `aoe hooks status` prints that next to the files. There is no revoke command: set `has_acknowledged_agent_hooks = false` in `<app_dir>/state.toml` to take it back. A sandboxed session stages its hooks inside its own container config and is never gated. It is not the repo trust gate: `aoe add --trust-hooks` covers the hooks a repository declares in `.agent-of-empires/config.toml` and its project-local MCP servers, which are a separate decision. See [Hook Trust System](repo-config.md#hook-trust-system).

Turning `agent_status_hooks` off stops AoE installing status hooks, but it does not end the gate: identity hooks, which native resume depends on, stay installed. How many agents remain gated then depends on which ones declare an identity event, so check `aoe hooks status` for the list under a given profile. A session launched with its own command resolves the file that command names, which the TUI creation dialog describes exactly. `aoe hooks status` has neither a session nor a project directory: it resolves each tool from the profile config, so for a repository that sets `session.agent_detect_as` it can name a different agent than the same session launched inside that repository.

### Custom Agents

Custom agents name commands AoE cannot detect as built-in binaries: SSH wrappers, local scripts, or a CLI using another account. Configure them once, then select the name in the TUI, `aoe add --tool <name>`, or the web wizard.

```toml
[session]
default_tool = "lenovo-claude"
custom_agents = { "lenovo-claude" = "ssh -t lenovo claude" }
agent_detect_as = { "lenovo-claude" = "claude" }
```

- **`custom_agents`** maps a display name to the command AoE runs in a tmux pane.
- **`agent_detect_as`** reuses built-in status detection and ACP adapter inheritance. It never proves which native CLI owns a terminal conversation. Use [status rules](#status-rules-for-custom-agents) when output differs from the built-in.
- **`agent_acp_cmd`** (optional): ACP launch command that lets the agent run in the structured view (see below).
- **`agent_execution_as`** declares the native agent actually invoked by an opaque wrapper, together with its `agent_config_dir` store contract. This is required for managed resume and fork through such a wrapper.
  Bare, non-path wrappers can start fresh and capture their pane-scoped published ID without this contract. Resuming the recorded conversation requires the contract. See [Execution identity and wrappers](session-resume.md#execution-identity-and-wrappers) for wrapper contracts and [Pinning or resetting a conversation](session-resume.md#pinning-or-resetting-a-conversation) for failed-resume recovery.
- **`agent_config_dir`** (optional): The config directory the wrapper points its CLI at, when that is not the built-in default (see below).
- **`default_tool`** can select a custom-agent name for new sessions.

Custom agents remain selectable even for remote or unmanaged commands. Maps are editable in config files or the TUI. Profile maps recursively merge with global maps; scalar and array values replace global values. `agent_detect_as` also permits repo overrides. `agent_execution_as` and `agent_config_dir` are trusted global/profile declarations, never repo overrides. The web wizard can select a custom agent but never edits these command strings.

### One CLI, two accounts

A wrapper selecting another login may export a config-dir variable after AoE has chosen its hook files. Name that directory in `agent_config_dir` so folder trust and [native MCP discovery](mcp-servers.md) use the agent's actual config, until the conversation records its own store.
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

Host structured sessions export the entry as the agent's config-dir variable (`CLAUDE_CONFIG_DIR`, `CODEX_HOME`, `OPENCODE_CONFIG_DIR`, `PI_CODING_AGENT_DIR`), the same as terminal sessions, so the wrapper does not need to. To use another account, log in once with that variable set (for example `CLAUDE_CONFIG_DIR=~/.claude-personal claude auth login`) and name the directory here.

The value is a host path. Host sessions use the directory itself. Each
sandboxed session uses a separate `sandbox-v2/<instance-id>` child that AoE
creates and mounts at the resolved built-in config path. Do not mount the
`agent_config_dir` tree or one of its `sandbox-v2` children through
`extra_volumes`: a shared manual mount bypasses per-instance isolation.
The wrapper must read the resolved built-in config path inside the container.
Once a host Claude conversation has recorded a store, repointing the entry moves new
sessions only: the folder-trust record still lands in the directory named here, and a
host terminal launch logs a warning naming both stores. See [Native Session
Resume](session-resume.md#swapping-the-engine-on-a-restart) for the account swap that carries
a conversation across.
Host sessions still need `pre_trust_agent_folders`. Sandboxed sessions seed
folder trust and install status and identity hooks in their own staged config
tree, including Pi's pane-scoped identity extension.

### Status rules for custom agents

`agent_detect_as` fits wrappers whose output matches a built-in. For a different harness, declare pane status rules instead:

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

Put specific states such as `waiting` and `error` before broad `running` matches. See [`agents.<name>.status_rules`](#session) for the rule format and precedence.

#### OMP terminal status

Built-in OMP detection reads the visible pane and locates activity, pinned errors,
and blocking dialogs relative to the current composer. Question dialogs remain
waiting when paging hides the selected option; session pickers accept bracketed
key legends. Extension status rows, including plugin badges, do not need
agent-specific matching rules.
Editable composer text is excluded from dialog controls, even without status segments.

A recognized working terminal title supplements visible controls; it does not
override a blocking dialog or pinned error. Visible retry and compaction controls
can indicate activity even when the turn timer is stopped. Disabling OMP title
updates or terminal program-status reports does not disable visible-control detection.

Untimed interrupt hints must start in the native two-space key column; alignment
padding alone does not supply activity evidence.
Custom editors or widgets can hide or replace these controls. Without a recognizable
turn timer or working title, passive text that exactly reproduces a loader row
remains indistinguishable from that control. Use the explicit status rules above
for such interfaces; they remain authoritative, including their no-match behavior.

### Running a custom agent in the structured view

Give the agent an ACP launch command; it must speak the [Agent Client Protocol](https://agentclientprotocol.com).

```toml
[session.custom_agents]
"oc-superpowers" = "ocp run sp"

[session.agent_acp_cmd]
"oc-superpowers" = "ocp run sp acp"
```

The value is split into argv and executed with no shell, so wrap explicitly for shell features (`sh -lc '...'`). A matching `custom_agents` entry is needed only for the agent picker, not for an explicit ACP switch target. Built-in names take precedence.

A wrapper around a supported agent needs no `agent_acp_cmd` at all: map it with `agent_detect_as` and it inherits the base agent's ACP adapter, which also lights up **Switch to structured view** on an existing terminal session. **The wrapper binary is never executed** in that case, so anything it does outside the CLI (selecting an account, gateway, or profile by setting env) does not apply. Inheritance works only for bases with a built-in adapter (`claude`, `codex`, `opencode`, `gemini`, `vibe`, `pi`, `omp`, `kimi`, `prime-agent`).

To pass those overrides to a host structured session anyway, use the session's `extra_env` or [`environment`](#host-environment). A Docker-sandboxed session reads only `sandbox.environment` and pins its own config dir at a container path, so set the container-side value there, or give the wrapper a real `agent_acp_cmd`. An explicit `agent_acp_cmd` wins if you set both.

## Agent command overrides

An override replaces the command AoE launches for an agent, which is how you run it with fixed options, through a script, or under a sandbox such as [nono](https://github.com/always-further/nono/).

```toml
[session.agent_command_override]
opencode = "nono run --profile opencode-dev --allow-cwd -- opencode"
```

Set the same thing in the TUI under **Agents**, using `<agent>=<cmd>`, or per session with `aoe add --cmd-override <CMD>`. Overrides are evaluated per-session first, then profile, then global (a repo config cannot set one).

A configured override also applies to plain `aoe add --cmd <agent>`, and the on-PATH check validates the resolved override binary, so a session works when only the wrapper is installed. For native resume and fork behavior, including wrappers and automatic restarts, see [execution identity and wrappers](session-resume.md#execution-identity-and-wrappers).

The web wizard previews the resolved command in its **Agent** panel, including the ACP registry args a structured view session adds (`opencode acp`). Extra args are ignored for structured view sessions, so change the command override instead.

Host launches in Terminal view run overrides through your `$SHELL`, falling back to `bash` when `$SHELL` is unset or non-POSIX (`fish`, `nu`, `pwsh`). If your wrapper is a function or abbreviation in a non-POSIX shell, write it as a script or spell the command out here.

When an override applies to an ACP adapter, AoE splits it into a program and arguments and launches the program directly, without a shell. Shell expansions, assignments and operators are not evaluated. Use [environment settings](#host-environment), a wrapper script or an explicitly invoked shell when needed. Overrides do not replace every adapter: they apply only when the adapter command matches the agent's native binary.

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

Native host Codex launches add `--no-daemon` only when the attested direct
executable advertises it. The probe uses that frozen executable and environment,
including the profile's `PATH`; it does not resolve another installation.
Wrappers, sandboxed launches and unattested executables are not probed for this flag.

### What host sessions inherit automatically

Independent of that list, AoE forwards `DISPLAY`, `WAYLAND_DISPLAY`, `XAUTHORITY`, `DBUS_SESSION_BUS_ADDRESS`, `SSH_AUTH_SOCK`, and every `XDG_*` variable into host sessions, so a browser the agent launches (an OIDC login) can reach your desktop. Worth knowing what that grants: `DISPLAY` plus `XAUTHORITY` is X11 access to your whole session, which includes screen capture and input injection. A sandboxed session never receives them.

To forward everything else too:

```toml
[session]
inherit_host_environment = true
```

Every variable AoE holds then reaches host sessions, except `AOE_*` and `AGENT_OF_EMPIRES_*` (its own wiring) and `TERM` (owned by tmux). Off by default, since it widens what every agent can read, including tokens exported in your shell. In the terminal view the pairs ride `tmux new-session -e`, so a secret is briefly visible in `ps` while that command runs.

Managed OpenCode selects this policy before probing or launching the native agent. It also retains process essentials and OpenCode database, configuration and workspace selector inputs for validation when wholesale inheritance is off. Unsupported selectors remain unsupported. Explicit profile `environment` entries and before-session overrides take precedence; login-only exports are not imported. See [managed OpenCode contexts](session-resume.md#supported-managed-contexts).

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

To inspect what the running Daemon inherited on Linux:

```bash
tr '\0' '\n' < /proc/$(cat ~/.config/agent-of-empires/serve.pid)/environ
```

macOS has no `/proc`, so use `ps` there:

```bash
ps eww -o command= -p "$(cat ~/.agent-of-empires/serve.pid)"
```

## Worktree

`[worktree]` controls automatic worktree creation for new sessions:

```toml
[worktree]
enabled = false                                       # auto-enable worktrees for new sessions
path_template = "../{repo-name}-worktrees/{branch}"   # template vars: {repo-name}, {branch}, {session-id}
auto_cleanup = true                                   # prompt to remove the worktree on session delete
```

See [Git Worktrees](worktrees.md) for the remaining keys and path templates.

## Sandbox (Docker)

`[sandbox]` configures container sandboxing:

```toml
[sandbox]
enabled_by_default = false                                    # auto-enable sandbox for new sessions
default_image = "ghcr.io/agent-of-empires/aoe-sandbox:latest" # container image
environment = ["GH_TOKEN=$AOE_GH_TOKEN"]                      # env vars forwarded into the container
```

See [Container Sandbox](sandbox.md) for options, environment grammar, credentials and storage isolation. Host sessions use the separate [Host Environment](#host-environment) list.

## Host Hooks

`[host_hooks]` commands run on the **host**, including for sandboxed sessions. They can mint short-lived, repo-scoped credentials or choose an account at launch without exposing host tooling to the container.

```toml
[host_hooks]
before_start   = ['echo "GH_TOKEN=$(my-mint-tool "$AOE_REPO_SLUG")"']  # sandboxed sessions
before_session = ['my-account-switcher env --profile "$AOE_PROFILE"']  # host sessions
before_destroy = ['sync-transcripts "$AOE_SESSION_ID"']                 # any session, at purge
```

A launch runs exactly one of `before_start` (sandboxed) and `before_session` (host), matching the split between `sandbox.environment` and the top-level [`environment`](#host-environment) list.

`before_destroy` is not a launch hook: it runs on the host for every session, sandboxed or not, when the session is purged, before container and worktree cleanup. With `session.delete_to_trash` on (the default), moving a session to trash does not run it; it runs when retention expires, on `empty-trash`, or on `aoe remove --purge`. Its output is ignored and never logged. A failure is logged without blocking removal, and a command still running after 2 minutes is killed.

`before_start` runs when a container is created or restarted, including after a runtime restart. An already-running container reuses its last values, backfilling only when none are stored. Stdout `KEY=VALUE` lines become inherited container environment variables, never Docker argv; other lines are ignored and stdout is never logged. A non-zero exit aborts startup. Invalid identifier keys are warned and dropped; diagnostic keys containing spaces are ignored.

Sandbox preparation retains its durable lifecycle reservation while host hooks run, but releases title, lifecycle and store-transition flocks, including native-content admission. Native HTTP and WebSocket entrypoints release their API locks too, so hooks can call the Daemon. Competing lifecycle operations receive a conflict. Before runtime effects, AoE reacquires locks, revalidates the reservation and session, and re-admits native content.

`before_session` runs before every host agent launch, including restart and a view switch that respawns it. It uses the same stdout and failure contract as `before_start`, and persists no values between launches.

Minted pairs override the static `environment` list in both views. Structured view uses process environment; terminal view uses `tmux new-session -e` and drops conflicting shell-assignment prefixes.

A minted terminal value stays out of the pane command's argv, unlike static `environment` entries. It remains readable through `tmux show-environment` by any client of that server. For values that must stay out of both argv and tmux environment, use a sandboxed session with `before_start`.

For example, an account switcher can print `CLAUDE_CONFIG_DIR=/absolute/account/path` and an endpoint for the account selected at launch.

Scope note: `before_session` applies to the **agent** launch, matching the static `environment` list. A plain tool session (the extra shell terminal in a session) is not an agent launch and does not run it.

The command's environment carries:

- **Lifecycle variables:** `AOE_SESSION_ID`, `AOE_SESSION_TITLE`, `AOE_PROJECT_PATH`, `AOE_PROFILE`, `AOE_TOOL`, `AOE_GROUP_PATH`, `AOE_SESSION_BRANCH` for worktrees, and `AOE_REPO_SLUG` when the origin remote parses as `owner/repo`. Structured `before_session` receives the available subset: ID, profile, tool and project path.
- **Sandbox input** (`before_start` only): the session's environment, or its effective profile/global `sandbox.environment`. Set per-session values in the new-session dialog. Repo config cannot supply this host-hook input. `aoe add --sandbox --hook-env KEY=VALUE` (repeatable), or the Host hook env list in the TUI dialog (or the web wizard), adds hook-only input that wins a same-key entry and never enters the container; a value the container needs must be printed by the hook as `KEY=VALUE`.

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
| `status_bar` | `"auto"` | Paint AoE's themed status bar on its sessions. `"auto"` defers whenever you have a tmux config; `"enabled"` always paints it. `"disabled"` reverts AoE's session-scoped `status*` overrides so your config governs, rather than hiding the bar. |
| `mouse` | `"auto"` | Enable copy-mode scrolling on AoE sessions. `"auto"` defers only when your tmux config sets `mouse`, and enables it otherwise; `"enabled"` always enables it and `"disabled"` disables it. |
| `clipboard` | `"auto"` | Forward OSC 52 clipboard writes to your terminal or dashboard using `set-clipboard on` and `allow-passthrough on`. `"auto"` defers to each option in your tmux config; `"enabled"` always sets them. Live-send forwarding stays on for both modes regardless of tmux config. `"disabled"` turns forwarding off. |

Detection for the per-option modes (`mouse`, `clipboard`) reads `~/.tmux.conf`, `$XDG_CONFIG_HOME/tmux/tmux.conf`, and `~/.config/tmux/tmux.conf`, looking for a `set` / `setw` of the option. It is deliberately conservative: an option reached via `source-file`, wrapped in `if-shell`, guarded by a false `%if`, or set from inside a key binding (`bind m set -g mouse`) is not detected, and aoe applies its own value. Set the mode to `"disabled"` if you keep yours in one of those places. `/etc/tmux.conf` is not consulted; it is not your file.
| `socket_name` | unset | Private tmux server name (`tmux -L <name>`), separating AoE from hand-managed sessions. Leave unset to share the default server. Bare names only; `/` or `\` is ignored. Applied at the next AoE start. Global/profile only. |
| `vt_live` | `true` | Render agent previews and the web dashboard's agent terminal from a persistent VT channel: `tmux pipe-pane` streams the pane into an in-process terminal grid, and on tmux 3.8 or newer keystrokes go back over the same socket (older tmux keeps `send-keys`). The paired host and container shells and every fallback keep tmux's rendered `capture-pane` snapshots to avoid prompt-paint races; there a raw observer retains OSC 52 clipboard forwarding without affecting rendering. A split window is composited from `capture-pane` snapshots, with the TUI preview keeping pane 0 on its VT grid. Disabling this setting puts every surface on the slower `capture-pane` / `send-keys` path. Applies in place on the next TUI capture cycle. |

## systemd

```toml
[systemd]
enabled = true
slice = "background-aoe-{profile}.slice"
survive_daemon_restart = true
# memory_max = "4G"
tasks_max = 0
```

| Option | Default | Description |
|--------|---------|-------------|
| `enabled` | `true` | Start every session through `systemd-run --user --scope`, so it runs in its own transient systemd scope. Sessions start unscoped when `systemd-run` is missing or the user manager is unreachable. `false` only prevents new scopes and skips the check below; stopping, deleting and trashing sessions still stop their existing scopes. Global only. |
| `slice` | `"background-aoe-{profile}.slice"` | Template for the slice the session scopes live in; must end in `.slice`. `{profile}` expands to the session's profile name, escaped for systemd: every byte other than ASCII alphanumerics, `:` and `.` becomes `_<hex>` (so `-` and `_` are escaped too, as are spaces, `/` and non-ASCII characters). A value without `{profile}` is used as is. An invalid value falls back to the default with a warning. Global only. |
| `survive_daemon_restart` | `true` | Keep sessions running when the aoe daemon service stops: agents live in their own scopes, outside the daemon's cgroup. `false` binds the scopes the daemon creates to its service with `PartOf=`, so stopping or restarting the daemon service stops them, and systemd does not start them again; this only takes effect when the daemon itself runs as a `.service`, otherwise it is ignored with a warning. Scopes created from the CLI or TUI are never bound. Global only. |
| `memory_max` | unset | Memory cap for each session scope, a number followed by `b`, `k`, `m`, or `g` (e.g. `4G`, `512M`, `512b`). Unset or empty means unlimited; any other value is ignored with a warning. Global only. |
| `tasks_max` | `0` | Task cap for each session scope (0 to 4194304). `0` means unlimited. Global only. |

Each session start runs the agent through a fresh unit named `aoe-session-<escaped-id>-<app-namespace>-<start-ms>-<nonce>.scope`. The app-directory namespace and random per-launch nonce distinguish separate installations and repeated starts; they are ownership-consistency keys, not authentication. The scope description contains only the first eight id characters, and `aoe list` maps the id to the title. With the default `slice` template the scopes sit in `background.slice/background-aoe.slice/background-aoe-<profile>.slice`; a custom slice has its own hierarchy. Inspect them with:

```sh
systemctl --user status 'aoe-session-*'
systemd-cgls --user
```

A session that changes profile keeps its old slice until its next start. To cap a whole profile or all of aoe, put limits on the slice itself with `systemctl --user set-property background-aoe-<profile>.slice MemoryMax=8G` or a drop-in. There `<profile>` is the escaped profile name, for example `my_2dprofile` for `my-profile`. The `memory_max` and `tasks_max` settings apply to each session scope on its own. Set `slice` to group sessions elsewhere, for example `aoe.slice`.

aoe checks once per process that `systemd-run` works with the configured limits, with `--expand-environment=no` so `$` in session commands stays literal. The check is one attempt of at most 5 seconds. Sessions start unscoped, with one line in the log, when `systemd-run` or `env` is not on `PATH`, when `systemd-run` cannot reach the user manager or times out, when it rejects the configured `memory_max`/`tasks_max` (a warning naming them), or when `enabled` is `false` (`systemd.enabled=false`). If `systemd-run` fails after that check passed, the session start fails visibly: the tmux pane dies, and an ACP session reports an attach error after the dial deadline, with the `systemd-run` error in the runner log.

Sessions already running are not moved into a scope. Stopping, deleting or trashing a session waits for only its captured exact scope units to stop and have empty cgroups, also when `enabled` is `false`. Scoped structured workers use the same captured PID, worker-generation and producer-lease guards as other ACP retirement; the CLI removal path does not bypass that proof. A failed manager command, a live cgroup or unresolved legacy scope ownership refuses resource cleanup and retains retry ownership instead of guessing from a SID glob. Restarting an old terminal row can record a new scope but does not erase unresolved prior scope evidence.

For a sandboxed session only the container CLI or runner process is in the scope, so `memory_max` and `tasks_max` do not constrain the container itself; use the sandbox settings for that. Docker and Podman containers are created with `--cgroup-parent` set to the session's slice, so they are grouped there and slice-level limits apply. This needs a rootless daemon on the systemd cgroup driver, checked with `docker info` or `podman info` on every creation. Under `cgroupfs` the flag would be read as a path, and a rootful daemon would create the slice in the system manager instead of the user manager, so the flag is left off in both cases. It is also left off when `sandbox.extra_run_args` already sets `--cgroup-parent` or a `--cgroups` mode of `disabled` or `split`, which cannot be combined with one. Podman's network helper (pasta) is forked by the process that calls `podman run` or `podman start`, so aoe runs those two calls in their own scope in the same slice. `podman start` does so only for containers aoe created with the slice, recorded in the `com.agent-of-empires.cgroup-parent` label; older containers start unscoped. The slice is fixed when the container is created.

The tmux server is not scoped. A tmux server started by the daemon lives in the daemon's cgroup, and restarting the daemon service can kill it and every pane with it, although the agent processes are still in their session scopes. Either set `KillMode=process` on the daemon unit (this leaves the daemon's other processes running after a stop), or select a dedicated tmux socket with `tmux.socket_name` whose server is started outside the daemon.

The daemon reports `STATUS=<n> running, <n> waiting, <n> idle` to systemd, visible in `systemctl --user status <daemon unit>`.

`aoe list --json` adds a `systemd_unit` field to live sessions with a running process when `enabled` is true and the user-manager socket exists (`$XDG_RUNTIME_DIR/systemd/private`, `$XDG_RUNTIME_DIR/bus` or the socket in `DBUS_SESSION_BUS_ADDRESS`). Its value is the app-namespace-qualified expected pattern `aoe-session-<escaped-id>-<app-namespace>-*.scope`, not proof that a unit exists: listing does not call systemd and this display glob never authorizes cleanup.

## Diff

```toml
[diff]
default_branch = "main"   # auto-detected when unset
context_lines = 3
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

## Profiles

Profiles have separate sessions, groups, and overrides of profile-overridable settings. Press `P` in the TUI to choose from the last applied Daemon profile list. Reopen it to see later changes. A removed profile stays selected until you choose another filter; choose `all` to see remaining profiles.

```bash
aoe                        # "default" profile
aoe -p work                # the "work" profile
aoe profile create client-xyz
aoe profile list
aoe profile default work
```

## Repo config

Per-repo settings live in `.agent-of-empires/config.toml`; `aoe init` writes a template. A repo may set `[hooks]`, parts of `[session]`, `[sandbox]`, and `[worktree]`, and nothing else. See [Repo Config & Hooks](repo-config.md) for which keys are accepted and why the rest are not.
