# Modes, Approvals & Model Controls

The composer footer is where you steer a structured-view session: the permission mode decides what runs without asking, approval cards gate the rest, and model and reasoning-effort selectors tune the agent when the adapter advertises them. For the surrounding UI, see [Interface](interface.md).

![A destructive-action approval card with a long-press confirmation ring](../assets/structured-view/approval.png)

## Permission modes and YOLO

A session runs in one of the modes its ACP adapter advertises. The mode picker shows whatever the agent reports; for `claude-agent-acp` the typical set is:

| Mode | Meaning |
|------|---------|
| `default` | Every Write/Edit/Bash routes through an approval card. |
| `acceptEdits` | Edit-kind tools auto-approved; Bash and unknown tools still prompt. |
| `bypassPermissions` | All tools auto-approved (the YOLO mode). |
| `plan` | Read-only; the agent drafts a plan but runs no side-effectful tools. |

Other adapters report their own ids (Gemini's `auto_edit` and `yolo` map onto `acceptEdits` and `bypassPermissions`).

Turning on `[session] yolo_mode_default` (or the wizard's "Auto-approve actions" toggle) asks the adapter to start in `bypassPermissions`. This is best-effort: if the adapter accepts, the picker flips and approval cards stop; if it rejects, an amber notice appears and the session keeps running in whatever mode it landed on. `claude-agent-acp` only offers `bypassPermissions` when `aoe serve` was launched with `ALLOW_BYPASS=1` in its environment; without it, use `acceptEdits` or approve as you go.

## Approvals

When the agent wants to run a tool that needs approval, the structured view shows a card:

- **Benign tools** (read, search, list): single tap.
- **Destructive tools** (`rm -rf`, `git push --force`, writes to system paths): long-press 800ms with a confirmation ring; single tap is reserved for deny.

```toml
[acp]
approval_timeout_secs = 300              # a pending approval auto-cancels after this
destructive_require_double_confirm = true
```

The card clears as soon as your decision is accepted. If it already resolved on the daemon (a concurrent decision or a watchdog), resolving again clears it quietly instead of erroring.

### Notifications and sound

When an approval lands and you're away from the dashboard, two channels fire:

- **Web push.** If the PWA is installed and notifications are enabled, the daemon sends an OS push tagged `acp-approval-<session>`; tapping it deep-links back. Unlike status-change pushes, approval pushes are not suppressed when the dashboard or TUI is active (focused clients get an in-app toast). See [Push notifications](../push-notifications.md).
- **Browser sound.** The dashboard tab plays `[sound] on_approval` whenever pending approvals go from zero to non-zero. It plays client-side because `aoe serve` often runs on a remote box where the host speaker is on the wrong side of the wire.

## Model and reasoning effort

When the ACP adapter advertises them, the composer footer shows a model dropdown and a reasoning-effort selector beside the mode pill. `claude-agent-acp` v0.39.0+ advertises a model selector for every session, and adds a reasoning-effort selector when the current model reports `supportsEffort`. Adapters that advertise neither show no pickers (by design, so non-Claude backends don't grow empty chrome).

Click an option to switch. The chip keeps showing the previous value until the adapter confirms, so it never snaps back on a slow connection. The reasoning-effort dropdown includes a `Default` option that drops any session-level effort pin so the model uses its own default budget. If a switch is rejected (rate limit, transient error), an amber non-blocking notice shows the rejected value and the adapter's reason; it clears when a later snapshot reports the requested value as current. The selector list clears when you switch agents but survives `/clear`.

How the two underlying ACP channels are normalized into one dropdown is in [Structured View Internals](../development/internals/structured-view.md#permission-modes-and-model-channels).

## Session persistence

Structured-view workers and transcripts outlive an `aoe serve` restart, a closed laptop, and a reconnect: in-flight turns continue and the next `aoe serve` reattaches. To actually terminate a worker, use `aoe acp stop <session>` (graceful) or `aoe acp kill <session>` (force). For agents that support session restoration (Claude today), the model also retains context across restarts, so a follow-up like "what did we just decide?" still works. The mechanics are in [Structured View Internals](../development/internals/structured-view.md#worker-lifecycle-and-persistence).
