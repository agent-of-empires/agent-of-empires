# Structured View Quickstart

The default AoE session is a **Structured view** session running the bundled `aoe-agent`
adapter against **Otari**, Mozilla AI's model gateway. Nothing global gets installed, no
`[acp]` section has to be hand-written, and the only thing you supply is a provider key.

```bash
aoe setup
```

`aoe setup` prints every setting it wrote, how to undo it, and starts nothing until you
create a session:

```bash
aoe add . --agent aoe-agent --structured-view \
  --model openai-compatible:anthropic/claude-fable-5
```

The model id is what routes the turn through Otari; without one the adapter falls back to
its Anthropic default. [Choosing the model](#choosing-the-model) has the other forms.

## What `aoe setup` does

| Step | Effect |
|------|--------|
| Detect | Reads what `aoe agents` and `aoe acp doctor` already report: which agents resolve on this machine, and whether the bundled adapter is installed. |
| Default the agent | Writes `acp.default_agent = "aoe-agent"`, the one adapter AoE ships and maintains, while the value is still the built-in `claude-code`. A value you already set is kept, and `--agent` is how you change it. |
| Default the view | Writes `acp.structured_view = "structured"` while it is still `auto`. `terminal` and `off` are left alone. |
| Install the adapter | Offers the bundled `aoe-agent` adapter with consent. It installs into the data dir with bundled Node's npm, exactly as `aoe acp doctor --fix --adapter aoe-agent` does. |
| Otari guidance | With no gateway credential present, prints the account steps below. Setup never blocks on it. |

Flags:

- `--quick` never prompts. The adapter is installed only with `--install-adapter` or an
  `acp.allow_agent_install = true` already in `config.toml`; otherwise the install command is
  printed.
- `--install-adapter` is the consent, so an unattended run can install in one go.
- `--agent <name>` defaults to an agent that is already on this machine instead of the
  bundled one. Reusing `claude`, `codex`, `opencode` and friends keeps their existing login
  and needs no provider key of its own.

Re-running is safe: a setting already at the target value is not written again, and a run
without `--agent` never overwrites a default you chose (including one written by an earlier
`--agent` run). To undo, edit `config.toml` (see
`aoe settings explain acp.default_agent`) and set the key back to `claude-code` or `auto`.

## Create an Otari account

`aoe-agent` needs a provider key; Otari is the gateway AoE defaults to.

1. Create a free account with GitHub, Google or email: https://otari.ai/dashboard/#/signup
2. Create an API key in the dashboard.
3. Export it where `aoe serve` runs, then re-run `aoe setup`:
   ```bash
   export OTARI_API_KEY="paste-the-key-here"
   ```
4. Pin the model on the session, using an id from Otari's catalogue:
   ```bash
   aoe add . --agent aoe-agent --structured-view \
     --model openai-compatible:anthropic/claude-fable-5
   ```

Otari serves an OpenAI-compatible API at `https://api.otari.ai/api/v1` (the `/api` prefix is
part of the path). Its model ids are vendor-namespaced, so a gateway id always carries the
`openai-compatible:` prefix described below.

- Getting started: https://docs.otari.ai/getting-started/quickstart
- Support: Mozilla AI Discord at https://discord.gg/4gf3zXrQUc

`OTARI_API_KEY` and `OTARI_BASE_URL` are on the `aoe-agent` provider allowlist, so the Daemon
passes them to the adapter without an `environment` override. Neither is ever printed.

## Choosing the model

The model id selects the provider. `aoe-agent` reads it from `AOE_AGENT_MODEL`, which AoE
sets per session from `--model`, the model picker, or `acp.acp_defaults`; the Daemon forwards
only allowlisted provider variables, so exporting `AOE_AGENT_MODEL` in the shell does not
reach the adapter. The forms, all still supported:

| Model id | Provider | Request model |
|---|---|---|
| `claude-opus-4-7` | Anthropic | `claude-opus-4-7` |
| `anthropic:claude-sonnet-4-5` | Anthropic | `claude-sonnet-4-5` |
| `gpt-5` | OpenAI | `gpt-5` |
| `openai:gpt-5-mini` | OpenAI | `gpt-5-mini` |
| `gemini-2.5-pro` | Google | `gemini-2.5-pro` |
| `google:gemini-2.5-flash` | Google | `gemini-2.5-flash` |
| `openai-compatible:anthropic/claude-fable-5` | OpenAI-compatible gateway | `anthropic/claude-fable-5` |
| anything else | Anthropic, unchanged | the id as given |

An `openai-compatible:` id keeps everything after the prefix as the request's `model`, so
vendor-namespaced gateway ids need no rewriting. The endpoint comes from `OTARI_BASE_URL`
when set, which is how a self-hosted gateway works, and otherwise from Otari. The key comes
from `OTARI_API_KEY`. The endpoint is per adapter, not per session, so it is environment, not
a setting.

## Requirements

- Node.js 22.6 or newer for `aoe-agent`, which runs from TypeScript sources with
  `--experimental-strip-types`. `aoe acp doctor --fix` downloads the pinned runtime when none
  qualifies.
- A provider key: Otari via `OTARI_API_KEY`, or `ANTHROPIC_API_KEY`, `OPENAI_API_KEY` or
  `GOOGLE_GENERATIVE_AI_API_KEY`.

See [Structured View](../structured-view.md) for the full agent list, provider allowlists and
troubleshooting.
