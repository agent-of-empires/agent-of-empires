# Native Session Resume

Explicit resume and fork require the native agent, physical store, working directory, and filesystem to agree with the prepared launch. Automatic start and restart also try a stored native ID when its binding cannot be attested, using the agent's normal resume selector and failed-resume probe. A status label or raw session ID does not qualify that ID for explicit use. Existing transcripts are never deleted to repair a mismatch.

Runtime conversation changes such as `/clear`, `/new`, fork, continue, or a fresh pane generation rotate the recorded identity when the native agent publishes the change. The old identity and any artifact predating the launch boundary cannot be recaptured after an AoE process restart.

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

`agent_detect_as` controls status detection and ACP adapter inheritance, not terminal execution identity. Direct built-in commands identify their native agent independently. Opaque wrappers need the explicit contract described below for explicit resume or fork and capture from shared stores.

## Execution identity and wrappers

A conflicting built-in tool and command, such as tool `claude` with command `codex`, is rejected for managed resume and fork. An opaque wrapper requires both `session.agent_execution_as` and `session.agent_config_dir` in trusted global or profile configuration. This asserts that the wrapper invokes that native agent, forwards native arguments unchanged, and uses only the declared store and working-directory/filesystem context. Repository overrides are refused because a repository must not grant itself access to another native conversation store.

A Default start or restart can pass native resume flags to a bare, non-path wrapper mapped by `agent_detect_as` and capture its pane-scoped published ID (for example Claude, Cursor, or Pi), even without the execution contract. If a failed-resume marker or disabled `auto_resume_on_restart` selects a fresh launch, Cleared intent starts fresh through the same wrapper, minting an ID and passing fresh-session flags where supported, then capturing its pane-scoped publication. The failed-resume marker applies to the next automatic attempt for that ID; a subsequent restart follows the resulting durable conversation state. Neither path proves the wrapper's execution identity or store. Shared-store capture, explicit resume, and fork still require the contract; shell-active commands, remote launchers, paths, and different built-in commands do not gain the exception. A known conversation can also be tried after an external working-directory or store move on Default; its old binding stays authoritative until a qualified observation updates it. Explicit operations still require a matching known binding before launch.

Shell pipelines, remote launchers, redirections, expansion, and unrecognized context-changing arguments are not supported managed invocations. The program, routing environment, and native namespace arguments are fixed from one validated launch snapshot and restored after the login shell. If shell startup changes a pinned routing value, AoE refuses the launch instead of dispatching against the wrong store.

## Supported managed contexts

- **Claude:** the resolved `CLAUDE_CONFIG_DIR` or default Claude store. Conflicting store selectors are refused.
- **Codex:** a local host-readable `CODEX_HOME`, file-backed API-key authentication, and local SQLite/thread storage. Cloud, keyring, profile, project-routing, managed-policy, and host macOS contexts are not currently proven.
- **OpenCode:** an explicit `OPENCODE_DB`, or the common database when `OPENCODE_DISABLE_CHANNEL_DB` is already enabled. A workspace-routed target or different stored working directory is refused.
- **Pi and OMP:** the verified transcript and exact store. Recovery uses `--store` with the transcript file. OMP also verifies its stored working directory and pins the resolved profile.
- **Gemini, Cursor, Kimi, and Copilot:** their native config/share/home store inputs. AoE does not combine unrelated environment roots.
- **Prime:** the resolved agent root and session directory, including a verified `--cwd`. A declared wrapper can resume an explicitly bound conversation but does not gain capture or fork capability.
- **Hermes:** an explicitly declared configuration root and local `state.db`. Stored lineage and working directory must match the launch context.
- **Vibe:** an explicitly declared configuration root and default session store. Remote and alternate-store selectors are refused.

Disabling `agent_status_hooks` removes status writers only; identity hooks declared for native resume stay installed.

**Prime Agent** captures depth-zero roots, not the child sessions its recursive runtime spawns, and requires `-e <extension>` plus a numeric `rlmDepth: 0` in native root headers. If a root publishes a conversation whose transcript is confirmed absent, a restart starts an empty conversation rather than resuming. Capture also needs a session directory mapped into its writable managed store, with bounded regular settings files: symlinked settings are refused, since their container-visible target cannot be inferred from the host path, and the refusal is logged under `session.capture` and retried after 30 seconds. Pass an explicit `--session-dir` inside `/root/.prime/agent` to select a verified directory.

## Pinning or resetting a conversation

Pin a terminal session to a specific native conversation:

```sh
aoe session set-session-id <session-name-or-id> <native-session-id>
```

This records an assertion about the intended native target, separately from any observed conversation. The pin is sticky. If the execution context changes, restore it or explicitly rebind the intended conversation before retrying. Legacy IDs and IDs from old unqualified publishers remain unknown after migration; current configuration does not relabel them.

Automatic start or restart attempts an unknown stored ID instead of discarding it. A capture from an attested launch can qualify the binding; without an attested launch it remains unknown even if native resume succeeds. If the native resume probe fails, AoE preserves the ID and starts fresh on the next automatic restart. Explicit resume pins and forks still require a qualified binding. To assert a store explicitly:

```sh
aoe session set-session-id <session> <native-id> --store /absolute/native/store
```

`--store` explicitly selects a Claude configuration directory. For Pi and OMP it must name the exact existing transcript file, whose header must name the requested ID. Other agents resolve their store from configuration and reject explicit store routing.

To start fresh once:

```sh
aoe session set-session-id <session-name-or-id> ""
```

Automatic capture then takes over where supported. The abandoned conversation stays excluded only in its recorded agent, store, and filesystem namespace. Legacy exclusions without a known namespace remain ID-wide.

Structured-view conversations remain managed by ACP. For a Claude terminal handoff, AoE records the native execution resolved for the current ACP ID. A handoff whose store cannot be resolved, or which the structured worker does not share, is refused before worker teardown with recovery guidance. Other structured resume-target changes are rejected.

## Forking a session

A fork starts a new, independent session from an existing session's conversation, so you can take the same history in a different direction. Only the fork is new: the original session and its transcript are untouched.

- **TUI**: the command palette's **Fork session (resume context, diverge)**, or **Fork session** on a session row's right-click menu. The dialog inherits the source working directory, group, and title. A rejected launch preserves the pending fork instead of silently starting fresh.
- **Web**: **Fork session** on the sidebar context menu of a forkable session.
- **CLI**: `aoe add --fork-from <session-id-or-title>`.

The fork inherits the parent tool, group, working directory, and qualified conversation binding. That binding must establish the native agent and store through a qualified publication, import, or explicit recovery assertion. A raw or preallocated ID is insufficient; status detection and matching tool labels grant no authority. A different tool, conflicting native command, or user-supplied resume/fork selector is refused before dispatch without clearing the pending fork. `--fork-from` cannot be combined with `--worktree` / `--new-branch` or `--sandbox` / `--sandbox-image`.

The child gets its own AoE ID and native conversation. The parent row and transcript remain unchanged. Automatic recovery still depends on the agent's capture capability and supported execution context; dispatching a native fork adds no new child-ID discovery path.

Forking needs an agent that can branch a conversation: claude, codex, and opencode in the supported managed contexts, and the Claude adapter for structured sessions. Resume-only agents (gemini, vibe, copilot) and agents without resume in AoE (cursor, droid, kiro, qwen) hide or refuse the action.

## Swapping the engine on a restart

The restart dialog can change the tool a session runs. Session IDs live in per-agent namespaces, so swapping to a different agent parks the outgoing agent's conversation under its own tool name and starts a new one; swapping back restores what was parked.

Two tool names can also point at the same agent on different accounts, through `[session.agent_config_dir]`. That swap changes the agent's config root, so the conversation is still on disk but under the account you swapped away from. AoE carries it across: it copies the transcript into the incoming account's config root so the agent resumes where it left off, and the row keeps its conversation id, model, and effort setting, since none of those changed agent.

The carry applies only when the new tool resolves to the same built-in agent, the session has a conversation to carry, and AoE knows the agent's transcript layout (Claude Code today). Anything else takes the parking swap above.

The outgoing account keeps its own copy, so swapping accounts back and forth stays continuous: the account you swap away from is the one that was just running, so on the way back its transcript replaces the older copy the earlier swap left behind. A copy the incoming account wrote more recently than the outgoing one is left alone.

Editing one tool's `agent_config_dir` entry in place is not that swap. A Claude conversation on the host resumes in the store its own binding recorded, and that store outranks the entry, so repointing or removing it leaves the session on the account it recorded and only new sessions follow the entry. A structured session resuming the same conversation pins that store without logging, and a sandboxed Claude session follows the entry instead, because its store is a per-session child of the entry ([Per-session agent stores](sandbox.md#per-session-agent-stores)).

A host launch records a `session.store` warning when the two stores differ, naming the store the launch used as `launch_store`, the store a new session would take as `new_session_store`, and where that one comes from as `new_session_store_source`: `agent_config_dir`, `environment`, or `default`. The line goes to the log the TUI and the daemon write, which `aoe logs` opens, and the default level passes it. A one-shot `aoe session restart` has no log of its own, so the line appears there only with `AOE_LOG_LEVEL` or `AGENT_OF_EMPIRES_DEBUG` set, and a level of `error` filters it away. See [Environment variables](configuration.md#environment-variables) for both.

Rebinding the record with `aoe session set-session-id --store` copies nothing, so it only resumes if the target account already holds the conversation; swapping tool names as above is what has AoE copy the transcript.

## Picking up an upgraded agent CLI

Upgrading the agent binary from inside a session does not replace the process in the pane. Restart the session instead of creating a new one: press `e` (`E` with strict hotkeys) or `F5`, or run `aoe session restart <session>` (`--all` for every session in the profile). A restart runs only `on_launch`, whose failures are warnings, and resumes the conversation while `session.auto_resume_on_restart` is on (the default). A new session runs `on_create`, whose failure [aborts creation](repo-config.md#hooks).

## Importing an existing Claude conversation

Conversations started outside AoE can be pulled into a structured-view session from the web wizard's **Import from Claude** tab, which appears only when both Claude Code and `claude-agent-acp` are installed, since the import resumes through that adapter. It lists the Claude Code sessions on disk (under `$CLAUDE_CONFIG_DIR` or `~/.claude/projects`), newest first, with each one's first prompt, working directory, and last-used time.

Picking one creates a structured-view session in that conversation's original working directory and resumes it, so the prior transcript is there and you can keep going. It always uses the recorded directory and never creates a worktree, because the conversation only resolves where it started. The original is read in place, not copied.

The list hides conversations not worth importing: AoE's own Claude sessions, scratch sessions, and anything inside an AoE worktree directory. Sessions whose directory no longer exists are hidden until you tick "show missing directories", and then shown disabled.
