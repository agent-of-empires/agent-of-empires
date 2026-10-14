---
name: aoe
description: Manage AI coding agent sessions via Agent of Empires (aoe)
metadata:
  openclaw:
    requires:
      bins:
        - aoe
        - tmux
    homepage: https://github.com/agent-of-empires/agent-of-empires
---

# Agent of Empires (aoe) Skill

Use `aoe` to create, manage, and monitor AI coding agent sessions (Claude Code, Codex, OpenCode, etc.) in tmux. Prefer `aoe` over raw `tmux` commands for agent management.

## When to use this skill

- Launching one or more AI coding agents on project directories
- Monitoring agent progress (waiting vs running vs idle)
- Capturing agent output for review
- Organizing agents into groups or profiles
- Setting up parallel worktree-based development

Do NOT use this skill for general tmux window/pane management unrelated to coding agents.

## Core concepts

- **Session**: An agent process running in a tmux session. Each session has an ID, title, tool (e.g. `claude`), and project path.
- **Group**: A named folder for organizing sessions (supports nesting with `/`, e.g. `backend/api`).
- **Profile**: A separate workspace with its own sessions and config. Use `-p <name>` globally or set `AGENT_OF_EMPIRES_PROFILE`.
- **Status**: One of `running`, `waiting`, `idle`, `stopped`, `error`, `starting`, `unknown`.

## Command reference

### Adding sessions

```bash
# Add a session for the current directory
aoe add . -t "my feature"

# Add with group, launch immediately
aoe add /path/to/repo -t "API work" -g backend -l

# Add with specific tool
aoe add . -t "codex session" -c codex

# Add in a git worktree on a new branch, YOLO mode (omit -b to check out an existing branch)
aoe add . -t "fix-123" -w fix/issue-123 -b -y -l

# Base the new branch on another branch instead of the default
aoe add . -t "fix-124" -w fix/issue-124 -b --base-branch fix/issue-123 -l

# Add in Docker sandbox
aoe add . -t "sandboxed" -s -l

# Add as sub-session of another
aoe add . -t "sub task" -P <parent-id>

# Enable YOLO mode (skip permission prompts)
aoe add . -t "yolo" -y -l
```

Without `--base-branch`, `-w <branch> -b` starts the branch from `worktree.default_base_branch` if set, otherwise from the repo's detected default branch (chosen among each remote's `HEAD` target and `main`/`master`), fetched first. It is not the branch checked out where you run `aoe add`, so pass `--base-branch` to build on a feature branch.

A sandboxed session (`-s`) sees only its mounts: the project, its agent store and credentials, `~/.gitconfig`, and configured `extra_volumes`. Work that needs host-only resources (macOS Keychain, launchd, the aoe app directory, other paths outside the project) must run without `-s`. See the [sandbox guide](https://github.com/agent-of-empires/agent-of-empires/blob/main/docs/guides/sandbox.md).

### Listing sessions

```bash
# Human-readable list
aoe list

# JSON output for parsing
aoe list --json

# List across all profiles
aoe list --all

# Skip trashed and archived rows (default is every persisted session)
aoe list --json --state=live
```

**JSON output shape** (`aoe list --json`):
```json
[
  {
    "id": "a1b2c3d4-...",
    "title": "my feature",
    "path": "/home/user/project",
    "group": "backend",
    "tool": "claude",
    "command": "claude",
    "profile": "default",
    "state": "live",
    "created_at": "2025-01-01T00:00:00Z",
    "workspace_repos": []
  }
]
```

`command` is omitted when empty; `worktree` appears only for worktree-backed sessions. `state` is `live`, `archived`, or `trashed`; `trashed_at` and `archived_at` are set independently, and trashing leaves `archived_at` alone, so a session archived and then trashed carries both keys and reports `trashed`. Neither timestamp is durable, but they do not clear alike: `archived_at` is cleared by `aoe session unarchive`, by `aoe session favorite`, and by anything that wakes the session (`aoe send`, and `aoe session restart` when it sends its wake message), while `trashed_at` is cleared only by `aoe session restore`. Key off `state` rather than caching a timestamp. A trashed session stays in the listing and keeps its title, so check `state` before treating a row as a live session. `list --json` does not include live status; use `aoe status --json` or `aoe session capture --json` for that.

`snoozed_until` and `pinned_at` round out the four state timestamps the REST API exposes: `pinned_at` appears whenever the session is pinned for the web sidebar, while `snoozed_until` appears only while the snooze is still active (the API's own gate), so an expired deadline omits the key instead of advertising a snooze that already ended. Both are independent of `state`, and neither key appears on a row carrying neither marker. `snoozed_until` also clears readily, more often than its siblings: waking the session (`aoe send`), archiving, favoriting, unsnoozing, or pinning all remove it on the next read, so treat a missing key as no active snooze rather than a listing gap.

### Session lifecycle

```bash
aoe session start <id-or-title>
aoe session stop <id-or-title>
aoe session restart <id-or-title>
aoe session attach <id-or-title>   # interactive attach
```

### Inspecting sessions

```bash
# Show session metadata
aoe session show <id-or-title> --json

# Capture tmux pane content (key for monitoring)
aoe session capture <id-or-title> --json
aoe session capture <id-or-title> -n 100 --strip-ansi
aoe session capture <id-or-title>   # plain text, good for piping

# Quick status summary
aoe status --json
aoe status -q   # just the waiting count (for scripting)
```

`-n` has no cap, but capture only reaches back as far as tmux scrollback (`history-limit`, 2000 lines by default), and agent TUIs that redraw the screen leave little of a long turn in it. For a long report, ask the agent to write it to a file in its worktree and read that file from the host (`path` in `aoe session show --json`).

**JSON output shape** (`aoe session capture --json`):
```json
{
  "id": "a1b2c3d4-...",
  "title": "my feature",
  "status": "waiting",
  "tool": "claude",
  "content": "... pane text ...",
  "lines": 50
}
```

**JSON output shape** (`aoe session show --json`):
```json
{
  "id": "a1b2c3d4-...",
  "title": "my feature",
  "path": "/home/user/project",
  "group": "backend",
  "tool": "claude",
  "command": "claude",
  "status": "running",
  "state": "live",
  "profile": "default"
}
```

`state` is `live`, `archived`, or `trashed`, the same vocabulary `aoe list --json` uses; `trashed_at` and `archived_at` are set independently, and trashing deliberately leaves `archived_at` alone, so a session archived and then trashed carries both keys and reports `trashed`. Neither timestamp is durable, and they do not clear alike: `archived_at` is cleared by `aoe session unarchive`, by `aoe session favorite`, and by anything that wakes the session, which means `aoe send` and `aoe session restart` when it sends its wake message (with `session.restart_wake_message` set to the empty string it does not send one, and the archive survives the restart). `trashed_at` is cleared only by `aoe session restore`. Read the current `state` rather than caching a timestamp; `status` is the pane's live status and does not carry it, since an archived session can still be running.

The same two keys apply here as in `aoe list --json`: `pinned_at` whenever the session is pinned for the web sidebar, `snoozed_until` only while the snooze is still active.

**JSON output shape** (`aoe status --json`):
```json
{
  "waiting": 1,
  "running": 2,
  "idle": 1,
  "stopped": 1,
  "error": 0,
  "total": 5
}
```

### Auto-detection (inside a tmux pane)

When called from within an aoe-managed tmux session, identifier can be omitted:

```bash
aoe session show          # auto-detects current session
aoe session capture       # auto-detects current session
aoe session current --json
```

### Sending messages

```bash
aoe send <id-or-title> "message"
aoe send <id-or-title> "message" --no-revive   # fail instead of starting a stopped session
```

`aoe send` starts a stopped session unless `--no-revive` is given, waits at most 5 seconds for the agent to look ready, then sends anyway. A message that arrives before the agent's prompt is up is lost or left unsubmitted, so after launching, poll capture until the agent is ready instead of sleeping a fixed time. `status` also reads `idle` for a blank or unrecognized screen, such as the pane before Claude Code draws, so for Claude Code in any permission mode also require its `❯` input line. Send only after both were observed; on a capture error or deadline, stop without sending:

```bash
ready=
for _ in $(seq 60); do
  out=$(aoe session capture "fix-123" --json --strip-ansi) || { echo "capture failed" >&2; exit 1; }
  st=$(printf '%s\n' "$out" | jq -er .status) || { echo "unreadable capture" >&2; exit 1; }
  if [ "$st" = idle ] && printf '%s\n' "$out" | jq -r .content | grep -q '^[[:space:]]*❯'; then
    ready=1; break
  fi
  sleep 2
done
[ -n "$ready" ] || { echo "fix-123 not ready after 120s" >&2; exit 1; }
aoe send "fix-123" "Fix issue #123 and open a PR"
```

After sending, confirm the agent picked it up (status `running`, or the message in the transcript) before assuming delivery. Messages of 16 bytes or more, or with a newline, are pasted (bracketed when the agent enables it) and followed by Enter, and an agent may show them collapsed (Claude Code renders `[Pasted text #1 ...]`). Claude Code's input box can also show a dimmed suggested next prompt that looks like typed text; it was not submitted. Only text in the transcript above the input box has been sent.

### Renaming and organizing

```bash
aoe session rename <id> -t "new title"
aoe session rename <id> -g "new/group"

aoe group create mygroup
aoe group move <id-or-title> mygroup
aoe group list --json
aoe group delete mygroup --force
```

### Profiles

```bash
aoe profile list
aoe profile create staging
aoe profile delete staging
aoe profile default staging   # set default
aoe -p staging list            # use inline
```

### Worktrees

```bash
aoe worktree list
aoe worktree info <id-or-title>
aoe worktree cleanup -f
```

### Removing sessions

```bash
aoe remove <id-or-title>                    # move to trash (alias: rm)
aoe session restore <id-or-title>           # bring it back
aoe remove <id-or-title> --purge --delete-worktree --force   # delete permanently
```

With `session.delete_to_trash` on (the default), `aoe remove` stops the agent, keeps the record with `state` `trashed`, and moves an aoe-managed worktree into a `.aoe-trash/<id>` directory beside it. `--delete-worktree`, `--delete-branch`, and `--keep-container` only apply with `--purge`. For sandboxed sessions the move can fail (for example with `Permission denied`); aoe then prints `left worktree in place` and still trashes the session.

## Workflow patterns

### Single agent

```bash
aoe add /path/to/repo -t "feature X" -l
# ... wait ...
aoe session capture "feature X" --json
```

### Parallel worktree agents

```bash
aoe add . -t "issue-100" -w fix/issue-100 -b -l
aoe add . -t "issue-101" -w fix/issue-101 -b -l
aoe add . -t "issue-102" -w fix/issue-102 -b -l
aoe status --json   # check all at once
```

### Monitoring loop

Poll all sessions until none are running:

```bash
while true; do
  status=$(aoe status --json)
  waiting=$(echo "$status" | jq '.waiting')
  running=$(echo "$status" | jq '.running')
  if [ "$running" -eq 0 ] && [ "$waiting" -eq 0 ]; then
    echo "All agents finished"
    break
  fi
  echo "Running: $running, Waiting: $waiting"
  sleep 30
done
```

### Capture and review

```bash
for id in $(aoe list --json | jq -r '.[].id'); do
  echo "=== $id ==="
  aoe session capture "$id" -n 100 --strip-ansi
  echo
done
```

### Group operations via TUI

Groups are primarily managed through the `aoe` TUI (run `aoe` with no arguments). The TUI supports bulk start/stop/restart on groups. Use CLI commands above for scripted workflows.
