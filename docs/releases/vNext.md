# vNext

## Passive-status pipeline behavior changes (#2697)

Two user-observable behavior changes land from the passive-status pipeline
hardening in [#2729](https://github.com/agent-of-empires/agent-of-empires/pull/2729).

### Fresh sessions no longer show `<1m`

A session that was created but never touched (no explicit user action) now shows
a blank activity column instead of `<1m`. Previously, the first poll fabricated
a `last_accessed_at` timestamp, making it appear as if the session had been
accessed recently. The activity column now stays empty until an explicit user
action occurs.

### Structured sessions may show stale `idle_entered_at` after daemon restart

For structured (ACP) sessions, the `idle_entered_at` field displays the last
durable value after a daemon restart, until an ACP event handler re-emits fresh
state. During the ACP-reconnect window, the web dashboard and TUI activity
column may show a value from before the restart. This is the intended
contract; see the [Passive-status pipeline](../development/internals/sessions.md#passive-status-pipeline) section for the authority rules.
