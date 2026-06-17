# Structured view Interface

Structured view renders in both the TUI and the web dashboard. This page covers
how the two surfaces differ, the keybinds, how the composer behaves
across desktop and touch, and how the timeline keeps long turns
readable. For setup and the overview, see [Structured view](../structured-view.md).

![The web structured view composer with mode and model controls, above a stream of tool-call cards](../assets/structured-view/interface.png)

## TUI vs web dashboard

Both surfaces consume the same `aoe serve` daemon over the same HTTP/WS
surface, so the conversation log, pending approvals, and worker state
stay in sync.

- **Sessions started in structured view** appear in the TUI session list
  with a `[acp]` badge. Pressing Enter opens the native structured view,
  which requires an `aoe serve` daemon to be already running. If one
  isn't, the view shows an actionable error pointing at
  `aoe serve --daemon` (localhost), `aoe serve --daemon --remote`
  (Tailscale/Cloudflare), or `AOE_DAEMON_URL` (attach to a remote daemon
  you already have running). The TUI does not start a daemon for you, so
  the choice between localhost, tunnel, and named tunnel stays explicit.
- **Sessions started in tmux mode** work in both surfaces. The TUI
  attaches to the pane; the dashboard renders the pane via xterm.js.
- **Switching views** (web wizard or the per-session "Switch to
  structured view" / "Switch to tmux" action) destroys the in-memory
  conversation history for that session. The git worktree, files on
  disk, and any commits remain. The next prompt starts a fresh
  conversation under the new view.
- **TUI status indicators**: a healthy structured view session shows as
  Idle/Active in the session list, observed via the ACP event stream
  rather than tmux pane probing.
- **`--auth=passphrase` daemons**: the local TUI attaches to a same-host
  daemon without the passphrase exchange (loopback callers are protected
  by the 0600 serve files on disk). Remote callers proxied through a
  tunnel still hit the passphrase wall.

### TUI structured view keybinds

The TUI structured view has three focusable regions: composer (where
you type prompts), transcript (the activity feed), and approval cards
(one per pending tool authorization). Tab cycles focus; the status
banner at the bottom shows the current focus.

| Focus       | Key             | Action                                                |
| ----------- | --------------- | ----------------------------------------------------- |
| Composer    | `Enter`         | Send the buffered text, or queue it if a turn is active |
| Composer    | `Shift+Enter`   | Insert a newline (multi-line prompts)                 |
| Composer    | `@`             | Open the file-mention picker; keep typing to filter   |
| Composer    | `Enter` (empty) | Retry draining the queue when idle (e.g. after a failed send) |
| Composer    | `/`             | Type a slash at the start of an empty line to open the command picker |
| Composer    | `↑` / `↓`       | Recall queued prompts to edit (caret at start); `↓` past the newest restores your draft |
| Composer    | `↑` / `↓`       | Move the picker highlight (picker open)               |
| Composer    | `Ctrl+n` / `Ctrl+p` | Move the picker highlight down / up (picker open) |
| Composer    | `Enter` / `Tab` | Insert the highlighted command or file (picker open)  |
| Composer    | `Esc`           | Dismiss the picker, or return focus to the transcript |
| Transcript  | `j` / `↓`       | Scroll down one line                                  |
| Transcript  | `k` / `↑`       | Scroll up one line                                    |
| Transcript  | `PgDn` / `PgUp` | Scroll ten lines                                      |
| Transcript  | `g` / `G`       | Jump to top / bottom                                  |
| Transcript  | `i`             | Focus the composer                                    |
| Transcript  | `Tab`           | Cycle to the approval card (if any pending)           |
| Transcript  | `o`             | Open this session in the web dashboard                |
| Transcript  | `Esc`           | Close the structured view and return to the session list |
| Approval    | `a`             | Allow once                                            |
| Approval    | `Shift+A`       | Allow always (session-scoped allow-list entry)        |
| Approval    | `d`             | Deny                                                  |
| Approval    | `Esc`           | Return focus to the transcript                        |
| Any         | `Ctrl+C`        | Cancel the in-flight prompt                           |
| Any         | `Ctrl+O`        | Open the session in the web dashboard                 |
| Any         | `Ctrl+X`        | Clear every queued (not-yet-sent) prompt              |

**Slash-command picker.** When the composer holds a single-word slash
query (`/comp`, no spaces yet), a picker floats above it listing the
agent's advertised commands ranked against what you typed. Navigate
with the arrows or `Ctrl+n` / `Ctrl+p`, then press `Enter` or `Tab` to
insert `/{command} ` (it does not auto-send, so you can add arguments
first). `Esc` dismisses the picker. A query with no matching command is
left alone: `Enter` sends it verbatim. The picker only appears once the
agent has advertised commands.

**Focus isolation.** Approval keys (`a`/`Shift+A`/`d`) only resolve when
the approval card has focus. Typing "always allow" into the composer
will never approve a pending tool; the composer captures every
keystroke.

**Approval card detail.** The web approval card shows a one-line preview
of the tool call in its header (the command for a shell call, the path
for a read or edit) so you can act without expanding. A benign approval
starts collapsed; a destructive one starts expanded so the full
arguments are in view before a hold-to-allow. Click the header to toggle
the full argument list; the Allow / Always / Deny buttons stay reachable
either way.

**Markdown rendering.** Agent messages render as styled markdown:
headings and `**bold**` in bold, `*italics*` in italic, `` `inline
code` `` and fenced blocks in a dim block, and `-`/`1.` lists with
markers. The raw markup characters are not shown. Styling uses text
attributes only (bold, italic, dim) so it tracks your theme colors.
Syntax highlighting in code blocks is deferred; press `o` to open the
web dashboard for full-fidelity rendering. In the web dashboard, links
in transcript messages open in a new tab so following a docs, CI, or
repo link keeps your session open. Local `path:line` references (which
agents like Codex emit when citing source) are an exception: clicking
one opens that file in the in-app diff/file viewer and keeps you on the
session. A file outside the session's repo shows a brief notice and
leaves the view unchanged.

**Tool cards.** Tool calls render per kind rather than as a single
generic line. An edit or write shows the file path and a compact
added/removed diff (in your theme's diff colors); an execute shows the
command and a bounded output preview; a read shows the path and a
content preview; a delete shows the target path. The diff is capped at
20 changed lines and previews at 12 lines, with a "+N more" footer when
there is more; press `o` to open the web dashboard for the full diff and
output. A single patch touching several files shows each file's path and
diff in one card. In the web dashboard, Claude's harness tools render as dedicated cards too:
a tool search shows its query, a background monitor shows its description
and command, and a task stop shows the stopped task id. Other tool kinds
fall back to a generic one-liner (name, arguments, output).

**Structured completion payloads.** When a tool returns images, audio,
or resources, they render inline on the card (a textual placeholder in
the TUI, which can't draw them); anything that can't be shown degrades
to a labelled placeholder so output is never silently dropped.

**File-mention picker.** Typing `@` in the composer opens a picker
listing the session's workspace files, fetched once per session from the
daemon. Keep typing to fuzzy-filter; prefix matches rank above substring
matches. Selecting a file inserts it as `:file[<path>]`, matching what
the web composer sends, so both surfaces hand the agent identical
prompts. The picker closes on `Esc`.

### Web composer Enter behavior

On desktop, Enter sends the prompt and Shift+Enter inserts a newline,
matching the TUI convention above.

On touch-primary devices (phones, tablets without an attached keyboard),
plain Enter inserts a newline and the explicit Send button is the only
way to dispatch, avoiding accidental partial sends when reaching for a
line break. Devices with a hardware keyboard (for example an iPad with a
Bluetooth keyboard) keep the desktop Enter-to-send convention.

On-screen keyboard dictation (the mic icon, e.g. iOS Safari) commits
into the composer correctly.

On touch devices, tapping anywhere in the transcript focuses the composer
and brings up the soft keyboard, so you do not have to reach for the
composer field to start typing. Tapping a control inside a message (a
tool-call card, a link, a button) still does its own thing instead.

## Composer attachments (images, audio, files)

The web composer can send attachments alongside the prompt text when the
active agent advertises support. Three ways to add one:

- the paperclip button in the composer toolbar opens a file picker;
- paste an image (for example a screenshot) with Cmd/Ctrl+V while the
  composer is focused;
- drag and drop files onto the composer.

Staged attachments show as removable chips above the text area; images
render a thumbnail. A prompt can be attachment-only (no text), handy for
"what is wrong here?" screenshots.

Support depends on the agent's advertised capabilities: the paperclip is
disabled (with a tooltip) when the current agent doesn't accept
attachments, and the file picker only offers the kinds it does accept
(images, audio, embedded resources). `claude-agent-acp` advertises
images and embedded resources; other agents vary. The server re-checks
the capability and enforces size, count, and MIME limits, so oversize or
unsupported attachments come back as an error instead of reaching the
agent.

Attachments persist with the transcript so they re-render on reload, and
they queue alongside the prompt text: sending one while the agent is
mid-turn, disconnected, or restarting parks the message (the queued row
shows a thumbnail or chip) and the drain fires it once the session
resumes. A full page reload drops any queued attachment row (reattach
and resend). Audio and embedded resources are sent and stored, but
render as a labelled chip rather than an inline player or preview.

## Queued prompts (mid-turn + inactive session)

The web composer keeps your messages around even when the session can't
accept them yet. Three cases:

1. **Mid-turn follow-up.** While the agent is producing the current
   response, the Send button becomes a paper-plane with a pending-count
   badge. Click (or press Enter) and your text lands in the **Queued
   (N)** strip above the composer. Once the agent reports `Stopped`, the
   queue drains per the `acp.queue_drain_mode` setting (combined, the
   default, sends every parked entry as one prompt; serial fires them one
   at a time).
2. **Inactive session.** If the WebSocket is mid-reconnect or the worker
   is stopped or restarting, the composer still accepts submissions. The
   tooltip swaps to `Queue message until session resumes` and the parked
   entry stays editable. The drain fires once the connection and worker
   are back and the session's `Stopped` flag clears.
3. **Idle-dormant session.** If the worker was auto-stopped for
   inactivity, your prompt does not park indefinitely: the POST itself is
   the wake path. The server respawns the worker and holds the request
   until the fresh worker is ready, then delivers it. A prompt queued
   before the worker went dormant drains the same way.

Queued entries persist in per-origin local storage, so a page reload (or
closing and reopening the tab on the same origin) keeps them across the
reconnect window. Queued rows carrying attachments are the exception:
the whole row is dropped on reload rather than draining a text-only
prompt with the image missing. There is no server-side durability;
clearing site data wipes the queue.

**Editing a queued prompt.** Click any queued row to edit it inline, or,
with the composer empty (caret at the start), press `↑` to pull the most
recent queued prompt back into the composer; `↑` again walks toward older
entries and `↓` walks back toward newer ones, restoring your in-progress
draft once you step past the newest. While recalling, a banner above the
composer reads **Editing queued message N of M** so the mode is
unmistakable; `Esc` abandons the edit and restores your draft. Editing a
recalled prompt and pressing `Enter` updates that entry in place rather
than queueing a duplicate.

**TUI structured view.** The TUI has the same client-side queue.
Pressing `Enter` while a turn is active (or while the WebSocket is down)
parks the prompt in a **Queued (N)** strip instead of sending; the queue
drains on the next `Stopped` per the daemon's `acp.queue_drain_mode`
(read from `/api/about`, so a remote attach honors the remote daemon's
setting). `Ctrl+X` clears the queue, and pressing `Enter` on an empty
composer when idle retries the drain (useful if a send failed and left
prompts parked). Queued prompts can be recalled for editing the same way
as the web: with the composer empty (caret at the start), `↑` pulls
the newest queued prompt back into the composer, `↑` / `↓` walk the queue,
and editing then `Enter` updates that entry in place. While recalling, the
composer border title reads **Editing queued message N of M**, and `Esc`
restores your draft. One difference from the web composer remains: the TUI
queue is in-memory only, so it does not survive leaving the structured
view.

## Stopping a turn

While an agent turn is running, the composer shows a **Stop** button.
Clicking it sends a graceful cancel to the agent and the working spinner
switches to **Stopping...** with a short countdown to the escalation
deadline.

Some tools the agent runs internally (a monitor or `until` loop, a long
blocking command) do not honor a graceful cancel. When that happens a
**Force stop** button appears next to the spinner, even while a tool is
in flight. Force stop ends the turn immediately: it restarts the agent
worker and kills the whole command tree, so a runaway loop actually
stops instead of waiting out the grace window.

Clicking **Stop** a second time always escalates to a force stop, even
when the spinner is stuck "active" but the daemon no longer has a turn in
flight (for example after the worker was restarted mid-turn). The second
press no longer waits for the server to confirm the first cancel, so the
button is always a working escape and you should not need
`aoe acp restart` to clear a wedged spinner.

Force stop is a hard interrupt. The agent resumes from its saved
transcript on the next prompt, but any partial output from the tool that
was in flight is lost. Reach for **Force stop** only when a turn is
genuinely wedged; the graceful **Stop** is enough for a turn that is
merely taking a while.

## Timeline card grouping

To keep the timeline readable, structured view folds two kinds of runs
into single collapsible cards:

- **Silent tool work.** A run of three or more consecutive tool calls
  with no agent text between them (for example Read, Read, Grep, Read
  during investigation) collapses into one "actions" card. Expand it to
  see each call as its normal per-tool card.
- **Consecutive TodoWrite updates.** Three or more back-to-back todo
  updates fold into one todo card titled "updated N times". Collapsed,
  it shows the latest list so you see what the agent is working on
  without expanding. Expand it to inspect each update in order and audit
  how the plan evolved.

Folding only fires on an unbroken run of the same shape. A todo update
sandwiched between real tool work (Read, Edit) stays inline as its own
card, so a status update between actions is never buried. Two-in-a-row
stays inline; the threshold is three. A phase where the agent narrates
between each action produces a long stream of individual cards instead.

The **Compact tools** toggle at the top of the transcript collapses
every tool card to its header for scanning, and new cards arrive
collapsed while it stays on; the agent's narration stays visible and
errored cards stay open so a failure is never hidden. It is a
per-browser preference saved locally, and you can still expand any single
card while compact mode is on.
