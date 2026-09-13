# Forking Sessions

Forking a session starts a new, independent AoE session from an existing session's conversation context, so you can take the same history in a different direction. The original session and its transcript are left untouched.

To branch a conversation into a new session rather than continuing the same one in place, fork it. (To continue the same conversation, see [Session Resume](./session-resume.md).)

## How to fork

### TUI

Open the command palette and run **Fork session (resume context, diverge)**, or right-click a session row and choose **Fork session**. There is no keyboard shortcut by design; the palette and the context menu are the two entry points.

The new-session dialog opens prefilled with the source working directory, group, and title `<name> (fork)`. Changes to the native agent, conversation store, working directory, or filesystem are checked again before launch. A rejected launch preserves the pending fork instead of starting a fresh conversation.

### Web dashboard

Open a session's context menu in the sidebar and choose **Fork session**. The option appears for forkable structured sessions.

### CLI

Pass `--fork-from` with the source session's id or title:

```sh
aoe add --fork-from <session-id-or-title>
```

This creates a terminal session that resumes the source's conversation and then runs independently.

The fork inherits the parent tool by default. Its conversation must have a known native agent and store, established by a qualified publication, an import, or an explicit [recovery assertion](session-resume.md#pinning-or-resetting-a-conversation). A raw or preallocated ID is not enough. Status detection and matching tool labels do not authorize a fork. AoE rejects conflicting native commands and user-supplied resume/fork selectors before dispatch. `--fork-from` cannot be combined with `--worktree` / `--new-branch` or `--sandbox` / `--sandbox-image`.

## What gets inherited

The fork inherits:

- The parent's conversation context, which the fork resumes from.
- The working directory. This is required so the agent can resolve the prior conversation, so the fork runs in the same directory.
- The group.
- The tool (agent).

The fork has its own AoE ID and native conversation. Automatic recovery after a restart depends on that agent’s [capture capability and supported execution context](session-resume.md); dispatching a native fork does not add a new way to discover its child ID.

## The original is untouched

Forking only reads the parent's conversation. The parent session and its transcript are never modified, so you end up with two independent sessions: the original, exactly as it was, and the new fork.

## Which agents support forking

Forking needs an agent that can branch a conversation:

- **Terminal sessions**: claude, codex, and opencode, within the [supported managed contexts](session-resume.md#supported-managed-contexts).
- **Structured (ACP) sessions**: the Claude adapter (`claude-agent-acp`).

Resume-only agents (such as gemini, vibe, and copilot) and agents without resume enabled in AoE (such as cursor, droid, kiro, and qwen) cannot fork. For those, the Fork option is hidden or refused.

## Fork vs. resume

Resume continues the same conversation in place; fork branches it into a new, separate one. For resuming a session's own conversation, see [Session Resume](./session-resume.md).
