# Structured View Setup

How to confirm prerequisites, choose the structured view or terminal view per
session, and drive it from a remote daemon or the command line. For what
the structured view is and which agents it supports, see the
[Structured view overview](../structured-view.md).

## Requirements

- aoe built with `--features serve` (the structured view ships alongside the
  web dashboard).
- Node.js 20 or newer on `PATH`. The structured view spawns an ACP agent
  subprocess; for the bundled `aoe-agent` runtime it uses Vercel AI
  SDK 6, which requires Node 20+.
- For Claude Code via the official ACP adapter, you also need a
  `claude login` session.

If Node.js is missing or too old, the structured view refuses to start and a
session for that agent falls back to the terminal view with an actionable
warning pointing at the install path for your OS.

### Verify

```bash
aoe acp doctor
```

Sample output on a machine where Claude is installed but the others
aren't:

```text
ACP doctor
==========

The structured view is the ACP-based structured rendering. It is the default
in the web dashboard; `aoe add` and the TUI default to the terminal view.
Pass --structured-view or --agent to opt a CLI session in (or flip a session
from the session view).

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

`aoe acp doctor --fix` will `npm install -g` the npm-distributed
adapters (claude / codex / pi). The native CLIs (opencode / gemini /
vibe) you install through their own channels.

If Node is missing the report exits 1; if some agents are unreachable
it exits 2; otherwise 0. Pass `--json` for machine-readable output.

## Choosing the view

### Per session

#### From the web wizard (primary)

The web new-session wizard shows a per-session **Use structured view** toggle,
on by default for ACP-capable agents. Open `aoe serve`, click **New
session**, pick the agent, leave the toggle on, and create. No CLI
required. Tools without a verified ACP adapter (and custom agents without
`agent_acp_cmd`) have no toggle and run in the terminal view.

#### From the CLI (optional)

Unlike the web wizard, `aoe add` defaults to the **terminal view**, matching
the TUI. Opt into the structured view per session with `--structured-view`,
or by naming a specific ACP agent with `--agent` (which implies it):

```bash
# Terminal/PTY view (the default).
aoe add . --cmd claude

# Structured view for an ACP-capable tool.
aoe add . --cmd claude --structured-view

# Pick a specific ACP agent + model (implies the structured view).
aoe add . --agent aoe-agent --model gpt-5
aoe add . --agent aoe-agent --model llama3.3:ollama
aoe add . --agent gemini
```

If you pass `--agent` for an agent whose adapter isn't installed, `aoe add`
errors with an install hint; with `--structured-view` (no `--agent`), a
missing adapter falls back to the terminal view with a warning so the
command still succeeds.

### Launch command and session name

`--cmd <tool>` resolves through `session.agent_command_override` for
structured-view sessions, the same as for terminal sessions. With

```toml
[session.agent_command_override]
opencode = "opencode-plannotator"
```

`aoe add . --cmd opencode` launches `opencode-plannotator`,
not the bare `opencode` binary; the override's binary replaces the
registry command and the agent's required ACP args are preserved (so
`opencode acp` becomes `opencode-plannotator acp`). The override is
applied only to a built-in agent whose registry binary matches the
tool's own binary; adapter-backed agents such as Claude keep using
`session.agent_acp_cmd` for a full command swap.

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

For web-created structured-view sessions, configure per-agent defaults under
`[session.acp_defaults.<agent>]` so the dashboard starts that agent with
the desired model and, when the adapter advertises it, reasoning effort:

```toml
[session.acp_defaults.opencode]
model = "openai/gpt-5.5"
effort = "high"
```

## Global configuration

The structured view's tuning knobs live in `config.toml` under `[acp]`:

```toml
[acp]
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
silent_orphan_grace_secs = 120  # daemon-side watchdog grace when the adapter stops talking with no in-flight tool; 0 disables (#1240)
silent_orphan_fast_grace_secs = 20  # accelerated grace used once a cost-populated UsageUpdate has arrived for the current prompt (#1240)
auto_stop_idle_secs = 0  # auto-stop agent workers idle this many seconds; 0 disables (default); the next prompt respawns the worker (#1689)
```

`max_concurrent_resumes` bounds how many agent workers the reconciler
spawns/attaches in parallel on `aoe serve` cold start. Default 4 keeps
Node.js bootup memory bounded for laptops/Pis; raise on beefier hosts.
Clamped at runtime by `min(this, max_concurrent_workers).max(1)`. The
supervisor's per-agent install gate serialises only the first spawn of
each agent per daemon lifetime, so the claude-agent-acp lazy-install
race is safe even at high parallelism (#1088).

`auto_stop_idle_secs` reclaims resources from abandoned sessions. When
set to a positive value, the daemon stops any agent worker that has
seen no events and has no in-flight turn for that many seconds, freeing
its claude-agent-acp subprocess. The stop is seamless: the session keeps
its place in the sidebar, the timeline shows a `Stopped` event with
reason `idle_auto_stop`, and the next prompt you send respawns a fresh
worker (resuming the agent-side transcript) within a couple of seconds.
A mid-turn worker is never stopped. The check runs about once a minute,
so the effective stop can lag the threshold by up to a minute. Default
`0` disables the feature. This covers agent workers only; plain
TUI/tmux sessions are not affected (#1689).

`AOE_ACP_NODE=/path/to/node` overrides Node discovery for one process
(useful when the host's PATH-side Node is the wrong version and you can't
change PATH).

> Upgraders: migration v005 seeded the old `[cockpit]` section and v006
> flipped its `replay_events` to unlimited. Migration v012 renames the
> section to `[acp]` (dropping the retired master switch and
> `default_for_claude` keys) and migrates per-session state, so an
> upgraded config lands on `[acp]` automatically.

## Choosing terminal vs structured

Which view a new session gets depends on where you start it:

- **Web wizard**: defaults to the structured view; leave the **Use structured
  view** toggle off to get the terminal view.
- **CLI (`aoe add`)** and the **TUI**: default to the terminal view. From the
  CLI, opt into the structured view with `--structured-view` or `--agent`.
- Either way, you can switch an existing session from the session view at any
  time (the agent restarts in a fresh pane; open files and worktree state are
  preserved).

Non-ACP tools always run in the terminal view, with no toggle.

## Cross-machine attach

Set `AOE_DAEMON_URL` (and optionally `AOE_DAEMON_TOKEN`) to point at
a remote `aoe serve` daemon, then either:

```sh
# Browse the remote daemon's structured-view sessions and pick one.
AOE_DAEMON_URL=https://aoe.example.com AOE_DAEMON_TOKEN=… aoe

# Or jump straight into a known session id.
aoe acp attach <session_id> --daemon-url https://aoe.example.com
```

When `AOE_DAEMON_URL` is set, the TUI swaps the local home view for
a remote agent-session picker. Local-only operations (tmux attach,
`aoe stop`, file edit) aren't available against a remote; for
those, use the web dashboard or SSH into the host machine.

The env override also retargets `aoe serve --status` and the
`aoe acp *` verbs: with `AOE_DAEMON_URL` set, `--status` pings
the remote endpoint and reports its reachability instead of inspecting
the local `serve.pid` file. Unset the variable (or run `env -u
AOE_DAEMON_URL aoe serve --status`) to fall back to local introspection.

## Headless CLI verbs

For scripting and quick checks, every structured-view operation has a
matching `aoe acp <verb>` that talks to the same daemon:

| Verb                              | What it does                                                |
| --------------------------------- | ----------------------------------------------------------- |
| `aoe acp history <id>`        | Dump the persisted transcript                               |
| `aoe acp status <id>`         | Print highest/lowest seq and the daemon source              |
| `aoe acp prompt <id> <text>`  | Send a prompt (`-` reads from stdin)                        |
| `aoe acp approve <id> <nonce> [--always\|--deny]` | Resolve a pending approval        |
| `aoe acp cancel <id>`         | Cancel the in-flight prompt                                 |
| `aoe acp tail <id>`           | Stream broadcast frames to stdout as JSON lines             |
| `aoe acp attach <id>`         | Open the TUI structured view directly for this session id        |

Every verb (including `attach`) requires an `aoe serve` daemon to be
already running, and exits with an actionable hint if none is found.
Start one with `aoe serve --daemon` (localhost) or
`aoe serve --daemon --remote` (Tailscale/Cloudflare), or set
`AOE_DAEMON_URL` to attach to a remote daemon. The CLI deliberately
does not spawn a daemon on your behalf so the localhost-vs-tunnel
choice stays explicit.

## CLI reference

```text
aoe acp doctor [--json] [--fix]
aoe acp agents
aoe acp ps [--json]
aoe acp stop <session>            # graceful: SIGTERM the runner
aoe acp stop --all
aoe acp kill <session>            # immediate: SIGKILL the runner
aoe acp logs [--session <id>] [--follow]
aoe acp restart <session>         # stop + let daemon respawn
```
