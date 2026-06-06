# Session Resume (Claude)

Claude Code sessions launched through AoE resume their prior conversation automatically after a reboot, an `aoe` upgrade, or a `kill-server`. No need to hunt through `/resume` to find the right session.

This is automatic and on by default. Runtime conversation changes (via `/clear`, `--fork-session`, `--continue`, or starting fresh in the pane) are picked up too, in both host and sandboxed (Docker) modes.

## Pinning or resetting a conversation

Pin a session to a specific Claude conversation:

```sh
aoe session set-session-id <session-name-or-id> <claude-session-uuid>
```

The pin is sticky: every launch passes `--resume <uuid>` until you change it. If a pinned conversation becomes invalid, the next launch starts fresh automatically.

Start fresh once:

```sh
aoe session set-session-id <session-name-or-id> ""
```

This is one-shot; the next launch starts fresh, then auto-resume takes over again. To stay fresh every launch, clear before each restart.

Structured-view sessions manage their own conversation through ACP and reject `set-session-id`. Toggle the session out of structured view first, or set the resume target through the structured view UI.

## Disabling

There is no toggle. To start fresh once, use `set-session-id ""`. To drop the persisted state entirely, delete the session and recreate it.

## Storage

State lives in `sessions.json` in your AoE config directory:

- **Linux**: `$XDG_CONFIG_HOME/agent-of-empires/profiles/<profile>/sessions.json`
- **macOS/Windows**: `~/.agent-of-empires/profiles/<profile>/sessions.json`

Two relevant fields:

- `agent_session_id`: the observed conversation ID. Auto-managed; do not edit.
- `resume_intent`: your intent (`Default`, `Use(uuid)`, `Cleared`). Set via the CLI above. Absent when `Default`.
