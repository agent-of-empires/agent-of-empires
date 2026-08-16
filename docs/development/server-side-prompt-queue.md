# Design: server-side prompt queue (closed-app delivery + force-send)

Status: partially implemented. Opened 2026-08-14.

Implemented so far (backend + API client):

- Durable per-session queue on the `Instance` (`queued_prompts` +
  `queued_prompt_next_seq`, `QueuedPromptEntry`), persisted like
  `pending_initial_turn`. Chosen over a dedicated SQLite table for consistency
  with the existing pending-turn pattern and free WS/multi-device sync via the
  session list (revises Q1).
- `SessionService` enqueue / edit / remove / clear / snapshot and
  `drain_queued_prompts_once`; a reconciler pass drains the head batch at
  turn-end (`Status::Idle`, live worker) with the `/clear`-boundary split
  matching the client. `queued_prompts` is surfaced on `SessionResponse`.
- HTTP: `POST/GET/DELETE /api/sessions/{id}/queue`,
  `PATCH/DELETE /api/sessions/{id}/queue/{promptId}`, CityHall-classified like
  `acp/prompt`. Web API client wrappers + Vitest.

Still to do, and each item is a real blocker with a design fork or a
verification requirement, not a mechanical increment (investigated 2026-08-16,
code citations below):

1. **Attachments on a queued prompt cannot reuse `acp_attachments` as-is.**
   That store keys blobs by the `seq` of the `UserPromptSent` event they ride
   with, and the retention prune drops any blob whose owning event no longer
   exists (`src/events/mod.rs:349`, `DELETE ... WHERE seq <= cutoff AND seq NOT
   IN (SELECT seq FROM events)`). A queued-but-unsent prompt has no
   `UserPromptSent` seq, so its bytes would either be pruned early or need a
   synthetic-seq hack that fights the retention invariant. `pending_initial_turn`
   avoids this only because it replays refs whose bytes were already stored under
   a prior real prompt's seq. Doing queued attachments right needs a dedicated
   blob store keyed by `(session_id, prompt_id, attachment_id)` with its own
   lifecycle (a new table + migration), plus an upload endpoint and a per-session
   byte cap. This is Q1/Q5, still open.
2. **The client rewire regresses dormant-worker delivery unless wake-on-drain
   lands first.** The server drain pass skips any session whose worker is not
   live (`src/server/acp_reconciler.rs:1589`, `if !is_running { continue }`). The
   client drain, by contrast, wakes an idle-auto-stopped worker by letting the
   POST itself fire the resume (`web/src/hooks/useAcpSession.ts` `workerIdleStopped`
   exception). Drop the client drain before the server can wake a dormant worker
   and a prompt queued against a dormant session never delivers.
3. **Wake-on-drain hits the #3172 re-entrant-spawn deadlock.** `send_turn`
   already wakes a dormant/dead worker (`src/server/session_service.rs:453`,
   `needs_resume = woke_idle_dormant || !is_running`), so the fix looks like
   "let the drain run for dormant sessions too." But `drain_queued_prompts_once`
   holds the session `instance_lock` across `send_turn`, and its own doc says
   that is safe *only because callers pre-gate on `is_running`* so no spawn
   happens under the lock. `trigger_resume_background`'s detached spawn takes that
   same `instance_lock` (`src/server/acp_reconciler.rs:1782`, "Callers MUST NOT
   hold the session's `instance_lock`"), so waking under the drain lock
   deadlocks. Safe wake-on-drain needs the drain restructured to not hold the
   lock across a waking `send_turn`, validated by a live-daemon e2e.

Also still to do (straightforward once the above are settled): the `send-now`
endpoint (G3), the one-time localStorage migration (Q2), and the live-daemon
e2e for the full closed-app round-trip.

**Net:** the backend store + endpoints + live-worker drain are done and green,
but they are inert until the client POSTs into the queue, and the client rewire
that would do that (a) cannot be verified from CI/sandbox (needs a live daemon +
real agent + the iOS PWA) and (b) regresses dormant delivery until wake-on-drain
is built, which itself needs the deadlock-careful refactor above. The rewire and
wake-on-drain should land together, verified on-device, not as blind increments
on top of a working PR.

## Problem

Two structured-view (SV) asks share one root cause:

1. A follow-up prompt the user queues while the agent is busy is **not delivered
   if the PWA is closed** on the client. On an installed iOS PWA the user
   queues a message, locks the phone, and the message never sends.
2. There is **no reliable "force send now"** for a queued prompt when a turn is
   active and the agent is not steerable. The shipped per-row "Send now" (see
   below) only fires when an immediate send can already reach the agent.

Both stem from the queue being **client-only**.

## Current architecture (as of this doc)

- The SV prompt queue is React reducer state (`state.queuedPrompts`) mirrored to
  `localStorage` under `aoe:acp-state:v1:<id>` (`useAcpSession.ts`). Queued rows
  with attachments are dropped from persistence; attachments live in memory only.
- Delivery is a **client drain effect**: when the turn ends and the socket is
  open and the worker is running, the browser combines the leading batch and
  POSTs `/api/sessions/{id}/acp/prompt`. Off-screen sessions drain via up to 3
  headless `AcpBackgroundDrainers`, still only while a tab is open.
- The daemon has **no user-prompt queue**. `pending_initial_turn` is the
  rate-limit resume continuation, not the user's mid-turn queue.
- The service worker (`web/public/sw.js`) is push-only: no `fetch` handler, no
  Background Sync. iOS Safari does not support Background Sync, so a
  service-worker retry path cannot deliver on the primary target platform.

Net: close the app and the queue (state + drain) dies with the page.

## What already shipped (partial force-send)

A per-row "Send now" affordance exists in `QueuedPromptsStrip`, backed by
`useAcpSession.sendQueuedNow(prompt)`. When the agent is free (idle, steerable
mid-turn, or dormant, in which case the POST wakes it) it sends that single row
immediately under the per-session drain lock. When a live non-steerable turn is
blocking it, "Send now" **interrupts**: it cancels the current turn, and the
queued prompts drain via the normal turn-end path (no POST during the cancel,
which the daemon would treat as a wedge). The button's tooltip warns when it
will interrupt. It still does nothing when the app is closed, which is the gap
this design covers.

## What already works with the client closed (audit)

The requirement is "an in-progress SV session keeps working when the phone is
closed." Most of it already holds, because the agent does not run on the phone:

- **The agent worker runs on the daemon host**, as a detached `aoe __acp-runner`
  process registered in `src/process/worker_registry.rs`. It outlives client
  disconnects and even `aoe serve --stop`. Closing the phone does not stop a
  running turn.
- **The transcript is durably persisted** in the daemon's event store
  (`src/acp/event_store.rs`, SQLite). A turn runs to completion and its events
  are stored whether or not a client is connected; a reconnecting client
  replays them. So "the agent stops when I lock my phone" is not the case
  today.

What genuinely still needs *a* client open (not necessarily the phone):

1. **Delivering a queued follow-up prompt.** The queue and its drain are
   client-only, so a message queued behind a busy turn is stranded when the app
   closes. This is the gap this design closes (G1/G2).
2. **Answering a permission/approval prompt** when the session is not in
   yolo/bypass mode. Approvals sit in `pending_approvals` until a client
   resolves them (`src/acp/approvals.rs`); the turn blocks meanwhile. This is
   inherently interactive, not a queue problem: it is already answerable from
   any signed-in device, and push notifications alert the user. For unattended
   phone-closed operation the session should run in yolo/bypass mode, or the
   user answers from another device. Out of scope for the queue work, but worth
   stating so "phone closed" is understood end to end.

So after this design lands, the only remaining phone-closed caveat is approvals
on a non-yolo session, which is a deliberate interaction, not a bug. (The daemon
host running `aoe serve` must of course still be up; "phone closed" refers to
the mobile client, not the server.)

## Goals

- G1: A queued prompt is delivered even if the client PWA is closed, as soon as
  the agent is free, with no tab open.
- G2: Queued prompts (and their attachments) survive a client reload / device
  handoff.
- G3: "Force send now" works even during an active non-steering turn (cancel the
  turn, then send), as an explicit user action.
- G4: No duplicate delivery when both a live client and the server could drain.

## Proposed design

Move the queue's **source of truth to the daemon**, per session, and let the
server own draining. The client becomes a view/editor of the server queue plus a
fast optimistic path.

### Data model

A durable per-session queue in the daemon (alongside the session store or in the
event-log DB):

```
queued_prompt(
  session_id      TEXT,
  id              TEXT,     -- client-minted, stable across edits (existing QueuedPrompt.id)
  seq             INTEGER,  -- server order; assigned on enqueue
  text            TEXT,
  attachments     BLOB,     -- serialized PromptAttachmentInput[]; see attachment note
  created_at      TEXT,
  origin_device   TEXT,     -- which device enqueued (for provenance / multi-device)
  PRIMARY KEY (session_id, id)
)
```

Ordering is by `seq`. The `/clear`-boundary batching rule stays server-side so a
queued clear-command still fires as its own POST.

### Lifecycle

- **Enqueue**: client POSTs the prompt to a new `POST /api/sessions/{id}/queue`
  the moment it decides to queue (today it only writes localStorage). The row is
  persisted server-side immediately, independent of turn/socket state. Response
  echoes the assigned `seq`.
- **Drain**: the daemon drains the head of the queue into the worker when the
  turn ends (it already observes turn end for status), applying the same
  clear-boundary split. This runs in the reconciler / supervisor, not the
  client, so it fires with no tab open. Idle-auto-stopped workers are woken the
  same way a direct prompt wakes them.
- **Edit / remove / reorder / clear**: `PATCH`/`DELETE` on
  `/api/sessions/{id}/queue/{promptId}` and a clear-all. These mutate the
  server queue; the client reflects via the existing WS event stream.
- **Force-send now (G3)**: `POST /api/sessions/{id}/queue/{promptId}/send-now`.
  Server behavior:
  - idle: dequeue + prompt immediately.
  - active + steerable: inject via `_session/steering`.
  - active + non-steerable: cancel the current turn, then prompt (explicit,
    confirmed on the client since it interrupts the running turn).

### Client changes

- `sendPrompt`'s enqueue branch calls the queue POST instead of only
  dispatching `enqueue_prompt` locally. Keep an optimistic local row for
  instant UI, reconciled against the server queue that arrives over the WS.
- Drain effect and `AcpBackgroundDrainers` are **removed** (server drains now).
  `sendQueuedNow` becomes a thin call to `send-now`.
- The queue is now surfaced by the server, so it is visible across the user's
  devices (a queued prompt on the phone shows on the laptop). Decide whether
  that is desired (see Q3).

### Attachments (G2)

Attachments currently never persist (quota) and drop on reload. Server-side
storage removes the browser-quota constraint: store attachment bytes in the
session's artifact dir keyed by prompt id, referenced from the queue row. Cap
total queued attachment bytes per session; reject over-cap enqueues with a clear
error rather than silently dropping.

### Dedup / single-drain (G4)

The daemon is the only drainer, so the client-vs-client and tab-vs-tab races the
current module-scoped drain lock guards go away. The remaining race is
optimistic-echo vs server-confirmed row: reuse the existing optimistic-id
rollback pattern (`rollback_optimistic_prompt`) keyed on the queue row id.

## Alternatives considered

- **Background Sync in the service worker**: retries a POST after the page
  closes. Rejected as primary: unsupported on iOS Safari (the main PWA target),
  and it only retries a send the client already committed to, not a
  wait-for-idle drain.
- **Keep client queue, add `keepalive`/`sendBeacon` on unload**: delivers an
  in-flight send during teardown, but cannot wait for the agent to be free, so
  it does not solve a message queued behind a long-running turn.

## Open questions

- Q1: Storage home for the queue: the session store JSON vs the event-log
  SQLite. The event log already has per-session durable storage and retention;
  leaning that way.
- Q2: Migration for prompts currently sitting in `localStorage`. On first load
  after upgrade, flush any local `queuedPrompts` to the server queue, then drop
  the local copy. One-time, best-effort.
- Q3: Multi-device visibility. A server queue is naturally cross-device; the
  current copy is per-browser. Confirm the product wants the queue shared across
  a user's devices (it likely does, and it matches how sessions already sync).
- Q4: CityHall / read-only interactions and the operator agent allowlist: the
  drain runs server-side, so it must re-check policy at drain time, not just at
  enqueue.
- Q5: Retention: how long a queued-but-undrained prompt lives before it expires
  (e.g. a session that never becomes idle again).

## Test plan (when built)

- Rust: enqueue/drain/edit/remove/send-now unit + a live-daemon e2e that queues
  behind an active turn, drops the client, and asserts delivery on turn end.
- Web: live Playwright for the queue round-trip and cross-device visibility;
  Vitest for the optimistic-reconcile reducer path. Update
  `web/tests/coverage-matrix.json`.
