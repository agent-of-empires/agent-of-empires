# Structured View Internals

Contributor reference for the structured view (ACP) subsystem. Users want the [Structured View overview](../../structured-view.md) instead.

aoe is the ACP *client*; each agent (Claude Code, Gemini, `aoe-agent`, ...) is the *server*. The daemon (`aoe serve`) supervises one detached worker per session and brokers the protocol between that worker and the web and TUI clients.

## Worker lifecycle

Workers are detached `aoe __acp-runner` processes that outlive the daemon. `aoe serve --stop` drops the connection without terminating them, and a later daemon reattaches over the control socket, so in-flight turns survive a daemon stop, crash, suspend, or build-only update. `aoe acp stop|kill <session>` terminates one deliberately.

Each runner registers at `<app_dir>/acp-workers/<session_id>.json` (pid, socket path, cached ACP session id, `build_version`, `runner_version`) beside its `.control.sock` and `.log`; `aoe ps --acp --dead` lists them. The runner terminates the ACP protocol: it owns the handshake, the turn, and every JSON-RPC id on the agent's stdin, while the daemon speaks a length-framed control protocol over the control socket. Notifications and turn completion share that one ordered channel, so a turn's terminal event cannot overtake the chunks before it. That wire order is not an application-event completion barrier: asynchronous SDK notification dispatch can lag terminal or handshake bookkeeping.

Invariants worth knowing before touching this code:

- **One owner per generation.** Every spawn, attach and respawn has a local lease epoch tied to its captured runner generation. Shutdown, the reaper, the drain task and the reconciler refuse stale authority. A spawn completing after a stop tears its own runner down instead of installing it. Stop remains pending until physical runner and scope retirement is proven, holding prompts and refusing resumes while the reconciler retries.
- **Compatibility is not liveness.** `runner_version` is 7 and the control wire is v4. The optional registry `control_protocol_version` is a prerequisite, not proof of native usability; missing/null stays Unknown without rewriting old records. Unknown, incompatible or unconfirmed live ownership is refused before adoption, replacement, budget or queue effects. The actual connected Unix peer must match the captured PID and kernel birth before Attach. Compatible v4 workers retain the existing build-staleness replacement policy.
- **Process-group termination.** Runners are group leaders via `setsid`, and every termination path signals the group, so the node wrapper and SDK child die with the runner rather than reparenting to PID 1. Prefer the verbs over `kill -9`, which leaves the runner no chance to clean up.
- **Self-termination watchdog.** A runner polls its own registry record and exits when abandoned: record gone, superseded, or detached from any daemon past a 48h retention window (reset on reattach). This is the backstop for a daemon that dies without killing its runners.
- **Detach buffer.** Notifications, retained `PromptCompleted` and the non-sheddable FIFO `NativeLost` observation share a 4096-frame / 128 MiB replay ceiling. Admission charges the actual retained completion wire bytes, prefix included, and reserves the loss frame separately; a large error completion cannot overfill the consumer by refilling notifications. Ordinary notification delivery remains best effort. Reverse calls outstanding at detach are cancelled toward the agent and queued replies are dropped; `terminal/release` remains idempotent cleanup.
- **Native-session admission.** Only lifecycle results select native identity; notification and callback metadata never do. Unrelated updates are ignored and unrelated session callbacks are rejected before any transcript, permission, filesystem, or terminal effect. While identity is pending, retained notifications are bounded by the runner's detach-queue ceiling, so a reattach admits the whole flushed backlog and an overflow fails the connection rather than publishing a partial replay. Stored history is never rewritten by the guard.
- **Launch authority.** Preparation pins the native profile, title, lifecycle generation and provider under owned namespace, submission and instance exclusion. Startup is reserved before those preparation guards are released; hooks and readiness never run while holding them. Blocking admission owns its namespace guard through native checkpoints, including caller cancellation.
- **Producer-owned cancellation.** Spawn records exact process identity before awaiting readiness; attach captures original identity and scope at admission. Cancellation wakes connection and handshake work. Abort closes the captured control socket and bounds task join; the supervisor retains physical retirement.
- **Borrowed attachment.** Before installation, timeout or waiter drop releases only the attempted connection and reservation. Retry preserves the original runner and turn. Explicit stop claims the captured original before waking its handshake. A captured stop may displace an uninstalled observer, never an installed replacement.
- **Exact scope retirement.** Scoped runners persist their exact transient unit alongside PID and generation; unscoped and unresolved ownership are distinct. Explicit stop waits for only the captured unit to stop and become empty before settling. Rejected or timed-out scope retirement keeps its lease and retry debt, even after the leader exits. A SID wildcard never authorizes ACP retirement or recovered purge cleanup.
- **Callback parity and replay accounting.** Session-scoped filesystem, terminal, and permission callbacks wait for that announcement with either named or positional parameters, while request-scoped elicitation stays independent of session authority. Pending replay charges each original control frame, its four-byte prefix included, against the runner's 4096-frame / 128 MiB budget; typed re-serialization can expand native JSON and is not a substitute.
- **Mid-turn reattach.** `ResumeSession` adopts the runner's committed session identity and never sends ACP `session/new` or `session/load`. A new local prompt supersedes adoption, and only the matching canonical request id may resolve it; a 30-second resume-idle watchdog is the fallback.
- **Background sub-agents across a restart.** The async-Task tailer (see [Stuck-turn watchdogs](#stuck-turn-watchdogs)) is per connection, so a `BackgroundAgentLaunched` can be left with no completion when its worker dies. The next owner reconciles that: `attach` re-tails every unresolved launch that still carries a transcript path, since the runner it adopted is provably alive; `spawn`, the drain task's respawn, and every terminal drain arm detach unconditionally. `output_file` is persisted for that resume and stripped from every client-facing frame.

## Error evidence and failed operations

Runtime evidence and operation failure are independent. A terminal Error banner,
retry exhaustion or tool failure does not establish process exit, lost history or
permission to retry. A live pane is evidence about that pane process, not agent
readiness. A dead pane records exit with an unknown exit code when none was
observed; neither zero nor nonzero exit establishes a crash cause.

Malformed JSON retains the SDK's existing parse-error response behavior. That
response alone does not prove closure; real transport closure remains fatal.

| Producer | Evidence and consumer behavior |
| --- | --- |
| Terminal detector | The winning current rule owns the diagnostic. Terminal text remains heuristic; pane metadata supplies independent liveness. Ongoing retry and newer activity retain their existing Status rules. |
| Ordinary local prompt result | Emits `PromptRuntimeError` then one `Stopped("prompt_failed")`. SDK result provenance and reported code do not establish provider origin. The connection, native identity and history remain available for the next explicit request. |
| Ordinary adopted runner completion | Preserves the same diagnostic and failed terminal, ordered before a new local turn. Aborted completion is not success. |
| ACP transport or control closure | Remains a connection failure, including idle EOF and closure after a request error. It is not relabelled as a provider response or automatically retryable evidence. |
| Rate limit / lifecycle load rejection | Retains its existing distinct parking or `SessionContextReset` path. Error ingestion does not authorize a fresh conversation. |
| State, Status and clients | Failed completion closes the turn and projects Error independently of the running worker. Transcript, usage and pending approvals/elicitations survive. No implicit prompt replay occurs. |

`prompt_failed` never replays the failed request. A distinct already-explicit
queued intent may follow under the existing governor only while the same current
native channel is genuinely Available. Unknown/refused/lost ownership cannot wake,
spend a retry budget or deliver that intent. A late cancel cannot replace the
already-published failed terminal.
Unsupported-session prose in an ordinary prompt error cannot discard the native
SID or authorize reset; the next explicit input uses the same available runtime.
Ordinary request failure preserves actual approval and elicitation responders.
Qualified channel unavailability instead retires impossible owned controls and
folded/SQL pending metadata without synthesizing approval decisions or nonce
resolutions. Historical diagnostics and transcript content remain intact.
A refused management attachment, including a borrowed pre-Hello timeout, does
not establish native-channel loss. Its current controls remain unavailable,
while retained turn/tool/monitor state, callback history and retry-budget
boundaries remain intact. Only actual qualified native loss closes those
durable boundaries; no synthetic prompt completion or permission answer is emitted.
An SDK-private pre-loss pending-request set can remain frozen until explicit
teardown; this is not permission for new native I/O or callback effects.
An already correlated completion remains its genuine outcome even if its SDK
relay marker, or a later relay, fails. The separate connection loss fences
further native controls; it cannot rewrite a completed result as `NativeLost`.
An idle established command loop observes the ordered loss boundary so SDK
close cannot wait forever for that foreground to finish. Pending-ingress
overflow fails closed, discards its uncommitted replay and establishes its own
ordered protocol-failure boundary; it does not wait for the SDK close that
depends on foreground completion.
Shared turn delivery, pending-initial-turn drains, dormant queue wakes and the
existing rate-limit auto-resume admission consult qualified unavailability
before resume, publication, redelivery-budget or mutation effects. Refusal
retains queued and pending intent; it is not a startup failure.
Admitted requests and notifications share the identity fence for their effects.
A pending human response therefore does not block native progress, a later
prompt error, or EOF dispatch. Identity begin and commit/replay take exclusive
access; candidate requests wait for commit without holding shared access.
An explicit reset still waits for old callback replies within its existing
deadline. Expiry leaves the conversation and unanswered cards intact; it does
not answer or discard them.

The existing terminal claim records the failed outcome of its owned main turn.
Ambiguous later output, thinking or accounting cannot mint a second main-turn
terminal for that failed owner. Native content and tool/background bookkeeping
still flow, and background work keeps its own records. The main failure can
therefore remain Error while background work continues. An explicit next prompt,
successful intentional conversation reset, or new runtime ownership releases
the local outcome fence. This is producer ownership, not permission to retry,
replace a provider, or create a conversation. Rate-limit parking for owned
outcomes is unchanged. Error and Idle alone never authorize effects.

Optional `PromptRuntimeError.evidence` carries source, operation, independently
observed runtime facts, blocking scope, reported code, ingestion time and the
publishing worker epoch. Unknown origin or recoverability stays unknown.
Terminal observations use the separate instance lifecycle-generation namespace.
Publication rejects superseded workers before allocating a sequence, persisting
or folding. Snapshot metadata is a diagnostic hint, never effect authority.
The exact original drain may still publish its tagged tail while its current
lease retains retirement custody, including a late native session ID needed for
Keep handoff. This does not authorize an absent uncaptured publisher, a replaced
lease, or publication after the retained join slot is quiesced. Retirement-only
publication does not mutate an absent worker or replacement configuration.
Adopted completions claim their captured turn before waiting for event capacity.
Their enqueue and the resume-idle fallback recheck that turn under the same short
publication exclusion used by local activation. No synchronous guard crosses an
await. SDK EOF first fails pending requests. Its close callback drains the local
prompt and already-observed correlated nonprompt outcomes through publication
before fatal connection handling. It never waits indefinitely for unresolved
native requests. Early abandonment still cancels the prompt's owned future.
An adopted completion uses the existing private SDK completion-marker lane.
Its self-contained payload holds only the bounded classified diagnostic and
captured turn, not arbitrary ACP data or another local outcome's waiter.
Publication waits until initial identity commit has applied its buffered replay.
Initialize and Resume replies bypass that SDK pipe through the existing control
handshake channel, so a cached completion before Initialize cannot block the
handshake. Native notifications cannot mint these daemon completion payloads.
Control EOF drops a pending local waiter, but does not supersede an adopted
completion marker already ordered into the SDK pipe. That marker still checks
explicit local supersession and its captured terminal epoch before publishing
the diagnostic and failed terminal ahead of fatal transport handling.

Old events with missing or null evidence replay as unknown. The SDK message is
the primary public diagnostic. Only a direct string `data.details` may add a
labelled human-diagnostic supplement; other fields, nested values and non-string
details stay private. Both texts are scrubbed for known secret patterns and the
combined diagnostic, label and truncation marker are capped at 2048 UTF-8 bytes.
This allowlist is a human-diagnostic convention, not a guarantee that a string is
safe: the scrubber does not detect arbitrary secrets. The supplement grants no
origin, liveness or retry authority. Remaining causes stay in private tracing.
Instance `last_error` and its typed metadata are runtime-only (`serde(skip)`), not
a durable audit trail. No registry migration is required.
Terminal evidence also travels through the existing session projection and
runtime/HTTP response to TUI and web clients. Those optional response fields do
not turn runtime observations into stored registry data or retry authority.
Web request failures use a separate transient interaction error, with an optional
card kind and nonce. That error temporarily takes banner precedence without
overwriting the canonical operation diagnostic. A matching successful resolution
or dismissal clears the transient error and reveals the retained diagnostic,
including legacy errors without metadata. Reattach and optimistic submission do
not retire the diagnostic; an accepted next prompt, explicit dismissal, or
successful intentional clear/switch does. Transient request errors are not
persisted in the browser cache.
Approval and elicitation resolution hooks reject failed HTTP and network
submissions so their cards restore controls for retry of the same nonce.
HTTP failures retain HTTP diagnostics, not a network-error label. Retrying a
destructive approval still requires a fresh complete hold.

### Current native-input projection

Physical worker liveness, sidebar Status, operation diagnostics and native-input
availability are separate facts. `AcpConnectionUnavailable` exposes only bounded
source/cause/local-phase fields; neither EOF nor a banner asserts process death.
Original event semantics and the captured owner/native context stay private,
including when journal failure substitutes an `AgentStartupError`. The common
projection rechecks physical profile, instance lifecycle generation, original
worker epoch and the live connection latch after waits. A stale producer cannot
allocate a sequence, persist, fold or mutate the current instance.

Cold replay, warm folds, status/list consumers, lag recovery and WebSocket
snapshots use that projection. Historical readiness is never current health;
a genuine current owned Available receipt is sufficient without a historical
ready event. Current loss dominates an earlier ready receipt. An unreadable or
refused owner remains unavailable or Unknown, not permission to replace it.
An actual new positive owned channel receipt can heal a stale connection-error
Status from its current folded projection, under the same identity/epoch fences.
It preserves operation diagnostics and history; cached identity, a superseded
subject or a sticky live loss cannot perform that repair. A typed channel loss
at final delivery still returns `unavailable` (HTTP 409), preserves pending and
named queued intent, and never becomes retryable worker-not-ready permission.

Publication samples supervisor worker state before taking publication or instance
exclusion. Producers and native-control admission try the native flock/cache
without blocking under workers/lifecycle guards. On contention, they release both
global guards, wait outside them and recheck every current lease/actor/native fence
before effects or sequence allocation. The original pending event is retained
exactly once. Read-only current-availability projection is nonblocking and returns
Unknown rather than creating an inverse publication/native-lock dependency.
The existing attach deadline also bounds the control Hello exchange, not just
socket connection and runner registration.
A timeout while observing an attested retained runner releases only the borrowed
attachment and re-arms the existing reconciler tick. It does not manufacture a
persistent ownership refusal, terminate native work or authorize replacement.

Stop can preserve a private, qualified binding receipt already journaled by its
exact original lease/actor/native context before incrementing the stop generation.
It does not accept an unqualified trailing frame or change the selected native store.
Keep handoff may explicitly capture a metadata-only publisher capability in its
existing original retirement resources, under the promoted root generation,
physical profile, row creation, exact runner birth/lease and selected conversation/store.
Only correlated SID observations survive that capability, with readiness false;
old turn/status/callback traffic consumes no new-root sequence. The original
native-control fence remains unchanged and cannot gain input or fresh-conversation
permission from metadata custody. Replacement or a newer choice supersedes it.

The live `reduced_state` wrapper sequence qualifies `connection_ready` and
`connection_unavailable`. A same-sequence refresh is meaningful; an older
snapshot or covered raw frame cannot overwrite newer authority. Socket-open,
session IDs, cached views and optimistic rows grant no controls. Disconnect and
view reopen withdraw authority without discarding the previous diagnostic.
Only actual current Available enables native mode/config/cancel/responders.
The runtime-only `prompt_wake_eligible` snapshot field separately represents the
existing explicit WorkerDown queue/wake and initial-start governor, including ordinary known
absence, dormancy, snooze and rate-limit parks. It is not native health, generic
unreadable/ambiguous-owner admission, automatic replay or fresh-conversation permission.
TUI consumers accept the live wrapper at sequence zero and same-sequence
refreshes too. They retain wake eligibility separately from native readiness;
the optional wake field is deserializable live metadata but is omitted from
`AcpState` serialization, never cached/history authority. Both full-screen and
embedded management disconnects withdraw the accepted receipt. Submission and
native responder/mode/cancel effects check it before consuming a composer,
recall edit or picker selection. Existing named queue edits/deletes, link opening
and read-only picker inspection remain management/local actions. Refused new
intent cannot silently become a queue write; a retained ordinary request failure
alone does not withdraw a genuine current Available receipt.

Current work and current actionability stay distinct in the WebSocket projection:
a RunnerRecord management admission refusal disables native controls but preserves
the observed active/cancelling/compacting turn, tool/thinking state and historical
pending responder metadata. Actual qualified SDK/control/native channel loss
retires impossible current controls and work; neither projection fabricates a
human response or rewrites the original event history.

A header can precede raw catch-up. Covered raw frames still restore fields absent
from the reduced snapshot: configuration/results, usage and cost baselines,
capabilities, diagnostic text, cap/park bookkeeping, wake/monitor/primer state
and background-agent history. They cannot replace current native availability,
turn or responder authority, and only a matching prompt echo settles its local POST.
Replay tracks turn context at its own raw cursor separately: an old stop followed
by a new prompt must reset cap/park metadata even when the newer visible snapshot
is already active and steering is enabled. Actual same-turn steering retains that
turn's metadata; neither case rewrites current turn or callback authority.
Queue listing is safe on socket-open, but unconfirmed cached intent remains local
until a current Available or explicitly governed wake receipt permits migration.
An empty queue listing preserves unconfirmed cached rows; confirmed drained rows
are never re-created.
A queue GET may reconcile only the unchanged local queue it began reading.
Acknowledged delivery settles the named row again, and retained-row replies
upsert by ID, so a delayed list cannot erase new intent, duplicate a refused row
or resurrect an already-claimed row.



## Terminal handoff

Terminal handoff persists intent before retiring the original runner. It retains that intent through proven retirement, the exact Terminal commit, required terminal launch and projection cleanup. Cancellation or a committed-write adoption failure leaves recoverable debt, not a successful receipt. Recovery selects the matching intent and original authority; reservation age never permits another driver to steal it.

A CLI-resumable handoff preserves the final native conversation and its exact execution/store binding, including newer identity from that recorded execution. Missing custody remains pending rather than launching a fresh conversation. Restore the original native store or explicitly bind a verified target with `aoe session set-session-id SESSION ID --store /absolute/store`. Explicitly clearing the ID with `aoe session set-session-id SESSION ""` authorizes a fresh terminal conversation. Target changes require completed original retirement, no active launch and no live pane; they retain handoff identity and invalidate older generation results. A marker-proven owned dead pane may remain for the ordinary start path to remove. A live, unowned or mismatched pane is retained. Retry disable to adopt an already-live matching pane without another launch.


The ACP load-session capability belongs to structured runtime state: same-view reloads retain it, while terminal handoff and a later enable clear it rather than resurrecting the previous adapter observation.

The shared start core distinguishes admission from a no-op: native start requires its authoritative row even when a live pane already exists. The default-store entry point retains its preexisting-live-pane `Skipped` contract without requiring a persisted row; any actual launch still rereads and validates its row before reserving a generation.

## Session deletion

`session/delete` fires only on permanent removal (a purge or a terminal handoff that discards the conversation). Reversible teardown (`aoe acp stop`, snooze, archive, trash, idle auto-stop) and CLI-resumable terminal handoff deliberately do not fire it, so the transcript stays on disk and the next respawn resumes through `session/load`.

An in-flight reversible stop can gain permanent deletion debt at any await. The lease keeps only its original attested adapter route. Late `session/delete` is best effort on that route before close when possible; an unavailable or retired route is failure, never a successful deletion receipt. Physical retirement cannot fabricate adapter success, and a replacement route is never borrowed.

Trash is the reversible middle state: a delete moves the session there by default (`session.delete_to_trash`), keeping its transcript, worktree, branch, and container until it is restored, purged, or auto-purged after `session.trash_retention_minutes`. Retention is enforced by the daemon (a startup sweep, then one every tenth of the shortest window, between a minute and an hour), so without one, expired trash waits for the next daemon start or a manual purge. When the daemon permanently deletes a structured session it sends a best-effort `session/delete` (2s timeout) so adapters that implement it can release their own state, then proceeds with cancel, SIGTERM, and on-disk cleanup. CLI purges have no running worker, so they delete the local transcript only.

Purge captures the selected row's path, branch and runner references before destroy hooks. Before irreversible row removal, it adds fresh row references and persists ownership in `pending-purge-owners.json`. Failed cleanup retains ownership across requests and daemon restart; success releases only its own token. Missing, malformed or unsupported journals refuse cleanup. Both ordinary and irreversible purges require a captured process group to exit before host cleanup, even if its worker record disappeared. Ordinary purge retains the row and releases its reservation when that check fails. Uncaptured evidence is not proof of runtime absence. A refused irreversible commit settles its durable purge reservation before returning when the row remains on disk, so an immediate retry is not blocked b…

An ambiguous tmux `no current target` failure is not absence proof. While
the captured server is live, teardown requires a bounded inventory on the
same socket that excludes the exact target. Failed or undecodable inventory
retains the error and resources. Native process handles distinguish an
unreaped exited server from a stopped server; matching birth alone proves
identity, not liveness.

Committed purge retains its original cleanup and process authority through retirement. Cleanup requires fresh store health, row absence, the exact live journal owner and proof that captured runtimes exited. A replacement lease, runner or reused row never inherits that authority; failed proof retains the journal and resources.

A cancelled pre-birth constructor may settle only its exact producer epoch and construction UUID after durable completion acknowledgement. Missing records and historical uncaptured evidence do not prove that no child was born. Event-store projection and supervisor cleanup obey the same absence and owner fence, including ID reuse without a replacement worker.

Retained purge owners prevent profile rename and deletion until recovery settles.
The fence includes both resolved sessions and groups bindings, so a retained
owner reached through a supported registry-file alias protects the physical
profile too. An unrelated registry remains mutable.
Admitted catalogue mutations retain their namespace and publication ownership
through canonical adoption even when the HTTP waiter disconnects.
Ordinary native HTTP mutations are owned by the runtime mutation scope too.
Shutdown closes new admissions and drains admitted producers through disk commit
and canonical adoption before lifetime ownership is released. The shared storage
transition precedes publication exclusion; admission and completion must not be
inferred from whether a snapshot read blocks.
Canonical registry durability includes the resolved target parent, even when the
profile's registry is a symlink outside the application directory.
Recovered absent-row purges confirm that same directory barrier before accepting
the retained owner as cleanup authority; another sync failure keeps the owner.

Purge journals retain exact captured scope ownership, so a reaped leader cannot authorize checkout cleanup while escaped cgroup members remain. Legacy unknown scope evidence stays unresolved until complete authoritative inventory proves its unknown component absent; captured units and all process-birth and filesystem authority remain unchanged. Native child birth, scope capture and immediate PID/generation recording share durable launch-admission exclusion. Cancellation and birth publication fencing prevent a dropped constructor waiter from publishing after ownership release. See [Ownership recovery](../../installation.md#ownership-recovery) for strict recovery prerequisites and original-journal restoration.

Terminal scope ownership is recorded in each instance row before
physical pane birth, under the existing launch generation and instance lifecycle
exclusion. A known unscoped start is an empty scope list; missing legacy evidence
is unresolved, never implicit classic-process proof. Ordinary terminal purge
freezes that row's exact units and proves their cgroups empty before checkout,
worktree, sandbox or scratch cleanup. Coordinated stop reloads the authoritative
row under its lifecycle exclusion so a cached clone cannot omit a newer unit.

Per-ID mutation exclusion stays shared with every held or queued operation. Reusing an ID must not create a second mutex while prior references remain.

Live owner references include missing descendants and symlink traversal dependencies. Permission errors, symlink loops and non-directory traversal refuse cleanup instead of being treated as absent owners.

## Who owns the state

The daemon folds the event stream once per WebSocket connection into two projections, so clients do not re-derive them:

- **Control state**: turn flags, pending approvals and elicitations, usage, plan, modes, slash commands, shipped as `reduced_state` on connect and after every event. `unchanged` names the cold fields the socket already holds, which the daemon omits rather than re-serializing. The connect frame is folded over the whole session even with a `since` cursor, because clients adopt it verbatim.
- **Transcript rows**: a `transcript_snapshot` on connect plus a `transcript_delta` (`Append` / `Patch` / `Remove`, reconciled by row id) per event, with `GET /api/sessions/{id}/acp/replay?view=rows` for history. Presentation (markdown, tool cards, diffs) stays client-side.

Raw event frames still stream for what the daemon does not model (worker lifecycle latches, monitor and wakeup badges, the usage cost baseline, rejected prompts). A client reading only the projections passes `?frames=0`, as the native view does. A `notice` row carries a failed startup, a dead turn, a refused mode switch, or an auto-resume; the native view renders it inline while the web shows the same information as a dismissible banner.

The daemon also owns prompt dispatch: `POST /acp/prompt` runs `acp::dispatch::decide` and returns `sent`, `steered`, or `queued`, and the server-owned queue persists follow-ups until its turn-end drain. Cancelling and compacting turns always queue; a dormant worker is sent the prompt so the request wakes it.

## Persistence and the context primer

Transcripts persist in a SQLite event log. The web client mirrors each session's reduced state into `localStorage` (7-day expiry, full replay past the origin quota) so a reload hydrates instantly and fetches only the seq delta.

Native ACP HTTP history decodes each existing JSON replay page incrementally, with a fixed-size read buffer and no copy of the entire encoded body. Replay pages and individual events or rows may exceed 16 MiB; the generic daemon response decoder remains size-bounded. The native history decoder retains the 15-second body-read deadline, requires a complete valid document through EOF, and cancels a pending body read if its caller is dropped. Count-based paging and raw/rows cursor metadata are unchanged. This does not change the pre-existing WebSocket message limits.

A cold open loads recent-first rather than folding from seq 0 before first paint. `GET /api/sessions/{id}/acp/replay?before=<seq>&limit=N` returns the events closest below `before`, aligned to a user-turn boundary so a page never seams a split turn. The client renders the tail immediately and, on a long session, also pulls a small `since=0` prefix for the pinned handshake snapshot (capabilities, slash palette, agent and model) the composer needs. Scrolling to the top pages older history, prepending with the scroll position frozen; each page is fetched twice, once for frames and once for `view=rows`, and a failure on either leg abandons the page rather than advancing the cursor.

If context restoration fails (the agent's stored session is gone) the view falls back to a fresh session, renders a "Conversation context reset" callout, and offers `GET /api/sessions/{id}/acp/context-primer?before_seq=<reset-seq>`: a compact markdown recap of the last ~20 turns (capped ~24k chars, bulky tool I/O elided), pre-filled into the composer and never auto-sent.

`aoe-agent` ships as sources inside `aoe`, is installed into the data dir on demand (a digest over its lockfile and sources decides whether the installed copy is current), and persists each native conversation in `${AOE_ARTIFACT_DIR}/aoe-agent-<native-session-id>.jsonl`. Only completed user/assistant text is persisted, not tool calls. A load of a missing or unreadable transcript fails explicitly rather than reporting an empty successful resume, which the view surfaces as a context reset; without an artifact directory, sessions are ephemeral and load is unavailable.

## Permission modes and model channels

Modes come from `NewSessionResponse.modes`, and the picker shows whatever the adapter reports; Gemini's `auto_edit` / `yolo` fold onto `acceptEdits` / `bypassPermissions`. YOLO fires `session/set_mode("bypassPermissions")` after `session/new`, best-effort. `claude-agent-acp` gates that mode on `ALLOW_BYPASS=1` in the daemon's environment; without it `set_mode` returns "not available" and the session stays `default`, surfaced as an amber notice.

Model and reasoning-effort selectors arrive over two wire mechanisms, normalized into one dropdown: `SessionUpdate::ConfigOptionUpdate` (a full snapshot of every selector whenever one changes, so the client replaces its cached list) and the `unstable_session_model` capability (`SessionModelState` on `session/new` and `session/load`, switched with `session/set_model`). With both present, `config_option` wins, since it has a push path; `session/set_model` only acks, so the client synthesizes the confirming update. The UI is pessimistic (the chip keeps the prior value until a confirming update arrives) to avoid snap-back on slow tunnels. The cached list clears on `AgentSwitched` but survives `/clear`, since capabilities are process-scoped.

Selector persistence and backend changes are fenced against disk reloads. Model/config-option and legacy mode requests share the provider switch's per-session submission lock; worker readiness waits run outside the instance mutation lock.

Session notices (`SessionUpdate::Notice`) are live advisories, not conversation history. The adapter only sends them when the client advertises `clientCapabilities.session.notices`; without that it folds each one into a bold-label agent message indistinguishable from the model's reply, so advertising the capability is what makes them identifiable. They ride on `AcpState` capped and scoped to the current turn, which is what feeds the dismissible strip on both surfaces, and they also land in the transcript as an `advisory` row, which unlike a `notice` row both surfaces render, so the history survives a dismissal. Dismissal is client-local: the folded list is shared, so clearing it server-side would blank every other client.

Notice IDs use the durable event sequence, not the count of retained events, so replay after pruning preserves client-local dismissal.

Approval nonces are server-generated and single-use, and are never revealed to the agent. Resolving an already-resolved approval clears the card quietly.

## Stuck-turn watchdogs

Three layers recover a turn that stops progressing:

1. **Cancel escalation.** The agent ignores `session/cancel` mid-tool. After a ~10s grace the daemon ends the connection, SIGTERMs the runner, and respawns via `session/load`. Banner reason `agent_unresponsive`.
2. **Force end turn (client).** No streaming chunk for 30s with no tool in flight surfaces a button that publishes a synthetic `Stopped` plus a best-effort cancel. With a tool in flight, or during a latched compaction phase, it stays hidden so it cannot discard real progress.
3. **Silent-orphan watchdog (daemon).** The adapter finished streaming but never sent the `PromptResponse` that closes `session/prompt`. It fires only when no tool call is in flight, at least one progress notification has arrived, and none has arrived for `silent_orphan_grace_secs` (120, dropping to a fixed 20s once a cost-populated `UsageUpdate` lands). A turn that already emitted its cost-populated usage with no off-protocol work pending ends cleanly; otherwise the daemon cancels, waits 10s, SIGTERMs, and respawns.

**Off-protocol work** suppresses the watchdog, because some agent features go quiet with no ACP signal. An async `Agent` tool is tracked precisely (a tailer follows the sub-agent's transcript and an in-flight set keeps the watchdog from firing). `/compact` is detected from the ACP `compaction_update` the client advertises `session.compaction` for, or from text markers for agents without it, and its start latches a 30-minute grace floor so a large compaction is never cut short; the same signals publish `ConversationCompactionStarted` / `ConversationCompacted` so both clients know the phase, followed by `ConversationCompactionSummary` when the agent reports the summary it retained. A backgrounded `Bash` latches the floor until a cost-populated usage update arrives. Separately, a backgrounded `Bash` is tracked as a task: its start (`BackgroundTaskStarted`) comes from the tool result's `toolResponse.backgroundTaskId`, and its end (`BackgroundTaskFinished`) from the `<task-notification>` Claude Code writes to the session transcript (`task_transcript.rs`), which a tailer follows through the shared file watcher (a 30s re-read covers a dropped kernel event, and a 500ms poll takes over when no watcher is available). The adapter's own `async_task_*` updates are not used: declaring the AIR extension that unlocks them makes the adapter strip the output of every Read that names a path. Tasks are tracked for host sessions only; a sandboxed session's transcript is in the container. An open task keeps the session `Running` and the busy signal lit like a background sub-agent. A reattached worker's open tasks resume tailing from the start of the transcript, and teardown or a worker replacement finishes any left open. A `ScheduleWakeup` suppresses recovery until `wakeup_at` plus the floor, on a monotonic deadline that multiple wakeups extend. An agent-initiated turn that streamed output but reported no cost-bearing end and scheduled no wake is a stalled stream rather than a parked monitor, and recovers on its own 120s grace.

Task monitoring uses the host Claude store captured for the launch or attachment.
Transcript completion matches task ID and captured tool-use ID in the machine header, never quoted summary or result text.
Reusing a task ID supersedes its prior monitor, which cannot retire the replacement.
Pending tasks block conversation reset and idle shutdown, and restore canonical busy status after daemon restart or reattachment.
Client busy indicators include tasks without changing the main-turn rules for sending prompts or Esc-to-cancel.

## Rate-limit handling

A backend reporting `errorKind: "rate_limit"` is a clean terminal state, not a crash: the daemon emits `RateLimit` plus `Stopped { reason: "rate_limited" }`, drops the worker handle, and does not respawn into the same limit. The park is durable (the latest `RateLimit` with no prompt, agent switch, or unrelated stop after it) and survives a daemon restart, so a failed resume neither clears the banner nor disarms auto-resume. `RateLimitInfo.resets_at` is filled only from a reset the agent attributed to the window it rejected, and stays `None` otherwise, so no fabricated time reaches the UI.

With `[acp] rate_limit_auto_resume`, the reconciler resumes the same worker once `resets_at` plus a 15s grace passes, or, with no reported reset, an hour after the park with the wait doubling per redelivery (1h, 2h, 4h, 8h, 16h). Each resume re-delivers the interrupted prompt once, capped at 5 per streak, after which the session parks on `Stopped { reason: "rate_limit_exhausted_retries" }`. The streak is counted from persisted `RateLimitAutoResumed` breadcrumbs since the last organic turn end or agent switch, kept in a per-session row outside the pruned transcript. Nothing un-parks a session on a timer otherwise: dispatch treats the park like idle dormancy, which is what makes "send a new prompt" true.

## Crash-loop park

A worker that exits within ~10s (broken command, missing adapter, failed handshake) logs a `warn` on the `acp.runner` target and counts against a respawn budget: more than 5 spawns in a rolling 60s parks the session, publishes one `AgentStartupError`, and stops auto-respawning. Recovery is a dashboard retry, `aoe acp restart <session>`, or an `aoe serve` restart.

## Agent switching

`POST /api/sessions/{id}/acp/switch-agent` stops the current worker, spawns the target, persists `agent_name`, clears `acp_session_id` (the old id belongs to a different vendor), resolves the model from the request alone so the old backend's pick does not follow the session across vendors (as `Instance::swap_tool` does), and emits `AgentSwitched { from, to, reason }` so reducers drop backend-specific transient state and the transcript shows a divider. The composer is pre-filled with a context-primer recap, never auto-sent. CLI: `aoe acp switch-agent <session> <target> [--model <name>]`. `reason` is `manual` or `rate_limited`. Completion commits through the scoped native store under canonical exclusion. An automatic respawn retains the originating launch intent; an independent launch supersedes it and refuses the old completion.

Claude-family provider changes restart the worker, preserve conversation and reasoning effort, and reset the model to the target provider default. Active turns and background work refuse the change. The web client keeps one pending request per session across route remounts; an observed canonical provider change invalidates a late response echo, including an away-and-back transition.

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
replay_events = 0                 # 0 = unlimited; caps per-session rows (rows of still-open background work are kept) and the client buffer
node_path = ""
show_tool_durations = true
wrap_tool_output = false          # initial line wrap of tool output blocks; each block can toggle
compaction_reminder = false       # opt-in /compact nudge past the threshold
compaction_reminder_percent = 75  # 1..99
notice_display = "persistent"     # persistent | timeout | hidden; errors always persist
notice_timeout_secs = 10          # auto-dismiss delay for notice_display = "timeout"; 1..2147483
silent_orphan_grace_secs = 120    # 0 disables
auto_stop_idle_secs = 3600        # 0 disables; the next prompt respawns the worker
rate_limit_auto_resume = false
```

Cold-start resume parallelism is a fixed 4 spawns, clamped to `max_concurrent_workers`, to bound Node bootup memory. `auto_stop_idle_secs` stops an event-idle worker with no in-flight turn (the session keeps its sidebar slot and shows `Stopped { reason: "idle_auto_stop" }`); mid-turn workers are never stopped and the check runs about once a minute. `AOE_ACP_NODE=/path/to/node` overrides Node discovery for one process.
