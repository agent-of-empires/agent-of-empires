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

# Add in a git worktree (parallel branch)
aoe add . -t "fix-123" -w fix/issue-123 -l

# Add in Docker sandbox
aoe add . -t "sandboxed" -s -l

# Add as sub-session of another
aoe add . -t "sub task" -P <parent-id>

# Enable YOLO mode (skip permission prompts)
aoe add . -t "yolo" -y -l
```

### Listing sessions

```bash
# Human-readable list
aoe list

# JSON output for parsing
aoe list --json

# List across all profiles
aoe list --all
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
    "created_at": "2025-01-01T00:00:00Z",
    "workspace_repos": []
  }
]
```

`command` is omitted when empty; `worktree` appears only for worktree-backed sessions. `list --json` does not include live status; use `aoe status --json` or `aoe session capture --json` for that.

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
  "profile": "default"
}
```

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
aoe remove <id-or-title>
aoe remove <id-or-title> --delete-worktree --force
```

## Workflow patterns

### Single agent

```bash
aoe add /path/to/repo -t "feature X" -l
# ... wait ...
aoe session capture "feature X" --json
```

### Parallel worktree agents

```bash
aoe add . -t "issue-100" -w fix/issue-100 -l
aoe add . -t "issue-101" -w fix/issue-101 -l
aoe add . -t "issue-102" -w fix/issue-102 -l
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
