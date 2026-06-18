# Session & Worktree Internals

Contributor reference for the session layer: Claude conversation resume, worktree creation, scratch-session cleanup, and MCP forwarding. Users want the corresponding guides ([Session Resume](../../guides/session-resume.md), [Git Worktrees](../../guides/worktrees.md), [Scratch Sessions](../../guides/scratch-sessions.md), [MCP Servers](../../guides/mcp-servers.md)).

## Claude conversation resume

aoe persists each Claude Code session's conversation id so a session resumes the same transcript across restarts. The flow: aoe generates a UUID, launches `claude --session-id <uuid>`, records it, and relaunches with `claude --resume <uuid>`.

Two mechanisms keep the recorded id current as Claude rotates it (on `/clear`, `--fork-session`, `--continue`):

- **Hook sidecar (primary).** aoe installs `SessionStart` and `UserPromptSubmit` hooks in `~/.claude/settings.json`. They extract `session_id` from Claude's stdin and write it atomically to `/tmp/aoe-hooks-<euid>/<instance-id>/session_id` (per-user host base, issue #1844). The poller reads this before scanning, so rotations are caught within ~1 poll tick (~2s). The sidecar is host-only.
- **Filesystem-scan fallback.** If the sidecar is absent, stale (>5 min), or invalid, the poller scans `~/.claude/projects/<project>/` for the most recent `.jsonl`. Siblings sharing a project path are disambiguated via the tmux env `AOE_CAPTURED_SESSION_ID`. For Docker sessions the scan runs in-container via `docker exec` (5s cap).

`resume_intent` is decoupled from the poller's observed id so a peer CLI write isn't undone and a daemon restart can't resurrect a cleared value. The post-launch persist of the new id plus the one-shot `Cleared` auto-promote land in a single atomic flock, preserving a concurrent peer write during the launch window.

## Worktree creation

`git worktree add` only checks out tracked files; it does not copy `node_modules`, `.venv`, or `target/`, so creation is cheap and network IO (`git fetch`, `git submodule update`) dominates almost every slow run. For [multi-repo workspaces](../../guides/multi-repo-workspaces.md), the per-repo `create_worktree` calls run concurrently via `std::thread::scope`. See [Git Worktrees](../../guides/worktrees.md) for the bare-repo layout aoe auto-detects.

## Scratch-session cleanup

Scratch sessions store their working dir under the app data dir (not `/tmp`) so it survives reboots and stays inside the namespace the daemon sweeps. Delete-time cleanup runs only when the session's `scratch` flag is true AND the path lives under the scratch root: a tampered `project_path` pointing at, say, `/etc` is left alone. This invariant is what makes the orphan sweep safe to run unattended.

## MCP server forwarding

Native agent MCP configs are re-read live at session start, so edits apply on the next session; aoe only reads these files, never writes them. Precedence is per-server, not whole-file, and overrides are logged. Project-local `.mcp.json` from a repo must sit behind the same repo-trust gate as lifecycle hooks (an untrusted clone could otherwise launch commands on session open); per-profile and project-local source layers are tracked as higher layers.
