# Structured View Troubleshooting

A field guide to the structured view's failure modes. For the day-to-day interface see [Interface](interface.md); for the security model and what survives a restart, see [Structured View Internals](../development/internals/structured-view.md).

## Prerequisites

### `aoe acp doctor` says Node is missing

Install Node.js 22 or newer (`brew install node`, `apt install nodejs`, `nvm install 22`) and re-run `aoe acp doctor`. For a non-standard location, set `AOE_ACP_NODE=/path/to/node` or `acp.node_path` in `config.toml`.

### An adapter is missing or too old

aoe refuses to start a session whose adapter is below the version floor and reports the exact requirement. Install the official adapter (`npm install -g @agentclientprotocol/claude-agent-acp@latest`), then `claude login` if you have not.

`aoe-agent` ships inside the `aoe` binary and installs into the data dir on demand: `aoe acp doctor --fix --adapter aoe-agent`. It needs Node 22.6+. An `aoe` upgrade that changes its bundled sources makes an installed copy stale, and sessions refuse it until `doctor --fix` reinstalls it.

The dashboard surfaces the same thing inline: a compatibility screen with the installed and required versions, the install command to copy, and two controls. **Restart agent** respawns the worker and re-runs the version check, which is all you need after installing the adapter in a shell. **Update & restart** runs the agent's `npm install -g` on the host as the daemon user and then respawns, queueing every other session blocked on that adapter for a respawn too.

That second button appears only for npm-installable agents and only when `acp.allow_agent_install` is on. It is **off by default**, because a global install runs the package's lifecycle scripts as the daemon user; it is always blocked in read-only mode and is `local_only`, so the dashboard cannot turn it on. Enable it from the TUI settings (Advanced) or the config file. Inside a sandbox a host install would not reach the containerized agent, so the action is refused: install the agent in the image instead.

### "Failed to start structured view agent" while the adapter is installed

`aoe serve` captures the launching shell's `PATH` at startup. If the adapter lives under a node-version-manager directory (nvm, fnm, mise, asdf) and the node version on the daemon's `PATH` does not match, the spawn fails with `No such file or directory`. Restart `aoe serve` from a shell where `which claude-agent-acp` resolves, or symlink the binary into `/usr/local/bin` or `~/.local/bin`.

### Native binary launch failure

A banner reading `Claude Code native binary at ... exists but failed to launch` means the adapter found its bundled native sub-binary but the kernel rejected `execve`. Reinstalling the adapter does not help. The causes:

1. **Architecture mismatch.** The filename ends in a target triple (`...-linux-arm64/claude`); if the host or container reports a different `uname -m`, the loader refuses it. Most often an arm64 host pulling an amd64 image without `--platform`.
2. **Missing dynamic loader or old glibc** in a slim base image. `ldd <binary>` inside the container reports the gap.
3. **A `node_modules` bind-mounted across architectures.**

Use **Open agent log** on the banner, or `aoe acp logs --session <id>`, for the verbatim adapter error, and compare `file <binary>` with `uname -m` inside the container. Fix it by re-pulling the image with `--platform linux/<host-arch>` or installing the adapter inside the container rather than bind-mounting it.

## During a session

### "Project path no longer exists"

The session's working directory was renamed, moved, or deleted out from under `aoe serve`. Three ways back:

1. **Restart `aoe serve`.** For a managed worktree moved with `git worktree move`, the daemon repairs `project_path` from `git worktree list` at startup. That does not cover a plain `mv` (see the [worktrees guide](../guides/worktrees.md#when-the-directory-moves-outside-aoe)), does not happen while the daemon keeps running, and is skipped on a read-only daemon.
2. **Restore the directory** at the path the banner names, then click **Retry**. Transcript continuity is preserved.
3. **Stop the daemon and edit `project_path`** for the session in `sessions.json` (and `worktree_info.branch` if the branch was renamed), then start it again. History and `acp_session_id` are preserved.

### Agent stopped responding to cancel

If the agent ignores `session/cancel` mid-tool, aoe restarts the worker and resumes the transcript, showing a banner until the respawn lands. Follow-up prompts the daemon refused while the original turn was in flight appear as amber "Rejected" pills with a Retry button.

### Tool card stuck "running" after a stop

Stopping mid-tool settles that card into a muted **stopped** state: the timer freezes and the badge leaves "running". That is deliberate, since "stopped" is neither done nor failed and the tool's real outcome was never reported.

### "Force end turn" button under the spinner

If the agent finished but the spinner keeps rattling, a **Force end turn** button appears beneath it; clicking it clears the spinner and cancels the agent. It only appears for a silent model with no tool running, and the view auto-recovers on its own if you do nothing. It stays hidden while a tool is in flight, while a question or approval card awaits you, and during a `/compact` (where the spinner reads "Compaction in progress" instead), because compaction runs for a minute or more with no output and force-ending it would discard the summarization.

### "Restarting worker" after a turn looked done

Some adapters finish a turn, stream the final message and end-of-turn usage, but never send the protocol's turn-complete acknowledgement. When the usage arrived and no background or scheduled task was running, the daemon ends the turn cleanly. A genuine stall still restarts the worker and shows the banner; the transcript is preserved either way.

### The view feels stuck with no events

- Read `aoe acp logs --session <id>`, or **Open agent log** on the red startup-error banner.
- Check the connection chrome at the top of the view for reconnect status.
- A repeatedly-failing worker is parked with a red "session parked" banner: retry from the dashboard or run `aoe acp restart <session>`.

The view auto-reconnects with exponential backoff if the WebSocket drops and resumes the transcript where it left off. The banner counts the attempts and offers a manual **Reconnect** once they are exhausted; returning the tab to the foreground reconnects immediately.

### Approval card vanished without resolving

Approvals expire after `approval_timeout_secs` (default 300). The agent receives a structured cancellation and typically asks again. Raise the timeout if your approvals legitimately take longer.

## Rate limits and agent hand-off

When the active backend hits its rate limit, aoe parks the session rather than respawning into the same limit. The banner shows the reset time, or the agent's own wording when it reported no clock, and a **Continue in another agent** CTA. That opens a picker over the ACP registry, preselects `codex` when installed, switches on confirm, and pre-fills the composer with a recap of the prior conversation (including the prompt that triggered the limit). Review and send it yourself; nothing is auto-sent.

### Auto-resume after reset

To stay on the same backend instead, opt in:

```toml
[acp]
rate_limit_auto_resume = true
```

Resume fires once the reported reset plus a 15-second cushion passes, and the reset time survives an `aoe serve` restart. With no reported reset, the first retry is an hour after the park and each further attempt waits twice as long (1h, 2h, 4h, 8h, 16h), so a quota exhausted for days is retried on a matching schedule. The park survives a resume that fails to start.

Each resume re-sends the interrupted prompt, so it gives up after five re-sends that all come back rate-limited, spanning 31 hours. The banner then reads "Auto-resume stopped" and the session stays put. Recovering from the park resets the count, as do a completed turn and an agent switch; manual "Resume now" retries never count against it.

### Switching agents manually

The same hand-off is available any time, which is how you return to the original agent after a rate-limit switch.

- **Web**: right-click the session and pick "Switch agent". The picker lists built-in agents only.
- **CLI**: `aoe acp switch-agent <session> <target>` (`aoe acp agents` lists the target keys; `--model <name>` overrides the model it starts with). A custom agent with an `agent_acp_cmd` is a valid target even though neither surface lists it.

The transcript divider reads `Switched structured view agent from <from> to <to> (manual)`, distinct from the `(rate_limited)` one the recovery flow emits.

## `/clear` collapsed earlier turns

`/clear` wipes the model's context on the adapter side but preserves the visible transcript: the view appends a "Conversation cleared" divider, resets the plan, mode, approvals, and usage, then folds everything above the divider behind a `Show N earlier turns` banner. The slash palette and mode picker stay populated.

For claude and codex a clear starts a genuinely new agent conversation rather than clearing in place, so it survives a worker restart: after an idle-out or daemon restart the next prompt resumes the post-clear conversation. The trade-off is that a clear is refused while background sub-agents or tool calls are still draining, since switching conversations mid-drain would file their output under the new one. Wait and send it again. A clear can also fail outright if the new conversation cannot start (an MCP server that will not re-initialize), leaving the existing conversation untouched.

A `/clear` queued mid-turn (or an agent's alias, such as `/new`) fires as its own send when the turn ends, so `foo`, `/clear`, `bar` lands as three prompts; the queued strip shows an amber `fires separately` divider between rows landing in different sub-batches. The session cost in the composer footer counts from the most recent `/clear` or `/compact`, not the session's lifetime.

## Diff viewer is blank

If the Changes panel shows a file's header but no diff body, with no error and a clean console, a page-restyling browser extension is almost certainly overriding the styling: the diff renders into a shadow DOM and "dark mode for every site" extensions reach into it and make the rows invisible. This is most common on Firefox. Confirm in Troubleshoot Mode, then disable the extension for the dashboard or allowlist the dashboard's origin.

## Editing settings asks for the passphrase again

Day-to-day flows (prompts, cancels, approvals, mode switches, worker restarts, terminal attach) never re-prompt; editing persisted config does. See [step-up elevation](../guides/web-dashboard.md#security).

## Sharing debug logs

`AOE_LOG_LEVEL=debug` writes agent stderr verbatim to `debug.log` in the app data dir. Common API-key shapes (`sk-...`, `ghp_...`, `AKIA...`, `Bearer <token>`) are scrubbed before they hit disk, but the scrub is best-effort: a hand-rolled secret with no recognizable shape passes through. Skim the file before attaching it to a bug report.
