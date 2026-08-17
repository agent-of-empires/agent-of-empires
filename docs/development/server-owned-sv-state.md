# Server-owned structured-view state

Status: shipped 2026-08-17. The daemon owns the structured-view (SV) control
state, the transcript, and the prompt-dispatch decision; both clients render
them. What the web still folds from raw events is the state the daemon does not
model: the worker-lifecycle latches, the monitor and wakeup badges, the usage
cost baseline, rejected prompts, and the optimistic turn counters.

This doc is the record of why the arc happened and what was rejected along the
way. For what the wire actually looks like today, read
`internals/structured-view.md`; for the dispatch decision,
`server-owned-prompt-dispatch.md`. The arc follows the server-owned prompt
queue (`server-side-prompt-queue.md`), which moved the queue itself.

## The problem it solved

The daemon was pure event-sourcing. It broadcast and persisted raw `Event`s
(`AcpBroadcastFrame = { session_id, seq, event }`, `src/acp/protocol.rs`) and
**every client re-reduced the stream independently**:

- web `useAcpSession` / `reduceFrames` (`web/src/lib/acpTypes.ts`),
- native TUI `AcpTranscript` (`src/tui/structured_view/reducer.rs`),
- and a third reducer, `AcpState::apply_event` (`src/acp/state.rs`), which
  existed but ran only in tests.

So the control-state reduction (turn active, steerable, cancelling, compacting,
pending approvals and elicitations, usage, available commands, modes, plan) was
implemented three times and the two frontends drifted. Adding a third frontend
meant a fourth reducer. Migrating the web surfaced five places where the
daemon's fold disagreed with it, each a past fix that had only ever landed in
one client (#1128, #1213, #3028, the suppressed AskUserQuestion in-flight
pointer, and `Stopped` leaking the in-flight tool).

## What shipped

**Control state.** `AcpState` gained the four turn flags it lacked
(`turn_active`, `steering`, `cancelling`, `compacting`), ported from
`AcpTranscript`, which was the only implementation that already computed all
four. The daemon pushes the whole reduced state as a `reduced_state` frame and
both clients render it. Two gaps had to close on the way: `AcpState` no-op'd
`ModesAvailable` / `CurrentModeChanged`, and the "context lost, re-prime?" latch
became a derivation over the transcript rows.

**Transcript.** The larger half of the duplication. `TranscriptModel`
(`src/acp/transcript.rs`) folds the ordered rows once, the WS ships them as
`transcript_snapshot` / `transcript_delta`, and `GET /acp/replay?view=rows`
serves them for history. Presentation stays client-side: markdown, tool cards,
path shortening and diff rendering all read the rows. One behavior changed:
approvals are control state, not rows, so the native TUI stopped recording a
resolved approval inline in the transcript. The web never had such a record, so
the two clients now agree.

**Dispatch.** The last thing each client derived was whether a prompt could be
sent at all. See `server-owned-prompt-dispatch.md`.

### Where the reduction runs (per-connection)

Each ACP WebSocket connection reduces its own event stream through
`AcpState::apply_event` and pushes the result (`src/server/acp_ws.rs` `handle`).
Reduction is per-connection but deterministic, the same reducer over the same
ordered stream, so every client converges on the same control state. This
avoids a central mutable `HashMap<SessionId, AcpState>`, a new actor task, and
any change to the broadcast channel type, and it makes the connect snapshot
fall out naturally (reduce over the replay drain, then send). The cost is
re-reducing per connection; ACP sessions have few concurrent WS clients.

This is the arc's least settled decision. Three bugs found in review came from
it: a connect snapshot folded from the client's `since` cursor rather than the
whole log, a missing seq guard where the drain overlaps the live broadcast, and
a permanent desync after a lag. All three are fixed and pinned by tests, but a
single per-session writer folding from seq 0 would have made them impossible by
construction rather than by patch. That remains the obvious follow-up.

Tier 3 pushed it half way there. `src/acp/control_cache.rs` keeps one folded
`AcpState` per session at the publish choke point, because dispatch needed a
control state on an HTTP request and rebuilding one per POST cost up to 342ms
while holding the event store's connection mutex. So the daemon now has a
single-writer control-state fold; the per-connection folds in `acp_ws::handle`
still exist alongside it, and collapsing them onto it is what the follow-up
would finish. The transcript fold has no equivalent yet.

No new persistence: on daemon restart a connection rebuilds state by reducing
over `EventStore::replay_from` at connect, and the event log remains the source
of truth.

### The optimism decision

The open question when this was planned was turn-active latency: the web bumps
`pendingUserPromptSeq` optimistically before the server echoes, so the composer
flips to "working" instantly. The answer was **server-authoritative with a thin
client optimistic overlay**: the client keeps a local "I just sent a prompt"
flag that OR's into the rendered turn-active until the next `reduced_state`
frame arrives, then defers to the server. That preserves instant feedback
without a second source of truth for the steady state. `turnActive` and its
prompt / stop counters are the one carve-out that stayed client-side.

## Alternatives considered

- **On-demand `GET /acp/state` snapshot** (reduce over replay per request):
  cheapest, no writer, but pull-only, so clients keep reducing the live stream
  and the duplication survives for the live view. Rejected as the primary
  mechanism. Tier 3 ended up using this shape for the one caller that genuinely
  is pull-only.
- **Expand `SessionResponse`** with turn and pending counts: only serves the
  sidebar poll, not the live SV. A narrow badge win, not the goal.
