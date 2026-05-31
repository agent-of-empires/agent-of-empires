# Cockpit (Native Agent Rendering, Beta)

> **Beta, opt-in.** Cockpit ships disabled by default behind a single
> master switch: `cockpit.enabled = true` in `config.toml` (default
> `false` from migration v005). Toggle it from the web settings
> (Cockpit tab) or by editing `config.toml` directly.
>
> While the switch is off:
>
> - the web wizard auto-routes new sessions through tmux,
> - `aoe add --cockpit` refuses with an actionable error,
> - the reconciler doesn't auto-spawn workers for any session.
>
> The data model (`cockpit_mode: bool` per session) is stable; the
> UI and reliability story are still evolving; see "What's deferred".

Cockpit is aoe's native rendering surface for AI coding agents. Instead
of viewing the agent through a terminal pane (PTY bytes piped through
xterm.js), cockpit renders the agent's structured state directly: plan,
tool calls, diffs, and approvals. It's mobile-first, with a desktop
layout that scales the same components into a richer multi-pane view.

Cockpit speaks the [Agent Client Protocol](https://agentclientprotocol.com/)
(ACP), a JSON-RPC standard for editor-agent communication. aoe is the
*client*; the agent (Anthropic's Claude Code, our `aoe-agent`, Google's
Gemini CLI, etc.) is the *server*. Any ACP-conformant agent works.

## Supported agents

aoe ships a registry entry for each tool whose ACP server we've verified
against [agentclientprotocol.com](https://agentclientprotocol.com/get-started/agents.md).
The wizard greys out the cockpit option for tools not in this set.

| aoe tool   | Substrate B (cockpit)                                      | Auth                                   |
|------------|------------------------------------------------------------|----------------------------------------|
| `claude`   | `claude-agent-acp` (Zed adapter for the Claude SDK, requires >=0.39.0) | `claude /login` writes `~/.claude/credentials`; or `ANTHROPIC_API_KEY` |
| `opencode` | `opencode acp` (native, SST)                               | `OPENCODE_API_KEY` env var; or provider-specific env (set up via `opencode auth`) |
| `gemini`   | `gemini --acp` (native, Google)                            | `GEMINI_API_KEY` env var, OAuth via `gemini auth`, or Vertex `GOOGLE_API_KEY` |
| `codex`    | `codex-acp` (Zed adapter, npm `@zed-industries/codex-acp`) | `OPENAI_API_KEY` env var, or ChatGPT login (local-only) |
| `vibe`     | `vibe-acp` (native, Mistral)                               | Mistral API key; set up via `vibe` first |
| `pi`       | `pi-acp` (adapter, requires `@earendil-works/pi-coding-agent`) | `pi-acp --terminal-login` for OAuth, or env vars per provider |
| `aoe-agent`| Bundled multi-provider agent (Vercel AI SDK 6)             | Whatever provider env vars Vercel AI SDK expects |
| *aider, cursor, copilot, droid, settl, hermes* | not yet wired into the cockpit registry; fall back to terminal mode |

The four env vars cockpit always forwards to the agent process are
`ANTHROPIC_API_KEY`, `ANTHROPIC_AUTH_TOKEN`, `CLAUDE_CODE_OAUTH_TOKEN`,
`CLAUDE_CONFIG_DIR`. For the others, set them in the env that runs
`aoe serve` (or use the per-session `extra_env` field) and the agent's
own auth path will pick them up via the forwarded `HOME`.

## Quickstart

```bash
# 1. Confirm prerequisites: aoe, Node.js >= 20, claude login.
aoe cockpit doctor

# 2. Create a Claude Code session in cockpit mode.
aoe add . --cmd claude --cockpit

# 3. Open the dashboard, pick the session, and you should see the
#    structured plan + tool-call cards instead of a terminal.
aoe serve
```

A first-time mobile user pointed at a remote `aoe serve` will install
the PWA, tap the session, and see the plan panel render the moment the
agent emits its first plan event.

## Requirements

- aoe 1.5.0 or newer, built with `--features serve` (cockpit ships
  alongside the web dashboard).
- Node.js 20 or newer on `PATH`. Cockpit spawns an ACP agent
  subprocess; for the bundled `aoe-agent` runtime it uses Vercel AI
  SDK 6, which requires Node 20+.
- For Claude Code via the official ACP adapter, you also need a
  `claude login` session.

If Node.js is missing or too old, cockpit refuses to start and prints
an actionable error pointing at the install path for your OS.

### Verify

```bash
aoe cockpit doctor
```

Sample output on a machine where Claude is installed but the others
aren't:

```
Cockpit doctor  (Beta)
======================

Cockpit is the structured-rendering substrate (ACP-based).
Tmux passthrough remains the default for tool sessions; cockpit
is opt-in per session via `aoe add --cockpit` or the web wizard.

[OK] Node runtime  v22.21.0
    path: /opt/homebrew/bin/node

Configured agents:
[!! ] aoe-agent  (aoe's bundled multi-provider agent (Vercel AI SDK 6))
[OK] claude  (Anthropic Claude via the official ACP adapter …)
[OK] claude-code  (Alias for `claude` (legacy name))
[!! ] codex  (OpenAI Codex CLI via Zed adapter …)
    install: npm install -g @zed-industries/codex-acp
[!! ] gemini  (Google Gemini CLI; native ACP via `gemini --acp`)
    install: npm install -g @google/gemini-cli  (then `gemini --acp`)
[!! ] opencode  (OpenCode (SST); native ACP via `opencode acp`)
    install: curl -fsSL https://opencode.ai/install | bash  (then `opencode acp`)
[!! ] pi  (Pi coding agent (`pi`) via the pi-acp adapter …)
    install: npm install -g pi-acp (also requires `npm install -g @earendil-works/pi-coding-agent`)
[!! ] vibe  (Mistral Vibe; native ACP via the bundled `vibe-acp` binary)
    install: follow https://github.com/mistralai/mistral-vibe (ships the `vibe-acp` binary)

Overall: partial
```

`aoe cockpit doctor --fix` will `npm install -g` the npm-distributed
adapters (claude / codex / pi). The native CLIs (opencode / gemini /
vibe) you install through their own channels.

If Node is missing the report exits 1; if some agents are unreachable
it exits 2; otherwise 0. Pass `--json` for machine-readable output.

## Enabling cockpit

### Per session

```bash
# Force cockpit on for this session, regardless of defaults.
aoe add . --cmd claude --cockpit

# Force terminal/PTY on, regardless of defaults.
aoe add . --cmd claude --no-cockpit

# Pick a specific cockpit agent + model.
aoe add . --cockpit --agent aoe-agent --model gpt-5
aoe add . --cockpit --agent aoe-agent --model llama3.3:ollama
aoe add . --cockpit --agent gemini
```

### Globally

The settings live in `config.toml` under `[cockpit]`:

```toml
[cockpit]
enabled = true
default_for_claude = true
default_agent = "aoe-agent"
approval_timeout_secs = 300
destructive_require_double_confirm = true
max_concurrent_workers = 5
max_concurrent_resumes = 4  # cap on parallel cold-start spawns/attaches (#1088)
replay_events = 0  # 0 = unlimited history; set a positive value to cap per-session rows (also caps the web client's in-memory activity buffer, #1111)
replay_bytes = 5_242_880
node_path = ""
show_tool_durations = true  # per-tool elapsed-time label in the web UI
queue_drain_mode = "combined"  # how the composer drains client-side queued prompts: "combined" | "serial" (#1031)
force_end_turn_threshold_secs = 30  # seconds of streaming silence before the spinner offers a "Force end turn" button (#1100)
silent_orphan_grace_secs = 120  # daemon-side watchdog grace when the adapter stops talking with no in-flight tool; 0 disables (#1240); bumped from 60 in #1360 for async-agent flows; nonzero values below 120 clamp up at runtime
silent_orphan_fast_grace_secs = 20  # accelerated grace used once a cost-populated UsageUpdate has arrived for the current prompt (#1240); ignored while an async-agent wait is active (#1360)
auto_stop_idle_secs = 0  # auto-stop cockpit workers idle this many seconds; 0 disables (default); the next prompt respawns the worker (#1689)
```

`max_concurrent_resumes` bounds how many cockpit workers the reconciler
spawns/attaches in parallel on `aoe serve` cold start. Default 4 keeps
Node.js bootup memory bounded for laptops/Pis; raise on beefier hosts.
Clamped at runtime by `min(this, max_concurrent_workers).max(1)`. The
supervisor's per-agent install gate serialises only the first spawn of
each agent per daemon lifetime, so the claude-agent-acp lazy-install
race is safe even at high parallelism (#1088).

`enabled = false` is a master kill switch; cockpit refuses to spawn
even if a session has `--cockpit`. `default_for_claude = true` makes
new Claude sessions cockpit-mode by default on mobile clients.

`auto_stop_idle_secs` reclaims resources from abandoned sessions. When
set to a positive value, the daemon stops any cockpit worker that has
seen no events and has no in-flight turn for that many seconds, freeing
its claude-agent-acp subprocess. The stop is seamless: the session keeps
its place in the sidebar, the timeline shows a `Stopped` event with
reason `idle_auto_stop`, and the next prompt you send respawns a fresh
worker (resuming the agent-side transcript) within a couple of seconds.
A mid-turn worker is never stopped. The check runs about once a minute,
so the effective stop can lag the threshold by up to a minute. Default
`0` disables the feature; no worker is ever stopped for inactivity. This
covers cockpit workers only; plain TUI/tmux sessions are not affected
(#1689).

Migration v005 seeds these defaults on upgrade so the section already
exists if you came from 1.4.x. Migration v006 then flips the v005-seeded
`replay_events = 500` to `0` so upgraders pick up the new unlimited
default; any user who has explicitly chosen a different cap is left
alone.

## Disabling / escape hatches

- `--no-cockpit` per session (CLI).
- `cockpit.enabled = false` in `config.toml` (persistent master). The
  reconciler short-circuits, REST endpoints return 503, and the CLI
  refuses `--cockpit`. The web settings panel toggles this live;
  flipping the switch shuts down running workers within a couple of
  seconds and respawns them when re-enabled, no `aoe serve --stop`
  required.
- `AOE_COCKPIT_NODE=/path/to/node` overrides Node discovery for one
  process (useful when the host's PATH-side Node is the wrong version
  and you can't change PATH).

### Fully turn cockpit off

The fastest path: open the web settings, go to the Cockpit tab, and
flip the master switch off. Workers exit within a couple of seconds.

Or edit `config.toml` directly and restart:

```bash
aoe serve --stop
$EDITOR ~/.config/agent-of-empires/config.toml  # [cockpit] enabled = false
aoe serve
```

`aoe cockpit doctor --fix` will install missing ACP tooling but **will
not** flip `cockpit.enabled` on for you; toggling that is always an
explicit operator action.

## TUI vs web dashboard

Cockpit renders natively in the TUI alongside the web dashboard.
Both consume the same `aoe serve` daemon over the same HTTP/WS
surface, so the conversation log, pending approvals, and worker
state are always in sync.

- **Sessions started in cockpit mode** appear in the TUI session list
  with a `[cockpit]` badge. Pressing Enter opens the native cockpit
  view, which requires an `aoe serve` daemon to be already running.
  If one isn't, the view renders an actionable error pointing at
  `aoe serve --daemon` (localhost), `aoe serve --daemon --remote`
  (Tailscale/Cloudflare), or `AOE_DAEMON_URL` (attach to a remote
  daemon you already have running). The TUI intentionally does not
  start a daemon on your behalf, so you keep the choice between
  localhost, tunnel, and named tunnel explicit.
- **Sessions started in tmux mode** work in both surfaces as before.
  The TUI attaches to the pane; the dashboard renders the pane via
  xterm.js.
- **Switching substrates** (web wizard or the per-session "Switch to
  cockpit" / "Switch to tmux" action) destroys the in-memory
  conversation history for that session. The git worktree, files on
  disk, and any commits remain. The next prompt starts a fresh
  conversation under the new substrate.
- **TUI status indicators**: a cockpit session that's healthy shows
  as Idle/Active in the TUI session list, since cockpit health is
  observed via the ACP event stream rather than tmux pane probing.
- **`--auth=passphrase` daemons**: the local TUI attaches to a
  same-host daemon without going through the passphrase exchange.
  Loopback callers are treated as fs-trusted because the daemon's
  serve files under `~/.agent-of-empires/serve.*` are already 0600,
  so the filesystem permission boundary protects same-host access.
  Remote callers proxied through a tunnel still hit the passphrase
  wall as expected. See #1525.

### TUI cockpit view keybinds

The TUI cockpit view has three focusable regions: composer (where
you type prompts), transcript (the activity feed), and approval
cards (one per pending tool authorization). Tab cycles focus; the
status banner at the bottom of the screen shows the current focus.

| Focus       | Key             | Action                                                |
| ----------- | --------------- | ----------------------------------------------------- |
| Composer    | `Enter`         | Send the buffered text as a prompt                    |
| Composer    | `Shift+Enter`   | Insert a newline (multi-line prompts)                 |
| Composer    | `Esc`           | Return focus to the transcript                        |
| Transcript  | `j` / `↓`       | Scroll down one line                                  |
| Transcript  | `k` / `↑`       | Scroll up one line                                    |
| Transcript  | `PgDn` / `PgUp` | Scroll ten lines                                      |
| Transcript  | `g` / `G`       | Jump to top / bottom                                  |
| Transcript  | `i`             | Focus the composer                                    |
| Transcript  | `Tab`           | Cycle to the approval card (if any pending)           |
| Transcript  | `o`             | Open this session in the web dashboard                |
| Transcript  | `Esc`           | Close the cockpit view and return to the session list |
| Approval    | `a`             | Allow once                                            |
| Approval    | `Shift+A`       | Allow always (session-scoped allow-list entry)        |
| Approval    | `d`             | Deny                                                  |
| Approval    | `Esc`           | Return focus to the transcript                        |
| Any         | `Ctrl+C`        | Cancel the in-flight prompt                           |
| Any         | `Ctrl+O`        | Open the session in the web dashboard                 |

**Focus isolation.** Approval keys (`a`/`Shift+A`/`d`) only resolve
when the approval card itself has focus. Typing "always allow" into
the composer will never silently approve a pending tool; the
composer captures every keystroke, including those letters.

### Web composer Enter behavior

On desktop, Enter sends the prompt and Shift+Enter inserts a
newline, matching the TUI convention above.

On touch-primary devices (phones, tablets without an attached
keyboard), plain Enter inserts a newline and the explicit Send
button on the right of the composer is the only path to dispatch.
This matches the conventions of WhatsApp, Slack, ChatGPT mobile,
and Claude.ai mobile, and avoids the common foot-gun of accidentally
firing a partial multi-line prompt by reaching for a line break.
An iPad with a Bluetooth keyboard (or any device that reports both
`(pointer: coarse)` and `(any-pointer: fine)` to the browser) keeps
the desktop Enter-to-send convention so hardware-keyboard typing
feels natural. See #1129.

### iOS Safari dictation

Tapping the on-screen keyboard's mic icon to dictate into the composer
commits each partial recognition exactly once. The composer detects
WebKit's `insertReplacementText` burst, suspends its assistant-ui
controlled-input flush for the duration so WebKit's dictation range
pointer is not invalidated mid-utterance, then drains the final text
into the composer state on blur (typically when you tap Send) or
after a brief idle period. See #1431.

### Queued prompts (mid-turn + inactive session)

The web composer keeps your messages around even when the session
can't accept them yet. Two cases:

1. **Mid-turn follow-up.** While the agent is producing the current
   response, the Send button switches to a paper-plane with a small
   pending-count badge. Click (or press Enter) and your text lands in
   the **Queued (N)** strip above the composer. As soon as the agent
   reports `Stopped`, the cockpit drains the queue per the
   `cockpit.queue_drain_mode` setting (combined, the default, sends
   every parked entry as one prompt; serial fires them one at a time).
   See #1031 for the original feature.

2. **Inactive session.** If the WebSocket is mid-reconnect, the worker
   is stopped (`user_stopped`), or the worker is restarting
   (`restart_pending`, `agent_unresponsive`, `prompt_orphaned`), the
   composer still accepts submissions. The tooltip swaps to
   `Queue message until session resumes`, the strip heading changes to
   `Pending until session resumes (N)`, and the parked entry stays
   editable. The moment the WS reopens AND the worker reaches
   `running` AND the session-level `Stopped` flag clears (an
   `AcpSessionAssigned` event), the same drain effect fires the
   queue. See #1359.

Queued entries persist in the per-origin localStorage snapshot at
`aoe:cockpit-state:v1:<sid>`, so a page reload (and closing then
reopening the tab on the same origin) keeps them across the reconnect
window. Server-side durability is not currently implemented; clearing
site data wipes the queue.

### Cross-machine attach

Set `AOE_DAEMON_URL` (and optionally `AOE_DAEMON_TOKEN`) to point at
a remote `aoe serve` daemon, then either:

```sh
# Browse the remote daemon's cockpit sessions and pick one.
AOE_DAEMON_URL=https://aoe.example.com AOE_DAEMON_TOKEN=… aoe

# Or jump straight into a known session id.
aoe cockpit attach <session_id> --daemon-url https://aoe.example.com
```

When `AOE_DAEMON_URL` is set, the TUI swaps the local home view for
a remote-cockpit picker. Local-only operations (tmux attach,
`aoe stop`, file edit) aren't available against a remote; for
those, use the web dashboard or SSH into the host machine.

The env override also retargets `aoe serve --status` and the
`aoe cockpit *` verbs: with `AOE_DAEMON_URL` set, `--status` pings
the remote endpoint and reports its reachability instead of inspecting
the local `serve.pid` file. Unset the variable (or run `env -u
AOE_DAEMON_URL aoe serve --status`) to fall back to local introspection.

### Headless CLI verbs

For scripting and quick checks, every cockpit operation has a
matching `aoe cockpit <verb>` that talks to the same daemon:

| Verb                              | What it does                                                |
| --------------------------------- | ----------------------------------------------------------- |
| `aoe cockpit history <id>`        | Dump the persisted transcript                               |
| `aoe cockpit status <id>`         | Print highest/lowest seq and the daemon source              |
| `aoe cockpit prompt <id> <text>`  | Send a prompt (`-` reads from stdin)                        |
| `aoe cockpit approve <id> <nonce> [--always\|--deny]` | Resolve a pending approval        |
| `aoe cockpit cancel <id>`         | Cancel the in-flight prompt                                 |
| `aoe cockpit tail <id>`           | Stream broadcast frames to stdout as JSON lines             |
| `aoe cockpit attach <id>`         | Open the TUI cockpit view directly for this session id      |

Every verb (including `attach`) requires an `aoe serve` daemon to be
already running, and exits with an actionable hint if none is found.
Start one with `aoe serve --daemon` (localhost) or
`aoe serve --daemon --remote` (Tailscale/Cloudflare), or set
`AOE_DAEMON_URL` to attach to a remote daemon. The CLI deliberately
does not spawn a daemon on your behalf so the localhost-vs-tunnel
choice stays explicit.

## Tool compatibility

| Tool          | Cockpit?     | Notes                                              |
|---------------|--------------|----------------------------------------------------|
| Claude Code   | yes          | via the official ACP adapter (`claude-code`)        |
| aoe-agent     | yes          | bundled multi-provider runtime (Vercel AI SDK 6)   |
| Gemini CLI    | yes          | `gemini acp` (Google reference impl)               |
| OpenCode      | optional     | requires `opencode` with ACP support               |
| Codex CLI     | optional     | tracking upstream ACP support                      |
| Cursor CLI    | terminal only| no ACP support today                               |
| Factory Droid | terminal only| no ACP support today                               |
| OpenClaw      | terminal only| no ACP support today                               |

Tools without ACP support continue to work exactly as they do today
(tmux + PTY); cockpit is additive, not a replacement.

## Timeline card grouping

To keep the timeline readable, cockpit folds two kinds of runs into
single collapsible cards:

- **Silent tool work.** A run of three or more consecutive tool calls
  with no agent text between them (for example Read, Read, Grep, Read
  during investigation) collapses into one "actions" card. Expand it to
  see each call as its normal per-tool card.
- **Consecutive TodoWrite updates.** When the agent fires three or more
  `TodoWrite` calls back-to-back, the per-call snapshots fold into one
  todo card titled "updated N times". Collapsed, the card shows the
  latest list (the only snapshot whose pending/in-progress/done mix is
  current), so you see what the agent is working on without expanding.
  Expand it to inspect each individual update in order and audit how the
  plan evolved during the turn.

Folding only fires when every call in the run is the same shape. A
TodoWrite sandwiched between real tool work (Read, Edit) stays inline as
its own card rather than being hidden inside a group, so a status update
between actions is never buried. Two-in-a-row stays inline as well; the
fold threshold is three.

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
  the daemon arms a resume-idle watchdog that emits a synthetic
  `Stopped { reason: "reattach_idle" }` event after 10s of inbound
  silence. Sessions that the runner cannot reattach to (dead PID,
  missing socket, etc.) fall through to a fresh spawn; if the on-disk
  event log shows that fresh spawn's session was mid-prompt at the
  moment the daemon died, the reconciler publishes a
  `Stopped { reason: "orphaned_at_restart" }` event before the new
  agent starts so the UI clears immediately. The same path covers the
  `main`-branch case where there is no runner at all and every cockpit
  session takes the fresh-spawn branch on restart.

## Session deletion

When you delete a cockpit session, aoe opportunistically fires the
experimental `session/delete` ACP request against the live worker
(bounded by a 2-second timeout) whenever a stored ACP session id
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

## Permission modes and YOLO

Cockpit sessions run in one of the permission modes advertised by the
ACP adapter. The composer's mode picker shows whatever the agent
reports in its `NewSessionResponse.modes`; for `claude-agent-acp` the
typical set is:

| Mode id              | Meaning                                                                 |
|----------------------|-------------------------------------------------------------------------|
| `default`            | Every Write/Edit/Bash routes through an approval card.                  |
| `acceptEdits`        | Edit-kind tools auto-approved; Bash and unknown tools still prompt.     |
| `bypassPermissions`  | All tools auto-approved. The cockpit analogue of YOLO.                  |
| `plan`               | Read-only; the agent drafts a plan but does not run side-effectful tools. |

### YOLO mode maps to `bypassPermissions`

When `[session] yolo_mode_default = true` (or the wizard's "Auto-approve
actions" toggle is on), cockpit asks the adapter to start the session
in `bypassPermissions` immediately after `session/new`. This mirrors
what the tmux substrate does by appending
`--dangerously-skip-permissions` to the Claude CLI argv.

The wiring is best-effort: the cockpit fires `session/set_mode` after
the handshake and continues regardless of the response. If the adapter
accepts, the mode picker flips to `bypassPermissions` and you stop
seeing approval cards. If it rejects (see next section), a non-blocking
amber notice appears above the composer with the adapter's reason and
the session keeps running in whichever mode it landed on.

### `bypassPermissions` may not be available

`claude-agent-acp` gates `bypassPermissions` on the `ALLOW_BYPASS`
environment variable. If the daemon spawned the adapter without
`ALLOW_BYPASS=1` in its env, `session/set_mode("bypassPermissions")`
returns "Mode bypassPermissions is not available" and the session
stays in `default`. Two ways out:

1. Restart `aoe serve` with `ALLOW_BYPASS=1` so the adapter advertises
   the mode. The cockpit then drives it automatically on every new
   YOLO-mode session.
2. Live with `default` and approve as you go, or pick `acceptEdits`
   from the composer mode picker for edit-only auto-approval.

The Auto-approve toggle in the wizard does not configure
`ALLOW_BYPASS`; the env var is a daemon-process input set wherever
`aoe serve` actually launches.

## Model and reasoning effort

Cockpit sessions render two extra selectors in the composer footer
beside the mode pill when the ACP adapter advertises them: a model
dropdown and a reasoning-effort selector. Both come from one wire
mechanism, ACP `SessionUpdate::ConfigOptionUpdate`, stabilised
upstream in `claude-agent-acp` v0.37.0 (Opus 4.8 added in v0.38.0).
The adapter emits the full
snapshot of every selector (mode, model, reasoning effort, future
categories) whenever any one of them changes; the cockpit replaces
its cached list in full.

### When the pickers appear

The pickers only render if the adapter publishes the matching
category. `claude-agent-acp` v0.39.0+ advertises a model selector
for every session, and adds a reasoning-effort selector when the
current model reports `supportsEffort=true`. Older adapters that
emit no `config_option_update` show neither picker; this is by
design so non-Claude backends don't grow empty UI chrome.

### Setting a value

Clicking an option fires `POST /api/sessions/{id}/cockpit/config-option`
with `{ config_id, value }` which the cockpit supervisor turns into
ACP `session/set_config_option`. The UI is pessimistic: the chip
still shows the previous current value while the request is in
flight, with a subtle dim and a disabled re-click on the just-clicked
option, until the adapter pushes a confirming `config_option_update`.
This avoids "snap back" on slow networks (Cloudflare Tunnel can run
300-800ms round trips).

### The `Default` reasoning effort

The reasoning-effort dropdown includes a `Default` option alongside
adapter-supported levels like `Low | Medium | High`. `Default` is
not a fixed effort; the SDK resolves it per current model. Picking
it explicitly tells the adapter to drop any session-level effort
pin so the model uses its default reasoning budget. See upstream
PR agentclientprotocol/claude-agent-acp#701.

### When a switch fails

If the adapter rejects `session/set_config_option` (rate limit,
missing capability, transient error), an amber non-blocking notice
appears next to the composer with the configured selector name, the
rejected value, and the adapter's reason. The notice auto-dismisses
when a later snapshot reports the originally-requested value as
current (the user retried and won, or the adapter applied the value
asynchronously). The session keeps running in whichever value the
adapter last confirmed.

### Lifecycle clears

The cached selector list clears on `AgentSwitched` (a Claude-to-Codex
handoff invalidates Claude-specific models) but survives `/clear`:
adapter capabilities are process-scoped, not conversation-scoped.

## Approvals

When the agent wants to run a tool that requires approval, the cockpit
shows an approval card:

- **Benign tools** (read, search, list): single tap on a primary
  button.
- **Destructive tools** (`rm -rf`, `git push --force`, writes to
  system paths): long-press 800ms with a progress ring and a haptic
  confirmation. Single tap is reserved for the deny button.

You can configure how cockpit classifies destructive operations and
the timeout before a pending approval auto-cancels:

```toml
[cockpit]
approval_timeout_secs = 300
destructive_require_double_confirm = true
```

### Notifications and sound

When an approval lands, the cockpit fires two channels so a user away
from the dashboard still sees the agent is blocked:

- **Web push.** If the PWA is installed and notifications are
  enabled, the daemon sends an OS-level push tagged
  `cockpit-approval-<session>`. Tapping the notification deep-links
  back to the cockpit. Unlike status-change pushes, approval pushes
  are not suppressed when the dashboard or TUI is active; the service
  worker routes focused clients to an in-app toast instead of an OS
  banner. See [Push notifications](push-notifications.md).
- **Browser sound.** The cockpit plays a chime in the dashboard tab
  whenever pending approvals go from zero to non-zero. Configure the
  file via `[sound] on_approval` in the daemon config or the Sound
  category of the Settings TUI. The chime is independent of the
  host-side audio used by the tmux status flow; the cockpit case
  often runs `aoe serve` on a remote box and the host speaker would
  be on the wrong side of the wire.

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
#1101.

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

## CLI reference

```
aoe cockpit doctor [--json] [--fix]
aoe cockpit agents
aoe cockpit ps [--json]
aoe cockpit stop <session>            # graceful: SIGTERM the runner
aoe cockpit stop --all
aoe cockpit kill <session>            # immediate: SIGKILL the runner
aoe cockpit logs [--session <id>] [--follow]
aoe cockpit restart <session>         # stop + let daemon respawn
```

## What's deferred

These are tracked for follow-up releases:

- Mid-token interrupt (waiting on Anthropic's stable feature).
- Plan-mode and elicitation event mappings (the SDK supports them; the
  cockpit's typed schema covers the common path).
- Cross-agent handoff and unified search across cockpit sessions.
- Voice input/output on mobile.
- A read-only cockpit transcript view inside the TUI (today the TUI
  shows a `[web]` badge and an "open in dashboard" hint).
- Default `cockpit.enabled = true`: once the default-cockpit-on-web
  flow has burned in for one release, the master switch flips on by
  default and the wizard shows the substrate picker out of the box.
- Native unix-socket transport for in-container agents that natively
  speak the socket protocol. Today the sandbox path uses `docker exec`
  to keep stdio-only agents working without upstream changes.
