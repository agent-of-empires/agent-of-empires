# tmux Status Bar

Agent of Empires can display session information in your tmux status bar, showing:
- **Session title**: The name of your aoe session
- **Git branch**: For worktree sessions
- **Container name**: For sandboxed (Docker) sessions

## How It Works

When you start a session, aoe configures the tmux status bar to display this information in your active theme's colors (Empire by default).

**Example status bars:**
```
aoe: My Session | 14:30                           # Basic session
aoe: My Session | feature-branch | 14:30          # Worktree session
aoe: My Session ⬡ aoe-sandbox-a1b2c3d4 | 14:30     # Sandboxed session
aoe: My Session | main ⬡ aoe-sandbox-a1b2c3d4 | 14:30  # Worktree + sandbox
```

## Auto Mode (Default)

By default, aoe uses "auto" mode for the status bar:

- **If you don't have a `~/.tmux.conf`**: aoe automatically styles the status bar for aoe sessions
- **If you have a `~/.tmux.conf`**: aoe assumes you prefer your own configuration and does not modify the status bar

This ensures beginners get a helpful status bar out of the box, while experienced tmux users retain full control.

## Configuration

Configure the status bar behavior in `~/.agent-of-empires/config.toml`:

```toml
[tmux]
# "auto" (default) - Apply only if no ~/.tmux.conf exists
# "enabled"        - Always apply aoe status bar styling
# "disabled"       - Never apply, use your own tmux config
status_bar = "auto"
mouse = "auto"     # Same modes: auto, enabled, disabled
clipboard = "auto" # Same modes: auto, enabled, disabled
```

### Values

| Value | Description |
|-------|-------------|
| `auto` | Apply status bar if user has no tmux config (default) |
| `enabled` | Always apply aoe status bar to aoe sessions |
| `disabled` | Never modify tmux status bar |

## Clipboard Pass-through

TUI agents copy to the system clipboard via OSC 52 escape sequences, which tmux swallows by default, so "select to copy" inside the agent silently fails. With clipboard pass-through (the default in `auto` mode when you have no `~/.tmux.conf`), aoe lets those sequences reach your terminal emulator.

Set `clipboard = "disabled"` if you don't trust the wrapped agent's terminal output (pass-through lets the inner program write arbitrary escape sequences to your outer terminal).

If you manage your own `~/.tmux.conf`, set these yourself:

```tmux
set -g set-clipboard on
set -g allow-passthrough on
```

Some terminal emulators also need clipboard write permission enabled (Ghostty's `clipboard-write = allow`, etc.).

## Custom Integration

If you have your own tmux configuration but want to display aoe session info, use the `aoe tmux status` command.

### Basic Integration

Add this to your `~/.tmux.conf`:

```tmux
set -g status-right "#(aoe tmux status) | %H:%M"
```

This will show the aoe session title and branch when attached to an aoe session, and nothing when in other tmux sessions.

### JSON Output

For more advanced scripting:

```bash
aoe tmux status --format json
```

Output:
```json
{"title": "My Session", "branch": "feature-branch", "sandbox": null}
```

For a sandboxed session:
```json
{"title": "My Session", "branch": null, "sandbox": "aoe-sandbox-a1b2c3d4"}
```

Returns `null` if not in an aoe session.

### Example: Conditional Display

```tmux
# Only show aoe info if in an aoe session
set -g status-right "#{?#{==:#(aoe tmux status),},,%#(aoe tmux status) | }%H:%M"
```

## tmux User Options

aoe sets `@aoe_title`, `@aoe_branch` (worktree sessions), and `@aoe_sandbox` (sandboxed sessions) on each session, which you can reference in your own config:

```tmux
set -g status-right "#{@aoe_title} #{@aoe_branch} #{@aoe_sandbox} | %H:%M"
```

## Troubleshooting

### Status bar not showing

1. Check if you have a `~/.tmux.conf` or `~/.config/tmux/tmux.conf`
2. If so, either:
   - Set `status_bar = "enabled"` in your aoe config
   - Or add `aoe tmux status` to your tmux.conf manually

### Status bar shows old info

The tmux user options are set when the session starts. If you rename a session in aoe, the status bar will show the old name until you restart the session.

### Branch not showing

Branch is only displayed for worktree sessions (sessions created with `aoe add --worktree`). Regular sessions don't have a fixed branch.

### Container not showing

Container name is only displayed for sandboxed sessions (sessions created with `aoe add --sandbox`). The container name follows the pattern `aoe-sandbox-<session_id_first_8_chars>`.
