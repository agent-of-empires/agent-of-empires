# Structured View Internals

Contributor reference for how the structured view (ACP) subsystem works. Users want the [Structured View overview](../../structured-view.md) and its guides instead.

aoe is the ACP *client*; each agent (Claude Code, Gemini, `aoe-agent`, etc.) is the *server*. The daemon (`aoe serve`) supervises one detached worker per session and brokers the protocol between the worker and the web/TUI clients.

## Worker lifecycle and persistence

Workers run as detached `aoe __acp-runner` processes that outlive the daemon. `aoe serve --stop` drops the daemon's connection to each worker but does not terminate the runner; the agent keeps running and a later `aoe serve` reattaches over the worker's unix socket. In-flight turns survive `aoe serve --stop`, `aoe update`, daemon crashes, and host suspend/wake. To actually terminate a worker use `aoe acp stop|kill <session>`.

Each runner registers at `<app_dir>/acp-workers/<session_id>.json` (PID, socket path, cached ACP session id, `build_version`); the same dir holds the per-session `.sock` and `.log` (runner stderr drain). `aoe acp ps` lists them.

- **Process-group termination.** Runners are group leaders via `setsid`; every termination path signals the whole group so the node ACP wrapper and the SDK child die with the runner instead of reparenting to PID 1. A `SIGKILL`'d runner can still leak (it cannot run cleanup), so prefer the verbs over `kill -9`.
- **Self-termination watchdog.** The reapers above need a live daemon, so each runner also polls its own registry record and self-destructs when abandoned: record vanished, superseded by a newer runner, or detached with no daemon for longer than a 48h retention window (reset on every reattach; a pending `aoe acp restart` is exempt). Backstop for a daemon that dies without killing its runners (#1921).
- **Detach buffer.** Between `--stop` and the next `aoe serve`, the runner buffers up to 256 agent→daemon notification lines for replay on reattach. Permission requests issued while detached block the turn until reattach.
- **Mid-turn reattach.** The new daemon resumes the existing ACP session id directly (no `session/new`/`session/load`; the process never died). The orphaned in-flight `session/prompt` response is dropped by the transport (its request id belonged to the previous daemon), so a resume-idle watchdog arms and disarms on the first inbound notification; if none arrives within 30s it synthesizes `Stopped { reason: "reattach_idle" }`. Sessions the runner cannot reattach to fall through to a fresh spawn, which emits `Stopped { reason: "orphaned_at_restart" }` when the on-disk event log shows the session was mid-prompt at the crash.

## Build-version upgrade after `aoe update`

Survival across restart means a daemon on a new binary could re-adopt workers running the old build. To avoid silent mixed-version state, each runner records the spawning binary's `build_version` (package version paired with a git commit hash, so local rebuilds are detected) and the daemon compares on every reattach:

- Same build: reattached as usual.
- Older build, no in-flight turn: terminated and respawned on the new binary immediately.
- Older build, mid-turn: adopted so the turn keeps streaming, respawned at the next idle boundary (never hard-killed).

`aoe acp ps` tags a not-yet-respawned worker `(stale)`. The new binary takes effect only once the daemon restarts; `aoe update` offers that restart, and `aoe serve --restart` replays the host/port/mode/auth/passphrase it was launched with. Restart only touches daemons started by `aoe serve --daemon`; foreground/systemd/launchd daemons are left to their manager. See #1754, #1794.

## Session deletion semantics

`session/delete` fires only on permanent removal (purging a session, or disabling the structured view, which discards the conversation). Reversible teardown (`aoe acp stop`, snooze, archive, trash, idle auto-stop) deliberately does not fire it, so the transcript stays on disk and the next respawn resumes via `session/load` instead of resetting context (#1710). Trash is the reversible middle state between archive and permanent delete (#2489): deleting a session moves it to the trash by default (`session.delete_to_trash`), where it keeps its transcript, worktree, branch, and container until it is restored, purged, or auto-purged after `session.trash_retention_days` (0 = keep forever). Trash uses the same `shutdown` (not `shutdown_and_delete`) path as archive; purge is the historical permanent-delete path. Retention auto-purge is enforced by the `aoe serve` daemon only (a startup sweep plus an hourly tick); without a running daemon, expired trash is purged on the next daemon start or by an explicit manual purge (`aoe rm --purge`, `aoe session empty-trash`, or the web "Delete permanently" action). On permanent delete aoe fires a best-effort ACP `session/delete` (2s timeout) when a stored session id exists, then proceeds with the kill path (`session/cancel`, SIGTERM, on-disk cleanup). Adapters that implement it release adapter-side state (e.g. claude-agent-acp 0.37.0+ clears its on-disk session record); others reply `-32601 method_not_found`, logged at debug under `target = "acp.protocol"` with an `adapter=` field (#1404).

## Conversation persistence and context primer

Transcripts persist in a SQLite event log. The web client mirrors each session's reduced state into `localStorage` under `aoe:acp-state:v1:<session_id>` (7-day expiry, falls back to full server replay past the per-origin quota) so a reload hydrates instantly and only fetches the seq-delta. `clearAcpCache` and the delete handler drop the entry so a recreated session id never shows the prior transcript.

On a cold open (cache miss) the client loads recent-first instead of folding the whole transcript from seq 0 before first paint (#2236). The replay endpoint takes a `before` cursor (`GET /api/sessions/{id}/acp/replay?before=<seq>&limit=N`): it returns the `limit` events sitting closest below `before` in ascending order, aligned to a user-turn boundary when more history remains so a page never seams a split turn. The client fetches the tail (`before` = max), renders it immediately, and on a long session also pulls a small `since=0` prefix to project the pinned handshake snapshot (capabilities, slash palette, agent/model) without which the composer would be crippled. Scrolling to the top (or the "Load earlier messages" button) first reveals rows already in the reducer, then pages older events via `before` (lowest loaded seq), prepending them with the scroll position frozen. The render window grows as rows arrive so live turns never fold earlier messages back behind the control. The forward `since`/`limit` contract (WS catch-up, cached-reload seq-delta) is unchanged.

If context restoration fails (the agent's stored session is gone), the view falls back to a fresh session, renders a "Conversation context reset" callout, and offers a one-shot "Resume with prior context" banner. That calls `GET /api/sessions/{id}/acp/context-primer?before_seq=<reset-seq>`, which walks the event log and returns a compact markdown recap of the last ~20 turns (capped ~24k chars, bulky tool I/O elided). The primer is pre-filled into the composer and never auto-sent. `aoe-agent` restores context across a restart (#1005): it advertises `loadSession` and appends each completed text exchange to `${AOE_ARTIFACT_DIR}/transcript.jsonl`, then reseeds the model's message history on `session/load`. Tool calls are not carried across turns (they never were in its in-memory history), so a resumed model sees prior user/assistant text only.

## Permission modes and model channels

Modes come from `NewSessionResponse.modes`; the picker shows whatever the adapter reports. Gemini's `auto_edit`/`yolo` `ApprovalMode` names fold onto `acceptEdits`/`bypassPermissions`. YOLO (`[session] yolo_mode_default`) fires `session/set_mode("bypassPermissions")` after `session/new`, best-effort. claude-agent-acp gates `bypassPermissions` on the `ALLOW_BYPASS=1` daemon env var; without it `set_mode` returns "not available" and the session stays `default` (surfaced as a non-blocking amber notice).

Model and reasoning-effort selectors arrive over two wire mechanisms, normalized into one dropdown:

- **`SessionUpdate::ConfigOptionUpdate`** (stabilized in claude-agent-acp v0.37.0): the adapter emits a full snapshot of every selector whenever any one changes; the client replaces its cached list wholesale.
- **`unstable_session_model`** capability: `SessionModelState` on the `session/new`/`session/load` response, switched with `session/set_model`.

If both are present, `config_option` wins (it has a push/echo path). Because `session/set_model` only acks, the client synthesizes the confirming update. The UI is pessimistic (chip shows the prior value until the adapter pushes a confirming `config_option_update`) to avoid snap-back on slow tunnels. The `Default` effort drops any session-level effort pin (resolved per model upstream). The cached selector list clears on `AgentSwitched` but survives `/clear` (capabilities are process-scoped).

Approval nonces are server-generated and single-use; aoe never reveals them to the agent. Resolving an already-resolved approval (concurrent decision, watchdog) clears the card quietly rather than erroring.

## Stuck-turn watchdogs

Three layers recover a turn that stops progressing, in increasing depth:

1. **Cancel escalation.** The agent ignores `session/cancel` mid-tool (commonly a `block: true` TaskOutput on a wedged shell). After a ~10s grace the daemon ends the ACP connection, SIGTERMs the runner, and respawns via `session/load`. Banner reason `agent_unresponsive`.
2. **Force end turn (client).** No streaming chunk for a fixed 30s threshold with no tool in flight surfaces a "Force end turn" button that publishes a synthetic `Stopped` plus a best-effort `session/cancel`. With a tool in flight the spinner shows an elapsed label instead and the button stays hidden so it cannot discard in-flight progress (#1100, #1176).
3. **Silent-orphan watchdog (daemon).** The adapter finished streaming but never sent the `PromptResponse` that closes `session/prompt` (upstream claude-agent-acp#688). Fires only when all hold for the current prompt: `tool_calls_in_flight` is empty, at least one progress notification has arrived, and none has arrived for `silent_orphan_grace_secs` (120; reduced to a fixed 20s fast grace once a cost-populated `UsageUpdate` lands). Out-of-band notifications (mode/command/usage-without-cost) do not reset the timer. On fire: `session/cancel`, 10s grace, SIGTERM, respawn via `session/load` (#1240). Nonzero grace below 120 is clamped up; debug builds honor `AOE_SILENT_ORPHAN_GRACE_MS` and `AOE_ACP_SIMULATE_ORPHAN_NEXT_PROMPT=1` (single-shot, compiled out of release).

**Off-protocol work suppression (#1360, #1401).** Some Claude SDK features go quiet with no ACP signal; the watchdog lifts the grace to `OFF_PROTOCOL_WORK_GRACE_FLOOR` (30 min) for the rest of the prompt:

- `Agent` tool `isAsync: true` (#1360): a per-connection tailer follows the sub-agent's transcript and the launch is tracked in an in-flight set (keyed by agent_id, removed when the tailer reports terminal). The between-prompt watchdog treats a non-empty set as work in flight, so it cannot fire while an async agent runs. Because tracking is now precise, an `isAsync` launch does NOT latch the 30-min floor (that stays only for untracked backgrounded Bash). See #2573.
- `Bash` `run_in_background: true` (#1401): detected from `raw_input.run_in_background` at start AND the `Command running in background with ID:` completion text (defense in depth). Fire-and-forget, so once a cost-populated `UsageUpdate` arrives the suppression is dropped and recovery falls back to the fast grace (#1858). One exception to the floor (#2645): if the model then streams a partial message and the stream dies mid-chunk (the last timer refresh was a `Progress`, not a `BashOutput` poll), the watchdog treats it as a dead stream and recovers on the normal per-prompt base grace (~2 min) rather than the 30-min floor. A bash still being polled refreshes the timer via tool activity, so a genuinely-running background command keeps the floor.

**Between-prompt stalled stream (#2573).** An agent-initiated turn (a monitor / `/loop` resume with no aoe `session/prompt`) that streamed output but reported no cost-bearing end-of-turn marker and scheduled no wake is a stalled stream, not a parked monitor (which always carries a `wake_at`) and not backgrounded Bash (which latches the floor). It recovers on a dedicated `BETWEEN_PROMPT_STALL_GRACE` (120s) instead of the 30-min floor. Separately, `has_in_flight_turn` (the build-stale respawn probe) counts a launched/progressing background agent with no terminal as in-flight so a respawn cannot interrupt it mid-work and drop its transcript, bounded by `BACKGROUND_AGENT_STALE_AFTER_MS` so a crashed tailer cannot pin the probe forever. The pre-0.55 stdin `\n` keepalive (#2455) is gone: the `claude-agent-acp` floor is now 0.55.0, which ships the upstream fix (claude-agent-acp#825/#835) for turns that went idle without a `PromptResponse`.

**Scheduled-wakeup suppression (#1401).** A `ScheduleWakeup` with `delaySeconds: N` is deliberate off-protocol idling (a monitor or `/loop` run), so it is treated like the off-protocol kinds above: the watchdog is suppressed until `wakeup_at + OFF_PROTOCOL_WORK_GRACE_FLOOR` (30 min), and for the rest of the prompt the effective grace stays at that floor rather than dropping to the 20s fast grace even after a cost-populated `UsageUpdate` lands. The deadline is a monotonic `Instant` so wall-clock jumps don't perturb it. Multiple wakeups extend, never shorten. A daemon crash during sleep tears the prompt loop down, so the next attach starts fresh. Earlier this suppressed only until `wakeup_at + silent_orphan_grace_secs` and then re-armed with the fast grace, which killed monitor turns ~20s after the wake window lapsed.

## Rate-limit handling

When the backend reports `errorKind: "rate_limit"` on `session/prompt`, aoe treats it as a clean terminal state, not a crash: it emits a typed `RateLimit` event (banner reads its reset time) plus `Stopped { reason: "rate_limited" }`, drops the worker handle, and does not respawn. Earlier behavior respawned into the same limit and burned the restart budget. A daemon restart respects the parked signal in the event log. Optional opt-in `[acp] rate_limit_auto_resume` has the reconciler resume the same worker once `resets_at` plus a fixed 15s grace passes; it is vendor-agnostic and bounded by a minimum park window so a misbehaving adapter cannot drive a respawn loop. The banner's "Continue in another agent" CTA runs the agent-switch path below.

## Crash-loop park

A worker that exits within ~10s (broken command, missing adapter, handshake failure) used to respawn every reconciler tick silently. Now the runner logs a `warn` on the `acp.runner` target (session id, exit status, `elapsed_ms`), and the reconciler enforces a respawn budget: more than 5 (re)spawns in a rolling 60s parks the session, publishes one `AgentStartupError` (red startup banner), and stops auto-respawning. This is looser than the supervisor's in-flight restart budget (3 in 60s). Recovery: dashboard retry, `aoe acp restart <session>`, or an `aoe serve` restart (clears the in-memory budget for one more bounded burst). Empty 0-byte worker logs are swept on teardown.

## Agent switching

`POST /api/sessions/{id}/acp/switch-agent` stops the current worker, spawns the target, persists `agent_name` and clears `acp_session_id` (the old id belongs to a different vendor), and emits `AgentSwitched { from, to, reason }` so reducers drop backend-specific transient state (rate-limit banner, in-flight tool, usage, mode pills, commands) and the transcript shows a divider. The modal then pre-fills the composer with a context-primer recap (and the `unprocessed_prompt` if the user's last prompt triggered the limit); never auto-sent. CLI: `aoe acp switch-agent <session> <target> [--model <name>]`. `reason` is `manual` or `rate_limited`.

## Agent profiles

Each agent has two profile sources, kept aligned by registry key:

- **Server (Rust), `src/acp/agent_profiles.rs`:** `parent_meta_namespaces`, `clear_aliases`, and the `supports_exit_plan_mode` / `supports_wakeup_tools` capability gates.
- **Frontend (TS), `web/src/lib/agentProfiles.ts`:** the card-classifier alias map (`shell` → execute card, etc.), claude-specialized capabilities (`todos`, `skills`, `wakeup`), the MCP prefix list, and special-title patterns matched only when the capability is on.

Profiles are conservative: an unverified tool surface is omitted rather than guessed, so the generic tool card is the fallback. Mode-picker sources resolve in order: a `category:"mode"` config option (OpenCode, claude-agent-acp v0.37.0+), then the ACP `SessionModeState` `available_modes` channel (older claude), then, for claude-family agents only, the built-in Default/Plan/Accept-edits/Yolo taxonomy. Subagent indentation needs the adapter to emit `_meta.<namespace>.parentToolUseId` (claude-agent-acp emits `_meta.claudeCode.parentToolUseId`). To diagnose a tool rendering as a generic card, read the tool-start WS frame's `tool.kind`/`tool.name` in devtools and compare against the profile; the alias map only fires when `kind` is `"other"`.

## Security model

- `fs/read_text_file` / `fs/write_text_file`: agents never touch the disk directly; aoe reads and writes on their behalf and enforces sandbox roots (the session's worktree plus any explicit `--repo` paths).
- `terminal/*`: the command runs in aoe's process, in the worktree, or inside the sandbox container via `docker exec`.
- Approval nonces are server-generated and single-use; a compromised agent cannot synthesize one. `AOE_TOKEN` is not forwarded to the agent subprocess.
- **Sandboxed sessions** wrap the agent argv in `docker exec`; the daemon stays on the host. `fs/*` requests are translated from container paths to host paths before the inside-roots check; the unix socket stays on the host and the runner proxies the agent's stdio across the boundary (no socket bind-mount; reserved for a future socket-native agent). Path translation only covers the workspace mount(s); config/credential/`extra_volumes` mounts are not in the path map but are rejected by the worktree-only inside-roots check anyway. The `aoe-sandbox` image must bundle the ACP adapters or the `docker exec` handshake times out after 30s (exit 127); see [Sandbox Internals](sandbox.md).

## Global tuning (`[acp]`)

```toml
[acp]
default_agent = "aoe-agent"
approval_timeout_secs = 300
destructive_require_double_confirm = true
max_concurrent_workers = 100
replay_events = 0                 # 0 = unlimited; caps per-session rows and the web client buffer (#1111)
node_path = ""
show_tool_durations = true
silent_orphan_grace_secs = 120    # 0 disables (#1240)
auto_stop_idle_secs = 3600        # 0 disables; next prompt respawns the worker
rate_limit_auto_resume = false
```

Cold-start resume parallelism is a fixed constant (4 parallel worker spawns/attaches, keeping Node bootup memory bounded on laptops/Pis; #1088), clamped at runtime to `min(4, max_concurrent_workers).max(1)`. `auto_stop_idle_secs` stops an event-idle worker with no in-flight turn (the session keeps its sidebar slot; the timeline shows `Stopped { reason: "idle_auto_stop" }`; the next prompt respawns and resumes); mid-turn workers are never stopped, and the check runs ~once a minute. `AOE_ACP_NODE=/path/to/node` overrides Node discovery for one process.

Config migrations: v005 seeded the old `[cockpit]` section, v006 flipped its `replay_events` to unlimited, v012 renamed the section to `[acp]` (dropping the retired master switch and `default_for_claude` keys) and migrated per-session state, and v022 dropped the retired tuning knobs (`replay_bytes`, `max_concurrent_resumes`, `force_end_turn_threshold_secs`, `silent_orphan_fast_grace_secs`, `rate_limit_auto_resume_grace_secs`, `queue_drain_mode`), which are fixed constants now.
