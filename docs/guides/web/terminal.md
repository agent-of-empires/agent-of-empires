# Terminal View

For tmux-backed sessions the dashboard renders a real terminal in the page: the agent's pane streamed over a WebSocket PTY relay, plus an optional paired shell. This page covers both terminals, reconnect behavior, and the close codes you may see when a connection fails. For the structured-view rendering used by ACP sessions, see the [Structured view overview](../../structured-view.md).

![The agent terminal rendered in the browser via the PTY relay](../../assets/web/terminal.png)

## Agent terminal

The main terminal attaches to the session's tmux pane through an xterm.js front end. The server spawns `tmux attach-session` inside a PTY and relays the raw byte stream bidirectionally over the WebSocket, so every key sequence, color, and scrollback line behaves like `tmux attach` over SSH.

Scrolling up into history pauses the live tail and surfaces a **Back to live** button; scrolling back to the bottom (or clicking it) resumes the tail.

## Copy and scroll

The terminal uses tmux for scrollback and selection, so copy and scroll work with no modifier keys:

- **Scroll** with the mouse wheel (or a one-finger swipe on touch) through tmux scrollback. Touch scrolling follows the finger like any native list: drag down to look back through history, drag up to head back toward the live tail.
- **Select** by click-dragging across the text. Dragging upward past the top edge scrolls into scrollback and extends the selection. Releasing the drag copies to your system clipboard automatically; no Ctrl/Cmd+C needed.

Copy relies on the browser Clipboard API, which only works in a secure context: HTTPS (the remote-access tunnel modes) or `http://localhost`. On a plain-HTTP LAN/VPN origin the browser blocks clipboard writes, so the selection stays visible but is not copied. Firefox is best-effort (it lacks the async clipboard write); Chromium and Safari copy reliably.

## Paired terminal

Each session can open a **paired terminal**: a host (or, for sandboxed sessions, in-container) shell rooted at the session's working directory. On desktop it shares the split with the agent terminal; on mobile it is one of the right-panel picker's views. It stays alive in the background when you switch away, preserving scrollback and focus.

For sandboxed sessions, the **Container** tab launches the container user's login shell, resolved inside the container (passwd entry, then `$SHELL`, then bash, sh), so your prompt, aliases, and oh-my-zsh setup load like the Host tab.

## Reconnect

If the WebSocket drops (network blip, tunnel re-auth, daemon restart), the terminal reconnects on a fast-start retry ladder (200ms, 400ms, 800ms, 1.5s, 3s, 6s, 10s) so transient warm-up failures recover in well under five seconds. A disconnect banner shows the current state; a permanently dead pane surfaces a manual retry button instead of looping.

### Terminal WebSocket close codes

When the browser fails to reach a working terminal, the disconnect banner shows the close code returned by the server:

| Code | Reason string         | Meaning                                                                                                | Client behavior            |
| ---- | --------------------- | ------------------------------------------------------------------------------------------------------ | -------------------------- |
| 1001 | `server shutdown`     | Daemon is shutting down (SIGINT/SIGTERM).                                                               | Retry with normal backoff. |
| 1011 | `openpty_failed`      | Server could not allocate a PTY.                                                                        | Retry with normal backoff. |
| 1011 | `attach_spawn_failed` | Server could not spawn the `tmux attach-session` child process.                                        | Retry with normal backoff. |
| 1011 | `pty_reader_failed`   | Server could not clone the PTY reader handle.                                                           | Retry with normal backoff. |
| 1011 | `pty_writer_failed`   | Server could not take the PTY writer handle.                                                            | Retry with normal backoff. |
| 1013 | `tmux_not_ready`      | Pane did not become attachable within 2s. Usually a benign warm-up on first session open.              | Retry with normal backoff. |
| 4001 | `pty_dead`            | PTY relay was running but the pane permanently exited.                                                  | Show "Click retry" banner. |

## Read-only mode

When the server runs with `aoe serve --read-only`, the terminal renders the live stream but drops keystrokes: you can watch sessions but not type into them. The session-row Delete and triage actions are hidden too.

## On mobile

On touch devices the agent pane uses a different architecture, mirroring the TUI's live mode: instead of attaching a PTY, the server streams `tmux capture-pane` snapshots over the WebSocket and the dashboard renders them as real text. That makes the phone experience native:

- **Scrolling is the browser's own scroll**: momentum, rubber-banding, and finger-true tracking, over the pane's real tmux scrollback. No copy-mode round trips, and the agent keeps running while you read history.
- **Text selection is native**: long-press to select and copy, like any web page.
- **Typing** goes back over the same WebSocket and is delivered with `tmux send-keys`; tapping anywhere on the terminal brings up the soft keyboard, the floating keyboard button toggles it open and closed, and the terminal toolbar provides arrows, Tab, Esc, a `Ctrl` modifier toggle, interrupt, and paste.
- **Pinch** adjusts the font size; the pane resizes the tmux window to the resulting grid.

A "Back to live" pill appears while you are scrolled up; tapping it (or scrolling to the bottom) returns to the live tail. The pane stays mounted while you switch views so the connection and scroll position survive. Desktop keeps the full xterm.js PTY relay described above.
