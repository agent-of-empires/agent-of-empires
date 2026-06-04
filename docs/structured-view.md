# Structured View (Web Dashboard)

The **structured view** is the web dashboard's default rendering for AI coding
agents. Instead of viewing the agent through a terminal pane (PTY bytes
piped through xterm.js), the structured view renders the agent's structured
state directly: plan, tool calls, diffs, and approvals. It is mobile-first,
with a desktop layout that scales the same components into a richer
multi-pane view.

Any ACP-capable agent uses the structured view by default. A session can opt
into the **terminal view** (the raw tmux/PTY rendering) instead, per
session, and you can switch between the two from the session view at any
time. Agents with no ACP adapter always run in the terminal view.

The structured view speaks the [Agent Client Protocol](https://agentclientprotocol.com/)
(ACP), a JSON-RPC standard for editor-agent communication. aoe is the
*client*; the agent (Anthropic's Claude Code, our `aoe-agent`, Google's
Gemini CLI, etc.) is the *server*. Any ACP-conformant agent works.

![The structured view rendering an agent's plan, tool-call cards, and a pending approval](assets/structured-view/overview.png)

## In this section

- **[Setup](structured-view/setup.md)**: requirements, `aoe acp doctor`, choosing the structured view or terminal view per session, escape hatches, cross-machine attach, and the headless CLI verbs.
- **[Interface](structured-view/interface.md)**: the TUI and web structured views, keybinds, composer behavior, queued prompts, and timeline card grouping.
- **[Modes, approvals & model controls](structured-view/controls.md)**: permission modes, YOLO, approval cards, notifications, and the model / reasoning-effort selectors.
- **[Persistence & recovery](structured-view/persistence.md)**: worker survival across `aoe serve` restart, session deletion, and conversation persistence.
- **[Troubleshooting](structured-view/troubleshooting.md)**: the security model plus a field guide to every failure mode and its fix.
- **[Multi-agent support](structured-view/multi-agent.md)**: per-agent feature matrix and how the ACP profile resolves.

## Supported agents

aoe ships a registry entry for each tool whose ACP server we've verified
against [agentclientprotocol.com](https://agentclientprotocol.com/get-started/agents.md).
For tools in this set, the web wizard shows a per-session **Use structured view**
toggle (on by default), so you can opt a single session down to the terminal
view. Tools not in this set, and custom agents without an ACP command, have
no toggle and always run in the terminal view.

| aoe tool   | ACP adapter (structured view)                                   | Auth                                   |
|------------|------------------------------------------------------------|----------------------------------------|
| `claude`   | `claude-agent-acp` (Zed adapter for the Claude SDK, requires >=0.39.0) | `claude /login` writes `~/.claude/credentials`; or `ANTHROPIC_API_KEY` |
| `opencode` | `opencode acp` (native, SST)                               | `OPENCODE_API_KEY` env var; or provider-specific env (set up via `opencode auth`) |
| `gemini`   | `gemini --acp` (native, Google)                            | `GEMINI_API_KEY` env var, OAuth via `gemini auth`, or Vertex `GOOGLE_API_KEY` |
| `codex`    | `codex-acp` (Zed adapter, npm `@zed-industries/codex-acp`) | `OPENAI_API_KEY` env var, or ChatGPT login (local-only) |
| `vibe`     | `vibe-acp` (native, Mistral)                               | Mistral API key; set up via `vibe` first |
| `pi`       | `pi-acp` (adapter, requires `@earendil-works/pi-coding-agent`) | `pi-acp --terminal-login` for OAuth, or env vars per provider |
| `aoe-agent`| Bundled multi-provider agent (Vercel AI SDK 6)             | Whatever provider env vars Vercel AI SDK expects |
| *aider, cursor, copilot, droid, settl, hermes* | not yet wired into the ACP registry; always run in the terminal view |

A **custom agent** can use the structured view too: give it an ACP launch command via `agent_acp_cmd` in config (or the TUI settings screen). See [Running a custom agent in the structured view](guides/configuration.md#running-a-custom-agent-in-the-structured-view). The wizard reads each agent's `acp_capable` flag from the server, so a custom agent with an `agent_acp_cmd` offers the structured view just like a built-in; without one it stays terminal-only.

The four env vars the structured view always forwards to the agent process are
`ANTHROPIC_API_KEY`, `ANTHROPIC_AUTH_TOKEN`, `CLAUDE_CODE_OAUTH_TOKEN`,
`CLAUDE_CONFIG_DIR`. For the others, set them in the env that runs
`aoe serve` (or use the per-session `extra_env` field) and the agent's
own auth path will pick them up via the forwarded `HOME`.

For the full per-agent feature matrix, see [Multi-agent support](structured-view/multi-agent.md).

## Quickstart

The web new-session wizard is the primary way to start a session. You do
not need the CLI.

1. Run `aoe serve` and open the dashboard.
2. Click **New session**, pick your project, and choose an agent. Leave
   the **Use structured view** toggle on (it defaults on for ACP-capable agents).
3. Create and open the session: you see the structured plan and tool-call
   cards instead of a terminal.

A first-time mobile user pointed at a remote `aoe serve` will install
the PWA, tap the session, and see the plan panel render the moment the
agent emits its first plan event.

The CLI is the optional path for scripting or headless launches:

```bash
# Confirm prerequisites: aoe, Node.js >= 20, claude login.
aoe acp doctor

# Create a Claude Code session. `aoe add` defaults to the terminal view (like
# the TUI); pass --structured-view (or --agent) to opt into the structured view.
aoe add . --cmd claude --structured-view
```

Full setup detail, including the prerequisites check and how to choose the
structured view or terminal view per session, lives in [Setup](structured-view/setup.md).

## Tool compatibility

| Tool          | Structured view?  | Notes                                              |
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
(tmux + PTY) in the terminal view; the structured view is additive.

## What's deferred

These are tracked for follow-up releases:

- Mid-token interrupt (waiting on Anthropic's stable feature).
- Syntax highlighting for code blocks in the TUI transcript (today they
  render as a dim, uncolored block).
- Plan-mode and elicitation event mappings (the SDK supports them; the
  structured view's typed schema covers the common path).
- Cross-agent handoff and unified search across structured-view sessions.
- Voice input/output on mobile.
- A read-only structured-view transcript inside the TUI (today the TUI
  shows a `[web]` badge and an "open in dashboard" hint).
- Native unix-socket transport for in-container agents that natively
  speak the socket protocol. Today the sandbox path uses `docker exec`
  to keep stdio-only agents working without upstream changes.
