# Cockpit Setup

How to confirm prerequisites, enable cockpit per session or globally,
turn it off, and drive it from a remote daemon or the command line. For
what cockpit is and which agents it supports, see the
[Cockpit overview](../cockpit.md).

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

```text
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

#### From the web wizard (primary)

When the cockpit master switch is on, the web new-session wizard shows a
per-session **Use cockpit** toggle, on by default for supported agents.
Open `aoe serve`, click **New session**, pick the agent, leave the toggle
on, and create. No CLI required. Tools without a verified ACP adapter (and
custom agents without `agent_cockpit_cmd`) have no toggle and stay in tmux.

#### From the CLI (optional)

For scripting, headless, or non-interactive launches, `aoe add` starts
cockpit sessions too:

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

### Launch command and session name

`--cmd <tool>` resolves through `session.agent_command_override` for
cockpit sessions, the same as for tmux sessions. With

```toml
[session.agent_command_override]
opencode = "opencode-plannotator"
```

`aoe add . --cmd opencode --cockpit` launches `opencode-plannotator`,
not the bare `opencode` binary; the override's binary replaces the
registry command and the agent's required ACP args are preserved (so
`opencode acp` becomes `opencode-plannotator acp`). The override is
applied only to a built-in agent whose registry binary matches the
tool's own binary; adapter-backed agents such as Claude keep using
`session.agent_cockpit_cmd` for a full command swap.

The web new-session wizard shows the resolved launch command read-only
so you can confirm it before the session starts.

Session naming differs by entry point. By default `aoe add` does not
prompt for a name: it uses `--title` when given, otherwise the worktree
branch name, otherwise a generated name. Pass `-i`/`--interactive` to get
the same name prompt the TUI `n` flow and the web new-session wizard
provide; it shows the generated default and pressing Enter accepts it.
`--interactive` requires a terminal and is ignored when `--title` is
given, so scripted and non-interactive `aoe add` calls keep auto-naming.
To name a session non-interactively, pass `--title "<name>"`.

For web-created cockpit sessions, configure per-agent defaults under
`[session.cockpit_defaults.<agent>]` so the dashboard starts that agent with
the desired model and, when the adapter advertises it, reasoning effort:

```toml
[session.cockpit_defaults.opencode]
model = "openai/gpt-5.5"
effort = "high"
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

## Cross-machine attach

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

## Headless CLI verbs

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

## CLI reference

```text
aoe cockpit doctor [--json] [--fix]
aoe cockpit agents
aoe cockpit ps [--json]
aoe cockpit stop <session>            # graceful: SIGTERM the runner
aoe cockpit stop --all
aoe cockpit kill <session>            # immediate: SIGKILL the runner
aoe cockpit logs [--session <id>] [--follow]
aoe cockpit restart <session>         # stop + let daemon respawn
```
