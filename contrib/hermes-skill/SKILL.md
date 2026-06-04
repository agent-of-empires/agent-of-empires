---
name: aoe
description: Use when launching, monitoring, or controlling AI coding agents (Claude Code, Codex, OpenCode, etc.) in tmux via Agent of Empires (aoe). Covers creating sessions, capturing agent output, running parallel worktree agents, and organizing work into groups and profiles. Prefer aoe over raw tmux for agent management.
version: 1.0.0
author: njbrake (Agent of Empires)
license: MIT
metadata:
  hermes:
    tags: [coding-agents, tmux, orchestration, sessions, worktrees, automation]
    related_skills: [subagent-driven-development]
---

# Agent of Empires (aoe)

## Overview

`aoe` creates, manages, and monitors AI coding agent sessions (Claude Code, Codex, OpenCode, and others) inside tmux. Each session is an agent process with an ID, title, tool, project path, and live status. Use `aoe` instead of raw `tmux` commands whenever the work is about coding agents: it tracks status, captures output, manages git worktrees for parallel branches, and organizes sessions into groups and profiles.

## When to Use

- Launching one or more AI coding agents on project directories.
- Monitoring agent progress (waiting vs running vs idle).
- Capturing agent output for review.
- Organizing agents into groups or profiles.
- Setting up parallel worktree-based development.

**Don't use for:** general tmux window/pane management unrelated to coding agents.

## Requirements

The `aoe` and `tmux` binaries must be on `PATH`, and commands run through a shell. Install aoe from https://github.com/agent-of-empires/agent-of-empires.

## Core Concepts

- **Session**: An agent process running in a tmux session. Each has an ID, title, tool (e.g. `claude`), and project path.
- **Group**: A named folder for organizing sessions (supports nesting with `/`, e.g. `backend/api`).
- **Profile**: A separate workspace with its own sessions and config. Use `-p <name>` globally or set `AGENT_OF_EMPIRES_PROFILE`.
- **Status**: One of `running`, `waiting`, `idle`, `stopped`, `error`, `starting`, `unknown`.

## Adding Sessions

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

## Listing Sessions

```bash
aoe list              # human-readable
aoe list --json       # JSON for parsing
aoe list --all        # across all profiles
```

**JSON shape** (`aoe list --json`):
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

`command` is omitted when empty; `worktree` appears only for worktree-backed sessions. `list --json` does not include live status: use `aoe status --json` or `aoe session capture --json` for that.

## Session Lifecycle

```bash
aoe session start <id-or-title>
aoe session stop <id-or-title>
aoe session restart <id-or-title>
aoe session attach <id-or-title>   # interactive attach
```

## Inspecting Sessions

```bash
# Session metadata
aoe session show <id-or-title> --json

# Capture tmux pane content (key for monitoring)
aoe session capture <id-or-title> --json
aoe session capture <id-or-title> -n 100 --strip-ansi
aoe session capture <id-or-title>   # plain text, good for piping

# Quick status summary
aoe status --json
aoe status -q   # just the waiting count (for scripting)
```

**JSON shape** (`aoe session capture --json`):
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

**JSON shape** (`aoe session show --json`):
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

`parent_session_id` is included only for sub-sessions.

**JSON shape** (`aoe status --json`):
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

When called from within an aoe-managed tmux session, the identifier can be omitted:

```bash
aoe session show          # auto-detects current session
aoe session capture       # auto-detects current session
aoe session current --json
```

## Renaming and Organizing

```bash
aoe session rename <id> -t "new title"
aoe session rename <id> -g "new/group"

aoe group create mygroup
aoe group move <id-or-title> mygroup
aoe group list --json
aoe group delete mygroup --force
```

## Profiles

```bash
aoe profile list
aoe profile create staging
aoe profile delete staging
aoe profile default staging   # set default
aoe -p staging list           # use inline
```

## Worktrees

```bash
aoe worktree list
aoe worktree info <id-or-title>
aoe worktree cleanup -f
```

## Removing Sessions

```bash
aoe remove <id-or-title>
aoe remove <id-or-title> --delete-worktree --force
```

## Workflow Patterns

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

Poll all sessions until none are running or waiting:

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

## Common Pitfalls

1. **Expecting `aoe list --json` to carry live status.** It does not. The fields are static session metadata (`path`, `group`, `tool`, `command`, etc.). For status, call `aoe status --json` or `aoe session capture --json`.
2. **Using raw `tmux` to start or stop agents.** That bypasses aoe's tracking; the session's status and metadata go stale. Always use `aoe session start/stop/restart`.
3. **Forgetting `-l`/`--launch`.** `aoe add` creates a session but does not start it unless you pass `-l`.
4. **Running across the wrong profile.** Sessions are profile-scoped; use `-p <name>` or set `AGENT_OF_EMPIRES_PROFILE` when scripting, and `aoe list --all` to see everything.

## Verification Checklist

- [ ] `aoe` and `tmux` are on `PATH`.
- [ ] `aoe add` was followed by a launch (`-l`) or an explicit `aoe session start`.
- [ ] JSON parsing reads `path`/`group` (not `project_path`/`group_path`) and gets status from `aoe status`/`aoe session capture`, not `aoe list`.
- [ ] Scripted polling exits when both `running` and `waiting` reach 0.
