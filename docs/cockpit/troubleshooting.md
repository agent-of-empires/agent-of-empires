# Cockpit Troubleshooting

The security model cockpit enforces, followed by a field guide to every
failure mode and how to recover. For the day-to-day interface, see
[Cockpit Interface](interface.md); for what survives a restart, see
[Persistence & recovery](persistence.md).

## Security

- File system access uses ACP's `fs/read_text_file` and
  `fs/write_text_file`. Agents do **not** access the disk directly; aoe
  reads/writes on their behalf and enforces sandbox roots (the
  session's worktree + any explicit `--repo` paths).
- Terminal commands use ACP's `terminal/*`. The shell command runs in
  aoe's process, in the session's worktree (or inside the sandbox
  container when sandbox is enabled, via `docker exec`).
- Approval nonces are server-generated and single-use. A compromised
  agent process cannot synthesise approvals; aoe never reveals the
  nonce to the agent.
- Auth tokens (`AOE_TOKEN`) are explicitly *not* forwarded to the
  agent subprocess.

### Sandbox containers

Cockpit sessions honor the wizard's **Run in a safe container** toggle.
When enabled, the ACP agent runs inside the same `aoe-sandbox-<id>`
Docker container the tmux substrate uses. The daemon stays on the host
and wraps the agent argv in `docker exec`, so the agent never sees host
paths. `fs/*` requests are translated from container paths (e.g.
`/workspace/proj/foo.rs`) back to host paths before the inside-roots
check; `terminal/*` commands run via `docker exec`, so a `pwd` from the
agent returns the container's working directory, not the host's.

The unix socket between the daemon and the per-session runner stays on
the host. The runner proxies the agent's stdio across the container
boundary, so there is no bind-mount of the daemon's socket into the
container. That path is reserved for a future agent that natively
speaks the socket transport.

The published `aoe-sandbox` image bundles the ACP adapters cockpit
sessions need (`claude-agent-acp`, `codex-acp`, `pi-acp`) alongside the
underlying CLIs whose binaries already provide ACP themselves (`opencode
acp`, `gemini --acp`, `vibe-acp`). Custom sandbox images must include
the same adapters or the `docker exec` invocation will fail with exit
status 127 and the ACP handshake will time out after 30s.

Known limitations:

- `fs/*` path translation only covers the workspace mount(s) the
  container was built with. Agent-config mounts (`/root/.claude`),
  bind-mounted credentials, and user-configured `extra_volumes` are
  not in the path map. In practice the inside-roots check (worktree-
  only) already rejects those paths, so the safety property holds;
  the failure mode is just a generic "outside session roots" error.

## Troubleshooting

### `aoe cockpit doctor` says Node is missing

Install Node.js 20 or newer:

- macOS: `brew install node`
- Linux: `apt install nodejs` or `nvm install 20`
- Windows: download from <https://nodejs.org/>

Then re-run `aoe cockpit doctor` to verify. If you have Node installed
in a non-standard location, set `AOE_COCKPIT_NODE=/path/to/node` or
configure `cockpit.node_path` in `config.toml`.

### `aoe cockpit doctor` says aoe-agent is missing

`aoe-agent` ships with the aoe binary. If the doctor reports it
missing, your install is incomplete. Reinstall aoe via your package
manager (e.g., `brew reinstall aoe`).

### `aoe cockpit doctor` says claude-code adapter is missing

Install the official adapter once. aoe requires v0.39.0 or newer; the
cockpit refuses to enter a session with an older adapter and surfaces a
dedicated remediation screen with the exact install command:

```bash
npm install -g @agentclientprotocol/claude-agent-acp@latest
```

Then run `claude login` if you haven't already.

The minimum version is enforced at the ACP `initialize` handshake; the
check reads `agent_info.version` from the adapter's initialize response
and rejects anything below 0.39.0 with a structured `StartupError`
event. Newer versions are accepted. The floor tracks the newest
behavior aoe depends on; the earliest hard requirements landed in
v0.37.0:

- `memory_recall` tool calls (upstream
  agentclientprotocol/claude-agent-acp#703), so session-start memory
  loads render in the cockpit instead of disappearing into a dropped
  SDK event.
- Native `stopReason: "cancelled"` (upstream
  agentclientprotocol/claude-agent-acp#694), so cancel acknowledgement
  surfaces as a distinct turn outcome rather than collapsing into
  `end_turn`.

If you have an older version pinned by an internal mirror, set up the
mirror to ship 0.39.0 or override the global install with
`npm install -g @agentclientprotocol/claude-agent-acp@latest` before
starting `aoe serve`.

### "Failed to start cockpit agent" while the adapter is installed

`aoe serve` captures the launching shell's PATH at startup and keeps it for the daemon's lifetime. If the adapter is installed under a node-version-manager dir (`~/.nvm/versions/node/v<ver>/bin`, `~/.fnm/node-versions/.../installation/bin`, mise/asdf equivalents) and the active node version on the daemon's PATH doesn't match, the spawn fails with `agent spawn failed: No such file or directory`.

The spawn path scans common node-manager bin dirs (nvm, fnm, mise, asdf, Volta, `~/.npm-global/bin`, `~/.local/bin`, `/usr/local/bin`, `/opt/homebrew/bin`) per spawn, so a `nvm use <other-version>` after the daemon started is picked up on the next worker respawn without a daemon restart. If the binary lives somewhere else, either restart `aoe serve` from a shell where `which claude-agent-acp` resolves, or symlink it into one of those dirs.

### "Project path no longer exists" banner

The session's working directory was renamed, moved, or deleted out from under `aoe serve`. The most common trigger is a `git worktree move` or a manual `mv` on a worktree dir the session was bound to. The cockpit pre-flights `project_path` before spawning, so this fails fast with a typed banner instead of a generic ENOENT (which is indistinguishable on POSIX from "the adapter is missing"). Two ways to recover:

1. **Restore the directory at the path the banner shows** (e.g. `git worktree move <new> <old>`, or recreate the dir), then click **Retry** on the banner. Cockpit transcript continuity is preserved.
2. **Stop `aoe serve`**, edit `project_path` for this session in `~/.agent-of-empires/profiles/<profile>/sessions.json` to point at the new location, then start `aoe serve` again. If the worktree's branch was also renamed, update `worktree_info.branch` in the same file. Cockpit history + `cockpit_acp_session_id` are preserved; the conversation resumes against the new path.

Reinstalling the adapter does not help here; the adapter is fine, the cwd is gone.

### Agent stopped responding to cancel

If the agent ignores `session/cancel` mid-tool-call (most commonly a `block: true` TaskOutput against a wedged background shell), aoe escalates after a ~10s grace window: the daemon ends the ACP connection, SIGTERMs the wedged `aoe __cockpit-runner` subprocess, and the supervisor respawns a fresh worker via `session/load` so the transcript continues uninterrupted. The cockpit view shows "Agent stopped responding to cancel. Restarting worker; your transcript will be preserved" while the respawn is in flight; the banner clears automatically once the new worker comes online.

Follow-up prompts the daemon refused while the original turn was still in flight no longer vanish silently. The composer shows them as amber "Rejected" pills with a Retry button; clicking Retry re-dispatches the prompt through the normal send path against the freshly-respawned worker.

### Tool card stuck "running" after a stop

Stopping the agent while a tool call is mid-execution settles that tool's card to a distinct terminal **stopped** state: the elapsed-time timer freezes and the badge leaves the orange "running" state for a muted "stopped". This is intentional. The adapter resolves a cancelled prompt without sending a per-tool completion, so the cockpit closes any still-open tool calls itself when the turn ends. "stopped" is neither "done" (no success was reported) nor "failed" (no error was reported); the tool's real outcome was never reported. The same applies on reload (the state is reconstructed from the persisted turn-end event) and when the backend switches agents mid-turn.

### Rate-limit recovery

When the active ACP backend reports `errorKind: "rate_limit"` on `session/prompt` (Claude's adapter does this when the Anthropic account is over its limit), aoe treats this as a non-crash terminal state rather than as a worker crash:

- The connection task emits a typed `RateLimit` event (which the dashboard banner reads to show the reset time) and a `Stopped { reason: "rate_limited" }` lifecycle event, then exits cleanly.
- The supervisor drops the worker handle and does NOT respawn. Earlier behaviour respawned the runner inside the restart budget, then immediately hit the same limit on the next `session/prompt` and burned the budget. The session now sits parked until the user explicitly retries or hands off.
- `aoe serve` restart while a session is parked respects the `Stopped { reason: "rate_limited" }` signal in the on-disk event log and does NOT auto-resume the worker; otherwise daemon restart at minute 30 of a 90-minute window would undo the fix.

The rate-limit banner offers a primary "Continue in another agent" CTA. Clicking it opens a modal that lists the cockpit ACP registry (claude / codex / opencode / gemini / vibe / pi / aoe-agent by default, plus anything you've added via the settings TUI) and preselects `codex` when installed. Picking a target calls `POST /api/sessions/{id}/cockpit/switch-agent`, which:

1. Stops the current worker and waits for the runner subprocess to release its socket.
2. Spawns the target agent. On failure, the instance is left untouched.
3. Persists `cockpit_agent = <target>` and clears `cockpit_acp_session_id` (the old session id belongs to a different vendor and would be rejected by the new adapter).
4. Emits an `AgentSwitched { from, to, reason }` event so reducers drop transient state tied to the prior backend (rate-limit banner, in-flight tool, usage, mode pills, available commands) and the transcript shows a divider.

After the switch, the modal fetches the context primer and pre-fills the composer with a framed recap of the prior conversation. If the user's last prompt is what triggered the rate-limit (it was published to the event log before the adapter rejected it), the primer endpoint surfaces it separately as `unprocessed_prompt`; the modal drops it into the composer as the user's pending request so they don't have to retype it. The composer is NOT auto-sent; review and submit manually.

### Native binary launch failure

When the cockpit banner shows an error of the form

```text
Claude Code native binary at /usr/lib/node_modules/.../claude exists but failed to launch.
```

the adapter found its bundled Claude Code native sub-binary on disk
but `execve` was rejected by the kernel. Reinstalling
`claude-agent-acp` does not help; the binary is already there.

The common causes:

1. **Architecture mismatch.** The binary's filename ends in a target
   triple (`...-linux-arm64/claude`, `...-linux-x64/claude`, etc.).
   If the host or sandbox container reports a different arch via
   `uname -m`, the loader refuses the binary. Most often surfaces
   inside a sandboxed cockpit session where the container image's
   default arch differs from the host (e.g. an `arm64` host pulling
   an `amd64` image without `--platform`).
2. **Missing dynamic loader or old glibc.** Slim base images
   sometimes ship without `/lib64/ld-linux-x86-64.so.2` or with a
   glibc too old for the binary. `ldd <binary>` from inside the
   container reports the gap.
3. **Bind-mounted `node_modules` across arch.** If the host's npm
   prefix is bind-mounted into the container (so the container reuses
   the host install), an `arm64` host binary cannot launch in an
   `amd64` container and vice versa.

Use **Open agent log** on the red startup banner to see the verbatim
adapter error from the dashboard, or run `aoe cockpit logs --session
<id>` from a host terminal. To inspect the binary itself:

```sh
docker exec <container> file /usr/lib/node_modules/@agentclientprotocol/claude-agent-acp/node_modules/@anthropic-ai/claude-agent-sdk-*/claude
docker exec <container> uname -m
```

If the file's arch line does not match `uname -m`, the fix is either
re-pull the image with `--platform linux/<host-arch>` or install
`claude-agent-acp` inside the container (rather than bind-mounting
from the host).

### Cockpit feels "stuck" with no events

- Check `aoe cockpit logs --session <id>` for the runner stderr drain;
  the dashboard exposes the same content via the **Open agent log**
  affordance on the red startup-error banner.
- Check the dashboard's connection chrome at the top of the cockpit
  view; it shows reconnect status if the WebSocket is degraded.
- The supervisor watchdog respawns the agent up to 3 times in 60s
  after a crash; if all three burn, the cockpit shows a red
  "session parked" banner. Refresh the page to retry from scratch.
- On reconnect the client calls
  `GET /api/sessions/{id}/cockpit/replay?since={lastSeq}` to recover
  any frames it missed during a brief network blip. If the buffer no
  longer holds events that far back, you'll see a `History
  truncated` notice and reloading is the cleanest way to resync.

### Editing settings asks for the passphrase again

When passphrase login is configured, the daily-use cockpit flows
(sending prompts, cancelling turns, resolving approvals, switching
mode, restarting workers, attaching terminals) do NOT prompt for the
passphrase again. Your session cookie plus the device-binding
secret are sufficient, the same way an SSH session stays open after
the initial authentication. See #1137.

Editing the persisted config IS gated. Saving the global settings
panel, creating / deleting / renaming a profile, editing a profile's
settings, or changing the default profile requires that your login
session has been "elevated" within the last 15 minutes via `POST
/api/login/elevate`. The first such action after a fresh page load
surfaces an inline passphrase prompt; subsequent edits inside the
same 15-minute window go through without re-prompting. The narrow
scope catches the persisted-tamper attack (an attacker with stolen
session + binding plants a malicious Docker image, worktree
template, or profile, then waits for the owner to spawn a session
that runs it) without putting friction on the conversation surface.

### WebSocket auto-reconnect and keepalive

Mobile browsers and Cloudflare tunnels both close idle WebSocket
connections aggressively (Chrome / Safari at ~30 to 60 seconds in the
background, Cloudflare at 100 seconds), so the cockpit pairs an
application-level keepalive with a client-side reconnect envelope.
The server sends a Ping every 30 seconds and reaps any socket that
goes 90 seconds without a Pong reply. On the client, the
`useCockpit` hook re-dials the WebSocket on close with exponential
backoff (1s, 2s, 4s, 8s, 16s, 30s, 30s), reset on the next successful
`onopen`. The reconnect resumes from `?since={lastSeq}` so the
transcript stays continuous. The cockpit banner shows
`Reconnecting (N/7) in Xs...` while the auto-retry is armed, and a
manual **Reconnect** button after the seven attempts exhaust.
`visibilitychange`, `online`, and `pageshow` listeners trigger an
immediate reconnect when the tab returns to the foreground.

### Approval card vanished without resolving

Approvals expire after `approval_timeout_secs` (default 300). The
agent receives a structured cancellation; you'll typically see a
follow-up message asking again. Bump the timeout if you're in a
context where approvals legitimately take longer.

### `/clear` collapsed earlier turns

When you run `/clear` in a cockpit session, the model's context is
wiped on the adapter side but the visible transcript is preserved.
The cockpit appends a "Conversation cleared" divider, resets the
active plan, the current mode, any in-flight approvals, and the
session usage snapshot, then folds every row above the divider
behind a disclosure banner: `Show N earlier turns (cleared, not in
the model's memory)`. Click the banner to expand the older transcript
for your own reference; the model still won't see those turns. See
[#1101](https://github.com/agent-of-empires/agent-of-empires/issues/1101).

The slash-command palette and mode picker stay populated across a
`/clear`. `claude-agent-sdk` caches the supported command surface at
Query init and does not rotate it when conversation context is reset,
so the cached list stays authoritative for the lifetime of the
cockpit's underlying agent process. See #1128.

A `/clear` queued mid-turn (or any agent's clear alias, e.g. codex /
opencode `/new`) is honoured as a standalone POST when the turn ends,
even under `combined` drain mode. The drain effect splits the queued
prompts at each clear-command boundary, so an ordering like
`foo`, `/clear`, `bar` fires as three separate POSTs (`foo`, then
`/clear`, then `bar`) instead of one multi-paragraph prompt that would
otherwise glue `/clear` past the server's head-anchored detection. The
queued-prompt strip shows an amber `fires separately` divider between
rows that will land in different sub-batches. See #1356.

The session cost figure in the composer footer reads "since the most
recent `/clear` (or `/compact`)" rather than session-lifetime
cumulative. `claude-agent-acp` keeps reporting its cumulative cost
across the ACP session's whole lifetime (the adapter does not rotate
the ACP session id on `/clear`), so the cockpit captures the
cumulative at each boundary and subtracts it from incoming
`UsageUpdate` frames. Switching backends (`AgentSwitched`) or starting
a fresh ACP session (`SessionContextReset`) clears the baseline, since
the new backend reports its own cumulative starting at zero. The
`used` / context-window figures stay raw because the adapter already
reflects the post-boundary context size on its side. See #1354.

### "Force end turn" button under the spinner

If the agent finished a turn but the cockpit's working spinner is
still rattling (no streaming chunks landed for a while), a small
"Force end turn" button appears beneath it. Clicking it clears the
local spinner immediately and asks the daemon to publish a synthetic
`Stopped` plus a best-effort `session/cancel` to the agent. Pure
recovery affordance for a missed-event race (#1100); during a healthy
turn it never shows. Configure the inactivity threshold with
`cockpit.force_end_turn_threshold_secs` (default 30s).

While a tool is in flight (Write, Read, Task subagent, slow Bash,
etc.) the spinner still flips to an elapsed-time label after the
threshold ("Waiting on tool… 1m 23s") so the wait is visible, but the
button stays hidden so clicking it cannot discard the in-flight
tool's progress. The escape hatch is reserved for a silent model with
no tool running. See #1176.

### Silent-orphan watchdog

The cockpit daemon also watches for the case where the agent adapter
finishes streaming a turn but never sends the JSON-RPC
`PromptResponse` that closes out `session/prompt`. The user-visible
symptom is identical to the bug above (spinner stuck), but the cause
is a protocol violation on the adapter side: the response was lost,
not just delayed. Tracked upstream at
[agentclientprotocol/claude-agent-acp#688](https://github.com/agentclientprotocol/claude-agent-acp/issues/688).

When the daemon detects this, it sends `session/cancel`, waits the
existing cancel-escalation grace (10s) for the adapter to respond,
then SIGTERMs the runner and respawns via `session/load` so the
transcript is preserved. The web UI shows a distinct banner ("Agent
finished but didn't notify the daemon. Restarting worker; your
transcript will be preserved.") so the user can tell this apart from
the cancel-escalation path (`agent_unresponsive`). See #1240.

The detector fires only when ALL hold for the current prompt:
- `tool_calls_in_flight` is empty (no open tool call; long-running
  npm install / Playwright / Task subagent runs are never affected
  because their tool stays open until done).
- At least one progress notification has already arrived for this
  prompt (avoids false-firing on a slow first chunk).
- No further progress notification has arrived for
  `silent_orphan_grace_secs` (default 120), reduced to
  `silent_orphan_fast_grace_secs` (default 20) for the rest of the
  prompt once a cost-populated `UsageUpdate` has arrived. The
  accelerated path lowers MTTR on the specific claude-agent-acp
  failure shape without weakening the vendor-agnostic baseline.

Out-of-band notifications (mode changes, available_commands_update,
rate limit, usage updates without cost) explicitly do NOT reset the
timer, so an adapter that emits periodic ambient state after the
final transcript event still trips the watchdog.

**Off-protocol work suppression (#1360, #1401):** several Claude SDK
features intentionally make the agent quiet for long stretches, with
no ACP-layer signaling the daemon can observe. The watchdog detects
each and lifts the effective grace to `OFF_PROTOCOL_WORK_GRACE_FLOOR`
(30 minutes) for the rest of the prompt:

- `Agent` tool with `isAsync: true` (#1360). Sub-agent runs INSIDE the
  claude binary. Detected from the completion text `Async agent
  launched successfully` on the launch's `ToolCallUpdate`.
- `Bash` tool with `run_in_background: true` (#1401). The visible
  ToolCall completes immediately while a real subprocess keeps running
  off-protocol; the agent polls later via `BashOutput`. Detected from
  the `raw_input.run_in_background = true` flag at `ToolStarted` time
  AND from the completion text `Command running in background with
  ID:` (either signal alone is enough; defense in depth so a single
  SDK string drift can't reintroduce the false-positive class).

The off-protocol branch takes precedence over the cost-seen fast path
(a cost-populated UsageUpdate mid-wait could be intermediate billing
telemetry rather than turn termination). The grace stays finite by
design so a real adapter wedge during off-protocol work still
recovers, just slower. The async-agent path is a bandaid until
upstream `agentclientprotocol/claude-agent-acp#336` forwards the
SDK's `task_notification` / `task_started` system messages as proper
ACP SessionUpdates.

**Scheduled wakeup suppression (#1401):** when the agent calls the
Claude SDK `ScheduleWakeup` tool with `delaySeconds: N`, the daemon
suppresses the watchdog until `wakeup_at + silent_orphan_grace_secs`,
computed as a monotonic `Instant` deadline at signal receipt so
wall-clock jumps don't perturb suppression. Multiple wakeups in the
same prompt extend (not shorten) the suppression, and the later deadline
always wins. After the deadline passes the watchdog rearms with its
normal grace; if the scheduled wake does not produce follow-up
progress while the prompt loop is alive, the watchdog recovers
after the tail grace. Daemon crashes during sleep tear down the
in-memory prompt loop entirely, so the next attach starts fresh.

Set `cockpit.silent_orphan_grace_secs = 0` to disable. Both knobs are
editable per profile in the TUI Settings (`Cockpit` category) and in
the web dashboard's Settings tab under `Cockpit`, inside the collapsed
`Advanced` section alongside the other replay and watchdog tuning
knobs. Nonzero values
below 120 are clamped up to 120 at runtime so a typo cannot drop the
watchdog into a tight-loop false-positive regime; debug builds honour
`AOE_SILENT_ORPHAN_GRACE_MS` to keep test cadences sub-second.

In debug builds, set `AOE_COCKPIT_SIMULATE_ORPHAN_NEXT_PROMPT=1`
before sending a cockpit prompt to manually reproduce the wedge: the
daemon will discard the next prompt response, the watchdog will fire
within the configured grace, and you can verify the end-to-end UX
(banner, lockdown, SIGTERM, respawn). The env var is single-shot
(cleared after one use) and compiled out in release builds.

### Sharing debug logs

`AOE_LOG_LEVEL=debug` (or the legacy `AGENT_OF_EMPIRES_DEBUG=1`) writes
agent stderr verbatim to `debug.log` under the app data dir. We scrub
common API-key prefixes (Anthropic `sk-...`, GitHub `ghp_...`, AWS
`AKIA...`, `Bearer <token>`, etc.) before they hit disk, but the scrub
is best-effort; a hand-rolled secret with no recognisable shape will
pass through. Before attaching `debug.log` to a bug report, skim it
for anything that looks like a credential, and replace it with
`<redacted>` if needed.
