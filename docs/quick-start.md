# Quick Start

## Launch the TUI

```bash
aoe
```

The TUI starts or reuses the local Daemon through its private Unix socket and reads canonical session rows from it. Starting the TUI does not open a web listener. The shared Daemon keeps running when you quit the TUI; `aoe serve --stop` stops it.

If no Daemon can be reached or started, `aoe` exits with the error. If the connection drops later, the TUI shows the diagnostic and masks daemon-owned session state. Press `r` to reconnect after fixing the cause, or `q` to quit. A Daemon left over from before an upgrade shows as **Outdated** instead: when aoe can restart it, `r` does so in place and reconnects; otherwise the dialog says how and `r` checks again. See [Starting the server](guides/web-dashboard.md#starting-the-server).

| Key | Action |
|-----|--------|
| `n` | New session |
| `b` | New session from a saved project |
| `p` | Manage saved projects (on a project header, pins it instead) |
| `Enter` | [Live mode](guides/live-mode.md): type to the agent with the list still visible |
| `Tab` | Attach to the session's tmux pane |
| `d` | Delete a session |
| `t` | Toggle Agent / Terminal view |
| `T` | Attach to the session's paired terminal |
| `\|` | Split the preview: agent above its paired terminal |
| `D` | Open the [diff view](guides/diff-view.md) |
| `/` | Search sessions |
| `?` | Full keymap |
| `q` | Quit |
| `Ctrl+b d` | Detach from the tmux session |

These are the welcome wizard's defaults. Choosing tmux there, or skipping that page, swaps `Enter` and `Tab`.

## Your first session

Press `n` and fill in your project path (or leave `.`), or run `aoe add /path/to/project`.
The CLI asks the Daemon to provision and save the session. A terminal-view session stays
**Stopped** until you pass `--launch` or select it in the TUI and press `Enter`.
`Ctrl+b d` detaches back to the TUI. Inside tmux, attaching switches your client to the
agent's tmux session; `Ctrl+b L` switches back.

`aoe` and `aoe add` start a local Daemon when needed. Use `--no-autostart` to require one
already running; without one, the TUI refuses to open. With `--daemon-url` or
`AOE_DAEMON_URL`, creation runs only on the selected Daemon, never locally if that
connection fails. Pass an absolute path on that host, or `--scratch`; registered
`--project` names and agent settings also come from that host.
`--hook-env KEY` explicitly reads that client variable; no other client environment is sent.
For a remote terminal session without `--launch`, run the printed start command on the
Daemon host, not on the client machine. You can also start it from the Daemon dashboard.

During creation, hook progress appears on stderr. `Ctrl+C` requests cancellation and waits
for the Daemon's result. If the connection is lost, the CLI checks canonical state without
resubmitting creation. An unknown result includes a request key: inspect the Daemon's
session list before creating again.

Press `t` to toggle between the agent view and the paired shell terminal, where you can run builds and git commands without interrupting the agent.

Drag the border between the session list and preview to resize the list. In terminals
narrower than `session.narrow_breakpoint` columns (80 by default), the list moves above
the preview: drag their horizontal border up or down to change its height, including in
live mode. Width and height are remembered separately across restarts. A shorter window
temporarily limits the height to leave room for the preview; enlarging it restores your
chosen height.

## Projects and groups

A **project** is a saved directory path you register once so you can start sessions from it without retyping. Registering is only a convenience: `n` and `aoe add <path>` work on any path. Add one with `aoe project add /path/to/repo`, or press `p` in the TUI and then `a`. Once a project exists, `b` starts a session from it. See [Multi-repo workspaces](guides/multi-repo-workspaces.md#the-project-registry).

A **group** is unrelated: it is a label you assign to existing sessions to sort them in the sidebar, set from the rename dialog or with `aoe group move`. Projects are where sessions start; groups are how they are bucketed once they exist. See [the grouping axes](guides/web/dashboard.md#sidebar-grouping-repo-organization-group-both-or-status).

Rows keep their place when a session needs you. A session needs you when it is Waiting, in Error, flagged urgent, or finished and unread, and each header shows `●N` for those under it. `w` jumps to the next one, or to a session that just finished. **Show only sessions that need you** in the `Ctrl+K` palette hides the rest without reordering the list; a session that stops needing you while selected stays listed until you move off it.

## Worktrees

To work on a new branch in its own directory:

```bash
aoe add . -w feat/my-feature -b
```

In the TUI, press `n`, enter a title, and enable Worktree (`Ctrl+P` on the field for an explicit name). Deleting the session offers to clean the worktree up.

Omit `-b` to attach instead: AoE re-uses the worktree for that branch, or checks the branch out into a new one. The TUI and web wizard have an **Attach to existing branch** toggle. Removing a session only cleans up worktrees AoE created. See the [worktrees reference](guides/worktrees.md).

## Resume a Claude conversation

After attaching, run `/resume` in the Claude pane and pick a conversation. AoE captures the session id and persists it, so the next launch reattaches automatically. See [Session resume](guides/session-resume.md).

## Sandboxes and other agents

```bash
aoe add --sandbox .        # run the agent in a container
aoe add -c opencode .      # use a different agent
```

The sandbox mounts your project at `/workspace` and seeds the agent's credentials into a private per-session store on the host (Claude Code shares one credential file across its sandboxes); the TUI has a checkbox for it when a container runtime is available. The agent picker in the new-session dialog lists every detected tool. See the [container sandbox guide](guides/sandbox.md).

## The web dashboard

```bash
aoe serve                  # localhost only
aoe serve --remote         # reachable from your phone over HTTPS
aoe serve --daemon         # run in the background
```

Open the printed URL in any browser for the same sessions, live terminals, and controls, and install it as a PWA for an app-like experience. See the [web dashboard guide](guides/web-dashboard.md).

## Next steps

- [Repo config & hooks](guides/repo-config.md): per-project settings
- [Configuration reference](guides/configuration.md): every setting
- [CLI reference](cli/reference.md): every command and flag
