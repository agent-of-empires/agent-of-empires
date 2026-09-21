# Tool Sessions

Tool sessions keep dev tools like `lazygit`, `yazi`, `tig`, or `gitui` running alongside each agent session, scoped to that session's working directory. Each runs in its own persistent tmux session, so reattaching is instant and state (cursor position, staged hunks, browsed path) survives detaching.

The UX mirrors the terminal preview: a hotkey previews the tool in the home view, `Enter` attaches full-screen, and `Esc` returns with the tool still running.

## Configuring tools

Tools are defined in your global `config.toml` under `[tools.<name>]`. They are a config-file feature: edit the file and reload from the settings dialog (or restart `aoe`) to pick up changes.

```toml
[tools.lazygit]
command = "lazygit"
hotkey = "Alt+g"

[tools.tig]
command = "tig --all"     # no hotkey: picker and palette only

[tools.github]
command = "gh repo view --web"
hotkey = "Alt+o"
background = true
```

| Field | Required | Description |
| --- | --- | --- |
| `command` | yes | Shell command. Persistent tools pass it to `tmux new-session`, background tools to your shell; pipes and `&&` work in both. |
| `hotkey` | no | `Alt+<single-char>`. The modifier is case-insensitive and an ASCII letter is lowercased, so `Alt+G` becomes `Alt+g`. Multi-character keys and other modifiers are rejected. |
| `background` | no | Run fire-and-forget in the session's working directory instead of opening a tmux tool session. |

A hotkey that fails to parse is reported in a startup dialog, and only the binding is dropped: the tool stays reachable from the picker and palette. When two tools claim the same hotkey, the alphabetically-first name wins. Tool hotkeys are checked before built-in home-screen keybindings, so they shadow them.

## Using tools

Three ways to open one:

1. **Hotkey**: select a session and press it. The home view switches to a live preview of that tool's pane; the hotkey toggles back.
2. **Picker**: press `;` on the home view for a modal listing every tool with its command and hotkey.
3. **Command palette** (`Ctrl+K`): tools appear as "Open tool: \<name\>", and background tools as "Run: \<name\>".

In preview, `Enter` attaches and `Esc`, `;`, or `t` returns. Once attached the tool owns the screen, so detach the way that tool expects (most quit on `q`; detaching the tmux session with `Ctrl+B d` keeps its state for next time).

Background tools skip preview entirely and never create a tmux session. They run with stdin, stdout, and stderr detached, so redirect output in the command if you want a log; a non-zero exit goes to the debug log rather than a dialog.

## Lifecycle

Each tool session is tied to one agent session's working directory, so the same hotkey on a different session opens a separate tool session with its own state. Tool sessions are killed when their agent session is removed, and the sweep covers tools whose `[tools.*]` entry you have since renamed or deleted. Background tools are not tmux sessions and are not part of that cleanup: if one starts a daemon, its lifecycle is yours to manage.

Tool sessions are named `aoe_tool_<tool>_<title>_<id8>` (`aoe_dev_tool_` in debug builds), so `tmux attach -t <name>` works, though the three access paths above are faster.

## Where the tool runs

Tools always run on the **host**, in the agent session's working directory. For a sandboxed session the tool runs against the worktree path the container has mounted, not inside the container, which keeps file state consistent and avoids `docker exec` overhead on every attach. If your dev environment lives entirely inside the container (`$PATH`, `git config`, or tool binaries differ), wrap the command yourself:

```toml
[tools.lazygit]
command = "docker exec -it my-container lazygit"
```
