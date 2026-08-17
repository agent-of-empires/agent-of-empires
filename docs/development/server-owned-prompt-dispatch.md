# Server-owned prompt dispatch (send / steer / queue)

Status: shipped 2026-08-17. Tier 3 of the SV server-ownership arc, and the last
duplicated decision after control state and the transcript moved server-side.
See `server-owned-sv-state.md`.

## The problem it solved

`POST /api/sessions/{id}/acp/prompt` used to be unconditional: whatever reached
it was sent to the agent. Deciding whether a prompt could be sent **at all**
was the client's job, and both clients implemented it independently:

- web `useAcpSession.sendPrompt`, a `shouldEnqueue` expression over
  `turnActive`, `promptCapabilities.steering`, `cancelling`, `compacting`,
  `workerStopped`, `workerRestarting`, `workerIdleStopped`, the REST
  worker-state poll, and its own socket state.
- native TUI `should_queue_prompt_for`, the same decision over the same flags
  plus its own `in_flight` and socket state.

The decision is subtle in ways that are invisible until it is wrong, and the
web's version carried a 40-line comment because each clause is a fixed
incident:

- **#2805**: a steerable agent takes a mid-turn prompt directly, so parking it
  reintroduces the queue-after behavior steering exists to replace.
- **#1727**: except while a cancel is pending, because the daemon reads a
  prompt arriving mid-cancel as a wedged agent and **restarts the runner**. So
  Stop-then-type must park, or it respawns the worker.
- **#3219**: and except during `/compact`, because the adapter answers
  `Injected` and swallows the message into a turn that never replies to it.
- **#1689**: an idle-dormant worker must NOT park on "worker not running": the
  POST itself is the wake path, so parking leaves the prompt in a queue that
  never drains.

Every one of those is a fact about the daemon, discovered by the daemon, and
then re-derived by each client from an event projection. A third client would
re-derive it a third time, and the failure mode is not a cosmetic drift: it is
a wedged session or a respawned worker.

## Design

The client posts the prompt. The daemon decides whether to send it now, steer
it into the running turn, or queue it, and says which it did. No client
predicts the outcome.

### The decision is one function

`dispatch::decide(&AcpState, WorkerLiveness) -> PromptDispatch`
(`src/acp/dispatch.rs`) returns `Sent`, `Steered`, or `Queued { reason }`. It
carries the four incident clauses above and is table-tested against them by
name, so a future edit that reintroduces #1727 fails a test that says so.

The failure modes are asymmetric: wrongly sending where the old code parked can
restart a worker (#1727), while wrongly parking only delays a turn. So every
path not positively classified as sendable falls through to `Queued`.

**Where the `AcpState` comes from.** The WS folds are per-connection
(`acp_ws::handle`), so an HTTP handler has none to read.
`acp_ws::fold_control_state` serves one from a live per-session projection
(`src/acp/control_cache.rs`) that `ChannelSink::publish_persisted` keeps folded
as it records and broadcasts each event, rebuilding from the durable log only
on a miss.

That cache is not premature. The first cut rebuilt from the log on every POST,
which measured 4ms at 1k events, 68ms at 20k and **342ms at 100k**, against a
store whose retention default (`acp.replay_events`) is unlimited and where every
streamed message chunk is an event. Worse than the latency, `record()` and
`replay_page()` share one `Mutex<Connection>` on a single daemon-wide store, so
a prompt on one long session stalled event recording for every session. Cached,
the same read is ~12µs and the scan is paid once per session per daemon life.

The projection is only ever populated by a full hydrate, never by starting a
fold mid-stream: `PromptCapabilities` sits near seq 1 and never repeats, so a
partial fold would report a steerable agent as unsteerable and bring #2805
back. A seq that does not continue the sequence (a gap, or the counter reset by
an `acp_disable` / `acp_enable` round trip) evicts rather than folds, and a
failed persist evicts too, since a projection of a log missing an event is not
a projection of that log.

**Why a stale latch is not a wedge.** If a worker dies mid-turn without a
terminal `Stopped`, the fold keeps `turn_active` set and every later prompt
parks. That delays rather than strands them: the turn-end drain gates on the
*instance* `Status::Idle`, not on the fold, so it fires the queue from a
signal this decision does not share.

### The endpoint applies it

`acp_prompt` calls `decide` before `send_turn` and, on `Queued`, routes into
the server-owned queue (`server-side-prompt-queue.md`) instead of the
supervisor. The queue and its drain already wake dormant workers, so this is
wiring, not new machinery.

The response is a typed body (**breaking**: the endpoint used to answer 202
with no body):

```json
{ "disposition": "sent" }
{ "disposition": "steered" }
{ "disposition": "queued", "reason": "cancelling", "queued_id": "..." }
```

`queued_id` lets the client reconcile its optimistic row against the queue
entry the same way the transcript reconciles by row id. `reason` names the
gate, so a client can explain the wait.

The queue row a parked prompt creates goes through the same
`buffer_and_enqueue` helper as `POST /queue`, so it is byte-for-byte the row a
client would have created itself: same per-session attachment cap, same
idempotent-by-id replace, same blob bookkeeping.

One user-visible change falls out. A mid-turn prompt on a non-steerable agent
is now queued and delivered at turn end, where the daemon used to refuse it
with `agent_busy`.

### Clients stop deciding

Both send paths collapse to: post, then render what came back. Deleted with the
decision: the web's `shouldEnqueue` and `workerStateRef`, the TUI's
`should_queue_prompt` / `should_queue_prompt_for`, and, since the native view
no longer chooses to queue, `HttpClient::queue_enqueue`, the TUI's
`enqueue_prompt`, and `QueueMirror::upsert`.

The socket term (`wsClosed` on the web, `!socket_up` in the TUI) disappeared
rather than moving: a POST that returned at all reached the daemon, so "can my
socket reach it" was always a proxy for a question the response now answers
directly.

Two things stay client-side. The TUI's `in_flight` survives as a double-submit
lock over the POST round trip, not as a term in any dispatch decision. And the
wake PATCH for an archived or snoozed session (#1581) stays, because it is a
session-lifecycle action the user takes before the prompt exists.

## Alternatives considered

- **Keep the decision client-side but share it.** No shared runtime exists
  between a Rust TUI and a TypeScript SPA, so "sharing" means porting, which is
  the status quo.
- **Have the endpoint reject instead of queueing** and let the client retry as
  a queue insert. That was the `PromptRejected` path, and it costs a round trip
  during the window where the user is typing fast.
