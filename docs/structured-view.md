# Structured View (Web Dashboard)

The **structured view** is the default rendering for AI coding agents in the web dashboard and the native TUI. Instead of a terminal pane (PTY bytes through xterm.js), it renders the agent's structured state directly: plan, tool-call cards, diffs, and approvals. It is mobile-first and scales the same components into a richer multi-pane desktop layout.

It speaks the [Agent Client Protocol](https://agentclientprotocol.com/) (ACP), a JSON-RPC standard for editor-agent communication. aoe is the *client*; the agent (Claude Code, Gemini, the bundled `aoe-agent`, etc.) is the *server*. Any ACP-capable agent uses the structured view by default; a session can opt into the **terminal view** instead, per session, and you can switch at any time. Agents with no ACP adapter always run in the terminal view.

![The structured view rendering an agent's plan, tool-call cards, and a pending approval](assets/structured-view/overview.png)

## In this section

- **[Interface](structured-view/interface.md)**: the TUI and web surfaces, keybinds, composer, queued prompts, and timeline grouping.
- **[Modes, approvals & model controls](structured-view/controls.md)**: permission modes, YOLO, approval cards, notifications, and the model / reasoning-effort selectors.
- **[Troubleshooting](structured-view/troubleshooting.md)**: the security summary plus a field guide to each failure mode and its fix.

Contributors: see [Structured View Internals](development/internals/structured-view.md) for worker lifecycle, watchdogs, persistence, and profiles.

## Supported agents

aoe ships an ACP registry entry for each tool whose ACP server we've verified. For those tools the web wizard shows a per-session **Use structured view** toggle (on by default). Tools not in the set, and custom agents without an ACP command, have no toggle and always run in the terminal view.

| Agent | ACP adapter | Install | Auth |
|-------|-------------|---------|------|
| `claude` | `claude-agent-acp` (Zed, requires ≥0.39.0) | `npm install -g @agentclientprotocol/claude-agent-acp@latest` | `claude login`, or `ANTHROPIC_API_KEY` |
| `codex` | `codex-acp` (Zed) | `npm install -g @zed-industries/codex-acp` | `OPENAI_API_KEY`, or ChatGPT login (local-only) |
| `opencode` | `opencode acp` (native) | `curl -fsSL https://opencode.ai/install \| bash` | `opencode auth` / provider env |
| `gemini` | `gemini --acp` (native) | `npm install -g @google/gemini-cli` | `GEMINI_API_KEY`, OAuth, or Vertex |
| `vibe` | `vibe-acp` (native) | see [mistral-vibe](https://github.com/mistralai/mistral-vibe) | Mistral API key |
| `pi` | `pi-acp` (adapter) | `npm install -g pi-acp` (plus `@earendil-works/pi-coding-agent`) | `pi-acp --terminal-login`, or provider env |
| `aoe-agent` | bundled (Vercel AI SDK 6) | ships with `aoe` | provider env vars |

Tools not yet wired into the registry (aider, cursor, copilot, droid, hermes, kiro) always run in the terminal view. A **custom agent** can opt in by setting an ACP launch command via `agent_acp_cmd` (see [Configuration](guides/configuration.md#running-a-custom-agent-in-the-structured-view)).

The structured view always forwards `ANTHROPIC_API_KEY`, `ANTHROPIC_AUTH_TOKEN`, `CLAUDE_CODE_OAUTH_TOKEN`, and `CLAUDE_CONFIG_DIR` to the agent. For other agents, set their auth env in the environment that runs `aoe serve` (or the per-session `extra_env` field).

### Feature matrix

Each feature fires for any ACP agent, only when the agent's profile opts in, or claude-only.

| Feature | Claude | Codex | OpenCode | Gemini | Other ACP |
|---------|:------:|:-----:|:--------:|:------:|:---------:|
| Streaming text, tool-call cards, approvals | ✓ | ✓ | ✓ | ✓ | ✓ |
| Mode picker | ✓ | depends | ✓ | depends | depends |
| Slash-command palette | ✓ | depends | ✓ | ✓ | depends |
| Usage / context-window display | ✓ | depends | ✓ | ✓ | depends |
| `/clear` boundary divider | `/clear` | `/new` | `/new` | none | none |
| TodoWrite / Skill / ExitPlanMode / ScheduleWakeup cards | ✓ | — | — | — | — |
| Subagent indentation | ✓ | — | unverified | — | — |
| Session resume across `aoe serve` restart | ✓ | depends | ✓ | depends | depends |

Codex/opencode/gemini support is built from adapter docs and code reading rather than hands-on walkthroughs, so some tool aliases may need adjustment; file an issue with the observed `tool.kind` + `tool.name`. Mode picker, slash palette, and usage display depend on the adapter advertising the matching channels; when it doesn't, the UI stays empty rather than showing stale state. How profiles gate these is covered in [Structured View Internals](development/internals/structured-view.md#agent-profiles).

## Quickstart

The web new-session wizard is the primary path; no CLI needed.

1. Run `aoe serve` and open the dashboard.
2. Click **New session**, pick your project and agent, and leave **Use structured view** on.
3. Open the session: you see the structured plan and tool-call cards instead of a terminal.

The CLI is the optional path for scripting or headless launches. Unlike the wizard, `aoe add` defaults to the terminal view (matching the TUI):

```bash
aoe acp doctor                              # confirm prerequisites
aoe add . --cmd claude --structured-view    # structured view for an ACP tool
aoe add . --agent aoe-agent --model gpt-5   # pick an ACP agent + model (implies structured view)
```

`--agent` for an uninstalled adapter errors with an install hint; `--structured-view` (no `--agent`) falls back to the terminal view with a warning so the command still succeeds.

## Requirements

- aoe built with `--features serve`.
- Node.js 20+ on `PATH` (the structured view spawns an ACP agent subprocess; `aoe-agent` needs Node 20+ for Vercel AI SDK 6).
- For Claude Code, a `claude login` session.

If Node is missing or too old, the session falls back to the terminal view with an actionable warning. Verify with `aoe acp doctor`:

```bash
aoe acp doctor          # reports Node + each configured agent's reachability
aoe acp doctor --fix    # npm-install the npm-distributed adapters (claude / codex / pi)
```

It exits 1 if Node is missing, 2 if some agents are unreachable, else 0. Pass `--json` for machine-readable output. Install the native CLIs (opencode / gemini / vibe) through their own channels.

## Choosing the view per session

- **Web wizard:** defaults to the structured view; turn off **Use structured view** to get the terminal view.
- **CLI / TUI:** default to the terminal view. From the CLI, opt in with `--structured-view` or `--agent`.
- Either way, switch an existing session from the session view at any time. The agent restarts in a fresh pane; the worktree, open files, and commits are preserved, but the in-memory conversation for that session resets.

Non-ACP tools always run in the terminal view, with no toggle.

### Launch command and session naming

`--cmd <tool>` resolves through `session.agent_command_override` the same as terminal sessions, so an override like `opencode = "opencode-plannotator"` makes `--cmd opencode` launch `opencode-plannotator acp` (the required ACP args are preserved). Adapter-backed agents such as Claude use `session.agent_acp_cmd` for a full command swap instead. The wizard shows the resolved launch command read-only.

`aoe add` does not prompt for a name by default: it uses `--title`, else the worktree branch name, else a generated name. Pass `-i`/`--interactive` for the same name prompt the TUI and wizard show. Set per-agent defaults for web-created sessions under `[session.acp_defaults.<agent>]`:

```toml
[session.acp_defaults.opencode]
model = "openai/gpt-5.5"
effort = "high"
```

The `[acp]` block holds the structured view's global tuning knobs (timeouts, concurrency, watchdog grace). See [Structured View Internals](development/internals/structured-view.md#global-tuning-acp) for the full list.

## Cross-machine attach

Set `AOE_DAEMON_URL` (and optionally `AOE_DAEMON_TOKEN`) to point at a remote `aoe serve`:

```sh
AOE_DAEMON_URL=https://aoe.example.com AOE_DAEMON_TOKEN=… aoe   # remote session picker
aoe acp attach <session_id> --daemon-url https://aoe.example.com
```

When `AOE_DAEMON_URL` is set, the TUI swaps the local home view for a remote session picker, and `aoe serve --status` / the `aoe acp *` verbs retarget to the remote. Local-only operations (tmux attach, `aoe stop`, file edit) aren't available against a remote; use the web dashboard or SSH into the host. Unset the variable to fall back to local introspection.

## Headless CLI verbs

Every structured-view operation has a matching `aoe acp <verb>` against the same daemon:

| Verb | What it does |
|------|--------------|
| `aoe acp history <id>` | Dump the persisted transcript |
| `aoe acp status <id>` | Print highest/lowest seq and the daemon source |
| `aoe acp prompt <id> <text>` | Send a prompt (`-` reads stdin) |
| `aoe acp approve <id> <nonce> [--always\|--deny]` | Resolve a pending approval |
| `aoe acp cancel <id>` | Cancel the in-flight prompt |
| `aoe acp tail <id>` | Stream broadcast frames as JSON lines |
| `aoe acp attach <id>` | Open the TUI structured view for this session |
| `aoe acp ps` / `stop` / `kill` / `restart` / `logs` / `switch-agent` | Worker management |

Every verb requires a running `aoe serve` daemon and exits with a hint if none is found. Start one with `aoe serve --daemon` (localhost) or `aoe serve --daemon --remote` (Tailscale/Cloudflare), or set `AOE_DAEMON_URL`. The CLI does not spawn a daemon on your behalf, so the localhost-vs-tunnel choice stays explicit.
