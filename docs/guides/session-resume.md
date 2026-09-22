# Native Session Resume

A terminal session resumes the same native agent conversation after a reboot, an `aoe` upgrade, or a tmux server restart, when the agent exposes an authoritative identity source. AoE records only identities attributable to that pane or to its physically isolated sandbox store; an ambiguous shared-store match is ignored rather than guessed.

Runtime conversation changes (`/clear`, `/new`, a fork, a fresh pane generation) rotate the recorded identity when the agent publishes the change. Neither the old identity nor an artifact predating the launch boundary can be recaptured after an AoE restart.

## Automatic capture matrix

| Agent | Host terminal | Sandboxed terminal | Authoritative source |
|-------|---------------|--------------------|----------------------|
| Claude Code | Yes | Yes | Pane-scoped native hook |
| Cursor Agent | Yes | Yes | `beforeSubmitPrompt` hook `conversation_id` |
| Pi | Yes | Yes | Pane-scoped AoE extension |
| OMP | Yes | Yes | Pane-scoped routed terminal store |
| OpenCode | Opt-in | No | AoE-preassigned native id |
| Codex, Gemini CLI, Hermes, Kimi CLI | No | Yes | Isolated managed store |
| Prime Agent | No | Yes | Root-only publication and isolated managed store |
| Vibe, Droid, Copilot CLI, Settl, Qwen Code, Kiro CLI, Antigravity | No | No | None verified |

`No` means automatic discovery is unsupported there, not that resume is: a user-provided exact id stays authoritative for any agent with a verified resume contract, and agents with no such contract reject automatic resume entirely. OpenCode host capture also needs `session.opencode_preassign_session_id = true`. AoE never scans a shared store or infers an identity from recency.

Sandbox config and conversation stores are staged per AoE instance, including custom `agent_config_dir` roots, and a cross-process lease guards each managed store, so two sessions in the same directory cannot claim each other's conversation.

Custom agents inherit native resume when `agent_detect_as` resolves to a built-in and the launch command is either that built-in's own binary token or a single bare token (the renamed-wrapper shape); command overrides follow the same rule. Path-qualified scripts, remote launchers, redirections, pipes, and other shell control syntax fail closed. Automatic capture is stricter still: a renamed wrapper keeps it only where the agent publishes its identity under the pane's own AoE marker (Claude, Cursor, Pi). Every other source infers ownership from the launch, so a wrapped OpenCode, OMP, or managed-store agent resumes a pinned id but captures nothing on its own.

Disabling `agent_status_hooks` removes status writers only; identity hooks declared for native resume stay installed.

**Prime Agent** captures depth-zero roots, not the child sessions its recursive runtime spawns, and requires `-e <extension>` plus a numeric `rlmDepth: 0` in native root headers. If a root publishes a conversation whose transcript is confirmed absent, a restart starts an empty conversation rather than resuming. Capture also needs a session directory mapped into its writable managed store, with bounded regular settings files: symlinked settings are refused, since their container-visible target cannot be inferred from the host path, and the refusal is logged under `session.capture` and retried after 30 seconds. Pass an explicit `--session-dir` inside `/root/.prime/agent` to select a verified directory.

## Pinning or resetting a conversation

```sh
aoe session set-session-id <session> <native-session-id>   # pin
aoe session set-session-id <session> ""                    # start fresh once
```

A pin is sticky: every launch resumes it until you change it. If AoE cannot prove a pinned conversation invalid and only sees the resumed pane exit, it keeps the id and reports a recoverable resume failure rather than silently starting fresh. Clearing is one-shot: the next launch starts fresh, and automatic capture takes over again where the matrix supports it. To drop the persisted state entirely, delete and recreate the session.

Structured-view sessions manage their conversation through ACP and reject `set-session-id`. Toggle the session out of structured view first, or set the resume target from the structured view UI.

State lives in the profile's `sessions.json`: `agent_session_id` (the observed conversation id), `resume_intent` (`Default`, `Use(id)`, or `Cleared`, absent when default), `resume_probe_failed_sid` (the last pinned id whose probe failed ambiguously, which stops startup recovery retrying it until you act), and, for Pi, `pi_session_path` (the transcript path the pane published, since Pi indexes conversations by the directory they started in). Only `resume_intent` is yours to set, through the CLI above.

## Forking a session

A fork starts a new, independent session from an existing session's conversation, so you can take the same history in a different direction. Only the fork is new: the original session and its transcript are untouched.

- **TUI**: the command palette's **Fork session (resume context, diverge)**, or **Fork session** on a session row's right-click menu. There is no keyboard shortcut by design. The new-session dialog opens prefilled with the source's working directory and group, titled `<name> (fork)`.
- **Web**: **Fork session** on the sidebar context menu of a forkable session.
- **CLI**: `aoe add --fork-from <session-id-or-title>`.

The fork inherits the parent's conversation, agent, group, and working directory. The directory is required so the agent can resolve the prior conversation, which is also why `--fork-from` cannot be combined with `--worktree` / `--new-branch`, `--sandbox` / `--sandbox-image`, or a `--cmd` carrying its own `--resume` flags. Passing a `--tool` that differs from the parent's agent is rejected rather than run against the wrong agent, since a captured conversation is agent-specific. From there the fork is its own session: its own id, its own row, restarts independent of the parent.

Forking needs an agent that can branch a conversation: claude, codex, and opencode for terminal sessions, and the Claude adapter for structured sessions. Resume-only agents (gemini, vibe, copilot) and agents without resume in AoE (cursor, droid, kiro, qwen) hide or refuse the action.

## Swapping the engine on a restart

The restart dialog can change the tool a session runs. Session IDs live in per-agent namespaces, so swapping to a different agent parks the outgoing agent's conversation under its own tool name and starts a new one; swapping back restores what was parked.

Two tool names can also point at the same agent on different accounts, through `[session.agent_config_dir]`. That swap changes the agent's config root, so the conversation is still on disk but under the account you swapped away from. AoE carries it across: it copies the transcript into the incoming account's config root so the agent resumes where it left off, and the row keeps its conversation id, model, and effort setting, since none of those changed agent.

The carry applies only when the new tool resolves to the same built-in agent, the session has a conversation to carry, and AoE knows the agent's transcript layout (Claude Code today). Anything else takes the parking swap above.

The outgoing account keeps its own copy, so swapping accounts back and forth stays continuous: the account you swap away from is the one that was just running, so on the way back its transcript replaces the older copy the earlier swap left behind. A copy the incoming account wrote more recently than the outgoing one is left alone.

## Picking up an upgraded agent CLI

Upgrading the agent binary from inside a session does not replace the process in the pane. Restart the session instead of creating a new one: press `e` (`E` with strict hotkeys) or `F5`, or run `aoe session restart <session>` (`--all` for every session in the profile). A restart runs only `on_launch`, whose failures are warnings, and resumes the conversation while `session.auto_resume_on_restart` is on (the default). A new session runs `on_create`, whose failure [aborts creation](repo-config.md#hooks).

## Importing an existing Claude conversation

Conversations started outside AoE can be pulled into a structured-view session from the web wizard's **Import from Claude** tab, which appears only when both Claude Code and `claude-agent-acp` are installed, since the import resumes through that adapter. It lists the Claude Code sessions on disk (under `$CLAUDE_CONFIG_DIR` or `~/.claude/projects`), newest first, with each one's first prompt, working directory, and last-used time.

Picking one creates a structured-view session in that conversation's original working directory and resumes it, so the prior transcript is there and you can keep going. It always uses the recorded directory and never creates a worktree, because the conversation only resolves where it started. The original is read in place, not copied.

The list hides conversations not worth importing: AoE's own Claude sessions, scratch sessions, and anything inside an AoE worktree directory. Sessions whose directory no longer exists are hidden until you tick "show missing directories", and then shown disabled.
