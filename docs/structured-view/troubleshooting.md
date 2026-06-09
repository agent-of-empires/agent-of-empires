# Structured view Troubleshooting

The security model structured view enforces, followed by a field guide to every
failure mode and how to recover. For the day-to-day interface, see
[Structured view Interface](interface.md); for what survives a restart, see
[Persistence & recovery](../development/internals/structured-view.md).

## Security

- Agents never touch the disk directly. They go through ACP's
  `fs/read_text_file` / `fs/write_text_file`, and aoe reads/writes on their
  behalf, enforcing the sandbox roots (the session's worktree plus any explicit
  `--repo` paths).
- Terminal commands run in the session's worktree, or inside the
  `aoe-sandbox-<id>` container (via `docker exec`) when sandbox is enabled.
- Approval nonces are server-generated and single-use; aoe never reveals them
  to the agent, so a compromised agent cannot synthesise approvals.
- Auth tokens (`AOE_TOKEN`) are not forwarded to the agent subprocess.

### Sandbox containers

Structured view sessions honor the wizard's **Run in a safe container** toggle.
When enabled, the ACP agent runs inside the same `aoe-sandbox-<id>` Docker
container the tmux view uses, and the daemon wraps the agent argv in
`docker exec`.

The published `aoe-sandbox` image bundles the ACP adapters structured view
sessions need (`claude-agent-acp`, `codex-acp`, `pi-acp`) alongside the
underlying CLIs whose binaries already provide ACP themselves (`opencode acp`,
`gemini --acp`, `vibe-acp`). Custom sandbox images must include the same
adapters or the `docker exec` invocation fails with exit status 127 and the ACP
handshake times out after 30s.

## Troubleshooting

### `aoe acp doctor` says Node is missing

Install Node.js 20 or newer:

- macOS: `brew install node`
- Linux: `apt install nodejs` or `nvm install 20`
- Windows: download from <https://nodejs.org/>

Then re-run `aoe acp doctor` to verify. If you have Node installed in a
non-standard location, set `AOE_ACP_NODE=/path/to/node` or configure
`acp.node_path` in `config.toml`.

### `aoe acp doctor` says aoe-agent is missing

`aoe-agent` ships with the aoe binary. If the doctor reports it missing, your
install is incomplete. Reinstall aoe via your package manager (e.g.,
`brew reinstall aoe`).

### `aoe acp doctor` says claude-code adapter is missing

aoe requires `claude-agent-acp` v0.44.0 or newer. Install the official adapter:

```bash
npm install -g @agentclientprotocol/claude-agent-acp@latest
```

Then run `claude login` if you haven't already. If an older version is pinned by
an internal mirror, ship 0.44.0 from the mirror or run the `@latest` install
above before starting `aoe serve`.

### "Failed to start structured view agent" while the adapter is installed

`aoe serve` captures the launching shell's PATH at startup. If the adapter lives
under a node-version-manager dir (nvm, fnm, mise, asdf) and the node version on
the daemon's PATH doesn't match, the spawn fails with
`agent spawn failed: No such file or directory`.

Either restart `aoe serve` from a shell where `which claude-agent-acp`
resolves, or symlink the binary into a standard dir (`/usr/local/bin`,
`~/.local/bin`, etc.).

### "Project path no longer exists" banner

The session's working directory was renamed, moved, or deleted out from under
`aoe serve` (most often a `git worktree move` or a manual `mv`). Two ways to
recover:

1. **Restore the directory at the path the banner shows** (e.g.
   `git worktree move <new> <old>`, or recreate the dir), then click **Retry**.
   Transcript continuity is preserved.
2. **Stop `aoe serve`**, edit `project_path` for this session in
   `~/.agent-of-empires/profiles/<profile>/sessions.json` to point at the new
   location (update `worktree_info.branch` too if the branch was renamed), then
   start `aoe serve` again. History and `acp_session_id` are preserved; the
   conversation resumes against the new path.

Reinstalling the adapter does not help here; the adapter is fine, the cwd is
gone.

### Agent stopped responding to cancel

If the agent ignores `session/cancel` mid-tool-call, aoe restarts the worker and
resumes the transcript. The structured view shows "Agent stopped responding to
cancel. Restarting worker; your transcript will be preserved" while the respawn
is in flight, and the banner clears once the new worker is online.

Follow-up prompts the daemon refused while the original turn was still in flight
show in the composer as amber "Rejected" pills with a Retry button; clicking
Retry re-dispatches the prompt against the freshly-respawned worker.

### Tool card stuck "running" after a stop

Stopping the agent while a tool call is mid-execution settles that tool's card
to a muted **stopped** state: the elapsed-time timer freezes and the badge
leaves the orange "running" state. This is intentional. "stopped" is neither
"done" nor "failed"; the tool's real outcome was never reported. The same
applies on reload and when the backend switches agents mid-turn.

### Rate-limit recovery

When the active backend hits its rate limit, aoe parks the session rather than
respawning into the same limit. The dashboard shows a banner with the reset
time and a primary **Continue in another agent** CTA. Clicking it opens a picker
of the structured view ACP registry (claude / codex / opencode / gemini / vibe
/ pi / aoe-agent by default, plus anything you've added), preselects `codex`
when installed, switches on confirm, and pre-fills the composer with a recap of
the prior conversation (including your last prompt if it triggered the limit).
Review and send manually; it is not auto-sent.

#### Optional auto-resume after reset

If you would rather stay on the same backend and have AoE pick the session back
up automatically once the limit clears, enable the opt-in setting (off by
default):

```toml
[acp]
rate_limit_auto_resume = true
rate_limit_auto_resume_grace_secs = 15   # cushion added to the reported reset time
```

Both keys are editable in the structured view settings (TUI and web dashboard,
under "Advanced") and can be overridden per profile. The reset time survives an
`aoe serve` restart. The manual "Continue in another agent" and reconnect paths
stay available regardless of the setting.

### Switching agents manually

The same hand-off is available at any time, not just during a rate limit. This
matters when you handed a session off (say, claude to codex during a rate limit)
and later want to return to the original agent.

- **Web dashboard:** right-click a structured view session in the sidebar and
  pick "Switch agent". It opens the same picker and switches on confirm. The
  composer is pre-filled with a recap; review and send manually.
- **CLI:** `aoe acp switch-agent <session> <target>` (run `aoe acp agents` to
  list valid target keys). Pass `--model <name>` to override the model the new
  agent starts with.

The transcript divider reads `Switched structured view agent from <from> to <to>
(manual)`, distinct from the `(rate_limited)` divider the recovery flow emits.

### Native binary launch failure

When the structured view banner shows an error of the form

```text
Claude Code native binary at /usr/lib/node_modules/.../claude exists but failed to launch.
```

the adapter found its bundled Claude Code native sub-binary on disk but `execve`
was rejected by the kernel. Reinstalling `claude-agent-acp` does not help; the
binary is already there.

The common causes:

1. **Architecture mismatch.** The binary's filename ends in a target triple
   (`...-linux-arm64/claude`, `...-linux-x64/claude`). If the host or container
   reports a different arch via `uname -m`, the loader refuses it. Most often an
   `arm64` host pulling an `amd64` image without `--platform`.
2. **Missing dynamic loader or old glibc.** Slim base images sometimes ship
   without `/lib64/ld-linux-x86-64.so.2` or with a glibc too old. `ldd <binary>`
   from inside the container reports the gap.
3. **Bind-mounted `node_modules` across arch.** A host `arm64` binary cannot
   launch in an `amd64` container and vice versa.

Use **Open agent log** on the red startup banner for the verbatim adapter error,
or run `aoe acp logs --session <id>`. To inspect the binary:

```sh
docker exec <container> file /usr/lib/node_modules/@agentclientprotocol/claude-agent-acp/node_modules/@anthropic-ai/claude-agent-sdk-*/claude
docker exec <container> uname -m
```

If the file's arch line does not match `uname -m`, either re-pull the image with
`--platform linux/<host-arch>` or install `claude-agent-acp` inside the
container (rather than bind-mounting from the host).

### Structured view feels "stuck" with no events

- Check `aoe acp logs --session <id>`, or **Open agent log** on the red
  startup-error banner in the dashboard.
- Check the dashboard's connection chrome at the top of the view; it shows
  reconnect status if the WebSocket is degraded.
- A repeatedly-failing worker is parked with a red "session parked" banner.
  Retry from the dashboard or run `aoe acp restart <session>`.

### "Force end turn" button under the spinner

If the agent finished a turn but the working spinner is still rattling, a small
**Force end turn** button appears beneath it. Click it to clear the spinner and
cancel the agent. It only appears for a silent model with no tool running, and
the view auto-recovers on its own if you do nothing. During a healthy turn, or
while a tool is in flight, the button stays hidden.

### Editing settings asks for the passphrase again

When passphrase login is configured, the daily-use structured view flows
(sending prompts, cancelling turns, resolving approvals, switching mode,
restarting workers, attaching terminals) do NOT prompt for the passphrase again.
Your session cookie plus the device-binding secret are sufficient. See #1137.

Editing the persisted config IS gated. Saving the global settings panel,
creating / deleting / renaming a profile, editing a profile's settings, or
changing the default profile requires that your login session has been
"elevated" within the last 15 minutes. The first such action after a fresh page
load surfaces an inline passphrase prompt; subsequent edits inside the same
window go through without re-prompting.

### WebSocket auto-reconnect and keepalive

The view auto-reconnects with exponential backoff if the WebSocket drops, and
resumes the transcript from where it left off so it stays continuous. The banner
shows `Reconnecting (N/7) in Xs...` while the auto-retry is armed, and a manual
**Reconnect** button after the attempts exhaust. Returning the tab to the
foreground triggers an immediate reconnect.

### Approval card vanished without resolving

Approvals expire after `approval_timeout_secs` (default 300). The agent receives
a structured cancellation; you'll typically see a follow-up message asking
again. Bump the timeout if you're in a context where approvals legitimately take
longer.

### `/clear` collapsed earlier turns

When you run `/clear` in a structured view session, the model's context is wiped
on the adapter side but the visible transcript is preserved. The view appends a
"Conversation cleared" divider, resets the active plan, current mode, in-flight
approvals, and usage snapshot, then folds every row above the divider behind a
disclosure banner: `Show N earlier turns (cleared, not in the model's memory)`.
Click the banner to expand the older transcript for your own reference; the
model still won't see those turns. See
[#1101](https://github.com/agent-of-empires/agent-of-empires/issues/1101).

The slash-command palette and mode picker stay populated across a `/clear`.

A `/clear` queued mid-turn (or any agent's clear alias, e.g. codex / opencode
`/new`) fires as its own send when the turn ends. An ordering like `foo`,
`/clear`, `bar` lands as three separate prompts; the queued-prompt strip shows
an amber `fires separately` divider between rows that will land in different
sub-batches. See #1356.

The session cost figure in the composer footer reads "since the most recent
`/clear` (or `/compact`)" rather than session-lifetime cumulative. See #1354.

### Sharing debug logs

`AOE_LOG_LEVEL=debug` (or the legacy `AGENT_OF_EMPIRES_DEBUG=1`) writes agent
stderr verbatim to `debug.log` under the app data dir. We scrub common API-key
prefixes (Anthropic `sk-...`, GitHub `ghp_...`, AWS `AKIA...`, `Bearer <token>`,
etc.) before they hit disk, but the scrub is best-effort; a hand-rolled secret
with no recognisable shape will pass through. Before attaching `debug.log` to a
bug report, skim it for anything that looks like a credential and replace it
with `<redacted>` if needed.
