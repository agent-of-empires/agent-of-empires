# Plugins

Agent of Empires keeps its core small (sessions, tmux, worktrees) and grows a
plugin system so optional capabilities can be enabled or disabled at runtime
instead of bloating the core. The core ships first-party plugins bundled with
the binary and can install external community plugins from GitHub or a local
directory. Per-plugin settings and plugin-contributed UI land in follow-up
releases; running plugin code is not wired up yet, so an installed external
plugin records its grant and files but does not execute until a later release.

## Managing plugins

Three equivalent surfaces:

- **CLI**: `aoe plugin list`, `aoe plugin info <id>`, `aoe plugin enable <id>`,
  `aoe plugin disable <id>`, `aoe plugin install <source>`,
  `aoe plugin update <id>`, `aoe plugin uninstall <id>`.
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
| `aoe.web` | The web dashboard management marker. Present whenever the dashboard is compiled in (`--features serve`), so every released binary ships it, enabled by default. | `aoe serve` refuses to start until re-enabled (`aoe plugin enable aoe.web`). |

`aoe.web` is the only bundled plugin today, and it rides along with the web
dashboard. So a release binary (or any `cargo build --features serve`) shows it
in `aoe plugin list`, while a TUI-only build (`cargo build`, no `serve`) has an
empty registry and `aoe plugin list` reports no plugins. That is expected, not a
bug.

The bundled set is deliberately minimal while the system is proven out. More
first-party plugins land as each piece is verified.

## Installing external plugins

External plugins are community code that you install at your own risk. Install,
update, and uninstall are CLI-only (`aoe plugin` is reserved for management);
the TUI and web surfaces show the result but do not install.

```sh
aoe plugin install gh:owner/repo          # latest release (the audited default)
aoe plugin install gh:owner/repo@v1.2.3   # an explicit tag, branch, or commit
aoe plugin install ./path/to/plugin       # a local directory
aoe plugin update <id>
aoe plugin uninstall <id>
```

With no `@ref`, install resolves the repo's latest stable GitHub release (the
audited default path) and installs that tag. An explicit `@ref` installs
unverified, un-audited code and asks you to confirm first (`--yes` skips the
prompt). If the repo has published no release, install warns and falls back to
the default branch behind the same confirmation. The recorded source stays
ref-less, so `aoe plugin update` keeps tracking the latest release; an `@ref`
install keeps following that ref.

A plugin lands under `<app_dir>/plugins/<id>/`. A GitHub source is cloned and
pinned to the exact commit; if the plugin ships a compiled worker as a release
binary, the asset for your platform is downloaded into the plugin directory. To
install from a GitHub Enterprise host, set `AOE_GITHUB_CLONE_BASE` to its base
URL.

### Trust and capabilities

Bundled plugins are `builtin` and fully trusted. Installed plugins are
`community` and untrusted: their manifest declares the capabilities they need
(network access, filesystem access, spawning processes, and so on), and install
prompts you once to grant that exact set. Run non-interactively with `--yes` to
grant without prompting. A capability this version of aoe does not recognize is
rejected rather than granted; upgrade aoe.

A grant is pinned to the installed manifest. If an update changes the
capability set, the plugin is left installed but inactive and
`aoe plugin update <id>` re-prompts you to approve the new set. `aoe plugin
install` and `aoe plugin update` report the resolved trust level (`featured`,
`community`, or `local`) in their success output, and `aoe plugin list` and
`aoe plugin info <id>` show each plugin's trust level and whether it is granted. An external plugin cannot use the reserved `aoe.*` /
`agent-of-empires.*` id namespace.

Resolved versions live in `<app_dir>/plugins.lock` (the exact commit, manifest
hash, and release asset per plugin), so an install is reproducible.

Running plugin code, per-plugin settings, and plugin-contributed UI land in
follow-up releases.
