# Controlling AoE over MCP

The daemon can serve a [Model Context Protocol](https://modelcontextprotocol.io) server, so an agent can work with AoE Sessions through typed tools instead of scraping `aoe` CLI output. This is separate from [MCP Servers](mcp-servers.md), which forwards your own servers to the agents AoE launches.

## Turn it on

The server is off by default. Turn on the **MCP server** toggle in Settings, or add this to `config.toml`:

```toml
[web]
mcp_enabled = true
```

The daemon reads this global setting at startup. With it enabled, a fresh CLI or TUI acquisition opens the existing TCP API on loopback with token authentication, unless the registered `aoe.web` plugin is disabled. This includes MCP over Streamable HTTP at `/mcp`. Builds with the `web` Cargo feature also serve the dashboard. It does not request a LAN bind or a tunnel. Passphrase and CityHall protections still apply.

The private local runtime and `aoe plugin enable aoe.web` remain available while that plugin is disabled. Re-enabling it does not promote an existing private daemon. With readable global configuration, explicit TCP starts remain rejected until the plugin is enabled.

For an already-running TCP daemon, use `aoe serve --restart` to apply the setting. Restart preserves the recorded launch mode: it does not promote a core-only daemon. To enable TCP on a running core-only daemon, explicitly run `aoe serve --daemon`. This replaces the managed daemon and disconnects its clients; reconnect them afterwards.

### When configured TCP cannot start

A configured TCP startup failure is reported without automatically starting a core-only daemon. For example, an occupied listener port makes CLI reads fail and leaves the TUI disconnected. AoE does not silently drop the configured MCP service to keep local reads working.

Free the port or repair the reported startup failure, then retry the CLI command or press `r` in the TUI to reconnect.

If global settings or application state cannot be loaded, fresh automatic acquisition fails instead of choosing default service settings. Explicit serving and restart retain their selected launch mode and existing configuration-recovery behavior. Explicit TCP recovery may use default runtime settings, including when an unreadable configuration contains a Web-disable setting. Use `--core-only` for recovery without TCP.

To work intentionally without TCP, run `aoe serve --core-only --daemon`, then retry the command or reconnect the TUI. MCP remains unavailable on that daemon. Restart preserves core-only mode; after freeing the port, run `aoe serve --daemon` to explicitly promote it back to TCP.

## Connect an agent

On the daemon's host, run:

```bash
aoe mcp config
```

It prints an `mcpServers` entry for the running daemon: its `/mcp` URL, preferring loopback when available, and, when the daemon uses token authentication, an `Authorization: Bearer` header carrying its current token.

```json
{
  "mcpServers": {
    "aoe": {
      "type": "http",
      "url": "http://127.0.0.1:8080/mcp",
      "headers": {
        "Authorization": "Bearer <token>"
      }
    }
  }
}
```

Paste it into the agent's MCP configuration, for example a Claude Code `.mcp.json` in your project. The output contains the daemon token, so keep it out of version control.

Before printing, the command asks the running daemon whether it serves `/mcp`, because the daemon applies `mcp_enabled` only at startup. If you turned the setting on after the daemon started, it warns that `/mcp` is not served yet and that `aoe serve --restart` applies it; if you turned it off, it notes that the route goes away at the next restart. If the daemon is rate-limiting authentication from your address after repeated failed attempts (HTTP 429), it warns that it could not confirm the route; wait for the 15-minute lockout to end and run the command again. Without a running daemon it exits non-zero and tells you how to start one.

### Authentication modes

| Daemon | Entry |
| --- | --- |
| Token authentication, no passphrase (the default) | URL plus the `Authorization: Bearer` header |
| `--auth passphrase`, with or without `--behind-proxy` | None: the login wall covers every TCP caller, including loopback, and `/mcp` accepts only a browser login session. The command exits non-zero. Restart with `--auth token` and without `--passphrase` |
| Token authentication plus a passphrase, including `--remote` | None: `/mcp` still requires the login session cookie, so a bearer token is not enough. The command exits non-zero. `--remote` cannot drop its passphrase |
| `--auth none` | URL only |
| `aoe serve --core-only` | None: a core-only daemon does not listen on TCP |

If the daemon refuses the printed credential, for example because the token rotated at that moment, the command exits non-zero instead of printing an entry that cannot connect.

## Tools

The tools and their arguments are listed in the [HTTP API reference](../api.md#mcp-server). Every tool returns structured content with an output schema.

## Known limits

- The daemon must already be running. Connecting an MCP client does not start it.
- `aoe mcp config` targets the daemon on this machine, preferring its loopback address when available; a daemon bound only to a LAN or Tailscale address uses that address, which works only if the daemon started with `--dangerously-allow-plaintext-remote`. Without it the daemon answers plain HTTP from that address with `426`, and the command exits non-zero instead of printing an entry. It does not read `--daemon-url` or `AOE_DAEMON_URL`.
- A passphrase login wall, including `--auth passphrase` and `aoe serve --remote`, covers loopback. `/mcp` accepts that login only as a browser session cookie, so the command exits non-zero instead of printing an entry.
- A pasted token entry stops working after the daemon rotates its token. Run `aoe mcp config` again to get the current one.
- An MCP client has the same access as you at the keyboard. AoE adds no per-tool permissions; the agent's own tool rules decide what it may call.
