# Tool Sessions

Tool sessions let you keep dev tools like `lazygit`, `yazi`, `tig`, or
`gitui` running alongside each agent session, scoped to that session's
working directory. By default each tool runs in its own persistent tmux
session, so re-attaching is instant and state (cursor position, staged
hunks, browsed path) survives across detaches.

The UX mirrors the built-in terminal preview: press a hotkey to preview
the tool in the home view, `Enter` to attach to it full-screen, and
`Esc` to come back to the home view with the tool still running in the
background.

## Configuring tools

Tool sessions are defined in your global `config.toml` under
`[tools.<name>]`. The path follows the usual config conventions:

- **Linux**: `$XDG_CONFIG_HOME/agent-of-empires/config.toml`
  (defaults to `~/.config/agent-of-empires/config.toml`)
- **macOS / Windows**: `~/.agent-of-empires/config.toml`

Each entry has a required `command`, an optional `hotkey`, and an
optional `background` mode for fire-and-forget commands:

```toml
[tools.lazygit]
command = "lazygit"
hotkey = "Alt+g"

[tools.yazi]
command = "yazi"
hotkey = "Alt+f"

[tools.tig]
command = "tig --all"
# no hotkey, reachable from the picker and palette only

[tools.github]
command = "gh repo view --web"
hotkey = "Alt+o"
background = true
```

### Field reference

| Field | Required | Description |
| --- | --- | --- |
| `command` | yes | Shell command to run. Persistent tools pass it to `tmux new-session`; background tools pass it to the user's shell. Pipes and `&&` work in both modes. |
| `hotkey` | no | Hotkey binding in `Alt+<single-char>` format (case-insensitive on the modifier, normalized to lowercase on the letter). Examples: `"Alt+g"`, `"Alt+1"`, `"Alt+/"`. |
| `background` | no | When `true`, run the command fire-and-forget in the selected session's working directory instead of opening a persistent tmux tool session. Defaults to `false`. |

Tool sessions are intentionally a config-file feature today; they are
not editable from the settings TUI. Edit `config.toml` and reload from
the settings dialog (or restart `aoe`) to pick up changes.

### Hotkey rules

Tool hotkeys are limited to `Alt+<single-char>`:

- Modifier is case-insensitive: `Alt+g`, `ALT+g`, and `alt+g` all work.
- ASCII letter is normalized to lowercase: `Alt+G` becomes `Alt+g`.
  Non-ASCII characters are matched as-is (case-sensitive) and depend on
  your terminal sending them with the Alt modifier; ASCII bindings are
  recommended for portability.
- Multi-character keys (`Alt+gg`, `Alt+F1`) are rejected.
- Other modifiers (`Ctrl+g`, `Shift+g`) are not supported.

If a hotkey fails to parse, AoE shows an info dialog on startup (and on
settings reload) listing each invalid entry. The corresponding tool is
still reachable from the picker and command palette; only the dead
binding is dropped.

If two tools claim the same hotkey, the alphabetically-first tool name
wins. Tool hotkeys are checked before built-in home-screen keybindings,
so they shadow any built-in binding using the same combination.

## Using tools

Three ways to open or run a tool, in roughly the order you'll grow into them:

1. **Hotkey**. Select an agent session, press the configured hotkey
   (e.g. `Alt+g`). The home view switches to a live preview of that
   tool's tmux pane. Press `Enter` to attach.
2. **Picker**. Press `;` on the home view. A modal lists every
   configured tool with its command and hotkey. Pick one with arrow
   keys (or `j`/`k`) and `Enter`. `;` or `Esc` closes the picker.
3. **Command palette**. Press `Ctrl+K`. Tool sessions appear as
   "Open tool: \<name\>" entries you can fuzzy-search. Background
   tools appear as "Run: \<name\>".

Tools with `background = true` skip tool preview mode and do not create a
tmux session. They run with stdin, stdout, and stderr detached; redirect
output in the command if you need a log. Non-zero exits are written to
the debug log, not shown as an interactive dialog.

Once you're in tool preview mode:

- `Enter` attaches you to the tool full-screen.
- The hotkey **toggles** preview off and back to the structured view.
- `Esc`, `;`, or `t` returns to the structured view.

Once you're attached:

- The tool's own keybindings apply (it owns the screen).
- To detach: use the tool's quit/detach behavior. Most quit on `q`. For
  long-running tools, detach the underlying tmux session
  (`Ctrl+B d` by default) to keep state across reattaches.

## Lifecycle and cleanup

Each tool session is tied to one agent session's working directory.
Switching to a different agent session and pressing the same hotkey
opens a **separate** tool session against that worktree, with its own
independent state.

Tool sessions are automatically killed when their parent agent session
is removed (`aoe remove <id>`, "Remove session" in the TUI, or delete
in the web dashboard). Cleanup sweeps all of the agent's tool sessions
even if you renamed or deleted the `[tools.*]` entry, so nothing is left
orphaned.

Background tools are not tmux sessions and are not managed by this
cleanup path. Use them for short launch commands; if a command starts a
daemon, its lifecycle is up to that command.

## Where the tool runs

Tools always run on the **host**, in the working directory of the agent
session. For a sandboxed (Docker) agent session, the tool does not run
inside the container; it runs on the host against the worktree path
that the container has mounted. For most tools (lazygit, yazi, tig,
gitui) that is the right default: they read files via the same
worktree path the container sees, so file state stays consistent, and
host launches avoid `docker exec` overhead on every attach.

If your dev environment lives entirely inside the container (your
`$PATH`, `git config`, or tool binaries differ host vs. container),
keep that in mind: tool sessions reflect the host environment. A
config-only workaround is to define the tool's `command` as a
`docker exec` wrapper that runs the binary inside the container, e.g.
`command = "docker exec -it my-container lazygit"`.

## Examples

### lazygit per worktree

```toml
[tools.lazygit]
command = "lazygit"
hotkey = "Alt+g"
```

Now `Alt+g` on any agent session previews lazygit running against that
session's worktree; pressing `Alt+g` a second time toggles back to the
structured view. Each worktree has its own staged hunks.

### File browser with a custom config

```toml
[tools.yazi]
command = "yazi --config-dir ~/.config/yazi"
hotkey = "Alt+f"
```

### Composite commands

The `command` is shelled out, so multi-step launches work:

```toml
[tools.dbshell]
command = "psql $DATABASE_URL || sleep 5"
hotkey = "Alt+d"
```

### Hotkey-less tool

```toml
[tools.bench]
command = "btm"
# Reachable via `;` or `Ctrl+K`. No global hotkey.
```

### Background launcher

```toml
[tools.github]
command = "gh repo view --web"
hotkey = "Alt+o"
background = true
```

Now `Alt+o`, the picker, or the `Run: github` command palette entry opens
the selected session's repository in the browser without switching to a
tool preview.

## tmux session naming

Tool sessions are named `aoe_tool_<tool>_<title>_<id8>` (`aoe_dev_tool_` in debug builds; `<id8>` is the
first 8 characters of the agent session ID). You can attach manually
with `tmux attach -t <name>`, though AoE's three access paths are
faster.
