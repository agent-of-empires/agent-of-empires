# Cockpit Multi-Agent Support

AoE's cockpit (the web dashboard's structured-rendering substrate) speaks
the Agent Client Protocol (ACP) and supports every coding agent that ships
an ACP server. Claude has the most polished surface today; other agents
are first-class but lighter on per-tool polish.

## Supported Agents

| Agent | ACP entry | Install |
|-------|-----------|---------|
| Claude | `claude-agent-acp` (Zed adapter, requires >=0.39.0) | `npm install -g @agentclientprotocol/claude-agent-acp@latest` |
| Codex (OpenAI) | `codex-acp` (Zed adapter) | `npm install -g @zed-industries/codex-acp` |
| OpenCode (SST) | `opencode acp` (native) | `curl -fsSL https://opencode.ai/install \| bash` |
| Gemini (Google) | `gemini --acp` (native) | `npm install -g @google/gemini-cli` |
| Vibe (Mistral) | `vibe-acp` (native) | See https://github.com/mistralai/mistral-vibe |
| Pi | `pi-acp` (adapter) | `npm install -g pi-acp` (plus `@earendil-works/pi-coding-agent`) |
| aoe-agent | bundled | shipped with `aoe` |

Adding a new ACP-capable agent: see `docs/development/adding-agents.md`,
step 8 (Cockpit Profile).

## Feature Matrix

Each cockpit feature either fires for any ACP agent, fires only when the
agent's profile opts in, or is currently claude-only.

| Feature | Claude | Codex | OpenCode | Gemini | Other ACP |
|---------|:------:|:-----:|:--------:|:------:|:---------:|
| Streaming agent text | ✓ | ✓ | ✓ | ✓ | ✓ |
| Tool-call cards (`execute` / `read` / `edit` / `search` / `fetch`) | ✓ | ✓ | ✓ | ✓ | ✓ |
| Generic tool card fallback | ✓ | ✓ | ✓ | ✓ | ✓ |
| Permission / approval flow | ✓ | ✓ | ✓ | ✓ | ✓ |
| Mode picker | ✓ | depends | ✓ | depends | depends |
| Slash command palette | ✓ | depends | ✓ | ✓ | depends |
| Usage / context-window display | ✓ | depends | ✓ | ✓ | depends |
| MCP tool grouping | ✓ | claimed* | claimed* | claimed* | claimed* |
| `/clear` boundary divider | `/clear` | `/new` | `/new` | none | none |
| TodoWrite card | ✓ | — | — | — | — |
| Skill card | ✓ | — | — | — | — |
| ExitPlanMode synthesis | ✓ | — | — | — | — |
| ScheduleWakeup (`/loop`) | ✓ | — | — | — | — |
| Subagent indentation | ✓ | — | unverified | — | — |
| Session resume across `aoe serve` restart | ✓ | depends | ✓ | depends | depends |

\* All profiles default to the `mcp__` prefix. If your agent uses a
different MCP naming scheme, file an issue or PR adjusting the profile's
`mcpPrefixes`.

### Notes on the matrix

- **TodoWrite / Skill / ExitPlanMode / ScheduleWakeup** are claude-only
  tools today, so the cards stay quiet on other agents. The cockpit
  doesn't fire those cards based on coincidental tool names; gating
  happens in the agent profile.
- **`/clear`** is detected server-side by matching the user's prompt
  against the profile's `clearAliases`. Claude uses `/clear`; codex and
  opencode use `/new`. Gemini has no slash command verified as a
  conversation-clear boundary; `/restore` is a different semantic, so the
  cockpit doesn't treat it as a clear. The composer's `/` palette also
  surfaces each profile's clear aliases as suggestions, since the
  adapters' own `available_commands_update` channel does not always
  advertise them.
- **Subagent indentation** requires the adapter to emit a
  `_meta.<namespace>.parentToolUseId` field on child tool calls.
  claude-agent-acp emits `_meta.claudeCode.parentToolUseId`. OpenCode's
  `task` tool spawns subagents but its parent-linkage convention hasn't
  been verified, so the cockpit doesn't render the indent until the
  contract is observed.
- **Mode picker / slash palette / usage display** depend on whether the
  adapter advertises the matching channels (`available_modes`,
  `available_commands_update`, `usage_update`). When the adapter doesn't
  emit them, the UI simply stays empty rather than showing stale state.

## How the Profile Works

Each agent has two profile sources, kept aligned by registry key:

- **Server (Rust)**: `src/cockpit/agent_profiles.rs`. Carries
  `parent_meta_namespaces`, `clear_aliases`, and the
  `supports_exit_plan_mode` / `supports_wakeup_tools` capability gates.
- **Frontend (TypeScript)**: `web/src/lib/agentProfiles.ts`. Carries the
  card-classifier alias map (`shell` → execute card, `read_file` → read
  card, etc.), the claude-specialised capabilities (`todos`, `skills`,
  `wakeup`), the MCP prefix list, and the special-title patterns matched
  only when the capability is on.

Profile data is conservative on purpose: where an adapter's tool surface
hasn't been verified hands-on, the entry is omitted rather than guessed.
The cockpit then renders the generic tool card, which is the right
fallback. The user can file a PR adding the alias once they've used the
agent and confirmed the wire shape.

## Switching Agents on a Live Session

A cockpit session is not pinned to the agent it started with. You can hand it off to any other installed ACP backend at any time, keeping the transcript. The new agent starts fresh (a new ACP session) and is primed with a recap of the recent turns so it can pick up where the last one left off.

- **Web dashboard:** use the "Switch agent" control in the composer toolbar. The rate-limit recovery banner exposes the same picker via "Continue in another agent" when an agent is over its limit.
- **CLI:** `aoe cockpit switch-agent <session> <target>`, where `<target>` is a registry key from `aoe cockpit agents`. Add `--model <name>` to override the model.

This is what you reach for after a rate-limit hand-off: switch claude to codex when claude is limited, then `aoe cockpit switch-agent <session> claude` (or the toolbar control) to return once the window resets. The transcript records each switch with its reason (`manual` or `rate_limited`) on a divider.

## Known Limitations

- Codex / opencode / gemini cockpit support has been built from adapter
  docs and code reading rather than hands-on session walkthroughs. Some
  tool aliases may need adjustment once each agent has been exercised
  end-to-end. File an issue with the wire `tool.kind` + `tool.name`
  observed on the cockpit side and we'll update the profile.
- Gemini's `save_memory` tool lands on the generic card today. A dedicated
  card is a follow-up.
- Multimodal input (image upload in the composer) is not implemented;
  gemini is the canonical target when it lands.
- Runtime capability discovery from the ACP `InitializeResponse` isn't
  wired yet; profiles are the static source of truth. Runtime discovery
  is tracked separately.

## Diagnosing Profile Issues

If a tool call on a non-claude agent renders as a generic card when you
expect a specialised one:

1. Open browser devtools, find the tool-start WebSocket frame, note
   `tool.kind` and `tool.name`.
2. Check the agent's profile in `web/src/lib/agentProfiles.ts`. The
   alias map only fires when `tool.kind` is `"other"` or not a concrete
   card kind; if the agent sends a real `kind`, that drives dispatch
   directly.
3. If the cockpit fired a claude-only card (TodoCard / SkillCard) on a
   non-claude agent, that's a bug; file an issue with the wire shape.

For server-side gates (clear-boundary detection, plan / wakeup
synthesis), the debug log (`AGENT_OF_EMPIRES_DEBUG=1`) captures which
profile resolved for a session.
