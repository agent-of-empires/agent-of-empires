# Design: server-owned structured-view state (live reduced-state channel)

Status: in progress. Opened 2026-08-16. The server side is complete and the
native TUI renders both projections; the web still folds control state from raw
events (increment 2 below). Follows the server-owned prompt queue
(`server-side-prompt-queue.md`) as the next step of moving structured-view (SV)
logic off the clients so a web UI and a native TUI share one implementation.

This is "Tier 1" of the SV server-ownership plan. Tier 0 (the native TUI adopts
the server-owned prompt queue, retiring its local `PromptQueue`) shipped
alongside this doc.

## Problem

The daemon is pure event-sourcing today. It broadcasts and persists raw
`Event`s (`AcpBroadcastFrame = { session_id, seq, event }`,
`src/acp/protocol.rs`) and **every client re-reduces the stream independently**:

- web `useAcpSession` / `reduceFrames` (`web/src/lib/acpTypes.ts`),
- native TUI `AcpTranscript` (`src/tui/structured_view/reducer.rs`),
- and a third reducer, `AcpState::apply_event` (`src/acp/state.rs`), which
  **exists but runs only in tests** (`src/server/api/acp.rs` documents "the
  daemon keeps no live per-session `AcpState`").

So the control-state reduction (turn active, steerable, cancelling, compacting,
pending approvals/elicitations, usage, available commands, modes, plan) is
implemented three times and the two frontends drift. Adding a third frontend
means a fourth reducer.

## Goal

The daemon maintains the authoritative reduced SV control state per live session
and pushes it to clients, so a client renders control state instead of deriving
it. Clients still consume the raw event stream for the **transcript** (tool
cards, messages) in this tier; collapsing the transcript render model is a later
tier.

## Corrected finding (why this is bigger than it looks)

An earlier estimate assumed the server already computed `AcpState` at runtime. It
does not. Two consequences shape the design:

1. There is no live server-side reducer. We must run `AcpState::apply_event` in
   production, maintained by a single writer per session.
2. `turn_active`, `steering`, `cancelling`, `compacting` are **not fields on
   `AcpState`**. They are derived client-side (web `isTurnActive` etc.) and,
   independently, inside the TUI `AcpTranscript`. They must be added to
   `AcpState` and computed in `apply_event`. `AcpTranscript` is the reference
   implementation to port from (it already computes all four).

## Proposed design

### Reduced state

Extend `AcpState` (`src/acp/state.rs`, already `Serialize`) with the four turn
flags, computed in `apply_event`:

- `turn_active: bool` - a turn is in flight. Ported from `AcpTranscript`: set on
  the first `ToolCallStarted` / `Thinking` / agent-message of a turn, cleared on
  `Stopped` / terminal error. (The web's `pendingUserPromptSeq > lastStoppedSeq`
  scheme is a client-optimism detail; the server tracks the observed edges.)
- `steering: bool` - from the latest `PromptCapabilities.steering`.
- `cancelling: bool` - set on `CancelRequested`, cleared at turn end.
- `compacting: bool` - set on `ConversationCompactionStarted`, cleared on
  `Compacted` / turn end.

Everything else the clients need is already on `AcpState` (mode, current_plan,
todos, pending_approvals, pending_elicitations, usage, available_commands,
config_options, rate_limit, last_agent_switch, background_agents).

### Where the reduction runs (implemented: per-connection)

Each ACP WebSocket connection reduces its own event stream through
`AcpState::apply_event` and pushes the result (`src/server/acp_ws.rs` `handle`).
Reduction is per-connection but deterministic - the same reducer over the same
ordered stream - so every client converges on the same control state. This
avoids a central mutable `HashMap<SessionId, AcpState>`, a new actor task, and
any change to the broadcast channel type, and it makes the connect snapshot
fall out naturally (reduce over the replay drain, then send). The cost is
re-reducing per connection; ACP sessions have few concurrent WS clients, so this
is cheap. A central single-writer (reduce once at the broadcast choke point, fan
the reduced frame out) stays available as an optimization if connection counts
ever grow.

No new persistence: on daemon restart the connection rebuilds state by reducing
over `EventStore::replay_from` at connect (the on-connect drain), and the event
log remains the source of truth.

### Wire: a kind-tagged reduced-state frame

The WS already demultiplexes control frames by a top-level `kind`
(`heartbeat`, `lagged`), and the raw event frame has no `kind`
(`src/acp/protocol.rs` custom Serialize emits `{session_id, seq, event}`). So a
new `{ "kind": "reduced_state", "session_id", "seq", "state": <AcpState> }`
frame is **backward compatible**: the current web parser
(`useAcpSession.ts` `ws.onmessage`) silently ignores unknown kinds until taught.

Emission:

- After each event is applied, emit the updated reduced-state frame (coalescing
  is a later optimization; correctness first).
- On WS connect, after the replay drain, send the current reduced snapshot so a
  fresh client has authoritative control state without waiting for the next
  event (`acp_ws.rs` connect path; `drain_replay_into_socket`).

### Client consumption + the optimism decision

The server is authoritative for control state. The open decision (flagged when
this plan was proposed) is turn-active latency: the web bumps `pendingUserPromptSeq`
optimistically before the server echoes, so the composer flips to "working"
instantly.

Decision: **server-authoritative with a thin client optimistic overlay.** The
client keeps a local "I just sent a prompt" optimistic flag that OR's into the
rendered turn-active until the next reduced-state frame arrives, then defers to
the server value. This preserves instant feedback without a second source of
truth for the steady state. The web reducer's other derivations
(steering/cancelling/compacting/approvals) switch to reading the frame directly.

## Increments (each ships green)

1. **Server reducer + WS channel** (Rust, unit-testable end to end without a
   live agent): add the four flags to `AcpState::apply_event` with a table-driven
   test suite ported from `AcpTranscript`'s cases; maintain the per-session
   `AcpState` at the publish choke point; emit the `reduced_state` frame after
   each event and on connect. No client render changes; the web WS parser learns
   to accept and store the frame (asserted in Vitest) so the channel is exercised.
   **Shipped** (Tier 1.1).
2. **Web renders from the frame**: turn/steering/cancelling/compacting/approvals/
   usage/modes read the reduced state; keep the thin optimistic turn-active
   overlay. Delete the now-dead client derivations. Playwright + Vitest.
   **Not started.** `useAcpSession` still drops the frame explicitly and folds
   control state from raw events.
3. **TUI renders from the frame**: `AcpTranscript` drops its control-state
   derivation and reads the reduced state; the transcript-row building stays.
   **Shipped** (Tier 1.3), and by then the row building had already moved to the
   server too (Tier 4 below), so `AcpTranscript` reduces nothing at all: it holds
   the server's control state and the server's rows. Two gaps had to close first:
   `AcpState` no-op'd `ModesAvailable` / `CurrentModeChanged` (now reduced into
   `available_modes` / `current_mode_id`), and the "context lost, re-prime?"
   latch became a derivation over the transcript rows.

## Tier 4: the transcript follows the control state

Tier 1 left each client folding raw events into transcript rows, which was the
larger half of the duplication. That moved server-side too: `TranscriptModel`
(`src/acp/transcript.rs`) folds the ordered rows once, the WS ships them as
`transcript_snapshot` / `transcript_delta`, and `GET /acp/replay?view=rows`
serves them for history. The web migrated first (Tier 4 C1), then the native TUI
(Tier 4 D). Presentation stays client-side: markdown, tool cards, path
shortening and diff rendering all read the rows.

One behavior changed in the process. Approvals are control state, not rows, so
the native TUI stopped recording a resolved approval inline in the transcript;
the modal shelf shows it while pending and nothing persists after. The web never
had such a record, so the two clients now agree.

## Alternatives considered

- **On-demand `GET /acp/state` snapshot** (reduce over replay per request):
  cheapest, no writer, but pull-only - clients keep reducing the live stream, so
  it does not remove the duplication for the live view. Rejected as the primary
  mechanism; may still be useful as a cold-start hydrate.
- **Expand `SessionResponse`** with turn/pending counts: only serves the sidebar
  poll, not the live SV. A narrow badge win, not the goal.

## Test plan

- Rust unit: `apply_event` turn-flag table (ported from `AcpTranscript` tests);
  a publish-choke-point test that feeds a scripted event sequence and asserts the
  emitted reduced frames; a restart/replay-rebuild test.
- Web: Vitest on the WS parser accepting `reduced_state`; Vitest/Playwright that
  the composer reflects server turn-active (with the optimistic overlay) and that
  approvals/modes render from the frame.
- Live daemon e2e (extend `acp_focus_isolation_e2e.rs`): a scripted turn asserts
  the reduced frame drives the native view.
