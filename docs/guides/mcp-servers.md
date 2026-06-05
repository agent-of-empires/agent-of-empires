# MCP Servers

Agent of Empires forwards your configured [MCP](https://modelcontextprotocol.io)
servers to structured-view agents (Claude, Gemini, Codex) when a session
starts, so the agent can call those servers' tools. Without this, structured-view
sessions reach no MCP servers at all.

This applies to structured-view / ACP sessions only. tmux sessions run the
agent's own CLI, which loads MCP config through that tool's normal mechanism.

## Configuration

Create `mcp.json` in your AoE app directory:

- **Linux**: `$XDG_CONFIG_HOME/agent-of-empires/mcp.json` (defaults to
  `~/.config/agent-of-empires/mcp.json`)
- **macOS / Windows**: `~/.agent-of-empires/mcp.json`

Debug builds use the `agent-of-empires-dev` namespace instead.

The file uses the standard `.mcp.json` shape, the same `mcpServers` object
Claude, Gemini, and Codex already understand, so you can reuse definitions you
keep elsewhere:

```json
{
  "mcpServers": {
    "filesystem": {
      "command": "mcp-server-filesystem",
      "args": ["--root", "/home/me/projects"],
      "env": { "LOG_LEVEL": "info" }
    },
    "github": {
      "type": "http",
      "url": "https://api.example.com/mcp",
      "headers": { "Authorization": "Bearer ghp_..." }
    }
  }
}
```

Each entry is one of:

- **stdio** (default when `type` is omitted): `command` is required; `args` and
  `env` are optional. The agent launches the executable and speaks MCP over its
  stdio.
- **http** (`"type": "http"`): `url` is required; `headers` is optional.
- **sse** (`"type": "sse"`): `url` is required; `headers` is optional.

The same list is forwarded for fresh and resumed sessions.

## Per-profile servers

A profile can carry its own `mcp.json` that adds to, or overrides, the global
one. Create it in the profile's directory:

- `<app_dir>/profiles/<profile-name>/mcp.json`

It uses the exact same `mcpServers` shape as the global file. When a
structured-view session runs under a profile, AoE reads that profile's
`mcp.json` and merges it on top of the global file: a server name defined in
both is taken from the per-profile file (see Precedence below). A missing
per-profile file is normal and simply forwards nothing extra.

Per-profile entries are AoE state only. AoE never writes them back into any
agent's native config; the sync direction is native into AoE, never the
reverse.

## Project-local servers (trusted repos)

A repository can ship its own MCP servers in a `.mcp.json` at its root, the same
ecosystem-standard file other tools read. AoE forwards these as the
highest-precedence layer, but only after you have trusted the repository, because
a project-local stdio server would otherwise launch its `command` the moment a
session starts: opening a cloned, untrusted repo would be a zero-click way to run
its code. This is the same trust gate AoE already applies to repository lifecycle
hooks.

When you create a session for a repo whose `.mcp.json` (or hooks) you have not
yet approved, AoE shows a trust prompt listing the servers it found: each
server's name, transport, command and arguments or URL, and the NAMES of its env
vars and headers. Values are never shown. Approving records the trust; declining
creates the session without forwarding the project servers.

The file is read from the repository root (for a worktree session, the main
repository the worktree was created from), so the servers you reviewed in the
prompt are exactly the servers forwarded. The trust is re-checked on every
session start: if `.mcp.json` changes, AoE re-prompts the next time you create a
session for that repo, and in the meantime the changed servers are skipped.

Two current limitations:

- The trust prompt exists in the TUI and the `aoe add` CLI only. Sessions created
  from the web dashboard cannot approve project MCP yet, so their project-local
  `.mcp.json` is skipped (with a log notice) until you approve the repo from the
  TUI or CLI. A web trust surface is tracked separately.
- Per-worktree or per-branch `.mcp.json` divergence is not supported: the main
  repository's file is the one read. Use a per-profile `mcp.json` for servers
  that should differ per worktree.

## Native agent config

If you already declared MCP servers in your agent's own config, AoE reads them
too (read-only), so you do not have to copy them into `mcp.json`. The native
config read per agent:

- **Claude**: `~/.claude.json` (top-level `mcpServers`).
- **Gemini**: `~/.gemini/settings.json` (`mcpServers`; transport is chosen by
  which key the entry sets, `command` for stdio, `httpUrl` for http, `url` for
  sse).
- **Codex**: `~/.codex/config.toml` (`[mcp_servers.<name>]` tables).

## Precedence

When the same server name appears in more than one source, the higher-precedence
source wins (per server, not whole file):

```text
agent-native  <  mcp.json (global)  <  per-profile mcp.json  <  project-local .mcp.json (trusted)
```

So a server defined in both your agent's native config and the global `mcp.json`
is taken from `mcp.json`; one defined in both the global and per-profile files is
taken from the per-profile file; and a trusted project-local server outranks all
of them. Each override is logged. The project-local layer only participates once
the repository is trusted (see Project-local servers above).

## Inspecting the effective set

Because the effective set is merged from up to four layers, AoE gives you one
surface to see exactly which servers an agent will reach and where each came
from. Every value is redacted: you see a server's command, args, or URL, and the
names of its env vars and headers, but never their secret values.

### CLI

```text
aoe mcp list                 # effective set for the default tool
aoe mcp list --agent gemini  # for a specific agent
aoe mcp list --json          # machine-readable, same redaction
```

Each row shows the server name, transport, its winning provenance
(`agent-native:claude`, `global`, `profile:<name>`, `project-local`), and which
lower layers it shadowed on a name collision.

### Web dashboard

The dashboard has an **MCP servers** tab under Settings (when running
`aoe serve`). It shows the same merged set with provenance, plus two things the
CLI surfaces read-only:

- **Conflicts.** AoE remembers the last definition it saw for each server in an
  agent's native config. If that file changes on disk, the next time you open the
  surface the changed server is flagged as a conflict, and you choose which side
  wins. Keeping AoE's version stores it in the global `mcp.json` (which outranks
  the native layer); choosing the native version simply accepts the new
  definition. AoE never writes back to an agent-native config.
- **Kept after removal.** If a server disappears from a native config, AoE keeps
  it in view and warns rather than silently dropping it. You can **keep** it
  (promoting it into the global `mcp.json` so it keeps forwarding) or **drop** it.

AoE remembers this last-seen state in `mcp_state.json` in the app directory,
written owner-only. The stored definitions keep their secret values (so a kept
server still works) but those values are redacted everywhere they are displayed.

Live connection status and reconnect / authenticate actions are tracked
separately and are not part of this surface yet.

## Capability gating

Not every agent supports every transport. `stdio` works everywhere. `http` and
`sse` servers are forwarded only when the agent advertises support for them in
its handshake; otherwise that server is dropped (with a warning in the log) so
AoE never sends a request the agent would reject.

## Errors

A missing `mcp.json` (or native config) is normal and forwards nothing. A
malformed file, or a single broken entry inside one, is logged as a warning and
skipped without blocking your sessions. Check `debug.log` in the app directory
if a configured server does not show up.

## Security

`mcp.json` lives in your app directory and is owned by you, so its `command`
entries and any secrets in `env` / `headers` stay out of source control. Treat
it like any file that can launch processes on your behalf: a stdio server runs
its `command` locally when a session starts.

A per-profile `mcp.json` lives in the profile directory under your app
directory, so it is owned by you with the same trust as the global file. Treat
it the same way: its `command` entries can launch processes on your behalf.

The drift store `mcp_state.json` (used by the management surface) also lives in
your app directory, owner-only. It records the last-seen definition of each
agent-native server, including secret values, so keep-on-removal and conflict
resolution can reconstruct a working server; treat it with the same care as
`mcp.json`. Those values are redacted on every surface that displays them.

A project-local `.mcp.json` is repository-provided, so unlike the files above it
is NOT implicitly trusted: a cloned, untrusted repo could otherwise launch its
`command` the moment you open a session. It sits behind the same repo-trust gate
AoE uses for lifecycle hooks, forwarded only after you approve the repo, and
re-checked on every session start so a changed file re-prompts. See Project-local
servers above. The trust fingerprint includes env and header values, so rotating
a secret in a project `.mcp.json` re-prompts; the prompt itself never displays
those values.
