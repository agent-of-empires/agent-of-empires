# Server-owned prompt queue

The daemon owns each structured-view session's queued prompts. This lets a
follow-up survive browser closure, reload, and device handoff, and prevents
multiple clients from racing to drain the same queue.

## Storage and API

`Instance.queued_prompts` is persisted with the session and exposed on
`SessionResponse`. The queue API supports listing, enqueueing, editing,
removing, and clearing entries:

```text
GET, POST     /api/sessions/{id}/queue
PATCH, DELETE /api/sessions/{id}/queue/{prompt_id}
DELETE        /api/sessions/{id}/queue
```

Prompt ids are client-minted and idempotent. Sequence numbers define server
order. The web client may render an optimistic row, but reconciles it against
the server snapshot.

Queued attachment bytes live in the event store's `pending_attachments` table,
keyed by session and prompt id. They cannot use normal ACP attachment storage,
which is keyed by the sequence of an event that a queued prompt does not have
yet. A per-session size cap and hourly expiry bound pending storage. After
delivery, attachments are recorded under the real prompt event and the pending
copy is removed.

## Delivery

The reconciler is the only queue drainer. At turn end it sends the leading
batch, preserving `/clear` as a batch boundary, and retires only entries that
were delivered. Prompts added during a send remain queued.

An idle-auto-stopped session is woken in two phases to avoid taking the session
lock recursively: the reconciler clears its dormant marker, the normal resume
pass starts the worker, and the next tick drains the queue. The idle reaper does
not stop a session with queued prompts. Explicitly stopped sessions are not
woken.

The current "Send now" client action sends immediately when possible. During a
non-steerable turn it cancels first and lets the normal server drain deliver the
row. A dedicated force-send endpoint is not implemented.
