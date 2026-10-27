# Tool Sessions

Tool sessions keep tools such as `lazygit`, `yazi`, `tig` and `gitui` running beside an agent session in its working directory. Each persistent tool has its own tmux session, preserving state across detaches.

A hotkey previews the tool. `Enter` uses the default attach mode, including [live mode](live-mode.md); `Tab` opens a full tmux client. Leaving the preview does not stop it. Targeted Stop retires only that tool's verified original process cohort; see [Lifecycle and cleanup](#lifecycle-and-cleanup).

## Configuring tools

Define tools in global `config.toml` under `[tools.<name>]`; see [File Locations](configuration.md#file-locations). Edit the file and reload from settings, or restart AoE. Tools are not editable in the settings TUI.

```toml
[tools.lazygit]
command = "lazygit"
hotkey = "Alt+g"

[tools.yazi]
command = "yazi"
hotkey = "Alt+f"

[tools.tig]
command = "tig --all"     # no hotkey: picker and palette only

[tools.github]
command = "gh repo view --web"
hotkey = "Alt+o"
background = true
```

| Field | Required | Description |
|---|---|---|
| `command` | yes | Shell command. Persistent tools use `tmux new-session`, background tools use your shell; pipes and `&&` work in both. |
| `hotkey` | no | `Alt+<single-char>`. Modifier matching is case-insensitive; ASCII letters become lowercase. Multi-character keys and other modifiers are rejected. |
| `background` | no | Run fire-and-forget in the selected session's directory instead of creating a tmux session. Defaults to `false`. |

Non-ASCII hotkeys are case-sensitive and depend on terminal Alt support; ASCII is more portable. Invalid bindings appear in a startup or reload dialog; the tool remains in the picker and palette. When bindings collide, the alphabetically-first tool name wins. Tool hotkeys precede built-in home bindings.

## Using tools

Open a tool through its hotkey, the `;` picker (arrows or `j`/`k`, then `Enter`), or `Ctrl+K` **Open tool: <name>**. Background entries use **Run: <name>**. `;` or `Esc` closes the picker.

The Daemon prepares exactly one pane for an open request; rendering never launches a tool. Pressing `Enter` during preparation continues that same request.

Background tools skip preview and tmux. Their standard streams are detached; redirect output if needed. Non-zero exits go to the debug log, not an interactive dialog.

In preview, `Enter` uses the default attach mode and `Tab` opens full-screen tmux. The hotkey toggles preview; `Esc`, `;` or `t` returns to structured view. `x` opens Stop for the visible tool, not its agent; press `x` again to confirm or `Esc` to cancel. Strict hotkeys use `X`. Terminal preview uses the same gesture.

Once attached, the tool owns the screen. Use its quit behavior, or detach tmux with `Ctrl+B d` to preserve the tool for next time.

## Lifecycle and cleanup

Each tool session is tied to one agent session's working directory.
Switching to a different agent session and pressing the same hotkey
opens a **separate** tool session against that worktree, with its own
independent state.

Creating or recovering a tool uses the current session and configured
command. Launch is refused while the session is trashed, being created
or deleted, or reserved by another lifecycle operation.

Native integrations can call `POST /api/sessions/{id}/tools/ensure` with
`{"tool_name":"lazygit","size":{"cols":120,"rows":40}}`; size is optional.
The daemon resolves the command and working directory, and rejects client
command or directory fields. The response contains `tmux_session` and the
runtime cursor headers. Before attaching, clients must apply a snapshot in
the same epoch at that revision or later, require an alive auxiliary observation
whose `tmux_session` matches the returned target, and recheck their current
same-host native interaction grant.

Attachment preparation does not block TUI navigation. Leaving the session,
view or profile, opening an overlay, or pressing Escape cancels the pending
handoff. A cancelled attachment does not become active when preparation ends.

The daemon reports each tool as alive, dead, absent, or unknown. Unknown
means ownership or liveness could not be established; requests never guess an
owner. An explicit stop may still finish an already-authorized original
retirement, even when the current pane is absent or ownership is unknown.

A change to a session stays pending until the TUI receives the command
acknowledgement and applies its matching runtime snapshot; let it settle before
another change to the same session. A pane disappearing does not establish
completion.

If a TUI runtime change loses its acknowledgement, its session stays blocked because the outcome is unknown. Check the current daemon state before choosing **Unlock**; resolution does not replay the lost change. **Keep Blocked** leaves the quarantine intact. Reopen the prompt with `Ctrl+K`, then **Resolve unknown runtime change**, even if the session has disappeared from the list. A failed resolution remains blocked until a healthy, current daemon snapshot has been applied.

New tool panes record the full session ID and raw configured tool key. Older unmarked panes require explicit ownership consent before attach, live-send, send or Stop; name alone never proves ownership. The TUI shows the selected tool, row title, full row ID, profile, lifecycle generation, and exact tmux session and pane IDs, with the original pane leader PID when available. The request also carries the configured socket and original server PID and process birth. Confirm only if that exact target belongs to that row and tool. Stop adopts it before stopping only that tool.

The Daemon revalidates the captured server, session and pane identity and stamps only the same still-unmarked pane. Changed ownership, a replaced server or tmux target, or a respawned pane makes consent fail without affecting a replacement or agent. Ambiguous rows across profiles or raw tool keys are refused. The row title is context, not ownership authority; retitling alone never transfers a tool. A dead pane may lack its leader PID, but still requires the captured configured socket and live original server PID/birth proof.

Targeted tool Stop retains the title and lifecycle flocks and atomically advances
the row's durable generation, invalidating older continuations without installing
a whole-session Stop reservation. Fresh peer lifecycle reservations still refuse
the request; they are never cleared to admit recovery. Other stop operations keep
their existing lifecycle reservations.

Verified stops first capture the original process births, then atomically
recheck the live pane's full owner, name, immutable tmux IDs, and PID. Before
removing that exact tmux session, the daemon durably records its original
process cohort in `tool-retirements.json` in the application data directory.
The record also binds the original tmux socket and server process birth.
The daemon removes the live tmux target before settling its captured cohort;
this does not depend on tmux retaining a dead pane's PID.

An interrupted stop retains the original record until physical retirement is
proven and that exact record is acknowledged. While that original record remains,
retrying never captures a replacement's descendants or uses the record alone to
delete a current pane. If the original target is still present but no longer
matches, the request refuses without signals. If the original target or server is
provably gone, only the recorded original process births are settled. Once its
internal acknowledgement has cleared the record, a subsequent named Stop is a
new intent; a lost HTTP response does not provide exactly-once retry semantics.
After a positively proven reboot, obsolete original debt can be acknowledged
without inspecting recycled PIDs or adopting a current replacement pane.

Tool launch and parent-session cleanup must settle outstanding original debt
before treating a missing pane as complete. Pending tools remain discoverable
in the selected row's tool picker after restart, including tools removed from
configuration. Those entries are recovery-only: selecting one does not launch
or attach; use Stop to finish its recorded cleanup without adopting any current
replacement. A missing, unreadable, unsupported, or malformed journal is an
error, not an empty inventory.

A dead pane without a leader PID can be removed only with its captured server/socket proof and unchanged owner and immutable tmux target. Missing or unprovable original server evidence stays fail-closed. Older tmux-only pending stop receipts remain retained and refused; they are never discarded or replaced by a new process cohort.


Removing a parent session attempts cleanup of every owned tool, including renamed or deleted `[tools.*]` entries. Unsettled original debt remains a cleanup fence, not proof that an absent pane is complete.

Background tools are not tmux sessions and use no managed cleanup path. If a command starts a daemon, that command owns its lifecycle.

## Where the tool runs

Tools run on the **host** in the agent session's working directory, including for sandboxed agents. They share the mounted worktree's files, not the container's `$PATH`, git config or installed binaries. If a tool must run inside a container, wrap it explicitly:

```toml
[tools.lazygit]
command = "docker exec -it my-container lazygit"
```


## tmux session naming

Tool sessions are named `aoe_tool_<tool>_<title>_<id8>` (`aoe_dev_tool_` in debug builds; `<id8>` is the
first 8 characters of the agent session ID). For a manual attachment, use
`tmux attach -t "=<name>:"` so a missing name cannot match another session's
prefix. Use AoE's tmux socket with `-S <socket>` when it differs from the default.
