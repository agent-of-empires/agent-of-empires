# Push notifications

The web dashboard can send browser push notifications when an agent is waiting for your input. On iOS, these appear on the Lock Screen and tap-to-open deep-link into the session.

## What triggers a notification

Three status-driven events, each independently toggleable in Settings:

- **Waiting**: session stays in `Waiting` for at least five seconds (agent paused to ask you something).
- **Idle**: session finishes a long-running job and settles into `Idle`.
- **Error**: session crashes into `Error`.

A 60-second per-session cooldown prevents rapid re-buzzing when a session flickers between states. Per-session overrides beat the server-wide defaults (e.g. enable `Idle` only on the one long-running session you care about).

Two more events, **structured view approval** and **structured view question** (`AskUserQuestion`), fire immediately when a tool needs your permission or the agent asks you something. They bypass the suppression rules below: even with the dashboard or TUI foregrounded, the service worker shows an in-app toast so you still get a cue. The structured view also plays a browser-side chime; see [Sound effects](sounds.md).

Status notifications are suppressed when you're already looking at aoe (approvals and questions ignore this):

- **Dashboard focused (per-device):** if the PWA tab is visible and focused, that device shows an in-app toast instead of an OS notification.
- **TUI active (all devices):** if the `aoe` TUI is running on the server machine, all pushes are suppressed.
- **Web dashboard active (all devices):** if any browser has the dashboard open and making authenticated requests, all pushes are suppressed. So using the dashboard on your laptop prevents notifications on your phone.

## Stable HTTPS for persistent PWA installs (read this first if using mobile)

Push requires HTTPS, and an installed PWA is bound to the exact origin it was installed from. If the origin changes, the install breaks and you must delete and reinstall the PWA at the new URL.

`aoe serve --remote` with no other flags defaults to a Cloudflare **quick tunnel** with a fresh random URL on every restart. Fine for one-off sessions, but a PWA installed from it stops working after a restart. aoe picks a stable transport automatically when it can:

1. **Tailscale Funnel (preferred).** If `tailscale` is installed and logged in, aoe runs `tailscale funnel --bg --yes <port>` and uses the stable `https://<machine>.<tailnet>.ts.net` URL. One-time setup: enable Funnel for your tailnet at [login.tailscale.com/f/funnel](https://login.tailscale.com/f/funnel) and grant the `funnel` nodeAttr to this node in the ACL at [login.tailscale.com/admin/acls/file](https://login.tailscale.com/admin/acls/file).
2. **Named Cloudflare tunnel.** Pass `--tunnel-name <name> --tunnel-url <hostname>`. Requires a Cloudflare account and a one-time `cloudflared tunnel create` + DNS setup. Stable hostname on your own domain.
3. **Cloudflare quick tunnel.** Fallback when neither is available. aoe prints a notice when it falls back, so don't install the PWA from it.

## Setup on iPhone (iOS 16.4 or later)

iOS Web Push requires the dashboard installed as a Home Screen app; Safari tabs cannot receive pushes.

1. Open the dashboard URL in Safari (not Chrome).
2. Tap the Share icon, then *Add to Home Screen*, then *Add*.
3. Open the app from your Home Screen (not Safari).
4. Go to Settings, Notifications, tap *Enable notifications*, and grant permission.
5. Tap *Send test notification*. The server waits a few seconds before firing so you can lock your phone; the notification should appear on your Lock Screen.

If the test does not appear:
- Make sure the app was opened from the Home Screen, not Safari.
- Check iOS Settings, Notifications, Agent of Empires: banners and Lock Screen allowed.
- Check Focus modes; one may be silencing it.
- If you see *delivery failing* in Settings, the server's push endpoint is unreachable; check your tunnel.

## Setup on desktop (Chrome, Firefox, Edge, Safari)

1. Open the dashboard URL.
2. Go to Settings, Notifications, click *Enable notifications*, and grant permission.
3. Click *Send test notification*; it arrives shortly after.

Desktop Safari requires macOS 13 or later and needs no PWA install.

## How it works

Standard Web Push over VAPID: the server holds a long-lived keypair, each browser registers a subscription with its push service (Apple/Firebase/Mozilla), and payloads are encrypted end-to-end so the relay cannot read session titles or URLs. Subscriptions are bound to your bearer token and dropped when the token rotates past its grace period.

> **Operator note:** push can be disabled server-wide via `web.notifications_enabled = false` (TUI Settings, Web category, or the config file). When disabled, `/api/push/*` returns 404, no events are delivered, and clients show a *disabled by the server* state. Existing subscriptions persist; re-enabling resumes delivery. Requires a server restart.

## Upgrade note

Upgrading aoe replaces the service worker, but the new one does not activate until the next PWA open. If push stops working after an upgrade, open the installed PWA, let it reload, then send a test from Settings.

## Troubleshooting

**"Enable notifications" does nothing on iPhone.** Open the app from the Home Screen, not Safari.

**Test says delivered but nothing appears.** Check iOS Focus modes, Do Not Disturb, and notification allowances in iOS Settings.

**"Delivery failing" badge.** The server cannot reach the push endpoint, usually no outbound HTTPS access or the push service is down. Click Diagnose for the last error.

**"Disabled by the server".** Ask the operator to flip `web.notifications_enabled`.

**Notifications stop after a while.** Token rotation drops stale subscriptions. With `aoe serve --remote` the token rotates every four hours; grab a fresh dashboard URL and re-enable in the PWA.

**Tapping a notification opens the wrong port or hostname.** Push payloads carry the origin recorded at subscribe time. If you change `--port`/`--host`, move behind a different reverse proxy, or your remote URL changes, open Settings, Notifications, and click **Re-subscribe** on the affected device. Subscriptions created before origin tracking are skipped on send; Re-subscribe upgrades them.
