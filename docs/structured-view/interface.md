# Structured view Interface

Structured view renders in both the TUI and the web dashboard. This page covers
how the two surfaces differ, the keybinds, how the composer behaves
across desktop and touch, and how the timeline keeps long turns
readable. For setup, see [Structured view Setup](setup.md); for the
[Structured view overview](../structured-view.md), start there.

![The web structured view composer with mode and model controls, above a stream of tool-call cards](../assets/structured-view/interface.png)

## TUI vs web dashboard

Structured view renders natively in the TUI alongside the web dashboard.
Both consume the same `aoe serve` daemon over the same HTTP/WS
surface, so the conversation log, pending approvals, and worker
state are always in sync.

- **Sessions started in structured view** appear in the TUI session list
  with a `[acp]` badge. Pressing Enter opens the native structured view
  view, which requires an `aoe serve` daemon to be already running.
  If one isn't, the view renders an actionable error pointing at
  `aoe serve --daemon` (localhost), `aoe serve --daemon --remote`
  (Tailscale/Cloudflare), or `AOE_DAEMON_URL` (attach to a remote
  daemon you already have running). The TUI intentionally does not
  start a daemon on your behalf, so you keep the choice between
  localhost, tunnel, and named tunnel explicit.
- **Sessions started in tmux mode** work in both surfaces as before.
  The TUI attaches to the pane; the dashboard renders the pane via
  xterm.js.
- **Switching views** (web wizard or the per-session "Switch to
  structured view" / "Switch to tmux" action) destroys the in-memory
  conversation history for that session. The git worktree, files on
  disk, and any commits remain. The next prompt starts a fresh
  conversation under the new view.
- **TUI status indicators**: a structured view session that's healthy shows
  as Idle/Active in the TUI session list, since structured view health is
  observed via the ACP event stream rather than tmux pane probing.
- **`--auth=passphrase` daemons**: the local TUI attaches to a
  same-host daemon without going through the passphrase exchange.
  Loopback callers are treated as fs-trusted because the daemon's
  serve files under `~/.agent-of-empires/serve.*` are already 0600,
  so the filesystem permission boundary protects same-host access.
  Remote callers proxied through a tunnel still hit the passphrase
  wall as expected. See #1525.

### TUI structured view keybinds

The TUI structured view has three focusable regions: composer (where
you type prompts), transcript (the activity feed), and approval
cards (one per pending tool authorization). Tab cycles focus; the
status banner at the bottom of the screen shows the current focus.

| Focus       | Key             | Action                                                |
| ----------- | --------------- | ----------------------------------------------------- |
| Composer    | `Enter`         | Send the buffered text, or queue it if a turn is active |
| Composer    | `Shift+Enter`   | Insert a newline (multi-line prompts)                 |
| Composer    | `@`             | Open the file-mention picker; keep typing to filter   |
| Composer    | `Enter` (empty) | Retry draining the queue when idle (e.g. after a failed send) |
| Composer    | `/`             | Type a slash at the start of an empty line to open the command picker |
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

**Slash-command picker.** When the composer holds a single-word
slash query (`/comp`, no spaces yet), a picker floats above the
composer listing the agent's advertised commands ranked against what
you typed, the same ranking the web composer uses. Navigate with the
arrows or `Ctrl+n` / `Ctrl+p`, then press `Enter` or `Tab` to insert
`/{command} ` into the composer (it does not auto-send, so you can
add arguments first). `Esc` dismisses the picker without inserting;
it stays closed until the query text changes, so cursor movement
won't reopen it. A slash query with no matching command is left
alone: `Enter` sends it verbatim. The picker only appears once the
agent has advertised commands over the ACP stream.

**Focus isolation.** Approval keys (`a`/`Shift+A`/`d`) only resolve
when the approval card itself has focus. Typing "always allow" into
the composer will never silently approve a pending tool; the
composer captures every keystroke, including those letters.

**Approval card detail.** The web dashboard approval card shows a
one-line preview of the tool call in its header (the command for a
shell call, the path for a read or edit) so you can act without
expanding. A benign approval starts collapsed with that preview; a
destructive one starts expanded so the full arguments are in view
before a hold-to-allow. Click the header to toggle the full argument
list, and the Allow / Always / Deny buttons stay reachable in either
state. The toggle is per-card and never re-expands on its own after a
plan is approved.

**Markdown rendering.** Agent messages in the transcript are parsed as
markdown and rendered with styling: headings and `**bold**` show in
bold, `*italics*` in italic, `` `inline code` `` and fenced code blocks
in a dim block, and `-`/`1.` lists with bullet or number markers. The
raw `#`, `**`, backtick, and fence characters are not shown. Styling
uses text attributes only (bold, italic, dim) so it tracks your theme
colors. Code-block syntax highlighting is deferred; press `o` to open
the web dashboard for full-fidelity rendering. In the web dashboard,
links inside transcript messages open in a new browser tab so following
a docs, CI, or repo link keeps your structured view session open. Local file
references (the `path:line` links agents like Codex emit when citing
source) are an exception: clicking one opens that file in the in-app
diff/file viewer and keeps you on the current session, instead of
navigating away. A file that is not inside the session's repo shows a
brief notice and leaves the view unchanged.

**Tool cards.** Tool calls in the transcript render per kind rather than
as a single generic line. An edit or write shows the file path and a
compact added/removed line diff (colored with your theme's diff colors);
an execute shows the command and a bounded preview of its output; a read
shows the path and a content preview; a delete shows the target path.
The diff is capped at 20 changed lines and previews at 12 lines, with a
"+N more" footer when there is more; press `o` to open the web dashboard
for the full diff and output. Edit cards read the path and diff from the
structured diff content the agent emits (Codex routes `apply_patch` edits
this way, one entry per file), falling back to the legacy argument shape
when an agent sends that instead; a single patch touching several files
shows each file's path and diff in one card. Any other tool kind falls
back to the generic one-liner (name, arguments, output snapshot).

**Structured completion payloads.** When a tool reports its result as
structured content at completion (rather than streamed text), the web
card renders it below the card: images show inline (an embedded payload is
preferred, falling back to a referenced `uri`), audio plays inline from
its embedded payload, resource links and binary resources become download
links, and text resources render as text. A block whose payload can't be
shown (an image with neither inline data nor a uri, or audio with no
embedded data) degrades to a labelled placeholder so the output is never
silently dropped. The native TUI, which cannot draw images, shows a
textual placeholder (for example `[image image/png]`) for the non-text
blocks. Inline image/audio payloads larger than 4 MiB of base64 are
dropped from the event (the placeholder remains) to keep the replay
stream small.

**File-mention picker.** Typing `@` in the composer opens a picker
listing the session's workspace files, fetched once per session from
the daemon (the same `structured view/files` index the web composer uses, capped
at 5000 entries). Keep typing to fuzzy-filter; prefix matches rank above
substring matches. Selecting a file inserts it as `:file[<path>]`,
matching the text the web composer sends, so both surfaces hand the
agent identical prompts. The picker closes on `Esc` and stays closed
while you keep typing in that same token until you start a fresh `@`.

### Web composer Enter behavior

On desktop, Enter sends the prompt and Shift+Enter inserts a
newline, matching the TUI convention above.

On touch-primary devices (phones, tablets without an attached
keyboard), plain Enter inserts a newline and the explicit Send
button on the right of the composer is the only path to dispatch.
This matches the conventions of WhatsApp, Slack, ChatGPT mobile,
and Claude.ai mobile, and avoids the common foot-gun of accidentally
firing a partial multi-line prompt by reaching for a line break.
An iPad with a Bluetooth keyboard (or any device that reports both
`(pointer: coarse)` and `(any-pointer: fine)` to the browser) keeps
the desktop Enter-to-send convention so hardware-keyboard typing
feels natural. See #1129.

### iOS Safari dictation

Tapping the on-screen keyboard's mic icon to dictate into the composer
commits each partial recognition exactly once. The composer detects
WebKit's `insertReplacementText` burst, suspends its assistant-ui
controlled-input flush for the duration so WebKit's dictation range
pointer is not invalidated mid-utterance, then drains the final text
into the composer state on blur (typically when you tap Send) or
after a brief idle period. See #1431.

## Composer attachments (images, audio, files)

The web composer can send attachments alongside the prompt text when
the active agent advertises support for them. Three ways to add one:

- the paperclip button in the composer toolbar opens a file picker;
- paste an image (for example a screenshot) with Cmd/Ctrl+V while the
  composer is focused;
- drag and drop files onto the composer.

Staged attachments show as removable chips above the text area; images
render a thumbnail. A prompt can be attachment-only (no text), which is
handy for "what is wrong here?" screenshots.

Support is gated on the agent's ACP `prompt_capabilities`, reported
during the `initialize` handshake. The paperclip is disabled (with a
tooltip explaining why) when the current agent does not accept
attachments, and the file picker only offers the kinds it does accept:

- `image` for images,
- `audio` for audio,
- `embedded_context` for embedded resources (text / markdown / JSON /
  PDF).

`claude-agent-acp` advertises `image` and `embedded_context`; other
agents vary, so the button reflects whichever agent is running.

The server is the authority: it re-checks the agent capability, enforces
a per-attachment size limit (10 MiB), a total-per-prompt limit (20 MiB),
a count cap (8), a MIME allowlist (`image/svg+xml` and HTML are
excluded), and sniffs image magic bytes so a mislabeled file is
rejected. Oversize or unsupported attachments come back as an error
instead of reaching the agent.

Attachments are persisted with the transcript so they re-render on
reload. The bytes live in a dedicated store keyed to the prompt and are
pruned in lockstep with it (and dropped when the session is deleted), so
the event log stays lean. Replayed images are fetched lazily from
`GET /api/sessions/{id}/acp/attachments/{attachment_id}`.

Attachments queue alongside the prompt text. Sending one while the
agent is mid-turn, disconnected, or restarting parks the message in the
queue (the queued row shows a thumbnail / chip for each attachment) and
the drain fires it once the session resumes, the same as a text-only
follow-up. The bytes ride the queued row in memory only: they are kept
out of the per-origin localStorage snapshot, so a full page reload drops
any queued attachment row (you reattach and resend). Audio and embedded
resources are sent and stored, but render as a labelled chip rather than
an inline player or preview for now.

## Queued prompts (mid-turn + inactive session)

The web composer keeps your messages around even when the session
can't accept them yet. Two cases:

1. **Mid-turn follow-up.** While the agent is producing the current
   response, the Send button switches to a paper-plane with a small
   pending-count badge. Click (or press Enter) and your text lands in
   the **Queued (N)** strip above the composer. As soon as the agent
   reports `Stopped`, the structured view drains the queue per the
   `acp.queue_drain_mode` setting (combined, the default, sends
   every parked entry as one prompt; serial fires them one at a time).
   See #1031 for the original feature.

2. **Inactive session.** If the WebSocket is mid-reconnect, the worker
   is stopped (`user_stopped`), or the worker is restarting
   (`restart_pending`, `agent_unresponsive`, `prompt_orphaned`), the
   composer still accepts submissions. The tooltip swaps to
   `Queue message until session resumes`, the strip heading changes to
   `Pending until session resumes (N)`, and the parked entry stays
   editable. The moment the WS reopens AND the worker reaches
   `running` AND the session-level `Stopped` flag clears (an
   `AcpSessionAssigned` event), the same drain effect fires the
   queue. See #1359.
3. **Idle-dormant session.** If the worker was auto-stopped for
   inactivity (`auto_stop_idle_secs`, `Stopped` reason
   `idle_auto_stop`), the composer stays fully usable and your prompt
   does not park indefinitely: the POST itself is the wake path. The
   server clears the dormancy marker, the reconciler respawns the
   worker, and the request is held until the fresh worker is ready,
   then delivered. A prompt you had already queued before the worker
   went dormant drains the same way the moment the dormancy event
   lands. See #1689.

Queued entries persist in the per-origin localStorage snapshot at
`aoe:acp-state:v1:<sid>`, so a page reload (and closing then
reopening the tab on the same origin) keeps them across the reconnect
window. The one exception is queued rows that carry attachments: their
base64 bytes are never written to the snapshot (they would blow the
storage quota), and the whole row is dropped on reload rather than
draining a text-only prompt with the image silently missing. Server-side
durability is not currently implemented; clearing site data wipes the
queue.

**TUI structured view.** The TUI structured view has the same client-side queue.
Pressing `Enter` while a turn is active (or while the WebSocket is
down) parks the prompt in a **Queued (N)** strip above the composer
instead of sending it; the queue drains on the next `Stopped` per the
daemon's `acp.queue_drain_mode`, which the view reads from
`/api/about` so a remote attach honors the remote daemon's setting.
`Ctrl+X` clears the queue, and pressing `Enter` on an empty composer
when idle retries the drain (useful if a send failed and left prompts
parked). Two small differences from the web composer: there is no
in-place edit of queued rows (clear and retype), and combined mode
slices batches only at the `/clear` and `/new` boundaries rather than
the full per-agent clear-alias list. The TUI queue is in-memory only,
so it does not survive leaving the structured view.

## Stopping a turn

While an agent turn is running, the composer shows a **Stop** button. Clicking it sends a graceful cancel to the agent and the working spinner switches to **Stopping...** with a short countdown to the escalation deadline.

Some tools the agent runs internally (a monitor or `until` loop, a long blocking command) do not honor a graceful cancel. When that happens a **Force stop** button appears next to the spinner, even while a tool is in flight. Force stop ends the turn immediately: it restarts the agent worker and kills the whole command tree the agent had running, so a runaway loop actually stops instead of waiting out the grace window. Clicking **Stop** again while it already reads "Stopping..." does the same thing.

Force stop is a hard interrupt. The agent resumes from its saved transcript on the next prompt, but any partial output from the tool that was in flight is lost. Reach for **Force stop** only when a turn is genuinely wedged; the graceful **Stop** is enough for a turn that is merely taking a while.

## Timeline card grouping

To keep the timeline readable, structured view folds two kinds of runs into
single collapsible cards:

- **Silent tool work.** A run of three or more consecutive tool calls
  with no agent text between them (for example Read, Read, Grep, Read
  during investigation) collapses into one "actions" card. Expand it to
  see each call as its normal per-tool card.
- **Consecutive TodoWrite updates.** When Claude fires three or more
  `TodoWrite` calls back-to-back, or OpenCode sends three or more
  `todowrite` updates with a structured `todos` payload, the per-call
  snapshots fold into one todo card titled "updated N times". Collapsed,
  the card shows the latest list (the only snapshot whose
  pending/in-progress/done mix is current), so you see what the agent is
  working on without expanding. Expand it to inspect each individual
  update in order and audit how the plan evolved during the turn.

Folding only fires when every call in the run is the same shape. A
TodoWrite or `todowrite` update sandwiched between real tool work (Read,
Edit) stays inline as its own card rather than being hidden inside a
group, so a status update between actions is never buried. Two-in-a-row
stays inline as well; the fold threshold is three.

Automatic grouping needs an unbroken run, so a phase where the agent
narrates between each action (common right after a plan is approved)
produces a long stream of individual cards instead. The **Compact
tools** toggle at the top of the transcript collapses every tool card to
its header for scanning, and new cards arrive collapsed while it stays
on; the agent's narration stays visible and errored cards stay open so a
failure is never hidden. It is a per-browser preference saved locally,
and you can still expand any single card while compact mode is on.
