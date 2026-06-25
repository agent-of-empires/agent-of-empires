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

## Troubleshooting

### The cursor flickers or the preview stutters over SSH

If you run AoE on a remote host and reach it over SSH, you may see the
agent's input cursor blink out and back, or the preview stutter as you
type. On an otherwise idle screen the agent's blinking cursor is usually
the only thing moving, so it is the one place the stutter shows up.

This is your SSH connection, not AoE. AoE captures the agent's pane
locally on the box where AoE runs and renders the result there. When you
view that over SSH, the rendered screen (cursor included) travels back to
your terminal as one ordered TCP stream. On a lossy or high-latency link
TCP holds the stream until dropped packets are resent, so redraws arrive
late and in bursts.

Quick check: run `vim` or `htop` over the same SSH session. If their
cursors stutter the same way, it is the link.

What helps, by leverage:

* **Use [`mosh`](https://mosh.org) instead of `ssh` for flaky links.**
  Mosh runs over UDP with local echo prediction and adapts its frame
  rate to the connection, dropping stale frames instead of replaying a
  backlog. On a shaky link you get a slightly-behind but stable cursor
  instead of a stuttering one, and AoE manages its own screen so mosh's
  lack of scrollback passthrough does not affect it. The trade: on a fast
  clean link mosh's keystroke latency can be marginally worse than ssh,
  and a very large redraw can blank the screen a touch longer while mosh
  rebuilds it.
* **[Eternal Terminal](https://eternalterminal.dev) (`et`)** is a
  TCP-based alternative with fast reconnect if you would rather not run
  mosh.

This only applies when AoE itself runs on the remote host. Running AoE
locally, even with the agent sandboxed in a container, renders everything
on your machine and is not affected.
