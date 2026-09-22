# tmux Status Bar

aoe can paint its own themed status bar on the sessions it creates, showing the session title, the branch for worktree sessions, and the container name for sandboxed ones:

```
aoe: My Session | feature-branch | 14:30
aoe: My Session ⬡ aoe-sandbox-a1b2c3d4 | 14:30
```

`[tmux] status_bar` controls it: `"auto"` (default) paints the bar only when you have no tmux config, since the bar is a whole theme and a half-merge with yours would please nobody; `"enabled"` always paints it; `"disabled"` never does and reverts aoe's session-scoped `status*` overrides, so your own config governs. `mouse` and `clipboard` share the same three modes but a narrower `"auto"`: each defers only when your tmux config sets that specific option. See the [configuration reference](configuration.md#tmux) for the full table and which files are scanned.

## Clipboard pass-through

TUI agents copy to the system clipboard with OSC 52 escape sequences, which tmux swallows by default, so "select to copy" inside an agent silently fails. With pass-through on, aoe sets `set-clipboard on` and `allow-passthrough on` for its sessions and those sequences reach your terminal. Set `clipboard = "disabled"` if you do not trust the wrapped agent's output, since pass-through lets the inner program write arbitrary escape sequences to your outer terminal. If you manage your own config, set both options yourself. Some terminals also need clipboard write permission enabled (Ghostty's `clipboard-write = allow`).

## Using your own status bar

`aoe tmux status` prints the current session's info, and returns nothing outside an aoe session:

```tmux
set -g status-right "#(aoe tmux status) | %H:%M"
```

`aoe tmux status --format json` emits `{"title": "My Session", "branch": "feature-branch", "sandbox": null}` for scripting.

aoe also sets tmux user options on each session: `@aoe_title`, `@aoe_branch` (worktree sessions), `@aoe_sandbox` (sandboxed sessions), and `@aoe_kind`.

```tmux
set -g status-right "#{@aoe_title} #{@aoe_branch} #{@aoe_sandbox} | %H:%M"

# Only decorate the agent pane
set -g status-right "#{?#{==:#{@aoe_kind},agent},#{@aoe_title},} | %H:%M"
```

`@aoe_kind` is `agent`, `term` (paired terminal), `cterm` (container terminal), or `tool`. It is written at creation and survives renames, so it stays accurate where the session name cannot: a title such as `term notes` gives an agent session a name shaped like a paired terminal's. Sessions started by an earlier aoe carry no value until they restart.

Read `@aoe_kind`, but do not set it. A value at server, window, or global-window scope replaces aoe's for every session on the server rather than sitting behind it, and aoe discards values those scopes could have produced rather than believe a pane is something it is not. Setting one therefore costs you the marker entirely, and those sessions fall back to being classified by name, which is the ambiguity `@aoe_kind` exists to remove.

## Troubleshooting

- **No status bar**: you have a tmux config, so `"auto"` stepped aside. Set `status_bar = "enabled"`, or add `aoe tmux status` to your own config.
- **Stale info**: renaming a session refreshes `@aoe_title` on its agent pane immediately, but the paired terminal and container panes keep the old title, and `@aoe_branch` keeps the old branch after a tied branch rename, until those sessions restart.
- **No branch or container**: those are shown only for worktree and sandboxed sessions. Container names follow `aoe-sandbox-<first 8 chars of session id>`.
