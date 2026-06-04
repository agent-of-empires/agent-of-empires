# Session Resume (Claude)

Agent of Empires can persist Claude Code conversation IDs so sessions resume their prior context after a reboot, an `aoe` upgrade, or a `kill-server`. No more hunting through `/resume` to find the right session.

## How it works

When you launch a Claude session through AoE, AoE generates a UUID and passes it to `claude --session-id <uuid>`. Claude uses that UUID for the conversation; AoE records it in `sessions.json`. On every subsequent launch of the same instance, AoE invokes `claude --resume <uuid>` so the conversation picks up where it left off.

AoE tracks the active session ID via two converging sources:

1. **Hook sidecar (primary, near-instant).** AoE installs `SessionStart` and `UserPromptSubmit` hooks into `~/.claude/settings.json`. These hooks extract the active `session_id` from Claude's stdin payload and write it atomically to `/tmp/aoe-hooks/<instance-id>/session_id`. The poller reads this file before scanning the filesystem, so runtime rotations via `/clear`, `--fork-session`, or `--continue` are picked up within one poll tick (~2 s).
2. **Filesystem scan (fallback).** If the sidecar is absent, stale (> 5 min), or invalid, the poller falls back to scanning `~/.claude/projects/<project>/` for the most recent `.jsonl`. Sibling AoE instances sharing the same project path are filtered out via tmux env (`AOE_CAPTURED_SESSION_ID`) so each session keeps its own UUID.

For sandboxed (Docker) sessions, the filesystem scan runs inside the container via `docker exec` (capped at 5 seconds per call). The hook sidecar is host-only today; sandboxed `/clear` adoption falls back to the filesystem scan and resolves within one poll tick.

## What's covered

- Launch, store, resume across reboots and `aoe` upgrades, in both host and sandboxed modes.
- Runtime rotation via `/clear`, `--fork-session`, or fresh `claude` invocation in the same pane.
- Manual override via the CLI when you want to point a session at a specific conversation.

## Manual override

The CLI sets the *resume intent* for the next launch. Intent is decoupled from the poller's observed session ID, so a peer write cannot be silently undone by the poller, and a daemon restart cannot resurrect a value the user explicitly cleared.

Pin to a specific conversation:

```sh
aoe session set-session-id <session-name-or-id> <claude-session-uuid>
```

The pin is sticky: every subsequent launch passes `--resume <uuid>` until the user changes it.

Force a fresh start:

```sh
aoe session set-session-id <session-name-or-id> ""
```

The clear is one-shot: the next launch starts fresh, after which the system reverts to auto-resume from whatever conversation the agent ends up using. To stay fresh on every launch, clear before each restart.

If the cascade detects that a pinned conversation is no longer valid (the agent fails to resume it), the pin is automatically downgraded so the next launch is fresh.

`set-session-id` rejects structured-view sessions: structured view manages its own conversation lifecycle through ACP, and a CLI-set intent would be ignored. Toggle the session out of structured view first, or set the resume target through the structured view UI.

The persist after a successful launch is crash-safe: the new session ID and the one-shot `Cleared` auto-promote land in a single atomic flock, so a daemon crash mid-finalize cannot freeze disk in a state that would orphan the conversation just created. If a peer CLI write to `resume_intent` lands during the launch window, the peer's value is preserved (sid persisted, intent left as written) and the next launch follows the peer's pin.

## Disabling

There is no toggle. To start fresh once, use `set-session-id ""`. To delete the persisted state entirely, delete the session and recreate it.

## Storage

The session state lives in `sessions.json` in your AoE config directory:

- **Linux**: `$XDG_CONFIG_HOME/agent-of-empires/profiles/<profile>/sessions.json`
- **macOS/Windows**: `~/.agent-of-empires/profiles/<profile>/sessions.json`

Two fields are relevant:

- `agent_session_id`: what the poller has observed. Auto-managed; do not edit.
- `resume_intent`: user intent (`Default`, `Use(uuid)`, `Cleared`). Set via the CLI command above. Absent in the JSON when `Default`.
