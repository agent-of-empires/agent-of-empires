# MCP Servers

AoE forwards your configured [MCP](https://modelcontextprotocol.io) servers to structured-view agents when a session starts. Without this, structured-view sessions reach no MCP servers at all. Terminal sessions run the agent's own CLI, which loads MCP config its own way.

## Configuration

Create `mcp.json` in the AoE app directory (`$XDG_CONFIG_HOME/agent-of-empires/mcp.json` on Linux, `~/.agent-of-empires/mcp.json` otherwise). It uses the standard `mcpServers` shape, so definitions you keep elsewhere can be reused verbatim:

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

Each entry is **stdio** (the default; `command` required, `args` and `env` optional), **http**, or **sse** (`url` required, `headers` optional). The same list is forwarded for fresh and resumed sessions.

## Layers and precedence

Servers come from up to four sources, merged per server name:

```text
agent-native  <  mcp.json (global)  <  profile mcp.json  <  project-local .mcp.json (trusted)
```

- **Agent-native config** is read (never written) so you need not copy servers in: `~/.claude.json` (`mcpServers`), `~/.gemini/settings.json` (transport chosen by which key the entry sets), and `~/.codex/config.toml` (`[mcp_servers.<name>]`, honoring Codex's `enabled` flag). Claude's directory is the one the launched CLI reads: a `session.agent_config_dir` entry, then `CLAUDE_CONFIG_DIR` from the session's environment, then from AoE's own, then `~`.
- **`<app_dir>/profiles/<name>/mcp.json`** adds to or overrides the global file for sessions under that profile. A missing file is normal.
- **Project-local `.mcp.json`** at the repository root is highest, but only after you trust the repo (see below).

Each override is logged.

## Project-local servers need repo trust

A project stdio server would launch its `command` the moment a session starts, so opening a cloned, untrusted repo would be a zero-click way to run its code. It therefore sits behind the same trust gate as repository hooks.

Creating a session for a repo whose `.mcp.json` you have not approved shows a prompt listing each server's name, transport, command and arguments or URL, and the **names** of its env vars and headers. Values are never shown. Approving records the trust; declining creates the session without those servers. The trust fingerprint includes env and header values, so rotating a secret re-prompts, and trust is re-checked on every session start, with changed servers skipped until you approve again.

The file is read from the repository root (for a worktree session, the main repository it was created from), so the servers you reviewed are the servers forwarded. Two current limits: the prompt exists only in the TUI and `aoe add`, so sessions created from the dashboard skip project servers until you approve the repo elsewhere; and per-worktree divergence is unsupported, so use a per-profile `mcp.json` for servers that should differ per worktree.

## Inspecting the effective set

Every surface redacts values: you see a server's command, args, or URL and the names of its env vars and headers, never their secrets.

```text
aoe mcp list                 # effective set for the default tool
aoe mcp list --agent gemini  # for a specific agent
aoe mcp list --json
```

Each row shows the server name, transport, its winning provenance (`agent-native:claude`, `global`, `profile:<name>`, `project-local`), and which layers it shadowed.

The dashboard's **MCP servers** settings tab shows the same merged set plus two things the CLI does not:

- **Conflicts.** AoE remembers the last definition it saw in an agent's native config. If that file changes on disk, the changed server is flagged and you pick a side. Keeping AoE's version stores it in the global `mcp.json`, which outranks the native layer; choosing the native version accepts the new definition. AoE never writes back to an agent-native config.
- **Kept after removal.** A server that disappears from a native config is kept in view with a warning, so you can **keep** it (promoting it into the global `mcp.json`) or **drop** it.

That last-seen state lives in an owner-only `mcp_state.json` in the app directory. It stores secret values so a kept server still works, and redacts them everywhere they are displayed, so treat it as carefully as `mcp.json` itself: both can launch processes on your behalf.

Live connection status and reconnect or authenticate actions are not part of this surface yet.

## Capability gating and errors

`stdio` works everywhere. `http` and `sse` servers are forwarded only when the agent advertises support in its handshake, and are otherwise dropped with a log warning rather than sending a request the agent would reject.

A missing `mcp.json` or native config forwards nothing. A malformed file, or one broken entry inside it, is logged and skipped without blocking sessions; check `debug.log` in the app directory when a configured server does not appear.
