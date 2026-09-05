# Native Session Resume

AoE terminal sessions resume the same native agent conversation after a reboot, an `aoe` upgrade, or a tmux server restart when the agent exposes an authoritative identity source. AoE records only identities attributable to that pane or to its physically isolated sandbox store. An ambiguous shared-store match is ignored rather than guessed.

Runtime conversation changes such as `/clear`, `/new`, fork, continue, or a fresh pane generation rotate the recorded identity when the upstream agent publishes the change. The old identity and any artifact predating the launch boundary cannot be recaptured after an AoE process restart.

## Automatic capture matrix

| Agent | Host terminal | Sandboxed terminal | Authoritative source |
|-------|---------------|--------------------|----------------------|
| Claude Code | Yes | Yes | Pane-scoped native hook |
| OpenCode | Opt-in | No | AoE-preassigned native ID |
| Vibe | No | No | None verified |
| Codex | No | Yes | Isolated managed store |
| Gemini CLI | No | Yes | Isolated managed store |
| Cursor Agent | Yes | Yes | `beforeSubmitPrompt` hook `conversation_id` |
| Droid | No | No | None verified |
| Pi | Yes | Yes | Pane-scoped AoE extension |
| GitHub Copilot CLI | No | No | None verified |
| Settl | No | No | None verified |
| Hermes | No | Yes | Isolated managed store |
| Qwen Code | No | No | None verified |
| Kiro CLI | No | No | None verified |
| Antigravity | No | No | None verified |
| Kimi CLI | No | Yes | Isolated managed store |
| OMP | Yes | Yes | Pane-scoped routed terminal store |
| Prime Agent | No | Yes | Isolated managed store |

`No` means automatic identity discovery is unsupported in that environment. OpenCode host capture additionally requires `session.opencode_preassign_session_id = true`. AoE does not scan a shared store or infer an identity from recency. For an agent with native resume support, a user-provided exact ID remains authoritative and can still be passed explicitly in an unsupported automatic-capture environment. Agents with no verified native resume contract reject automatic resume entirely.

Sandbox config and conversation stores are staged under a separate directory for each AoE instance, including custom `agent_config_dir` roots. A cross-process lease guards each managed store. Two sessions in the same working directory therefore cannot claim each other's conversation.

Custom agents inherit native resume when `agent_detect_as` resolves to a built-in agent and the configured launch is either that built-in's own binary token or a single bare token, which is the renamed-wrapper shape. Built-in command overrides follow the same rule. Path-qualified scripts, remote launchers, comments, redirections, pipes, shell expansion, and other shell control syntax fail closed. A bare token is resolved by the launch shell's `PATH`, which is what ties it to the agent AoE resolved.

Automatic capture is stricter than resume. A renamed wrapper keeps automatic capture only where the agent publishes its identity under the pane's own AoE marker, meaning Claude, Cursor, and Pi. Every other source infers ownership from the launch itself and needs the built-in's exact binary token, so a wrapped OpenCode, OMP, or managed-store agent resumes an explicitly pinned ID but captures nothing on its own.

Disabling `agent_status_hooks` removes status writers only. Any authoritative identity hooks declared for native resume remain installed.

To branch a conversation into a new session instead of resuming it in place, see [Forking Sessions](./session-fork.md).

## Pinning or resetting a conversation

Pin a terminal session to a specific native conversation:

```sh
aoe session set-session-id <session-name-or-id> <native-session-id>
```

The pin is sticky: every launch uses the agent's native resume argument until you change it. If AoE cannot prove whether a pinned conversation is invalid and only sees the resumed pane exit, it preserves the pinned ID and reports a recoverable resume failure instead of starting fresh automatically.

Retry after fixing the underlying issue, set a different conversation ID, or explicitly start fresh once:

```sh
aoe session set-session-id <session-name-or-id> ""
```

This is one-shot. The next launch starts fresh, then automatic capture takes over again when the matrix supports that environment.

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
- `resume_intent`: your intent (`Default`, `Use(id)`, `Cleared`). Set via the CLI above. Absent when `Default`.
- `resume_probe_failed_sid`: the last pinned ID whose resume probe failed ambiguously.
  This loop-breaker prevents startup recovery from retrying that same ID automatically until user action changes the resume state.
