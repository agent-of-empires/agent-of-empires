# Structured View (Web Dashboard)

The **structured view** is the default rendering for AI coding agents in the web dashboard and the native TUI. Instead of a terminal pane showing the agent's rendered screen, it renders the agent's state directly: plan, tool-call cards, diffs, and approvals. It is mobile-first and scales into a multi-pane desktop layout.

It speaks the [Agent Client Protocol](https://agentclientprotocol.com/) (ACP), a JSON-RPC standard for editor-agent communication, with aoe as the *client* and the agent as the *server*. Any ACP-capable agent uses it by default and can opt into the terminal view per session; agents with no ACP adapter always run in the terminal view.

![The structured view rendering an agent's plan, tool-call cards, and a pending approval](assets/structured-view/overview.png)

- **[Interface](structured-view/interface.md)**: surfaces, keybinds, composer, queued prompts, timeline grouping.
- **[Modes, approvals & model controls](structured-view/controls.md)**: permission modes, approval cards, notifications, model selectors.
- **[Troubleshooting](structured-view/troubleshooting.md)**: the security model and a field guide to each failure mode.
- Contributors: [Structured View Internals](development/internals/structured-view.md).

## Supported agents

aoe ships an ACP registry entry for each tool whose ACP server we have verified. For those the web wizard shows a per-session **Use structured view** toggle (on by default) under **More options**.

| Agent | ACP adapter | Install | Auth |
|-------|-------------|---------|------|
| `claude` | `claude-agent-acp` (Zed) | `npm install -g @agentclientprotocol/claude-agent-acp@latest` | `claude login`, or `ANTHROPIC_API_KEY` |
| `codex` | `codex-acp` | `npm install -g @agentclientprotocol/codex-acp@latest` | `OPENAI_API_KEY`, or ChatGPT login (local only) |
| `opencode` | `opencode acp` (native, ≥1.16.0 recommended) | `curl -fsSL https://opencode.ai/install \| bash` | `opencode auth` / provider env |
| `gemini` (deprecated) | `gemini --acp` (native) | `npm install -g @google/gemini-cli` | `GEMINI_API_KEY`, OAuth, or Vertex |
| `vibe` | `vibe-acp` (native) | see [mistral-vibe](https://github.com/mistralai/mistral-vibe) | Mistral API key |
| `pi` | `pi-acp` | `npm install -g pi-acp` (plus `@earendil-works/pi-coding-agent`) | `pi-acp --terminal-login`, or provider env |
| `omp` | `omp acp` (native) | `curl -fsSL https://omp.sh/install \| sh` | OMP login or provider env |
| `kimi` | `kimi acp` (native) | `curl -fsSL https://code.kimi.com/kimi-code/install.sh \| bash` | `kimi login`, or provider env |
| `prime-agent` | `prime-agent --mode acp` (native) | `curl -fsSL https://app.primeintellect.ai/prime-agent/install.sh \| sh` | `/login` once, or provider env |
| `aoe-agent` | bundled (Vercel AI SDK 7) | `aoe acp doctor --fix --adapter aoe-agent` | provider env vars |

Gemini CLI is deprecated upstream for individual accounts; enterprise and API-key auth remain valid, and Antigravity CLI is the consumer replacement.

The `npm install -g` lines are optional: `aoe acp doctor --fix` installs the `claude`, `codex`, and `pi` adapters and `aoe-agent` into the data dir instead (see [Requirements](#requirements)).

Tools not in the registry (aider, cursor, copilot, droid, hermes, kiro) always run in the terminal view. A **custom agent** opts in either with an explicit `agent_acp_cmd`, or by mapping onto a supported base with `agent_detect_as`, which inherits that base's adapter; see [Configuration](guides/configuration.md#running-a-custom-agent-in-the-structured-view).

### Provider credentials

Each built-in adapter receives only the provider variables it is known to read, from the environment that runs `aoe serve`:

| Adapter | Forwarded |
|---|---|
| `claude` | `ANTHROPIC_API_KEY`, `ANTHROPIC_AUTH_TOKEN`, `CLAUDE_CODE_OAUTH_TOKEN`, `CLAUDE_CONFIG_DIR` |
| `codex` | `CODEX_API_KEY`, `OPENAI_API_KEY`, `OPENAI_BASE_URL`, `CODEX_HOME` |
| `opencode` | `ANTHROPIC_API_KEY`, `OPENAI_API_KEY`, `GOOGLE_GENERATIVE_AI_API_KEY`, `GOOGLE_API_KEY`, `GEMINI_API_KEY`, `OPENROUTER_API_KEY`, `OPENCODE_API_KEY` |
| `gemini` | `GEMINI_API_KEY`, `GOOGLE_API_KEY`, `GOOGLE_GENAI_USE_VERTEXAI`, `GOOGLE_APPLICATION_CREDENTIALS`, `GOOGLE_CLOUD_PROJECT`, `GOOGLE_CLOUD_LOCATION` |
| `aoe-agent` | `ANTHROPIC_API_KEY`, `OPENAI_API_KEY`, `OPENAI_BASE_URL`, `GOOGLE_GENERATIVE_AI_API_KEY` |
| `prime-agent` | `PRIME_API_KEY`, `PRIME_TEAM_ID`, every provider key its model registry reads, and the Vertex, Bedrock, and AWS credential-chain variables (see `src/acp/agent_registry.rs`) |

`vibe`, `pi`, `omp`, `kimi`, and custom adapters have no ambient allowlist yet: give them auth through the session's `extra_env`, the `environment` list, or `session.inherit_host_environment` for host sessions. In a sandboxed session the per-adapter keys above still cross the container boundary, but `inherit_host_environment` does not, so use `sandbox.environment` there. Allowlist entries naming a file or directory (`CLAUDE_CONFIG_DIR`, `CODEX_HOME`, `GOOGLE_APPLICATION_CREDENTIALS`, the AWS file variables) are host-only and never cross, since each agent's config dir is already bind-mounted at its canonical container location.

### Feature matrix

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

"Depends" means the adapter has to advertise the matching channel; when it does not, the UI stays empty rather than showing stale state. Codex does not advertise a Plan-mode channel, so no Codex Plan switch is rendered. Older opencode falls back to generic tool cards, verbose read text, and blind permission prompts. If a tool renders as a generic card, file an issue with the observed `tool.kind` and `tool.name`; how profiles gate these is covered in [Structured View Internals](development/internals/structured-view.md#agent-profiles).

## Quickstart

1. Run `aoe serve` and open the dashboard.
2. Click **New session**, pick your project and agent, and launch. Structured view is on by default.
3. Open the session: you get the plan and tool-call cards instead of a terminal.

The CLI is the optional path for scripting. Unlike the wizard, `aoe add` defaults to the terminal view:

```bash
aoe acp doctor                              # confirm prerequisites
aoe add . --cmd claude --structured-view    # structured view for an ACP tool
aoe add . --agent codex --model gpt-5       # pick an ACP agent + model (implies structured view)
```

`--agent` for an uninstalled adapter errors with an install hint; `--structured-view` without `--agent` falls back to the terminal view with a warning, so the command still succeeds.

## Requirements

- Node.js 22+ on `PATH` (the structured view spawns the adapter as a subprocess); `aoe-agent` needs 22.6+.
- For Claude Code, a `claude login` session.

If Node is missing or too old, the session falls back to the terminal view with an actionable warning. Verify with `aoe acp doctor`:

```bash
aoe acp doctor                            # Node + each configured agent's reachability
aoe acp doctor --fix                      # bundled Node if missing, then claude-agent-acp
aoe acp doctor --fix --adapter codex-acp  # a specific adapter
aoe acp doctor --fix --all-adapters       # every bundled adapter
```

`--fix` installs a pinned npm adapter under `$AOE_DATA_DIR/acp-worker/adapters/<adapter>/` with the bundled Node's own npm: no `npm install -g`, no sudo. Each adapter is a separate several-hundred-MB tree (`pi-acp` is the exception at ~7 MB), so `--fix` installs only `claude-agent-acp` unless you ask for more. An adapter already on your `PATH` normally wins, unless it is below the version floor aoe requires, in which case aoe uses the pinned copy and logs the substitution. `doctor` exits 1 if Node is missing, 2 if some agents are unreachable, else 0; `--json` for machine-readable output. Install the native CLIs through their own channels.

## Choosing the view per session

- **Web wizard**: structured view by default (set [`acp.default_new_session_view`](guides/configuration.md) to change it); turn off **Use structured view** for the terminal.
- **CLI / TUI**: terminal view by default; opt in with `--structured-view` or `--agent`, or the **Structured** field in the TUI new-session dialog.
- **An existing session** can switch either way from the web sidebar's right-click menu or the TUI context menu (which needs a running `aoe serve`). Both confirm first, and the worktree, files, and commits are always preserved. For a **claude** session the conversation is kept in both directions (`claude --resume` one way, the ACP adapter the other); every other agent restarts fresh on the target surface.

### Launch command and session naming

`--cmd <tool>` resolves through `session.agent_command_override` like a terminal session, so an override makes `--cmd opencode` launch `opencode-plannotator acp` with the required ACP args preserved. Adapter-backed agents such as Claude use `session.agent_acp_cmd` for a full command swap. The wizard shows the resolved command read-only.

`aoe add` names a session from `--title`, else the worktree branch, else a generated name (`-i` prompts like the TUI and wizard do). A structured session still carrying a generated name is auto-renamed once its first turn finishes, by a one-shot call to the session's own agent, so the title reflects both your prompt and the response. See `session.smart_rename` in the [configuration reference](guides/configuration.md#session).

If the rename never lands, right-click the session and pick **Auto-name now** (TUI: `v` or the command palette), which works even with `smart_rename` off and regenerates the title even over one you picked yourself (the automatic rename never does). The sidebar shows an `Auto-name` chip while a session is still default-named and a `Naming…` chip while the call is in flight. Two more chips flag a session that parked itself but is still alive: a `⏰` countdown to a scheduled wakeup, and a `👁 monitoring` badge while an armed `Monitor` keeps re-invoking the agent, which clears when you send a new prompt.

Per-agent startup defaults live under `[acp.acp_defaults.<agent>]` (model, effort, mode, `effort_by_model`, `pin_model`) and are editable from the web settings; see [Configuration](guides/configuration.md#session). The rest of `[acp]` is in [Structured View Internals](development/internals/structured-view.md#global-tuning-acp).

## Agent artifacts

Each session gets a managed artifact directory, exposed as `AOE_ARTIFACT_DIR` (bind-mounted at `/aoe/artifacts` in a sandbox). Files the agent writes there are served over an authenticated, session-scoped route: transcript links open them in a new tab and markdown images render inline. Files written anywhere else render as plain text, so write anything you want viewable under `$AOE_ARTIFACT_DIR`.

## Cross-machine attach

Point `AOE_DAEMON_URL` (and optionally `AOE_DAEMON_TOKEN`) at a remote `aoe serve`:

```sh
AOE_DAEMON_URL=https://aoe.example.com AOE_DAEMON_TOKEN=… aoe   # remote session picker
aoe acp attach <session_id> --daemon-url https://aoe.example.com
```

With `AOE_DAEMON_URL` set, the TUI swaps its home view for a remote session picker and `aoe serve --status` and the `aoe acp *` verbs retarget to the remote. Local-only operations (tmux attach, `aoe stop`, file edit) are not available against a remote; use the dashboard or SSH. The session list is read with a bearer token only over HTTPS or a loopback URL, so a token plus a plaintext remote URL is refused; the other daemon requests do not apply that check yet, so use HTTPS or a tunnel.

## Headless CLI verbs

Every structured-view operation has a matching verb against the same daemon:

| Verb | What it does |
|------|--------------|
| `aoe acp history <id>` | Dump the persisted transcript |
| `aoe acp status <id>` | Print highest/lowest seq and the daemon source |
| `aoe acp prompt <id> <text>` | Send a prompt (`-` reads stdin) |
| `aoe acp approve <id> <nonce> [--always\|--deny\|--option <id>]` | Resolve a pending approval |
| `aoe acp cancel <id>` | Cancel the in-flight prompt |
| `aoe acp tail <id>` | Stream broadcast frames as JSON lines |
| `aoe acp attach <id>` | Open the TUI structured view |
| `aoe acp stop` / `kill` / `restart` / `logs` / `switch-agent` | Worker management |
| `aoe ps --acp` | List workers with their ACP columns; `--dead` includes dead and orphaned ones |

Every verb needs a running `aoe serve` and exits with a hint if none is found. The CLI never spawns a daemon on your behalf, so the localhost-versus-tunnel choice stays explicit.
