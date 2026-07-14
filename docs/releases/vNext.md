# Release notes (next version)

Staging notes for behavior changes worth calling out to users in the next
release. The generated [`CHANGELOG.md`](../../CHANGELOG.md) already lists every
merged fix and feature by PR title; this page is for the subtle,
user-observable behavior contracts that a one-line changelog entry cannot
convey on its own.

## Session activity column

The activity-age fix in
[#2697](https://github.com/agent-of-empires/agent-of-empires/pull/2697),
hardened by
[#2729](https://github.com/agent-of-empires/agent-of-empires/pull/2729) (see
also [#2690](https://github.com/agent-of-empires/agent-of-empires/pull/2690)),
stopped the activity column from resetting to `<1m` for every session on a TUI
relaunch or a daemon restart. Sessions idle for hours now keep showing their
real age instead of looking freshly touched.

That fix comes with two behavior contracts that are intentional, not bugs:

1. **Fresh, never-touched Running sessions show a blank activity column instead
   of `<1m`.** A session's activity age now stays empty until the first real
   user touch, rather than being stamped with the moment it was first polled.
   Previously a brand-new session was given a fabricated "just now" timestamp;
   that timestamp is what used to reset on every reload. A blank column for a
   session you have not interacted with yet is expected.

2. **Structured (ACP) sessions may briefly show a stale idle time after a
   daemon restart.** For structured sessions the ACP event stream, not the
   passive poller, owns the idle timestamp. Immediately after a daemon restart
   the activity column (in both the web dashboard and the TUI) can display the
   last durable value until the next ACP event re-emits the current state, at
   which point it corrects itself. This stale window is bounded by the ACP
   reconnect, not indefinite.

For the technical rationale and the writer-authority rules behind both points,
see the
[Passive-status pipeline](../development/internals/sessions.md#passive-status-pipeline)
section of the session internals reference.
