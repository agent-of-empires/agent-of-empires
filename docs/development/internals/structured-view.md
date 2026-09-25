# Structured View Internals

Contributor reference for the structured view (ACP) subsystem. Users want the [Structured View overview](../../structured-view.md) instead.

aoe is the ACP *client*; each agent (Claude Code, Gemini, `aoe-agent`, ...) is the *server*. The daemon (`aoe serve`) supervises one detached worker per session and brokers the protocol between that worker and the web and TUI clients.

## Worker lifecycle

Workers are detached `aoe __acp-runner` processes that outlive the daemon. `aoe serve --stop` drops the connection without terminating them, and a later daemon reattaches over the control socket, so in-flight turns survive a daemon stop, crash, suspend, or build-only update. `aoe acp stop|kill <session>` terminates one deliberately.

Each runner registers at `<app_dir>/acp-workers/<session_id>.json` (pid, socket path, cached ACP session id, `build_version`, `runner_version`) beside its `.control.sock` and `.log`; `aoe ps --acp --dead` lists them. The runner terminates the ACP protocol: it owns the handshake, the turn, and every JSON-RPC id on the agent's stdin, while the daemon speaks a length-framed control protocol over the control socket. Notifications and turn completion share that one ordered channel, so a turn's terminal event cannot overtake the chunks before it. That wire order is not an application-event completion barrier: asynchronous SDK notification dispatch can lag terminal or handshake bookkeeping.

Invariants worth knowing before touching this code:

- **One owner per generation.** Every spawn, attach, and respawn is admitted under a lease for a fresh epoch stamped on the registry record. Shutdown, the reaper, the drain task, and the reconciler all act through the current lease and refuse a stale one, so a spawn completing after a stop tears its own runner down instead of installing it. A stop is not finished until the runner is proven dead (SIGTERM, then SIGKILL): the session stays `stopping`, holding prompts and refusing resumes, while the reconciler retries.
- **`runner_version` is an attachment-compatibility generation**, separate from liveness and `build_version`. Generation 4 requires the runner to announce authoritative native identity before session callbacks; the control wire version stays 3. An incompatible live runner is reaped before replacement, and an in-flight turn gets an explicit `Stopped { reason: "runner_protocol_upgraded" }`, since the daemon cannot truthfully drain a protocol it cannot attach to. Same-generation build changes stay attachable: idle workers are replaced immediately and busy ones respawn at the next idle boundary (`aoe ps --acp` marks them `(stale)`).
- **Process-group termination.** Runners are group leaders via `setsid`, and every termination path signals the group, so the node wrapper and SDK child die with the runner rather than reparenting to PID 1. Prefer the verbs over `kill -9`, which leaves the runner no chance to clean up.
- **Self-termination watchdog.** A runner polls its own registry record and exits when abandoned: record gone, superseded, or detached from any daemon past a 48h retention window (reset on reattach). This is the backstop for a daemon that dies without killing its runners.
- **Detach buffer.** Only agent notifications and `PromptCompleted` survive a detach, in a queue bounded by frame count and encoded bytes that sheds the oldest first. Delivery is best effort, not durable or exactly once. Reverse calls outstanding at detach are cancelled toward the agent and queued replies are dropped; within the admitted native session `terminal/release` is idempotent cleanup, so it needs no tombstone.
- **Native-session admission.** Only lifecycle results select native identity; notification and callback metadata never do. Unrelated updates are ignored and unrelated session callbacks are rejected before any transcript, permission, filesystem, or terminal effect. While identity is pending, retained notifications are bounded by the runner's detach-queue ceiling, so a reattach admits the whole flushed backlog and an overflow fails the connection rather than publishing a partial replay. Stored history is never rewritten by the guard.
- **Callback parity and replay accounting.** Session-scoped filesystem, terminal, and permission callbacks wait for that announcement with either named or positional parameters, while request-scoped elicitation stays independent of session authority. Pending replay charges each original control frame, its four-byte prefix included, against the runner's 4096-frame / 128 MiB budget; typed re-serialization can expand native JSON and is not a substitute.
- **Mid-turn reattach.** `ResumeSession` adopts the runner's committed session identity and never sends ACP `session/new` or `session/load`. A new local prompt supersedes adoption, and only the matching canonical request id may resolve it; a 30-second resume-idle watchdog is the fallback.
- **Background sub-agents across a restart.** The async-Task tailer (see [Stuck-turn watchdogs](#stuck-turn-watchdogs)) is per connection, so a `BackgroundAgentLaunched` can be left with no completion when its worker dies. The next owner reconciles that: `attach` re-tails every unresolved launch that still carries a transcript path, since the runner it adopted is provably alive; `spawn`, the drain task's respawn, and every terminal drain arm detach unconditionally. `output_file` is persisted for that resume and stripped from every client-facing frame.

## Session deletion

`session/delete` fires only on permanent removal (a purge, or disabling the structured view, which discards the conversation). Reversible teardown (`aoe acp stop`, snooze, archive, trash, idle auto-stop) deliberately does not fire it, so the transcript stays on disk and the next respawn resumes through `session/load`.

Trash is the reversible middle state: a delete moves the session there by default (`session.delete_to_trash`), keeping its transcript, worktree, branch, and container until it is restored, purged, or auto-purged after `session.trash_retention_days`. Retention is enforced by the daemon (a startup sweep plus an hourly tick), so without one, expired trash waits for the next daemon start or a manual purge. When the daemon permanently deletes a structured session it sends a best-effort `session/delete` (2s timeout) so adapters that implement it can release their own state, then proceeds with cancel, SIGTERM, and on-disk cleanup. CLI purges have no running worker, so they delete the local transcript only.

## Who owns the state

The daemon folds the event stream once per WebSocket connection into two projections, so clients do not re-derive them:

- **Control state**: turn flags, pending approvals and elicitations, usage, plan, modes, slash commands, shipped as `reduced_state` on connect and after every event. `unchanged` names the cold fields the socket already holds, which the daemon omits rather than re-serializing. The connect frame is folded over the whole session even with a `since` cursor, because clients adopt it verbatim.
- **Transcript rows**: a `transcript_snapshot` on connect plus a `transcript_delta` (`Append` / `Patch` / `Remove`, reconciled by row id) per event, with `GET /api/sessions/{id}/acp/replay?view=rows` for history. Presentation (markdown, tool cards, diffs) stays client-side.

Raw event frames still stream for what the daemon does not model (worker lifecycle latches, monitor and wakeup badges, the usage cost baseline, rejected prompts). A client reading only the projections passes `?frames=0`, as the native view does. A `notice` row carries a failed startup, a dead turn, a refused mode switch, or an auto-resume; the native view renders it inline while the web shows the same information as a dismissible banner.

The daemon also owns prompt dispatch: `POST /acp/prompt` runs `acp::dispatch::decide` and returns `sent`, `steered`, or `queued`, and the server-owned queue persists follow-ups until its turn-end drain. Cancelling and compacting turns always queue; a dormant worker is sent the prompt so the request wakes it.

## Persistence and the context primer

Transcripts persist in a SQLite event log. The web client mirrors each session's reduced state into `localStorage` (7-day expiry, full replay past the origin quota) so a reload hydrates instantly and fetches only the seq delta.

A cold open loads recent-first rather than folding from seq 0 before first paint. `GET /api/sessions/{id}/acp/replay?before=<seq>&limit=N` returns the events closest below `before`, aligned to a user-turn boundary so a page never seams a split turn. The client renders the tail immediately and, on a long session, also pulls a small `since=0` prefix for the pinned handshake snapshot (capabilities, slash palette, agent and model) the composer needs. Scrolling to the top pages older history, prepending with the scroll position frozen; each page is fetched twice, once for frames and once for `view=rows`, and a failure on either leg abandons the page rather than advancing the cursor.

If context restoration fails (the agent's stored session is gone) the view falls back to a fresh session, renders a "Conversation context reset" callout, and offers `GET /api/sessions/{id}/acp/context-primer?before_seq=<reset-seq>`: a compact markdown recap of the last ~20 turns (capped ~24k chars, bulky tool I/O elided), pre-filled into the composer and never auto-sent.

`aoe-agent` ships as sources inside `aoe`, is installed into the data dir on demand (a digest over its lockfile and sources decides whether the installed copy is current), and persists each native conversation in `${AOE_ARTIFACT_DIR}/aoe-agent-<native-session-id>.jsonl`. Only completed user/assistant text is persisted, not tool calls. A load of a missing or unreadable transcript fails explicitly rather than reporting an empty successful resume, which the view surfaces as a context reset; without an artifact directory, sessions are ephemeral and load is unavailable.

## Permission modes and model channels

Modes come from `NewSessionResponse.modes`, and the picker shows whatever the adapter reports; Gemini's `auto_edit` / `yolo` fold onto `acceptEdits` / `bypassPermissions`. YOLO fires `session/set_mode("bypassPermissions")` after `session/new`, best-effort. `claude-agent-acp` gates that mode on `ALLOW_BYPASS=1` in the daemon's environment; without it `set_mode` returns "not available" and the session stays `default`, surfaced as an amber notice.

Model and reasoning-effort selectors arrive over two wire mechanisms, normalized into one dropdown: `SessionUpdate::ConfigOptionUpdate` (a full snapshot of every selector whenever one changes, so the client replaces its cached list) and the `unstable_session_model` capability (`SessionModelState` on `session/new` and `session/load`, switched with `session/set_model`). With both present, `config_option` wins, since it has a push path; `session/set_model` only acks, so the client synthesizes the confirming update. The UI is pessimistic (the chip keeps the prior value until a confirming update arrives) to avoid snap-back on slow tunnels. The cached list clears on `AgentSwitched` but survives `/clear`, since capabilities are process-scoped.

Approval nonces are server-generated and single-use, and are never revealed to the agent. Resolving an already-resolved approval clears the card quietly.

## Stuck-turn watchdogs

Three layers recover a turn that stops progressing:

1. **Cancel escalation.** The agent ignores `session/cancel` mid-tool. After a ~10s grace the daemon ends the connection, SIGTERMs the runner, and respawns via `session/load`. Banner reason `agent_unresponsive`.
2. **Force end turn (client).** No streaming chunk for 30s with no tool in flight surfaces a button that publishes a synthetic `Stopped` plus a best-effort cancel. With a tool in flight, or during a latched compaction phase, it stays hidden so it cannot discard real progress.
3. **Silent-orphan watchdog (daemon).** The adapter finished streaming but never sent the `PromptResponse` that closes `session/prompt`. It fires only when no tool call is in flight, at least one progress notification has arrived, and none has arrived for `silent_orphan_grace_secs` (120, dropping to a fixed 20s once a cost-populated `UsageUpdate` lands). A turn that already emitted its cost-populated usage with no off-protocol work pending ends cleanly; otherwise the daemon cancels, waits 10s, SIGTERMs, and respawns.

**Off-protocol work** suppresses the watchdog, because some agent features go quiet with no ACP signal. An async `Agent` tool is tracked precisely (a tailer follows the sub-agent's transcript and an in-flight set keeps the watchdog from firing). `/compact` is detected from the adapter's text markers, since it emits no typed signal, and the start marker latches a 30-minute grace floor so a large compaction is never cut short; the same markers publish `ConversationCompactionStarted` / `ConversationCompacted` so both clients know the phase. A backgrounded `Bash` latches the floor until a cost-populated usage update arrives, and a `ScheduleWakeup` suppresses recovery until `wakeup_at` plus the floor, on a monotonic deadline that multiple wakeups extend. An agent-initiated turn that streamed output but reported no cost-bearing end and scheduled no wake is a stalled stream rather than a parked monitor, and recovers on its own 120s grace.

## Rate-limit handling

A backend reporting `errorKind: "rate_limit"` is a clean terminal state, not a crash: the daemon emits `RateLimit` plus `Stopped { reason: "rate_limited" }`, drops the worker handle, and does not respawn into the same limit. The park is durable (the latest `RateLimit` with no prompt, agent switch, or unrelated stop after it) and survives a daemon restart, so a failed resume neither clears the banner nor disarms auto-resume. `RateLimitInfo.resets_at` is filled only from a reset the agent attributed to the window it rejected, and stays `None` otherwise, so no fabricated time reaches the UI.

With `[acp] rate_limit_auto_resume`, the reconciler resumes the same worker once `resets_at` plus a 15s grace passes, or, with no reported reset, an hour after the park with the wait doubling per redelivery (1h, 2h, 4h, 8h, 16h). Each resume re-delivers the interrupted prompt once, capped at 5 per streak, after which the session parks on `Stopped { reason: "rate_limit_exhausted_retries" }`. The streak is counted from persisted `RateLimitAutoResumed` breadcrumbs since the last organic turn end or agent switch, kept in a per-session row outside the pruned transcript. Nothing un-parks a session on a timer otherwise: dispatch treats the park like idle dormancy, which is what makes "send a new prompt" true.

## Crash-loop park

A worker that exits within ~10s (broken command, missing adapter, failed handshake) logs a `warn` on the `acp.runner` target and counts against a respawn budget: more than 5 spawns in a rolling 60s parks the session, publishes one `AgentStartupError`, and stops auto-respawning. Recovery is a dashboard retry, `aoe acp restart <session>`, or an `aoe serve` restart.

## Agent switching

`POST /api/sessions/{id}/acp/switch-agent` stops the current worker, spawns the target, persists `agent_name`, clears `acp_session_id` (the old id belongs to a different vendor), resolves the model from the request alone so the old backend's pick does not follow the session across vendors (as `Instance::swap_tool` does), and emits `AgentSwitched { from, to, reason }` so reducers drop backend-specific transient state and the transcript shows a divider. The composer is pre-filled with a context-primer recap, never auto-sent. CLI: `aoe acp switch-agent <session> <target> [--model <name>]`. `reason` is `manual` or `rate_limited`.

## Agent profiles

Each agent has two profile sources, kept aligned by registry key:

- **Server**, `src/acp/agent_profiles.rs`: `parent_meta_namespaces`, `clear_aliases`, and the `supports_exit_plan_mode` / `supports_wakeup_tools` gates.
- **Frontend**, `web/src/lib/agentProfiles.ts`: the card-classifier alias map, claude-specialized capabilities (`todos`, `skills`, `wakeup`), the MCP prefix list, and special-title patterns matched only when the capability is on.

Profiles are conservative: an unverified tool surface is omitted rather than guessed, so the generic tool card is the fallback. Mode-picker sources resolve in order: a `category:"mode"` config option, then the ACP `SessionModeState` channel, then, for claude-family agents only, the built-in taxonomy. Subagent indentation needs the adapter to emit `_meta.<namespace>.parentToolUseId`. An off-protocol subagent that streams no children (opencode's `task`) is classified by `ToolCall.raw_name` through the profile's `subagentToolNames`, not by the mutable title an update overwrites. To diagnose a tool rendering as a generic card, compare the tool-start frame's `tool.kind` / `tool.name` against the profile; the alias map only fires when `kind` is `"other"`.

## Security model

- `fs/read_text_file` / `fs/write_text_file`: agents never touch the disk directly. aoe reads and writes on their behalf and enforces the sandbox roots (the session's worktree plus any explicit `--repo` paths).
- `terminal/*`: commands run in aoe's process, in the worktree, or inside the sandbox container via `docker exec`.
- Approval nonces are server-generated and single-use, so a compromised agent cannot synthesize one. `AOE_TOKEN` is not forwarded to the agent subprocess.
- **Sandboxed sessions** wrap the agent argv in `docker exec` while the daemon stays on the host, and `fs/*` paths are translated from container to host before the inside-roots check. Translation covers the workspace mounts only, so config, credential, and `extra_volumes` mounts are rejected. The image must bundle the ACP adapters, or the handshake exits with status 127.

## Global tuning (`[acp]`)

```toml
[acp]
default_agent = "claude-code"
approval_timeout_secs = 300
destructive_require_double_confirm = true
max_concurrent_workers = 100
replay_events = 0                 # 0 = unlimited; caps per-session rows and the client buffer
node_path = ""
show_tool_durations = true
compaction_reminder = false       # opt-in /compact nudge past the threshold
compaction_reminder_percent = 75  # 1..99
silent_orphan_grace_secs = 120    # 0 disables
auto_stop_idle_secs = 3600        # 0 disables; the next prompt respawns the worker
rate_limit_auto_resume = false
```

Cold-start resume parallelism is a fixed 4 spawns, clamped to `max_concurrent_workers`, to bound Node bootup memory. `auto_stop_idle_secs` stops an event-idle worker with no in-flight turn (the session keeps its sidebar slot and shows `Stopped { reason: "idle_auto_stop" }`); mid-turn workers are never stopped and the check runs about once a minute. `AOE_ACP_NODE=/path/to/node` overrides Node discovery for one process.
