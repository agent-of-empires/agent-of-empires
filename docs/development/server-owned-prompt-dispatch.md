# Design: server-owned prompt dispatch (send / steer / queue)

Status: planned. Opened 2026-08-16. Tier 3 of the SV server-ownership plan, and
the last duplicated decision after Tier 1 (control state) and Tier 4
(transcript) moved server-side. See `server-owned-sv-state.md`.

## Problem

`POST /api/sessions/{id}/acp/prompt` is unconditional: whatever reaches it is
sent to the agent. Deciding whether a prompt may be sent **at all** is the
client's job, and both clients implement it independently:

- web `useAcpSession.sendPrompt` (`web/src/hooks/useAcpSession.ts`), a
  `shouldEnqueue` expression over `turnActive`, `promptCapabilities.steering`,
  `cancelling`, `compacting`, `workerStopped`, `workerRestarting`,
  `workerIdleStopped`, the REST worker-state poll, and its own socket state.
- native TUI `should_queue_prompt_for` (`src/tui/structured_view/state.rs`),
  the same decision over the same flags plus its own `in_flight` and socket
  state.

The decision is subtle in ways that are invisible until it is wrong, and the
web's version carries a 40-line comment because each clause is a fixed
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

## Goal

The client posts the prompt. The daemon decides whether to send it now, steer
it into the running turn, or queue it, and says which it did. No client
predicts the outcome.

## Proposed design

### The decision moves to one function

A pure `PromptDisposition::decide(&AcpState, &WorkerLiveness) -> Disposition`
in `src/acp/`, returning `SendNow`, `Steer`, or `Queue { reason }`. It ports
the four incident clauses above and is table-tested against them by name, so a
future edit that reintroduces #1727 fails a test that says so.

It reads `AcpState`, which since Tier 1.1 already carries `turn_active`,
`steering`, `cancelling` and `compacting`, and a small worker-liveness input
the handler already computes (`touch_and_wake_if_sunk`'s `woke_idle_dormant`
plus the supervisor's readiness).

### The endpoint applies it

`acp_prompt` calls `decide` before `send_turn` and, on `Queue`, routes into
the existing server-owned queue (Tier 0) instead of the supervisor. The queue
and its drain already exist and already wake dormant workers, so this is
wiring, not new machinery.

The response grows from a bare 202 into a typed body:

```json
{ "disposition": "sent" | "steered" | "queued", "queued_id": "..." }
```

`queued_id` lets the client reconcile its optimistic row against the queue
entry the same way the transcript reconciles by row id.

### Clients stop deciding

Both send paths collapse to: post, then render what came back. The web deletes
`shouldEnqueue` and its inputs (`workerStateRef`, the socket-state read, the
worker latches feeding it); the TUI deletes `should_queue_prompt_for` and the
`should_queue_prompt` call site in its Enter handler. The socket term
(`wsClosed` on the web, `!socket_up` in the TUI) disappears rather than moving:
a POST that returned at all reached the daemon, so "can my socket reach it" was
always a proxy for a question the response now answers directly.

The TUI's `in_flight` term is the one client-local input with no server
equivalent: it is a double-submit guard covering the window between its POST
and the echo. It stays, as a submit lock rather than a dispatch decision.

What stays client-side is the wake PATCH for an archived / snoozed session
(#1581), which is a session-lifecycle action the user takes before the prompt
exists, not part of dispatch.

## Increments (each ships green)

1. **Decide + apply, web only.** The decision function with its incident table,
   the endpoint change, the typed response, and the web rewired to it. The
   three live specs that cover the incidents (`acp-mid-turn-steering`,
   `acp-compaction-phase`, `acp-cancel`) are the acceptance gate.
2. **TUI rewired**, deleting its copy of the decision.
3. **Delete the now-unused control fields** the clients kept only to feed the
   decision.

## Alternatives considered

- **Keep the decision client-side but share it.** No shared runtime exists
  between a Rust TUI and a TypeScript SPA, so "sharing" means porting, which is
  the status quo.
- **Have the endpoint reject instead of queueing** and let the client retry as
  a queue insert. That is the current `PromptRejected` path, and it costs a
  round trip during exactly the window where the user is typing fast.

## Risks

The failure mode is asymmetric: wrongly sending where the old code parked can
restart a worker (#1727), while wrongly queueing only delays a turn. The
decision function should therefore default to `Queue` on any state it cannot
positively classify, and the incident table is the regression net.
