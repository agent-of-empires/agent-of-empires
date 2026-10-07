# Live mode

Live mode is a "feels attached" alternative to a full tmux attach: the home view stays on screen (session list, preview, status bar) while every keystroke is relayed to the selected session's pane, so you can watch other sessions' states while you work.

## Entering and leaving

Press `Tab` on a runnable session, or set live mode as your default so `Enter` and a click drop straight into it. **New Session Mode** makes new sessions open in live mode without changing what `Enter` does. `Ctrl+Q` always leaves, in a single press, independent of the leader below. The status bar shows a `● LIVE → <session>` banner with the exit chord while you are relayed.

Control chords reach the agent: `Ctrl+C` interrupts it rather than quitting AoE, and the banner flashes a brief reminder on each press. To quit AoE, leave live mode first.

Live send works for the Terminal and Tool views too, not just the agent pane: whichever pane is on screen is what your keystrokes reach. Turning on **Auto Live-Send On View Switch** (Interaction settings, off by default) starts the relay as soon as you switch views.

Mouse input is forwarded when the pane on screen is a full-screen app that asked for it, so clicks, drags, and the wheel reach tools like lazygit and yazi, and an agent that tracks the pointer receives hover motion. Hold `Shift` to bypass forwarding and use AoE's own selection and copy.

Links in the preview are underlined, and a plain click opens one in your browser, ahead of that forwarding, whenever the preview is on screen. Both a marked-up hyperlink and a bare URL count. Resting the pointer on one shows its target in the status bar, which is worth a glance: a link's visible text is chosen by whatever is running in the pane. AoE also passes the link to your terminal, so hovering highlights it and your terminal's own gesture still works (and emulators that claim shifted clicks, such as WezTerm and iTerm2, take those before AoE sees them).

When AoE cannot reach a browser you would actually see (SSH, a headless host) it copies the address to your clipboard instead and says so. Set `BROWSER` to override that check, on platforms whose launcher reads it; macOS opens through the system and ignores it.

## The leader menu

Almost every key goes to the agent, so AoE reserves one **leader** chord, tmux-style. The default is `Ctrl+B`.

| Keys | Action |
| --- | --- |
| `Ctrl+B` then `k` | Open the command palette |
| `Ctrl+B` then `b` | Hide or show the sidebar, giving the preview full width |
| `Ctrl+B` then `q` | Exit live mode |
| `Ctrl+B` then `Ctrl+B` | Send a literal `Ctrl+B` to the agent |
| `Esc` or any other key | Cancel the menu, send nothing |

While the leader is armed the status bar becomes a which-key menu. Only the leader itself is taken from the agent, and pressing it twice still delivers it downstream, so every other chord, `Ctrl+K` included, passes through untouched. The command palette layers over live mode: `Esc` drops back into the relay, while choosing a command leaves live mode first, so the preview never shows one session while your keystrokes go to another. The sidebar always reappears when you exit live mode, so hiding it cannot strand you.

`Shift+PageUp` and `Shift+PageDown` scroll the preview through the agent's history without leaving live mode; bare `PageUp` and `PageDown` still pass through for agents that page their own UI.

`Shift+Enter` inserts a newline into the agent's input box on kitty-protocol-capable terminals (Ghostty, Kitty, WezTerm, foot, Konsole 24+, Alacritty 0.13+, recent xterm). Elsewhere it submits like bare `Enter`, so use `Alt+Enter` (`Option+Enter` on macOS), which sends `ESC+CR` natively on many terminals, or configure your terminal to send `ESC+CR` for `Shift+Enter`.

## Split windows

If the session's tmux window is split, by hand or by an agent that spawns a pane per teammate, the preview composites every pane onto the window grid with borders in the gaps, so nothing is hidden. Two things change while it is split: scrolling history is unavailable, because panes keep independent scrollbacks with no coherent way to stack them, and input still goes to the first pane, so a click or wheel aimed at a neighbouring pane does nothing. Attach to the session to interact with the other panes.

## The VT live transport

The TUI's agent preview and the dashboard's agent terminal render through a persistent VT channel by default: `tmux pipe-pane` streams the agent's raw output into an in-process terminal grid, and on tmux 3.8 or newer your keystrokes travel back over the same socket (on older tmux that socket can crash the server when the agent has just exited, so keystrokes stay on `send-keys`). Compared to the polling path (`capture-pane` scrapes plus a `send-keys` fork per keystroke), echo and streaming output land with near-attach latency, agent clipboard writes (OSC 52) reach your clipboard, and a full-screen agent that brackets its repaints in synchronized output is shown only between brackets, never mid-redraw.

The channel needs tmux 3.4 or newer. A pane that cannot arm one, a grid that could not be seeded or is catching up with a resize, an older tmux, or a non-Unix host falls back to polling automatically: everything still works, with more latency and no synchronized-output hold. A split window is composited from `capture-pane` snapshots, with the TUI preview keeping pane 0 on its VT grid. The paired host and container shells always poll.

To rule the transport in or out while troubleshooting, toggle **VT Live Transport** under Settings (Tmux tab, Advanced) or set `[tmux] vt_live = false`. The TUI applies a change on its next capture cycle; web connections pick it up when they reconnect.

## Configuration

Both chords are editable under Settings (Interaction) and in `config.toml`:

```toml
[session]
# Leader (prefix) chord for live-mode commands, tmux-style.
# Empty disables the leader, passing Ctrl+B straight to the agent.
live_send_leader = "C-b"

# Comma-separated chords that exit live mode, single press.
live_send_exit_chord = "C-q"
```

Specs are tmux-style (`C-b`, `C-a`, `M-x`, `F12`). A typo in the leader falls back to the default.

`Ctrl+B` is the default because a prefix steals exactly one chord from the agent, and it is the one tmux users already know, leaving candidates like `Ctrl+K` free for the shell. If you run AoE inside your own tmux session that also uses `Ctrl+B`, the outer tmux claims it first: rebind `live_send_leader` (to `C-a` or `F1`, say), and remember that `Ctrl+Q` always exits.
