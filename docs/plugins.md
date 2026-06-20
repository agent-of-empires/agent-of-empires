# Plugins

Agent of Empires keeps its core small (sessions, tmux, worktrees) and is
growing a plugin system so optional capabilities can be enabled or disabled at
runtime instead of bloating the core. This first release is deliberately
minimal: a registry of first-party plugins bundled with the binary, each one
something you can turn on or off. Installing external plugins, per-plugin
settings, and plugin-contributed UI all land in follow-up releases.

## Managing plugins

Three equivalent surfaces:

- **CLI**: `aoe plugin list`, `aoe plugin info <id>`, `aoe plugin enable <id>`,
  `aoe plugin disable <id>`.
- **TUI**: open the command palette and run "Manage plugins", or open Settings
  and select the Plugins tab (the same manager, hosted inline). Space toggles
  enable/disable.
- **Web dashboard**: Settings, then the Plugins tab. The same list and toggles.
  Enabling or disabling a plugin requires an elevated (passphrase) session when
  login is enabled and is blocked in read-only mode.

A plugin's enable-state is stored under `[plugins."<id>"]` in `config.toml` and
survives every config save.

## Bundled plugins

| Plugin | What it does | Disabled behavior |
|---|---|---|
| `aoe.web` | The web dashboard management marker. Included and enabled by default; absent only from `--no-default-features` source builds. | `aoe serve` refuses to start until re-enabled (`aoe plugin enable aoe.web`). |

The bundled set is deliberately minimal while the system is proven out. More
first-party plugins (status detection, and others) land as each piece is
verified, followed by external plugin installation and richer per-plugin
capabilities.
