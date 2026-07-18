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

aoe ships an ACP registry entry for each tool whose ACP server we've verified. For those tools the web wizard shows a per-session **Use structured view** toggle (on by default), under the wizard's **More options** disclosure. Tools not in the set, and custom agents without an ACP command, have no toggle and always run in the terminal view.

| Agent | ACP adapter | Install | Auth |
|-------|-------------|---------|------|
| `claude` | `claude-agent-acp` (Zed, recent version required) | `npm install -g @agentclientprotocol/claude-agent-acp@latest` | `claude login`, or `ANTHROPIC_API_KEY` |
| `codex` | `codex-acp` (ACP) | `npm install -g @agentclientprotocol/codex-acp@latest` | `OPENAI_API_KEY`, or ChatGPT login (local-only) |
| `opencode` | `opencode acp` (native, ≥1.16.0 recommended) | `curl -fsSL https://opencode.ai/install \| bash` | `opencode auth` / provider env |
| `gemini` | `gemini --acp` (native) | `npm install -g @google/gemini-cli` | `GEMINI_API_KEY`, OAuth, or Vertex |
| `vibe` | `vibe-acp` (native) | see [mistral-vibe](https://github.com/mistralai/mistral-vibe) | Mistral API key |
| `pi` | `pi-acp` (adapter) | `npm install -g pi-acp` (plus `@earendil-works/pi-coding-agent`) | `pi-acp --terminal-login`, or provider env |
| `kimi` | `kimi acp` (native) | `curl -fsSL https://code.kimi.com/kimi-code/install.sh \| bash` | `kimi login`, or provider env |
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

Codex/opencode/gemini support is built from adapter docs and code reading rather than hands-on walkthroughs, so some tool aliases may need adjustment; file an issue with the observed `tool.kind` + `tool.name`. opencode ≥1.16.0 is recommended: it classifies `apply_patch` as `edit` and `task` as `think`, populates `external_directory` permission context, and emits clean read-tool content. Older opencode still works but falls back to generic tool cards, verbose read text, and blind permission prompts. Mode picker, slash palette, and usage display depend on the adapter advertising the matching channels; when it doesn't, the UI stays empty rather than showing stale state. Codex currently does not advertise a Plan-mode channel, so AoE does not render a Codex Plan switch. How profiles gate these is covered in [Structured View Internals](development/internals/structured-view.md#agent-profiles).

## Quickstart

The web new-session wizard is the primary path; no CLI needed.

1. Run `aoe serve` and open the dashboard.
2. Click **New session**, pick your project and agent, and launch. Structured view is on by default; to confirm or change it, expand **More options** and leave **Use structured view** on.
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
- **CLI / TUI:** default to the terminal view. From the CLI, opt in with `--structured-view` or `--agent`; in the TUI new-session dialog, toggle the **Structured** field (shown for ACP-capable tools).
- Either way, an existing active session can switch views: the web sidebar's right-click menu (**Switch to terminal** / **Switch to structured view**) or the TUI's right-click context menu (needs a running `aoe serve` daemon; archived, trashed, and still-creating rows are excluded until they leave that state). Both surfaces confirm first. The worktree, open files, and commits are always preserved. For a **claude** session the conversation is kept in both directions: the terminal resumes it with `claude --resume`, and switching back to structured view reloads it via the ACP adapter. Every other agent restarts fresh on the target surface, resetting the in-memory conversation.

Non-ACP tools always run in the terminal view, with no toggle.

### Launch command and session naming

`--cmd <tool>` resolves through `session.agent_command_override` the same as terminal sessions, so an override like `opencode = "opencode-plannotator"` makes `--cmd opencode` launch `opencode-plannotator acp` (the required ACP args are preserved). Adapter-backed agents such as Claude use `session.agent_acp_cmd` for a full command swap instead. The wizard shows the resolved launch command read-only.

`aoe add` does not prompt for a name by default: it uses `--title`, else the worktree branch name, else a generated name. Pass `-i`/`--interactive` for the same name prompt the TUI and wizard show. Set per-agent defaults for web-created sessions under `[acp.acp_defaults.<agent>]`:

When a structured view session keeps its generated civilization name (no `--title`, no branch name), AoE auto-renames it from its first turn using the session's own agent in one-shot mode (`claude -p`, `codex exec`, `opencode run`, `gemini -p`). This is on by default and controlled by `session.smart_rename`. It renames the title only, never the worktree directory (the running agent holds it), and never touches a session you named yourself. Sandboxed sessions, agents with no one-shot mode, and command-overridden agents keep the generated name. See [Configuration: Session](guides/configuration.md#session).

The rename waits for the first turn to finish and titles from the whole transcript, your prompt and the agent's response, so the title reflects what the turn did (and the one-shot never races the live agent for the provider API).

To name with a different agent than the session's own (e.g. a cheaper or more obedient title model), set `session.smart_rename_agent` to any installed one-shot-capable agent; leave it empty to use the session's agent. The same setting also picks the agent for the conversation-summary one-shot. If the automatic rename never lands (the one-shot timed out or returned unusable output), right-click the session in the sidebar and pick "Auto-name now" to re-run it; the action is offered only while the session is still default-named.

The sidebar shows where each session stands: an `Auto-name` chip (sparkle) marks a session that is still default-named and will be renamed on its first message, and a `Naming…` chip (pulsing dot) shows while the one-shot title call is in flight. The chips disappear once the session is renamed or if it is not eligible.

Two chips flag a session that has parked itself but is still alive, so an agent waiting on background work does not read as a dead idle session. A `⏰` countdown shows when the agent scheduled a wakeup (a `ScheduleWakeup` call or a `/loop` run) and ticks down to the fire time. A `👁 monitoring` badge shows when the agent armed a `Monitor` (a background watch, for example waiting for a build or `cargo clippy` to finish); it has no fixed end time, so it stays put while the monitor keeps re-invoking the agent and clears once you send the session a new prompt.

```toml
[acp.acp_defaults.opencode]
model = "openai/gpt-5.5"
effort = "high"           # default thinking level
mode = "plan"             # default mode, applied when the agent advertises one

# Per-model thinking: overrides `effort` when that model is the resolved model.
[acp.acp_defaults.opencode.effort_by_model]
"openai/gpt-5.5" = "high"
"anthropic/opus" = "low"
```

`model` is forwarded when the worker starts. `effort` and `mode` are applied through the agent's ACP config options (`thought_level` and `mode`) once advertised; a value the agent does not advertise is skipped with a warning rather than failing the session. `effort_by_model` takes precedence over the flat `effort` when the resolved model matches a key. These defaults are also editable per agent from the web dashboard settings (Structured view tab, Structured View Defaults), where the dropdowns are populated from whatever each agent last advertised.

The `[acp]` block holds the structured view's global tuning knobs (timeouts, concurrency, watchdog grace). See [Structured View Internals](development/internals/structured-view.md#global-tuning-acp) for the full list.

## Agent artifacts

Each session gets a managed artifact directory, exposed to the agent as `AOE_ARTIFACT_DIR` (bind-mounted at `/aoe/artifacts` inside a sandbox). A screenshot or status file the agent writes there is served over an authenticated, session-scoped route and opens in the dashboard: transcript links open the file in a new tab, and markdown images render inline. Files written elsewhere (an arbitrary `/tmp` path, for example) cannot be served and render as plain, non-clickable text. To make a generated artifact viewable, write it under `$AOE_ARTIFACT_DIR`.

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
