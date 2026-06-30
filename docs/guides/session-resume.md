# Session Resume (Claude)

Claude Code sessions launched through AoE resume their prior conversation automatically after a reboot, an `aoe` upgrade, or a `kill-server`. No need to hunt through `/resume` to find the right session.

This is automatic and on by default. Runtime conversation changes (via `/clear`, `--fork-session`, `--continue`, or starting fresh in the pane) are picked up too, in both host and sandboxed (Docker) modes.

To branch a conversation into a new session instead of resuming it in place, see [Forking Sessions](./session-fork.md).

## Pinning or resetting a conversation

Pin a session to a specific Claude conversation:

```sh
aoe session set-session-id <session-name-or-id> <claude-session-uuid>
```

The pin is sticky: every launch passes `--resume <uuid>` until you change it. If AoE cannot prove whether a pinned conversation is invalid and only sees the resumed pane exit, it preserves the pinned ID and reports a recoverable resume failure instead of starting fresh automatically.

Retry after fixing the underlying issue, set a different conversation ID, or explicitly start fresh once with the command below.

Start fresh once:

```sh
aoe session set-session-id <session-name-or-id> ""
```

This is one-shot; the next launch starts fresh, then auto-resume takes over again. To stay fresh every launch, clear before each restart.

Structured-view sessions manage their own conversation through ACP and reject `set-session-id`. Toggle the session out of structured view first, or set the resume target through the structured view UI.

## Importing existing Claude Code sessions (web dashboard)

If you already have Claude Code conversations started outside AoE (plain `claude` in a terminal), you can pull one into a structured-view session from the web dashboard.

In the new-session wizard, open the **Import from Claude** tab. The tab only appears when both Claude Code and its ACP adapter (`claude-agent-acp`) are installed, since the import resumes the conversation through that adapter. It lists the Claude Code sessions found on disk (under `$CLAUDE_CONFIG_DIR` or `~/.claude/projects`), newest first, with each session's first prompt, working directory, and last-used time. Type in the filter box to narrow by title or path.

Pick a session and launch. AoE creates a structured-view session in that conversation's original working directory and resumes it, so the prior transcript shows up in the structured view and you can keep going. The import always uses the recorded working directory and does not create a worktree, because the conversation only resolves in the directory it was started in.

The list only shows conversations worth importing: AoE's own Claude sessions are filtered out, including scratch sessions, sessions AoE already manages, and any conversation living inside an AoE worktree directory (the `*-worktrees` folders AoE creates for sessions). Sessions whose working directory no longer exists are hidden by default, since they cannot be resumed; tick "show missing directories" to see them (they appear disabled).

This reads the existing conversation in place; the original session keeps existing and is not copied.

## Disabling

There is no toggle. To start fresh once, use `set-session-id ""`. To drop the persisted state entirely, delete the session and recreate it.

## Storage

State lives in `sessions.json` in your AoE config directory:

- **Linux**: `$XDG_CONFIG_HOME/agent-of-empires/profiles/<profile>/sessions.json`
- **macOS/Windows**: `~/.agent-of-empires/profiles/<profile>/sessions.json`

Three relevant fields:

- `agent_session_id`: the observed conversation ID. Auto-managed; do not edit.
- `resume_intent`: your intent (`Default`, `Use(uuid)`, `Cleared`). Set via the CLI above. Absent when `Default`.
- `resume_probe_failed_sid`: the last pinned ID whose resume probe failed ambiguously.
  This loop-breaker prevents startup recovery from retrying that same ID automatically until user action changes the resume state.
