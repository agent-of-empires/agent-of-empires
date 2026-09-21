# Push notifications

The web dashboard can push browser notifications when an agent wants your attention. On iOS they appear on the Lock Screen and tap-to-open deep-links into the session.

## What triggers one

Three status events, each independently toggleable in Settings and overridable per session: **Waiting** (the session holds that status for five seconds), **Idle** (a long job settles), and **Error**. A 60-second per-session cooldown prevents re-buzzing when a session flickers.

Two more, **approval** and **question** ([AskUserQuestion](structured-view/controls.md#questions-askuserquestion)), fire immediately and bypass the suppression rules below: with the dashboard or TUI foregrounded you still get an in-app toast, plus the browser chime described in [Sound effects](sounds.md).

Status notifications are suppressed while you are already looking at aoe: a focused dashboard tab shows an in-app toast instead of an OS notification (per device), and keyboard, paste, or mouse input in a TUI suppresses pushes for 30 seconds across every device. An unattended TUI does not silence your phone, and background polling does not count as foregrounded.

## A stable HTTPS origin first

Push requires HTTPS, and an installed PWA is bound to the exact origin it was installed from: if the origin changes you must delete and reinstall it. Plain `aoe serve --remote` falls back to a Cloudflare quick tunnel whose URL rotates on every restart, so install from a Tailscale Funnel or a named Cloudflare tunnel instead. See [Remote access](guides/web-dashboard.md#remote-access) for the setup; aoe prints a notice whenever it falls back to a quick tunnel.

## Setup

**iPhone (iOS 16.4+)**: iOS Web Push needs a Home Screen app, so open the dashboard in Safari, use Share then *Add to Home Screen*, and open the app from the Home Screen (not Safari). Then Settings, Notifications, *Enable notifications*, grant permission, and *Send test notification*; the server waits a few seconds so you can lock the phone.

**Desktop** (Chrome, Firefox, Edge, Safari): Settings, Notifications, *Enable notifications*, then *Send test notification*. Desktop Safari needs macOS 13 or later and no PWA install.

## How it works

Standard Web Push over VAPID: the server holds a long-lived keypair, each browser registers a subscription with its push service, and payloads are encrypted end to end, so the relay cannot read session titles or URLs. Subscriptions are bound to your bearer token and dropped when it rotates past its grace period.

Operators can disable push server-wide with `web.notifications_enabled = false` (TUI Settings, Web category, or the config file). `/api/push/*` then returns 404, nothing is delivered, and clients show a *disabled by the server* state. Existing subscriptions persist, and re-enabling resumes delivery after a server restart.

## Troubleshooting

- **"Enable notifications" does nothing on iPhone**: open the app from the Home Screen, not Safari.
- **Test says delivered but nothing appears**: check Focus modes, Do Not Disturb, and the app's notification allowances in iOS Settings.
- **"Delivery failing" badge**: the server cannot reach the push endpoint, usually no outbound HTTPS or a push service outage. Click Diagnose for the last error.
- **"Disabled by the server"**: ask the operator about `web.notifications_enabled`.
- **Notifications stop after a while**: token rotation drops stale subscriptions. With `--remote` the token rotates every four hours, so grab a fresh dashboard URL and re-enable.
- **A notification opens the wrong host or port**: payloads carry the origin recorded at subscribe time, so after changing `--host`, `--port`, or your remote URL, click **Re-subscribe** on the affected device. Subscriptions created before origin tracking are skipped on send and Re-subscribe upgrades them.
- **Push stops after an upgrade**: the new service worker activates on the next PWA open. Open the installed app, let it reload, then send a test.
