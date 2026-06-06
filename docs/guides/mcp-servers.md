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

## Native agent config

If you already declared MCP servers in your agent's own config, AoE reads them
too (read-only), so you do not have to copy them into `mcp.json`. The native
config read per agent:

- **Claude**: `~/.claude.json` (top-level `mcpServers`).
- **Gemini**: `~/.gemini/settings.json` (`mcpServers`; transport is chosen by
  which key the entry sets, `command` for stdio, `httpUrl` for http, `url` for
  sse).
- **Codex**: `~/.codex/config.toml` (`[mcp_servers.<name>]` tables).

When the same server name appears in both sources, `mcp.json` wins (per server).

> **Note:** `http` and `sse` servers are forwarded only to agents that advertise
> support for them; otherwise that server is dropped. `stdio` works everywhere.

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

Project-local `.mcp.json` (read from a repository) and per-profile MCP config
are not supported yet.
