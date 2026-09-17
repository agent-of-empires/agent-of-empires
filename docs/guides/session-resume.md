# Native Session Resume

AoE resumes a terminal conversation only when its native agent, physical store, working directory, and filesystem agree with the prepared launch. A status label or raw session ID does not establish that identity. Existing transcripts are never deleted to repair a mismatch.

Runtime conversation changes such as `/clear`, `/new`, fork, continue, or a fresh pane generation rotate the recorded identity when the upstream agent publishes the change. The old identity and any artifact predating the launch boundary cannot be recaptured after an AoE process restart.

## Automatic capture matrix

| Agent | Host terminal | Sandboxed terminal | Existing publication source |
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
| Prime Agent | No | Yes | Root-only publication and isolated managed store |

`No` means automatic identity discovery is unsupported in that environment. OpenCode host capture additionally requires `session.opencode_preassign_session_id = true`. These are publication capabilities, not an authorization to resume every discovered ID. A publication must also carry a verified source in a supported managed context below. Old panes without that source remain unknown; AoE does not add shared-store scans or infer provenance from recency.

Prime Agent captures depth-zero roots, not the child sessions spawned by its recursive language model (RLM) runtime. If a root publishes a new conversation whose transcript is confirmed absent, automatic restart starts an empty conversation instead of resuming the previous history. This boundary survives launch attempts, but the unwritten native ID is not preserved. Once the transcript exists and its root header is validated, normal resume uses that ID. An explicitly pinned ID remains authoritative.

The Prime integration is verified with 0.9.1 (the oldest tested version) and 0.9.4. It requires `-e <extension>` and numeric `rlmDepth: 0` in native root headers and the extension API's `getHeader()` result. Missing or nonnumeric depth is not treated as a root. Older versions are unverified; the sandbox installer follows the upstream stable channel, so these checks do not guarantee every future image build.

Prime automatic capture requires a session directory mapped into its writable, isolated managed store. Settings files must be readable, bounded regular files; symlinked settings are deliberately refused because their container-visible target cannot be inferred safely from the host path. An unresolved configuration does not trigger a scan of the default directory. Poller repair logs the refusal under `session.capture` and retries after 30 seconds. Use regular settings files, or an explicit `--session-dir` within `/root/.prime/agent` to select a verified directory without consulting settings.

Sandbox config and conversation stores are staged under a separate directory for each AoE instance, including custom `agent_config_dir` roots. A cross-process lease guards each managed store. Two sessions in the same working directory therefore cannot claim each other's conversation.

## Execution identity and wrappers

`agent_detect_as` controls status detection and ACP adapter inheritance, not terminal execution identity. A direct built-in command identifies its native agent independently. A conflicting built-in tool and command, such as tool `claude` with command `codex`, is rejected for managed resume and fork.

An opaque wrapper requires both `session.agent_execution_as` and `session.agent_config_dir` in trusted global or profile configuration. The declaration asserts that the wrapper invokes that native agent, forwards its native arguments unchanged, and uses only the declared store and working-directory/filesystem context. AoE cannot attest arbitrary wrapper internals. See the [two-account example](configuration.md#one-cli-two-accounts). Shell pipelines, remote launchers, redirections, expansion, and unrecognized context-changing arguments are not supported managed invocations.

Only existing capture capabilities are used. Declaring a wrapper does not enable capture for backends that require direct native invocation, or add a native fork capability. The absolute program and routing environment resolved by AoE are fixed from one launch snapshot and restored after the login shell. Native namespace arguments are pinned where supported. A later AoE configuration change requires a newly validated launch. A login-shell startup file that refuses to change a pinned routing variable, for example by marking it `readonly`, refuses the launch instead of dispatching the agent with the wrong store.

## Supported managed contexts

- **Claude:** the resolved `CLAUDE_CONFIG_DIR` or default Claude store. Remote/cloud, worktree, alternate settings sources, and conflicting selectors are refused.
- **Codex:** a local, host-readable `CODEX_HOME`, file-backed API-key authentication, and local SQLite/thread storage. AoE resolves the independent SQLite directory and pins it for dispatch. ChatGPT/cloud or keyring authentication, an unestablished login, selected profiles, project routing overrides, managed requirements/configuration, and host macOS execution are not currently proven and are refused. This is not general support for every Codex context.
- **OpenCode:** an explicit `OPENCODE_DB`, or the common database when `OPENCODE_DISABLE_CHANNEL_DB` is already enabled. AoE does not guess or change a compiled channel. A target routed to a workspace or restoring another working directory is refused.
- **Pi and OMP:** the verified transcript and its exact store directory. Recovery requires `--store` naming that transcript file. OMP also verifies its stored working directory and pins the resolved profile.
- **Gemini:** the resolved `.gemini` store. When a dotenv file could select another root, configure `GEMINI_CLI_HOME` explicitly. **Cursor**, **Kimi**, and **Copilot** use their native config/share/home store inputs, not a union of unrelated environment variables.
- **Prime:** the resolved agent root and session directory on the host, or the existing root-only managed-container layout, including a verified `--cwd <directory>`. Namespace options require separate values, not `--cwd=...` or `--session-dir=...`. A declared wrapper can resume an explicitly bound conversation without enabling capture. Host execution does not enable capture. Prime still cannot fork.
- **Hermes:** an explicitly declared configuration root and its `state.db`, with local terminal execution. The profile is pinned without following the sticky active profile. AoE resolves the conversation Hermes itself would resume, following native compression continuations to their tip, and refuses a stored lineage or stored working directory that contradicts the declared launch context. Dotenv credentials are allowed; dotenv routing overrides, managed configuration, container delegation and enabled external secret sources are refused. This does not enable host capture.
- **Vibe:** an explicitly declared configuration root with the default `logs/session` store and `session` prefix. Visible user, project, environment and selected-profile routing must agree with that layout; dotenv routing overrides are refused. This remains the end-to-end wrapper declaration above, not an independent attestation of Vibe's remote admin policy or arbitrary wrapper internals. AoE does not disable native policy, force a harness, scan for a different conversation or add capture.

Hermes and Vibe require readable, bounded regular configuration files for namespace validation; symlinked, oversized or unreadable configuration is refused.

Sandboxed managed operations additionally require a supported runtime endpoint, an immutable container execution identity, and the actual inspected filesystem/mount mapping. Docker and Podman dispatch to the inspected container ID, not its replaceable name. Apple Container execution identity is not currently proven for managed conversation access. An unavailable or unsupported projection is not treated as a local host path. These refusals leave the requested resume/fork and transcript intact; they do not silently launch fresh.

Disabling `agent_status_hooks` removes status writers only. Any authoritative identity hooks declared for native resume remain installed.

To branch a conversation into a new session instead of resuming it in place, see [Forking Sessions](./session-fork.md).

## Pinning or resetting a conversation

Pin a terminal session to a specific native conversation:

```sh
aoe session set-session-id <session-name-or-id> <native-session-id>
```

This records an assertion about the intended native target, separately from any observed conversation. The pin is sticky. If the execution context changes, restore it or explicitly rebind the intended conversation before retrying. Legacy IDs and IDs from old unqualified publishers remain unknown after migration; they are not relabeled from current configuration.

On automatic start or restart, an unknown stored conversation starts fresh with a warning. The previous transcript is left intact. Explicit resume pins and forks still require a qualified binding; use `set-session-id` with the original store to select that conversation again.

For an explicit store assertion:

```sh
aoe session set-session-id <session> <native-id> --store /absolute/native/store
```

For a terminal agent whose store comes from configuration, such as Claude, `--store` names the store directory the launch routes, so an explicit assertion restores a conversation that the session's own configuration would place elsewhere. The assertion wins over that configuration until it is changed.

For Pi and OMP, `--store` instead names the exact existing transcript file within the configured store. Its header must name the requested ID. A missing or incompatible store is an error, not an implicit fresh start.

Retry after fixing the underlying issue, set a different conversation ID, or explicitly start fresh once:

```sh
aoe session set-session-id <session-name-or-id> ""
```

This is one-shot. The next launch starts fresh, then automatic capture takes over again when the matrix supports that environment. The abandoned conversation stays excluded in its recorded agent, store, and filesystem namespace, not in unrelated stores that happen to reuse the same ID. Legacy exclusions without a known namespace remain ID-wide.

Structured-view conversations remain managed by ACP. `set-session-id` does not change their ACP ID. For a Claude terminal handoff, AoE records the native execution it resolves for the current ACP ID, so switching to terminal consumes that store; an explicit `--store` assertion names a different store instead. A handoff whose store cannot be resolved, or that the structured-view worker does not share, is refused before worker teardown, with recovery guidance. A refusal reports only that no shared store could be proven; the underlying resolution error is logged at debug level, not in the 409. Other structured resume-target changes are rejected.

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

Relevant fields:

- `agent_session_id` and `agent_session_binding`: the native ID and its source provenance, stored together. Preallocation is distinct from observation.
- `resume_intent` and `resume_binding`: the requested operation and its independently bound target. A fork retains its parent binding until validated dispatch.
- `active_execution`: the recorded publisher/runtime context for one launch. A retired publisher cannot be relabeled with a newer launch’s store.
- `resume_probe_failed_sid`: the ID whose ambiguous resume failure prevents automatic retry until user action.
- `pi_session_path`: the Pi transcript path, updated atomically with its ID and provenance.
  A missing transcript may permit a fresh automatic launch under the agent’s existing policy; an explicit managed resume/fork never becomes fresh. These fields are auto-managed. Do not repair provenance by editing JSON.
