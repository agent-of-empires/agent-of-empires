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

The forwarded list is the same for fresh sessions (`session/new`) and resumed
ones (`session/load`).

## Capability gating

Not every agent supports every transport. `stdio` works everywhere. `http` and
`sse` servers are forwarded only when the agent advertises support for them in
its handshake; otherwise that server is dropped (with a warning in the log) so
AoE never sends a request the agent would reject.

## Errors

A missing `mcp.json` is normal and means no servers are forwarded, identical to
the behavior before this feature. A malformed `mcp.json` is logged as a warning
and no servers are forwarded, so a single typo never blocks your sessions from
starting. Check the log (`debug.log` in the app directory) if a configured
server does not show up.

## Security

`mcp.json` lives in your app directory and is owned by you, so its `command`
entries and any secrets in `env` / `headers` stay out of source control. Treat
it like any file that can launch processes on your behalf: a stdio server runs
its `command` locally when a session starts.

Project-local `.mcp.json` (read from a repository) and per-profile MCP config
are not supported yet. A repository-provided server config would let a cloned,
untrusted repo launch commands the moment you open a session, so it must sit
behind the same repo-trust gate AoE already uses for lifecycle hooks; that work
is tracked separately.
