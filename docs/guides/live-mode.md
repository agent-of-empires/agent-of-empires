# Live mode

Live mode is a "feels-attached" alternative to a full tmux attach. When
you enter it, the AoE home view stays on screen (session list, preview,
status bar) and every keystroke is relayed straight to the selected
session's pane. You get the ambient awareness of the dashboard and the
immediacy of typing directly to the agent, without tmux taking over the
whole terminal.

Unlike a full `tmux attach` (which replaces the screen until you detach),
the dashboard never leaves, so you can watch other sessions' states while
you work.

## Entering and leaving

* **Enter:** press `Tab` on a runnable session, or set live mode as your
  default so that `Enter` (and clicking a row) drops you straight into
  it. See **Default Attach Mode** and **New Session Attach Mode** in
  Settings.
* **Leave (fast exit):** press `Ctrl+Q`. This is a single press and is
  always available, independent of the leader below.

The status bar shows a `● LIVE → <session>` banner while you are
relayed, including a reminder of the exit chord and the leader menu.

Live-send also works for the Terminal view and Tool views (lazygit,
yazi, and other embedded tools), not just the agent pane: whichever
pane is on screen is what your keystrokes reach.

Turning on **Auto Live-Send On View Switch** (Interaction settings, off
by default) skips the extra `Enter`/`Tab`/click when you switch into
Terminal or Tool view: live-send starts as soon as the view switches,
regardless of **Default Attach Mode**.

Mouse input is forwarded too, when the pane on screen is a full-screen
app that asked for it: clicks, drags, and the scroll wheel reach
mouse-driven tool UIs like lazygit and yazi, and an agent that tracks
the pointer (Claude Code highlights expandable content under your
cursor) also receives hover motion. Hold `Shift` while clicking or
dragging to bypass forwarding and use AoE's own text selection and
copy instead.

## The leader menu

Almost every key you press in live mode goes to the agent, so AoE
reserves a single **leader** chord (tmux-style prefix) to reach its own
commands. The default is `Ctrl+B`, matching tmux and herdr.

Press the leader, then a command key:

| Keys | Action |
| --- | --- |
| `Ctrl+B` then `k` | Open the command palette |
| `Ctrl+B` then `b` | Hide / show the sidebar (preview takes the full width) |
| `Ctrl+B` then `q` | Exit live mode |
| `Ctrl+B` then `Ctrl+B` | Send a literal `Ctrl+B` to the agent |
| `Esc` (or any other key) after the leader | Cancel the menu, send nothing |

When the leader is armed, the status bar turns into a which-key menu
listing these commands, so you do not have to memorize them.

Only the leader itself is taken away from the agent, and pressing it
twice still delivers it downstream (the same idea as tmux's
`send-prefix`). Every other chord, including `Ctrl+K`, passes through to
the agent untouched, so the agent's own keybindings keep working.

Opening the command palette layers it over live mode. Cancel it with
`Esc` to drop straight back into the relay; choosing any palette command
leaves live mode first, so the preview never shows one session while
your keystrokes go to another.

## Collapsing the sidebar

`Ctrl+B` then `b` hides the session list and hands the full terminal
width to the agent pane, then restores it on the next toggle. This is a
live-mode focus tool: the sidebar always reappears when you exit live
mode, so you can never get stranded in the normal home view with the
list hidden.

## Scrolling history

`Shift+PageUp` and `Shift+PageDown` scroll the preview back through the
agent's history without leaving live mode or forwarding the keys. Bare
`PageUp` / `PageDown` still pass through to the agent, so agents that
page their own UI keep working.

## Inserting newlines

`Shift+Enter` inserts a newline into the agent's input box on
kitty-protocol-capable terminals (Ghostty, Kitty, WezTerm, foot, Konsole
24+, Alacritty 0.13+, recent xterm). On terminals that do not speak the
kitty keyboard protocol (Apple Terminal, default iTerm2, Termius, Mosh),
`Shift+Enter` submits like bare `Enter`; use `Alt+Enter` (or
`Option+Enter` on macOS), which sends `ESC+CR` natively on many
terminals, or configure the terminal to send `ESC+CR` for `Shift+Enter`
as a fallback.

## Configuration

Both chords are editable under Settings, in the Interaction section, and
in `config.toml`:

```toml
[session]
# Leader (prefix) chord for live-mode commands. Tmux-style spec.
# Leave empty to disable the leader entirely (then Ctrl+B passes
# straight through to the agent).
live_send_leader = "C-b"

# One or more comma-separated chords that exit live mode. Single press,
# independent of the leader.
live_send_exit_chord = "C-q"
```

Chord specs are tmux-style: `C-b`, `C-a`, `M-x` (Alt), `F12`, and so on.
A typo in the leader falls back to the default; an empty value disables
it.

### Why `Ctrl+B`?

A prefix steals exactly one chord from the agent, so the obvious
single-key candidates like `Ctrl+K` (readline's kill-to-end-of-line)
stay free for the shell and the agent. `Ctrl+B` is the chord tmux and
herdr users already know as a leader. If you run AoE inside your own
tmux session that also uses `Ctrl+B`, the outer tmux will claim it
first; rebind `live_send_leader` to something free (for example `C-a` or
`F1`), and remember that `Ctrl+Q` always exits regardless.
