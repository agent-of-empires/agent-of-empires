# Persistence & Recovery

Cockpit workers and transcripts are designed to outlive a daemon
restart, a closed laptop, or a reconnect. This page covers what
survives, what gets cleaned up on delete, and how conversation context
is rehydrated. For the failure modes and their fixes, see
[Cockpit Troubleshooting](troubleshooting.md).

## Worker persistence across `aoe serve` restart

> **Behavior change (cockpit-only).** Prior releases tore down every
> cockpit ACP worker on `aoe serve --stop` (and any other daemon
> shutdown). As of this release, the daemon detaches without killing
> the runner: in-flight turns survive `aoe serve --stop`, `aoe update`,
> daemon crashes, and host suspend/wake. To actually terminate
> workers, use `aoe cockpit stop <session>` or `aoe cockpit stop --all`
> (graceful), or `aoe cockpit kill <session>` (force). tmux-based
> (non-cockpit) sessions are unaffected.

Cockpit workers run as detached `aoe __cockpit-runner` processes that
outlive the daemon. `aoe serve --stop` drops the daemon's connection
to each worker but does **not** terminate the runner: the agent
keeps running, in-flight turns continue, and a subsequent `aoe serve`
reattaches via the worker's unix socket.

Each runner registers itself at
`<app_dir>/cockpit-workers/<session_id>.json` with its PID, socket
path, and cached ACP session id. The same directory holds the
per-session `.sock` (unix socket) and `.log` (runner stderr drain)
files. `aoe cockpit ps` lists running workers.

Practical implications:

- `aoe update` followed by `aoe serve --stop` + `aoe serve` keeps
  every cockpit agent's in-flight turn alive.
- Closing the laptop or restarting the host with `aoe serve` running:
  the daemon dies on suspend, but the runner continues. On wake the
  next `aoe serve` reattaches.
- To actually terminate a worker, run `aoe cockpit stop <session>` or
  `aoe cockpit stop --all`. To force-kill, `aoe cockpit kill <session>`.
- **No orphaned agents on teardown.** Every termination path (idle
  auto-stop, `aoe cockpit stop|kill|restart`, snooze/archive, daemon
  `shutdown_all`, and a fresh spawn that supersedes a stale runner)
  signals the runner's whole process group, so the agent subprocess and
  its own children (the node ACP wrapper and the SDK child it execs) die
  with the runner instead of reparenting to PID 1. Earlier releases
  signalled only the runner pid, so superseded or torn-down runners
  leaked `node` / `claude` processes that accumulated across daemon
  restarts and e2e runs (#1689). A runner that is hard-killed with
  `SIGKILL` (which cannot run its own cleanup) can still leak; prefer
  the verbs above over `kill -9`.
- During the detach window (between `aoe serve --stop` and the next
  `aoe serve`), the runner buffers up to 256 agent → daemon
  notification lines so per-stream chunks emitted while the daemon was
  down get replayed on reattach. Permission requests issued while
  detached block the agent's turn until reattach.
- **Mid-turn reattach.** When the daemon comes back up against a
  session that was actively streaming a prompt, the new daemon resumes
  the existing ACP session id directly (no `session/new` or
  `session/load` is sent; the agent process never died, so its in-
  memory session is still addressable). The agent's eventual response
  to the orphaned in-flight `session/prompt` is dropped silently by
  the transport because its request id was issued by the previous
  daemon; to keep the UI from staying stuck on "thinking" forever,
  the daemon arms a resume-idle watchdog. It disarms on the first
  inbound notification the runner forwards after reattach: once any
  event arrives the turn is observable, and later gaps are normal
  mid-turn silence (Task subagents, slow Bash, reasoning between tool
  calls), not an orphan. It synthesizes a
  `Stopped { reason: "reattach_idle" }` event only when the runner
  forwards no notification at all within 30s of reattach. The narrow
  residual, a turn that completes after reattach whose lost response
  leaves a stale spinner, is rare and clears via Force end turn or the
  next prompt. Sessions that the runner cannot reattach to (dead PID,
  missing socket, etc.) fall through to a fresh spawn; if the on-disk
  event log shows that fresh spawn's session was mid-prompt at the
  moment the daemon died, the reconciler publishes a
  `Stopped { reason: "orphaned_at_restart" }` event before the new
  agent starts so the UI clears immediately. The same path covers the
  `main`-branch case where there is no runner at all and every cockpit
  session takes the fresh-spawn branch on restart.

## Session deletion

`session/delete` fires only on **permanent** removal: deleting a
cockpit session, or disabling cockpit mode on a session (which discards
the conversation). Reversible teardown, `aoe cockpit stop`, snooze,
archive, and idle auto-stop, deliberately does NOT fire it, so the
agent's transcript stays on disk and the next respawn resumes via
`session/load` instead of resetting the conversation. Firing it on
those paths previously reset context on every snooze / archive /
idle-stop. See
[#1710](https://github.com/agent-of-empires/agent-of-empires/issues/1710).

When you permanently delete a cockpit session, aoe opportunistically
fires the experimental `session/delete` ACP request against the live
worker (bounded by a 2-second timeout) whenever a stored ACP session id
exists, and then proceeds with the existing kill path
(`session/cancel` for in-flight prompts, then SIGTERM on the runner,
then on-disk cleanup). aoe does not inspect the initialize-time
capability flag: adapters that implement the method use the request
to release adapter-side persisted state, for example
`claude-agent-acp 0.37.0+` clearing the on-disk Claude session
record so a recreated session id does not inherit prior transcripts.
Adapters that do not implement the method (`aoe-agent`, `codex`,
`opencode`, older `claude-agent-acp`) reply with JSON-RPC
`-32601 method_not_found`; aoe logs that at `debug` and proceeds to
SIGTERM. A wedged adapter is bounded by the 2-second timeout, so
delete never stalls the UI. Outcomes are logged under
`target = "cockpit.acp"` in `debug.log` with an `adapter=<agent_key>`
field so operators can correlate behavior across adapters: success
and `-32601` land at `debug`, timeout and other errors at `warn`. See
[#1404](https://github.com/agent-of-empires/agent-of-empires/issues/1404).

## Conversation persistence

Cockpit transcripts survive page reloads, session switches, and
`aoe serve --stop`/restart cycles. For agents that support session
restoration (Claude today), the model itself also retains conversation
context across restarts; so a follow-up like "what did we just
decide?" still works after a daemon restart.

The web dashboard mirrors each session's reduced state into
`localStorage` under the `aoe:cockpit-state:v1:<session_id>` key so a
full page reload (mobile OS evicting the tab, Cloudflare tunnel
re-auth, PWA cold start) hydrates the chat surface instantly from the
last-known state and only fetches the seq-delta from the server. Entries
expire after seven days; an oversized session that exceeds the
per-origin quota falls back to the full server replay path without
warning. `clearCockpitCache` and the session-delete handler drop the
matching entry so a freshly-recreated session id doesn't briefly show
the prior transcript.

If context restoration fails (e.g., the agent's stored session is no
longer available), cockpit falls back to a fresh session and renders
an amber "Conversation context reset" callout in the transcript so
you know prior turns are no longer in the model's context window.

After that callout, an inline "Resume with prior context" banner
appears above the composer. Clicking it calls
`GET /api/sessions/{id}/cockpit/context-primer?before_seq=<reset-seq>`,
which walks the SQLite event log and returns a compact markdown
recap of the last ~20 turns (capped at ~24k characters, bulky tool
inputs/outputs elided, tool calls collapsed to one-liners). The
primer is pre-filled into the composer so you can review, trim, or
extend it before sending; nothing is sent silently. The banner is
one-shot per reset: dismiss it or submit any prompt and it stays
gone until the next `session/load` failure. See #1004.

The bundled `aoe-agent` doesn't yet support context restoration; its
transcript still replays from disk, but the model starts fresh on each
spawn. Tracked in
[#1005](https://github.com/agent-of-empires/agent-of-empires/issues/1005).
