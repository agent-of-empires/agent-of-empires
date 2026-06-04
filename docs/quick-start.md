# Quick Start

## Launch the TUI

```bash
aoe
```

This opens the dashboard. You'll see an empty session list on first run.

## Create Your First Session

**From the TUI:** Press `n` to open the new session dialog. Fill in the path to your project (or leave it as `.` for the current directory) and press `Enter`.

**From the CLI:**

```bash
aoe add /path/to/project
```

The session appears in the dashboard with status **Idle**.

## Attach to a Session

Select a session and press `Enter` to attach. You're now inside a tmux session running your AI agent (Claude Code by default).

To return to the TUI, press **`Ctrl+b d`** (the standard tmux detach shortcut).

## Use the Terminal View

Press `t` to toggle between Structured View and Terminal View. Each agent session has a paired shell terminal where you can run builds, tests, and git commands without interrupting the agent.

## Review Changes with Diff View

Press `D` to open the diff view. This shows changes between your working directory and the base branch. Navigate files with `j`/`k`, press `e` to edit, and `Esc` to close.

## Create a Worktree Session

To work on a new branch with its own directory:

```bash
# CLI
aoe add . -w feat/my-feature -b

# TUI: press n, enter a title, enable Worktree
# Optional: press Ctrl+P on Worktree and fill in Name
```

This creates a new git branch, a worktree directory, and a session pointing at it. When you delete the session, AoE offers to clean up the worktree too.

## Attach to Existing Work

Already have a worktree checked out, or a Claude Code conversation you want to keep going? AoE can wrap both without forcing you to start over.

**Attach to an existing branch or worktree.** Omit the `-b` flag and AoE re-uses the worktree if one exists for that branch, or checks the branch out into a new worktree if not:

```bash
# CLI: attach to whatever worktree/branch already exists
aoe add . -w feat/my-feature
```

In the TUI, press `Ctrl+P` on the Worktree field and toggle **Attach to existing branch**. The web wizard exposes the same toggle under the worktree name input. See [Worktrees Reference](guides/worktrees.md) for the full matrix.

When you later remove the session, AoE only cleans up worktrees it created; attached ones are left alone.

**Resume an existing Claude Code conversation.** After attaching, run `/resume` inside the Claude pane and pick the conversation you want. AoE installs hooks into `~/.claude/settings.json` to capture the active session ID and persists it so the next launch reattaches to the same conversation automatically. Details in [Session Resume](guides/session-resume.md), including the `aoe session set-session-id` command for setting the Claude UUID explicitly.

## Create a Sandboxed Session

To run an agent inside a Docker container:

```bash
aoe add --sandbox .
```

In the TUI, toggle the sandbox checkbox when creating a session. The agent runs in an isolated container with your project mounted at `/workspace` and authentication credentials shared via persistent Docker volumes.

Requires Docker to be installed.

## Choose a Different Agent

By default, AoE uses Claude Code. To use a different tool:

```bash
aoe add -c opencode .   # or any other supported agent
```

In the TUI, select the tool from the dropdown in the new session dialog.

## TUI Keyboard Reference

| Key | Action |
|-----|--------|
| `n` | New session |
| `b` | New session from saved project |
| `Enter` | Attach to session |
| `d` | Delete session |
| `t` | Toggle Agent/Terminal view |
| `D` | Open diff view |
| `/` | Search sessions |
| `?` | Show help |
| `q` | Quit |
| `Ctrl+b d` | Detach from tmux session |

## Use the Web Dashboard

Prefer a browser? Run `aoe serve` to start the web dashboard:

```bash
aoe serve                         # localhost only
aoe serve --host 0.0.0.0          # accessible from other devices (use with VPN)
aoe serve --daemon                # run in background
```

Open the printed URL in any browser -- your phone, tablet, or another computer. You get the same session list, live terminal streaming, and session controls. Install it as a PWA for an app-like experience.

See the [Web Dashboard Guide](guides/web-dashboard.md) for details.

## Next Steps

- [Web Dashboard](guides/web-dashboard.md) -- access sessions from any browser
- [Workflow Guide](guides/workflow.md) -- recommended setup with bare repos and parallel agents
- [Docker Sandbox](guides/sandbox.md) -- container configuration and custom images
- [Repo Config & Hooks](guides/repo-config.md) -- per-project settings
- [CLI Reference](cli/reference.md) -- every command and flag
