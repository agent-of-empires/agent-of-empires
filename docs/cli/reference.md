# Command-Line Help for `aoe`

This document contains the help content for the `aoe` command-line program.

**Command Overview:**

* [`aoe`↴](#aoe)
* [`aoe add`↴](#aoe-add)
* [`aoe agents`↴](#aoe-agents)
* [`aoe init`↴](#aoe-init)
* [`aoe list`↴](#aoe-list)
* [`aoe logs`↴](#aoe-logs)
* [`aoe log-level`↴](#aoe-log-level)
* [`aoe remove`↴](#aoe-remove)
* [`aoe send`↴](#aoe-send)
* [`aoe status`↴](#aoe-status)
* [`aoe session`↴](#aoe-session)
* [`aoe session start`↴](#aoe-session-start)
* [`aoe session stop`↴](#aoe-session-stop)
* [`aoe session restart`↴](#aoe-session-restart)
* [`aoe session attach`↴](#aoe-session-attach)
* [`aoe session show`↴](#aoe-session-show)
* [`aoe session rename`↴](#aoe-session-rename)
* [`aoe session set-worktree-name`↴](#aoe-session-set-worktree-name)
* [`aoe session capture`↴](#aoe-session-capture)
* [`aoe session current`↴](#aoe-session-current)
* [`aoe session set-session-id`↴](#aoe-session-set-session-id)
* [`aoe session set-base`↴](#aoe-session-set-base)
* [`aoe session snooze`↴](#aoe-session-snooze)
* [`aoe session unsnooze`↴](#aoe-session-unsnooze)
* [`aoe session favorite`↴](#aoe-session-favorite)
* [`aoe session unfavorite`↴](#aoe-session-unfavorite)
* [`aoe session archive`↴](#aoe-session-archive)
* [`aoe session unarchive`↴](#aoe-session-unarchive)
* [`aoe group`↴](#aoe-group)
* [`aoe group list`↴](#aoe-group-list)
* [`aoe group create`↴](#aoe-group-create)
* [`aoe group delete`↴](#aoe-group-delete)
* [`aoe group move`↴](#aoe-group-move)
* [`aoe profile`↴](#aoe-profile)
* [`aoe profile list`↴](#aoe-profile-list)
* [`aoe profile create`↴](#aoe-profile-create)
* [`aoe profile delete`↴](#aoe-profile-delete)
* [`aoe profile rename`↴](#aoe-profile-rename)
* [`aoe profile default`↴](#aoe-profile-default)
* [`aoe project`↴](#aoe-project)
* [`aoe project list`↴](#aoe-project-list)
* [`aoe project add`↴](#aoe-project-add)
* [`aoe project remove`↴](#aoe-project-remove)
* [`aoe worktree`↴](#aoe-worktree)
* [`aoe worktree list`↴](#aoe-worktree-list)
* [`aoe worktree info`↴](#aoe-worktree-info)
* [`aoe worktree cleanup`↴](#aoe-worktree-cleanup)
* [`aoe tmux`↴](#aoe-tmux)
* [`aoe tmux status`↴](#aoe-tmux-status)
* [`aoe sounds`↴](#aoe-sounds)
* [`aoe sounds install`↴](#aoe-sounds-install)
* [`aoe sounds list`↴](#aoe-sounds-list)
* [`aoe sounds test`↴](#aoe-sounds-test)
* [`aoe theme`↴](#aoe-theme)
* [`aoe theme list`↴](#aoe-theme-list)
* [`aoe theme export`↴](#aoe-theme-export)
* [`aoe theme dir`↴](#aoe-theme-dir)
* [`aoe telemetry`↴](#aoe-telemetry)
* [`aoe telemetry status`↴](#aoe-telemetry-status)
* [`aoe telemetry enable`↴](#aoe-telemetry-enable)
* [`aoe telemetry disable`↴](#aoe-telemetry-disable)
* [`aoe telemetry reset-id`↴](#aoe-telemetry-reset-id)
* [`aoe serve`↴](#aoe-serve)
* [`aoe url`↴](#aoe-url)
* [`aoe acp`↴](#aoe-acp)
* [`aoe acp doctor`↴](#aoe-acp-doctor)
* [`aoe acp agents`↴](#aoe-acp-agents)
* [`aoe acp ps`↴](#aoe-acp-ps)
* [`aoe acp stop`↴](#aoe-acp-stop)
* [`aoe acp kill`↴](#aoe-acp-kill)
* [`aoe acp logs`↴](#aoe-acp-logs)
* [`aoe acp restart`↴](#aoe-acp-restart)
* [`aoe acp history`↴](#aoe-acp-history)
* [`aoe acp status`↴](#aoe-acp-status)
* [`aoe acp prompt`↴](#aoe-acp-prompt)
* [`aoe acp approve`↴](#aoe-acp-approve)
* [`aoe acp cancel`↴](#aoe-acp-cancel)
* [`aoe acp tail`↴](#aoe-acp-tail)
* [`aoe acp attach`↴](#aoe-acp-attach)
* [`aoe acp switch-agent`↴](#aoe-acp-switch-agent)
* [`aoe uninstall`↴](#aoe-uninstall)
* [`aoe update`↴](#aoe-update)
* [`aoe completion`↴](#aoe-completion)

## `aoe`

Agent of Empires (aoe) is a terminal session manager that uses tmux to help you manage and monitor AI coding agents like Claude Code and OpenCode.

Run without arguments to launch the TUI dashboard.

**Usage:** `aoe [OPTIONS] [COMMAND]`

###### **Subcommands:**

* `add` — Add a new session
* `agents` — List supported agents and their install status
* `init` — Initialize .agent-of-empires/config.toml in a repository
* `list` — List all sessions
* `logs` — View the configured AoE log file with a pretty viewer
* `log-level` — Get or set the running daemon's log filter at runtime. Pass a bare level (debug/info/...) for the safe expansion, or `--filter <expr>` for raw EnvFilter syntax. `--get` prints the current filter. Changes are ephemeral and lost on daemon restart
* `remove` — Remove a session
* `send` — Send a message to a running agent session
* `status` — Show session status summary
* `session` — Manage session lifecycle (start, stop, attach, etc.)
* `group` — Manage groups for organizing sessions
* `profile` — Manage profiles (separate workspaces)
* `project` — Manage the project registry used by multi-repo session pickers
* `worktree` — Manage git worktrees for parallel development
* `tmux` — tmux integration utilities
* `sounds` — Manage sound effects for agent state transitions
* `theme` — Manage color themes (list, export, customize)
* `telemetry` — Manage anonymous opt-in usage telemetry
* `serve` — Start a web dashboard for remote session access
* `url` — Print the current dashboard URL of a running `aoe serve` daemon
* `acp` — Manage the ACP structured-view workers (doctor, ps, logs, prompt, approve, ...)
* `uninstall` — Uninstall Agent of Empires
* `update` — Update aoe to the latest release
* `completion` — Generate shell completions

###### **Options:**

* `-p`, `--profile <PROFILE>` — Profile to use (separate workspace with its own sessions)
* `--daemon-url <DAEMON_URL>` — Attach to a remote agent daemon instead of using the local session list. Equivalent to setting `AOE_DAEMON_URL`; pair with `AOE_DAEMON_TOKEN` for the bearer token. Only meaningful at the no-subcommand `aoe` invocation (the TUI dashboard); ignored otherwise



## `aoe add`

Add a new session

**Usage:** `aoe add [OPTIONS] [PATH]`

###### **Arguments:**

* `<PATH>` — Project directory (defaults to current directory). Omit when using `--scratch`

###### **Options:**

* `-t`, `--title <TITLE>` — Session title (defaults to folder name)
* `-i`, `--interactive` — Prompt for the session name, mirroring the TUI `n` flow. Shows the generated default; press Enter to accept it. Ignored when --title is given. Requires an interactive terminal
* `-g`, `--group <GROUP>` — Group path (defaults to parent folder)
* `-c`, `--cmd <COMMAND>` — Command to run (e.g., 'claude' or any other supported agent)
* `--tool <TOOL>` — Named built-in or configured custom agent to run
* `-P`, `--parent <PARENT>` — Parent session (creates sub-session, inherits group)
* `-l`, `--launch` — Launch the session immediately after creating
* `-w`, `--worktree <WORKTREE_BRANCH>` — Create session in a git worktree for the specified branch
* `-b`, `--new-branch` — Create a new branch (use with --worktree)
* `--base-branch <BASE_BRANCH>` — Branch to base the new worktree branch on (use with --new-branch). Defaults to the repository's default branch. Useful for stacking work on top of an in-flight PR branch, hot-fixing a release branch, or branching off a teammate's branch
* `-r`, `--repo <EXTRA_REPOS>` — Additional repositories for multi-repo workspace (use with --worktree)
* `--project <PROJECTS>` — Names of registered projects to include as extra repos (use with --worktree). Resolves against the union of global + profile project registries
* `--no-submodules` — Skip `git submodule update --init --recursive` after creating the worktree, overriding the `worktree.init_submodules` config (default true). Useful for repos with large or deeply nested submodule trees that you don't need inside the agent session
* `-s`, `--sandbox` — Run session in a container sandbox
* `--sandbox-image <SANDBOX_IMAGE>` — Custom container image for sandbox (implies --sandbox)
* `-y`, `--yolo` — Enable YOLO mode (skip permission prompts)
* `--trust-hooks` — Automatically trust repository hooks without prompting
* `--extra-args <EXTRA_ARGS>` — Extra arguments to append after the agent binary
* `--cmd-override <CMD_OVERRIDE>` — Override the agent binary command
* `--structured-view` — Render this session in the structured view (ACP-based native rendering) instead of the default terminal view. `aoe add` defaults to the terminal (raw tmux/PTY) so the CLI matches the TUI; pass this (or `--agent`) to opt into the structured rendering. Ignored for tools with no ACP adapter
* `--agent <AGENT>` — Pick a specific ACP agent for the structured view (e.g., aoe-agent, claude-code)
* `--model <MODEL>` — Override the model used by aoe-agent (e.g., claude-opus-4-7, gpt-5, gemini-2.5-pro). Forwarded to the agent at session start
* `--scratch` — Create the session in a fresh scratch directory under `<app_dir>/scratch/<id>/` instead of a project path. The directory is removed when the session is deleted (unless `aoe rm` is given `--keep-scratch`). Mutually exclusive with worktree-related flags



## `aoe agents`

List supported agents and their install status

**Usage:** `aoe agents`



## `aoe init`

Initialize .agent-of-empires/config.toml in a repository

**Usage:** `aoe init [PATH]`

###### **Arguments:**

* `<PATH>` — Directory to initialize (defaults to current directory)

  Default value: `.`



## `aoe list`

List all sessions

**Usage:** `aoe list [OPTIONS]`

###### **Options:**

* `--json` — Output as JSON
* `--all` — List sessions from all profiles



## `aoe logs`

View the configured AoE log file with a pretty viewer

**Usage:** `aoe logs [OPTIONS]`

###### **Options:**

* `-f`, `--follow` — Live-tail the log
* `-n`, `--lines <N>` — Show only the last N lines (fallback viewers; lnav handles its own)
* `--no-pager` — Skip viewer detection; write plain log to stdout
* `--path` — Print the resolved log file path and exit (no viewing)



## `aoe log-level`

Get or set the running daemon's log filter at runtime. Pass a bare level (debug/info/...) for the safe expansion, or `--filter <expr>` for raw EnvFilter syntax. `--get` prints the current filter. Changes are ephemeral and lost on daemon restart

**Usage:** `aoe log-level [OPTIONS] [LEVEL]`

###### **Arguments:**

* `<LEVEL>` — Bare level (trace|debug|info|warn|error). Expands to all known target roots, avoiding the firehose of dependency logs you would get from `RUST_LOG=debug`

###### **Options:**

* `--filter <FILTER>` — Raw EnvFilter directive. Use this for per-target tuning, e.g. `--filter acp.protocol=trace,info`. Bare `--filter debug` is rejected; use the positional `level` form instead
* `--get` — Print the current filter without changing it



## `aoe remove`

Remove a session

**Usage:** `aoe remove [OPTIONS] <IDENTIFIER>`

###### **Arguments:**

* `<IDENTIFIER>` — Session ID or title to remove

###### **Options:**

* `--delete-worktree` — Delete worktree directory (default: keep worktree)
* `--delete-branch` — Delete git branch after worktree removal (default: per config)
* `--force` — Force worktree removal even with untracked/modified files
* `--keep-container` — Keep container instead of deleting it (default: delete per config)
* `--keep-scratch` — For scratch sessions, keep the scratch directory on disk instead of removing it. The session record is still deleted; the kept path is logged so you can find the files later. No effect on non-scratch sessions



## `aoe send`

Send a message to a running agent session

**Usage:** `aoe send [OPTIONS] <IDENTIFIER> <MESSAGE>`

###### **Arguments:**

* `<IDENTIFIER>` — Session ID or title
* `<MESSAGE>` — Message to send to the agent

###### **Options:**

* `--no-revive` — Fail loud on dead/stopped sessions instead of auto-respawning. Default behavior is to revive the session so a `send` after a crash or stop just works; pass this for scripts that want the previous bail-out



## `aoe status`

Show session status summary

**Usage:** `aoe status [OPTIONS]`

###### **Options:**

* `-v`, `--verbose` — Show detailed session list
* `-q`, `--quiet` — Only output waiting count (for scripts)
* `--json` — Output as JSON



## `aoe session`

Manage session lifecycle (start, stop, attach, etc.)

**Usage:** `aoe session <COMMAND>`

###### **Subcommands:**

* `start` — Start a session's tmux process
* `stop` — Stop session process
* `restart` — Restart session (or all sessions with `--all`)
* `attach` — Attach to session interactively
* `show` — Show session details
* `rename` — Rename a session
* `set-worktree-name` — Edit a managed worktree session's workdir directory name (and, optionally, its git branch). Moves the worktree directory in place; the session must not be running. See #1723
* `capture` — Capture tmux pane output
* `current` — Auto-detect current session
* `set-session-id` — Set the resume target for a session (pin a conversation or force a one-shot fresh start)
* `set-base` — Set or clear the per-session diff base branch. The diff view compares the worktree against this ref instead of the auto-detected default. Useful when the PR target differs from the project default (stacked PRs, hotfix off `release/*`, renamed default branch). See #970
* `snooze` — Snooze a session for a duration (temporary archive, auto wakes)
* `unsnooze` — Wake a snoozed session immediately
* `favorite` — Mark a session as a favorite. Favorited rows pin to the top of their status tier in the Attention sort and render with a leading `* ` glyph plus bold + underline
* `unfavorite` — Clear the favorite flag on a session
* `archive` — Archive a session (sinks it to the bottom of the Attention sort). Kills the tmux pane unless `--no-kill` is passed. The worktree, branch, and container are preserved; use `aoe remove` (optionally with `--delete-worktree` / `--delete-branch`) to fully destroy a session
* `unarchive` — Unarchive a session (restores it to its tier in the Attention sort)



## `aoe session start`

Start a session's tmux process

**Usage:** `aoe session start <IDENTIFIER>`

###### **Arguments:**

* `<IDENTIFIER>` — Session ID or title



## `aoe session stop`

Stop session process

**Usage:** `aoe session stop <IDENTIFIER>`

###### **Arguments:**

* `<IDENTIFIER>` — Session ID or title



## `aoe session restart`

Restart session (or all sessions with `--all`)

**Usage:** `aoe session restart [OPTIONS] [IDENTIFIER]`

###### **Arguments:**

* `<IDENTIFIER>` — Session ID or title (required unless `--all` is passed)

###### **Options:**

* `--all` — Restart every session in the active profile. Useful after `aoe update`, after editing `sandbox.environment`, after a Docker hiccup, or after changing a hook. Mutually exclusive with `identifier`
* `--parallel <PARALLEL>` — Concurrency cap for `--all`. Restarting many sandboxed sessions in parallel pressures dockerd, so the default is intentionally modest. Ignored when `--all` is not set

  Default value: `3`



## `aoe session attach`

Attach to session interactively

**Usage:** `aoe session attach <IDENTIFIER>`

###### **Arguments:**

* `<IDENTIFIER>` — Session ID or title



## `aoe session show`

Show session details

**Usage:** `aoe session show [OPTIONS] [IDENTIFIER]`

###### **Arguments:**

* `<IDENTIFIER>` — Session ID or title (optional, auto-detects in tmux)

###### **Options:**

* `--json` — Output as JSON



## `aoe session rename`

Rename a session

**Usage:** `aoe session rename [OPTIONS] [IDENTIFIER]`

###### **Arguments:**

* `<IDENTIFIER>` — Session ID or title (optional, auto-detects in tmux)

###### **Options:**

* `-t`, `--title <TITLE>` — New title for the session
* `-g`, `--group <GROUP>` — New group for the session (empty string to ungroup)



## `aoe session set-worktree-name`

Edit a managed worktree session's workdir directory name (and, optionally, its git branch). Moves the worktree directory in place; the session must not be running. See #1723

**Usage:** `aoe session set-worktree-name [OPTIONS] --name <NAME> [IDENTIFIER]`

###### **Arguments:**

* `<IDENTIFIER>` — Session ID or title (optional, auto-detects in tmux)

###### **Options:**

* `--name <NAME>` — New workdir (worktree directory) name
* `--rename-branch` — Also rename the underlying git branch to match the new name



## `aoe session capture`

Capture tmux pane output

**Usage:** `aoe session capture [OPTIONS] [IDENTIFIER]`

###### **Arguments:**

* `<IDENTIFIER>` — Session ID or title (auto-detects in tmux if omitted)

###### **Options:**

* `-n`, `--lines <LINES>` — Number of lines to capture

  Default value: `50`
* `--strip-ansi` — Strip ANSI escape codes
* `--json` — Output as JSON



## `aoe session current`

Auto-detect current session

**Usage:** `aoe session current [OPTIONS]`

###### **Options:**

* `-q`, `--quiet` — Just session name (for scripting)
* `--json` — Output as JSON



## `aoe session set-session-id`

Set the resume target for a session (pin a conversation or force a one-shot fresh start)

**Usage:** `aoe session set-session-id <IDENTIFIER> <SESSION_ID>`

###### **Arguments:**

* `<IDENTIFIER>` — Session ID or title
* `<SESSION_ID>` — Resume target: a UUID/sid pins the next launches to that conversation; an empty string forces a one-shot fresh start (after which the system reverts to auto-resume)



## `aoe session set-base`

Set or clear the per-session diff base branch. The diff view compares the worktree against this ref instead of the auto-detected default. Useful when the PR target differs from the project default (stacked PRs, hotfix off `release/*`, renamed default branch). See #970

**Usage:** `aoe session set-base [OPTIONS] <IDENTIFIER> [BRANCH]`

###### **Arguments:**

* `<IDENTIFIER>` — Session ID or title
* `<BRANCH>` — Branch ref to diff against (short name like `main` or remote-qualified like `upstream/main`). Required unless `--clear` is passed

###### **Options:**

* `--clear` — Clear the override and fall back to the profile default / auto-detected base



## `aoe session snooze`

Snooze a session for a duration (temporary archive, auto wakes)

**Usage:** `aoe session snooze [OPTIONS] <IDENTIFIER>`

###### **Arguments:**

* `<IDENTIFIER>` — Session ID or title

###### **Options:**

* `--minutes <MINUTES>` — Snooze duration in minutes; if omitted, uses `session.snooze_duration_minutes` from the active config (default 30)



## `aoe session unsnooze`

Wake a snoozed session immediately

**Usage:** `aoe session unsnooze <IDENTIFIER>`

###### **Arguments:**

* `<IDENTIFIER>` — Session ID or title



## `aoe session favorite`

Mark a session as a favorite. Favorited rows pin to the top of their status tier in the Attention sort and render with a leading `* ` glyph plus bold + underline

**Usage:** `aoe session favorite <IDENTIFIER>`

###### **Arguments:**

* `<IDENTIFIER>` — Session ID or title



## `aoe session unfavorite`

Clear the favorite flag on a session

**Usage:** `aoe session unfavorite <IDENTIFIER>`

###### **Arguments:**

* `<IDENTIFIER>` — Session ID or title



## `aoe session archive`

Archive a session (sinks it to the bottom of the Attention sort). Kills the tmux pane unless `--no-kill` is passed. The worktree, branch, and container are preserved; use `aoe remove` (optionally with `--delete-worktree` / `--delete-branch`) to fully destroy a session

**Usage:** `aoe session archive [OPTIONS] <IDENTIFIER>`

###### **Arguments:**

* `<IDENTIFIER>` — Session ID or title

###### **Options:**

* `--no-kill` — Skip killing the tmux pane. By default archiving stops the running agent so the row renders as truly parked; pass this to keep the pane alive while still marking the session archived



## `aoe session unarchive`

Unarchive a session (restores it to its tier in the Attention sort)

**Usage:** `aoe session unarchive <IDENTIFIER>`

###### **Arguments:**

* `<IDENTIFIER>` — Session ID or title



## `aoe group`

Manage groups for organizing sessions

**Usage:** `aoe group <COMMAND>`

###### **Subcommands:**

* `list` — List all groups
* `create` — Create a new group
* `delete` — Delete a group
* `move` — Move session to group



## `aoe group list`

List all groups

**Usage:** `aoe group list [OPTIONS]`

###### **Options:**

* `--json` — Output as JSON



## `aoe group create`

Create a new group

**Usage:** `aoe group create [OPTIONS] <NAME>`

###### **Arguments:**

* `<NAME>` — Group name

###### **Options:**

* `--parent <PARENT>` — Parent group for creating subgroups



## `aoe group delete`

Delete a group

**Usage:** `aoe group delete [OPTIONS] <NAME>`

###### **Arguments:**

* `<NAME>` — Group name

###### **Options:**

* `--force` — Force delete by moving sessions to default group



## `aoe group move`

Move session to group

**Usage:** `aoe group move <IDENTIFIER> <GROUP>`

###### **Arguments:**

* `<IDENTIFIER>` — Session ID or title
* `<GROUP>` — Target group



## `aoe profile`

Manage profiles (separate workspaces)

**Usage:** `aoe profile [COMMAND]`

###### **Subcommands:**

* `list` — List all profiles
* `create` — Create a new profile
* `delete` — Delete a profile
* `rename` — Rename a profile
* `default` — Show or set default profile



## `aoe profile list`

List all profiles

**Usage:** `aoe profile list`



## `aoe profile create`

Create a new profile

**Usage:** `aoe profile create <NAME>`

###### **Arguments:**

* `<NAME>` — Profile name



## `aoe profile delete`

Delete a profile

**Usage:** `aoe profile delete <NAME>`

###### **Arguments:**

* `<NAME>` — Profile name



## `aoe profile rename`

Rename a profile

**Usage:** `aoe profile rename <OLD_NAME> <NEW_NAME>`

###### **Arguments:**

* `<OLD_NAME>` — Current profile name
* `<NEW_NAME>` — New profile name



## `aoe profile default`

Show or set default profile

**Usage:** `aoe profile default [NAME]`

###### **Arguments:**

* `<NAME>` — Profile name (optional, shows current if not provided)



## `aoe project`

Manage the project registry used by multi-repo session pickers

**Usage:** `aoe project <COMMAND>`

###### **Subcommands:**

* `list` — List registered projects
* `add` — Add a project to the registry
* `remove` — Remove a project from the registry



## `aoe project list`

List registered projects

**Usage:** `aoe project list [OPTIONS]`

###### **Options:**

* `--json` — Output as JSON
* `--scope <SCOPE>` — Filter by scope (default: all)

  Default value: `all`

  Possible values: `all`, `global`, `profile`




## `aoe project add`

Add a project to the registry

**Usage:** `aoe project add [OPTIONS] <PATH>`

###### **Arguments:**

* `<PATH>` — Path to the git repository

###### **Options:**

* `--name <NAME>` — Display name (defaults to the directory's basename)
* `--scope <SCOPE>` — Registry scope. When omitted: defaults to GLOBAL, unless `-p <profile>` was passed at the top level, in which case it defaults to PROFILE (scoping the entry to that profile only)

  Possible values: `global`, `profile`

* `--allow-override` — Allow registering this path even if it already exists in the other scope. Without this flag the command errors when the same canonical path is already registered globally (when adding to profile) or in any profile (when adding globally). When override is allowed and both scopes hold the same path, the profile entry shadows the global one
* `--base-branch <BASE_BRANCH>` — Default base branch for new worktree branches created against this project, whether it is the launch repo or an extra repo in a multi-repo workspace. An explicit session base wins; when omitted, falls back to the global/profile `worktree.default_base_branch`, then the repo's detected default branch



## `aoe project remove`

Remove a project from the registry

**Usage:** `aoe project remove [OPTIONS] <NAME_OR_PATH>`

###### **Arguments:**

* `<NAME_OR_PATH>` — Project name or path to remove

###### **Options:**

* `--scope <SCOPE>` — Registry scope to remove from. When omitted: defaults to GLOBAL, unless `-p <profile>` was passed at the top level, in which case it defaults to PROFILE

  Possible values: `global`, `profile`




## `aoe worktree`

Manage git worktrees for parallel development

**Usage:** `aoe worktree <COMMAND>`

###### **Subcommands:**

* `list` — List all worktrees in current repository
* `info` — Show worktree information for a session
* `cleanup` — Cleanup orphaned worktrees



## `aoe worktree list`

List all worktrees in current repository

**Usage:** `aoe worktree list`



## `aoe worktree info`

Show worktree information for a session

**Usage:** `aoe worktree info <IDENTIFIER>`

###### **Arguments:**

* `<IDENTIFIER>` — Session ID or title



## `aoe worktree cleanup`

Cleanup orphaned worktrees

**Usage:** `aoe worktree cleanup [OPTIONS]`

###### **Options:**

* `-f`, `--force` — Actually remove worktrees (default is dry-run)



## `aoe tmux`

tmux integration utilities

**Usage:** `aoe tmux <COMMAND>`

###### **Subcommands:**

* `status` — Output session info for use in custom tmux status bar



## `aoe tmux status`

Output session info for use in custom tmux status bar

Add this to your ~/.tmux.conf: set -g status-right "#(aoe tmux status)"

**Usage:** `aoe tmux status [OPTIONS]`

###### **Options:**

* `-f`, `--format <FORMAT>` — Output format (text or json)

  Default value: `text`



## `aoe sounds`

Manage sound effects for agent state transitions

**Usage:** `aoe sounds <COMMAND>`

###### **Subcommands:**

* `install` — Install bundled sound effects
* `list` — List currently installed sounds
* `test` — Test a sound by playing it



## `aoe sounds install`

Install bundled sound effects

**Usage:** `aoe sounds install`



## `aoe sounds list`

List currently installed sounds

**Usage:** `aoe sounds list`



## `aoe sounds test`

Test a sound by playing it

**Usage:** `aoe sounds test <NAME>`

###### **Arguments:**

* `<NAME>` — Sound file name (without extension)



## `aoe theme`

Manage color themes (list, export, customize)

**Usage:** `aoe theme <COMMAND>`

###### **Subcommands:**

* `list` — List all available themes (built-in and custom)
* `export` — Export a built-in theme as a TOML file for customization
* `dir` — Show the custom themes directory path



## `aoe theme list`

List all available themes (built-in and custom)

**Usage:** `aoe theme list`



## `aoe theme export`

Export a built-in theme as a TOML file for customization

**Usage:** `aoe theme export [OPTIONS] <NAME>`

###### **Arguments:**

* `<NAME>` — Theme name to export

###### **Options:**

* `-o`, `--output <OUTPUT>` — Output file path (defaults to <name>.toml in the themes directory)



## `aoe theme dir`

Show the custom themes directory path

**Usage:** `aoe theme dir`



## `aoe telemetry`

Manage anonymous opt-in usage telemetry

**Usage:** `aoe telemetry <COMMAND>`

###### **Subcommands:**

* `status` — Show the current telemetry opt-in state and install id
* `enable` — Opt in to anonymous usage telemetry
* `disable` — Opt out of telemetry (deletes the local install id)
* `reset-id` — Generate a fresh anonymous install id (only while opted in)



## `aoe telemetry status`

Show the current telemetry opt-in state and install id

**Usage:** `aoe telemetry status`



## `aoe telemetry enable`

Opt in to anonymous usage telemetry

**Usage:** `aoe telemetry enable`



## `aoe telemetry disable`

Opt out of telemetry (deletes the local install id)

**Usage:** `aoe telemetry disable`



## `aoe telemetry reset-id`

Generate a fresh anonymous install id (only while opted in)

**Usage:** `aoe telemetry reset-id`



## `aoe serve`

Start a web dashboard for remote session access

**Usage:** `aoe serve [OPTIONS]`

###### **Options:**

* `--port <PORT>` — Port to listen on (default: 8080; debug builds default to 8081 so a `cargo run` instance does not collide with an installed release `aoe`)
* `--host <HOST>` — Host/IP to bind to (use 0.0.0.0 for LAN/VPN access)

  Default value: `127.0.0.1`
* `--auth <AUTH>` — Authentication mode: `token` (default, random URL token), `passphrase` (no token URL, passphrase login wall only), or `none` (no auth at all, loopback-only unless --behind-proxy). Mutually exclusive with --no-auth (which aliases --auth=none)

  Possible values: `token`, `passphrase`, `none`

* `--no-auth` — Disable authentication (only allowed with localhost binding). Alias for --auth=none
* `--behind-proxy` — Mark this server as sitting behind a reverse proxy that terminates TLS upstream. Sets cookies as `; Secure` and trusts the `X-Forwarded-For` / `cf-connecting-ip` headers from loopback peers. Does NOT auto-spawn a tunnel (unlike --remote). Required when --auth=passphrase or --auth=none is combined with a non-loopback bind
* `--read-only` — Read-only mode: view terminals but cannot send keystrokes
* `--remote` — Expose the dashboard over a public HTTPS tunnel. Prefers Tailscale Funnel when `tailscale` is installed and logged in (stable `.ts.net` URL, installable PWAs survive restarts). Falls back to a Cloudflare quick tunnel otherwise (fresh URL on every restart)
* `--tunnel-name <TUNNEL_NAME>` — Use a named Cloudflare Tunnel (requires prior `cloudflared tunnel create`). Takes precedence over Tailscale auto-detection
* `--no-tailscale` — Skip Tailscale Funnel auto-detection and go straight to Cloudflare. Useful if you have Tailscale installed for unrelated reasons
* `--tunnel-url <TUNNEL_URL>` — Hostname for a named tunnel (e.g., aoe.example.com)
* `--daemon` — Run as a background daemon (detach from terminal)
* `--stop` — Stop a running daemon
* `--status` — Print the running daemon's PID, mode, URLs, and log path. Exits non-zero when no daemon is running. Useful for shell scripts that want to know whether a daemon is up without parsing `ps`.

   `--status` is read-only and incompatible with every flag that would change daemon state (`--stop`, `--daemon`, `--remote`) or the bind config of a fresh daemon (`--no-auth`, `--auth`, `--behind-proxy`, `--read-only`, `--passphrase`, `--port`, `--tunnel-name`, `--no-tailscale`, `--tunnel-url`, `--open`). Clap reports the misuse instead of silently ignoring the extras.
* `--passphrase <PASSPHRASE>` — Require a passphrase for login (second-factor auth). Can also be set via AOE_SERVE_PASSPHRASE environment variable
* `--open` — Open the dashboard URL in the default browser once the server is ready. Ignored under --daemon, --remote, SSH (SSH_CONNECTION/SSH_TTY), or when no display server is reachable on Linux/BSD
* `--restart` — Restart a running `aoe serve` daemon, replaying the host, port, mode, and auth it was launched with (read from `serve.launch`). The passphrase is recalled from `serve.passphrase` or `AOE_SERVE_PASSPHRASE` before the old daemon is stopped, so a passphrase-protected daemon is never left down. Incompatible with the flags that would change the daemon's bind config: that config comes from the persisted launch state



## `aoe url`

Print the current dashboard URL of a running `aoe serve` daemon

**Usage:** `aoe url [OPTIONS]`

###### **Options:**

* `--all` — Print every labeled URL (Tailscale / LAN / localhost) on its own line. The primary URL is printed first as `primary\t<url>`; alternates use `<label>\t<url>`. The tab-separated format makes the output easy to parse from shell scripts
* `--token-only` — Print only the auth token from the primary URL's `?token=` query parameter. Useful for scripted login flows or pasting into the PWA. Exits non-zero when the URL has no token (e.g. `--no-auth` server)



## `aoe acp`

Manage the ACP structured-view workers (doctor, ps, logs, prompt, approve, ...)

**Usage:** `aoe acp <COMMAND>`

###### **Subcommands:**

* `doctor` — Verify the structured view can start: Node runtime, configured agents, provider auth (claude login)
* `agents` — List configured agents (claude-code, aoe-agent, etc.)
* `ps` — List running agent workers (detached or attached)
* `stop` — Gracefully stop an agent worker (SIGTERM the runner, agent receives stdin EOF). Sessions can be reattached on the next `aoe serve` only if they are still alive afterward; `stop` destroys the worker
* `kill` — SIGKILL a worker immediately (use when `stop` doesn't take)
* `logs` — Tail the runner's log file for an agent session
* `restart` — Restart a wedged agent worker: stop the existing runner, then let the daemon's reconciler spawn a fresh one on the next tick
* `history` — Print the persisted transcript for an agent session
* `status` — Print live status for an agent session: highest/lowest seq, and whether the on-disk retention window has truncated history
* `prompt` — Send a prompt to an agent session's agent
* `approve` — Resolve a pending approval (default: allow). Use --always for a session-scoped allow-list entry, --deny to refuse the request
* `cancel` — Cancel the in-flight prompt for an agent session
* `tail` — Stream the agent broadcast for a session to stdout as JSON lines (one frame per line). Press Ctrl-C to stop
* `attach` — Open the TUI structured view directly for a known session id. Combine with `AOE_DAEMON_URL` (+ `AOE_DAEMON_TOKEN`) to attach across machines without going through the home session list
* `switch-agent` — Switch an agent session to a different ACP agent, keeping the transcript. The new agent starts fresh; use `aoe acp agents` to list valid targets. Handy for returning to claude after a rate-limit handoff to codex



## `aoe acp doctor`

Verify the structured view can start: Node runtime, configured agents, provider auth (claude login)

**Usage:** `aoe acp doctor [OPTIONS]`

###### **Options:**

* `--json` — Emit machine-readable JSON instead of a human report
* `--fix` — Attempt safe remediations: install missing claude-code-acp adapter, verify aoe-agent presence, etc. (Reserved for future release; the flag exists so scripts can opt in early.)



## `aoe acp agents`

List configured agents (claude-code, aoe-agent, etc.)

**Usage:** `aoe acp agents`



## `aoe acp ps`

List running agent workers (detached or attached)

**Usage:** `aoe acp ps [OPTIONS]`

###### **Options:**

* `--json` — Emit machine-readable JSON instead of a table



## `aoe acp stop`

Gracefully stop an agent worker (SIGTERM the runner, agent receives stdin EOF). Sessions can be reattached on the next `aoe serve` only if they are still alive afterward; `stop` destroys the worker

**Usage:** `aoe acp stop [OPTIONS] [SESSION]`

###### **Arguments:**

* `<SESSION>` — Session id to stop. Mutually exclusive with `--all`

###### **Options:**

* `--all` — Stop every running agent worker
* `--timeout-secs <TIMEOUT_SECS>` — Seconds to wait after SIGTERM before escalating to SIGKILL

  Default value: `5`



## `aoe acp kill`

SIGKILL a worker immediately (use when `stop` doesn't take)

**Usage:** `aoe acp kill <SESSION>`

###### **Arguments:**

* `<SESSION>` — Session id to kill



## `aoe acp logs`

Tail the runner's log file for an agent session

**Usage:** `aoe acp logs [OPTIONS]`

###### **Options:**

* `--session <SESSION>` — Session id whose worker logs to tail
* `--follow` — Follow new lines as they arrive



## `aoe acp restart`

Restart a wedged agent worker: stop the existing runner, then let the daemon's reconciler spawn a fresh one on the next tick

**Usage:** `aoe acp restart <SESSION>`

###### **Arguments:**

* `<SESSION>` — Session id whose worker to restart



## `aoe acp history`

Print the persisted transcript for an agent session

**Usage:** `aoe acp history [OPTIONS] <SESSION>`

###### **Arguments:**

* `<SESSION>` — Acp session id

###### **Options:**

* `--since <SINCE>` — Skip events at or below this seq

  Default value: `0`
* `--json` — Emit raw frames as JSON (one frame per line)



## `aoe acp status`

Print live status for an agent session: highest/lowest seq, and whether the on-disk retention window has truncated history

**Usage:** `aoe acp status [OPTIONS] <SESSION>`

###### **Arguments:**

* `<SESSION>` — Acp session id

###### **Options:**

* `--json` — Emit machine-readable JSON instead of a human report



## `aoe acp prompt`

Send a prompt to an agent session's agent

**Usage:** `aoe acp prompt <SESSION> <TEXT>`

###### **Arguments:**

* `<SESSION>` — Acp session id
* `<TEXT>` — Prompt text. Pass `-` to read from stdin



## `aoe acp approve`

Resolve a pending approval (default: allow). Use --always for a session-scoped allow-list entry, --deny to refuse the request

**Usage:** `aoe acp approve [OPTIONS] <SESSION> <NONCE>`

###### **Arguments:**

* `<SESSION>` — Acp session id
* `<NONCE>` — Approval nonce, as printed in the pending-approval banner

###### **Options:**

* `--always` — Allow this kind of operation for the rest of the session
* `--deny` — Refuse the request



## `aoe acp cancel`

Cancel the in-flight prompt for an agent session

**Usage:** `aoe acp cancel <SESSION>`

###### **Arguments:**

* `<SESSION>` — Acp session id



## `aoe acp tail`

Stream the agent broadcast for a session to stdout as JSON lines (one frame per line). Press Ctrl-C to stop

**Usage:** `aoe acp tail [OPTIONS] <SESSION>`

###### **Arguments:**

* `<SESSION>` — Acp session id

###### **Options:**

* `--since <SINCE>` — Start at this seq (default 0 = full replay then live)

  Default value: `0`



## `aoe acp attach`

Open the TUI structured view directly for a known session id. Combine with `AOE_DAEMON_URL` (+ `AOE_DAEMON_TOKEN`) to attach across machines without going through the home session list

**Usage:** `aoe acp attach <SESSION>`

###### **Arguments:**

* `<SESSION>` — Acp session id



## `aoe acp switch-agent`

Switch an agent session to a different ACP agent, keeping the transcript. The new agent starts fresh; use `aoe acp agents` to list valid targets. Handy for returning to claude after a rate-limit handoff to codex

**Usage:** `aoe acp switch-agent [OPTIONS] <SESSION> <TARGET>`

###### **Arguments:**

* `<SESSION>` — Acp session id
* `<TARGET>` — Registry key of the target agent (e.g. `claude`, `codex`)

###### **Options:**

* `--model <MODEL>` — Optional model override forwarded to the new agent



## `aoe uninstall`

Uninstall Agent of Empires

**Usage:** `aoe uninstall [OPTIONS]`

###### **Options:**

* `--keep-data` — Keep data directory (sessions, config, logs)
* `--keep-tmux-config` — Keep tmux configuration
* `--dry-run` — Show what would be removed without removing
* `-y` — Skip confirmation prompts



## `aoe update`

Update aoe to the latest release

**Usage:** `aoe update [OPTIONS]`

###### **Options:**

* `-y`, `--yes` — Skip confirmation prompt
* `--check` — Print update status and exit (no install)
* `--dry-run` — Detect install method and print what would happen, no download



## `aoe completion`

Generate shell completions

**Usage:** `aoe completion <SHELL>`

###### **Arguments:**

* `<SHELL>` — Shell to generate completions for

  Possible values: `bash`, `elvish`, `fish`, `powershell`, `zsh`




<hr/>

<small><i>
    This document was generated automatically by
    <a href="https://crates.io/crates/clap-markdown"><code>clap-markdown</code></a>.
</i></small>
