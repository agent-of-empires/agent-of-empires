# Push notifications

The web dashboard can send browser push notifications when an agent is waiting for your input. On iOS, these appear on the Lock Screen and tap-to-open deep-link into the session.

## What triggers a notification

Four event types. The first three are status-driven and independently toggleable in Settings:

- **Waiting** — session transitions to `Waiting` and stays that way for at least five seconds (the most common pattern: agent paused to ask you something). Longer dwell because Claude sometimes pauses briefly before resolving.
- **Idle** — session finishes a long-running job and settles into `Idle`.
- **Error** — session crashes into `Error`.

A shared 60-second post-send cooldown per session prevents rapid re-buzzing when a session flickers between states.

Each session also has per-session overrides that beat the server-wide defaults: you can enable `Idle` notifications only on the one long-running session you care about, for example, without flooding yourself every time any session finishes.

The fourth event type comes from structured view sessions and runs on its own rules:

- **Structured view approval**, a structured view-driven agent emits an `ApprovalRequested` event because a tool needs your permission. Fires immediately (no dwell), tag `acp-approval-<session>`, and **bypasses** the TUI/web-active suppression below. Even when the dashboard or TUI is foregrounded the approval still routes through web push: the service worker forwards focused clients to an in-app toast instead of an OS banner so you still get an audible/visual cue. The structured view also plays a browser-side chime keyed off `[sound] on_approval`; see [Sound effects](sounds.md). The corresponding `Waiting` status push is suppressed while the approval is unresolved, so you do not get a second OS banner for the same underlying event.

Status notifications are suppressed when you're already looking at aoe (structured view approvals ignore this list):

- **Dashboard focused (per-device):** if the PWA browser tab is visible and focused, that device skips the OS notification and shows an in-app toast instead.
- **TUI active (all devices):** if the `aoe` TUI is running on the same machine as the server, all push notifications to all devices are suppressed. The TUI writes a heartbeat file (`$app_dir/tui.active`) every 10 seconds; the push consumer skips delivery when the file was modified within the last 30 seconds.
- **Web dashboard active (all devices):** if any browser has the web dashboard open and is making authenticated API requests, all push notifications are suppressed. The auth middleware stamps the last request time; the push consumer skips delivery when a request arrived within the last 30 seconds. This means using the dashboard on your laptop prevents notifications from firing on your phone.

## Stable HTTPS for persistent PWA installs (read this first if using mobile)

Push requires HTTPS. An installed PWA is bound to the exact origin it was installed from, so if the origin changes the install breaks: the user has to delete the PWA from the home screen and reinstall at the new URL.

`aoe serve --remote` with no other flags defaults to a Cloudflare **quick tunnel** that gets a fresh random URL on every restart (`foo-bar-xxxx.trycloudflare.com`). That's fine for one-off remote sessions, but a PWA installed from that URL stops working the next time you restart the server.

aoe picks a stable transport automatically when it can:

1. **Tailscale Funnel (preferred).** If `tailscale` is installed on the host and logged in, aoe runs `tailscale funnel --bg --yes <port>` at startup (the Tailscale 1.52+ single-command Funnel syntax) and uses the resulting `https://<machine>.<tailnet>.ts.net` URL. That URL is stable across restarts, so a PWA installed from it keeps working forever. No domain or Cloudflare account needed. Two one-time steps: enable Funnel for your tailnet at [login.tailscale.com/f/funnel](https://login.tailscale.com/f/funnel) (tailnet-wide feature switch), and grant the `funnel` nodeAttr to this node in the tailnet ACL at [login.tailscale.com/admin/acls/file](https://login.tailscale.com/admin/acls/file). Then `tailscale up` on the host and `aoe serve --remote` does the rest.

2. **Named Cloudflare tunnel.** Pass `--tunnel-name <name> --tunnel-url <hostname>` to aoe. Requires a Cloudflare account and a one-time `cloudflared tunnel create` + DNS setup. Stable hostname on your own domain.

3. **Cloudflare quick tunnel.** Fallback when neither of the above is available. Works for ad-hoc sessions; don't install the PWA from it.

aoe prints a notice when it falls back to the quick tunnel so you don't accidentally install a PWA from an unstable origin.

## Setup on iPhone (iOS 16.4 or later)

Push notifications on iOS require the dashboard to be installed as a Home Screen app. Safari tabs cannot receive pushes.

1. Open the dashboard URL in Safari (not Chrome or another browser).
2. Tap the Share icon at the bottom of the screen.
3. Scroll down and tap *Add to Home Screen*, then *Add*.
4. Open the app from your Home Screen (not from Safari).
5. Go to Settings in the app.
6. In the Notifications section, tap *Enable notifications* and grant permission when iOS asks.
7. Tap *Send test notification*. The server waits a few seconds before firing the push so you have time to lock your phone; the notification should then appear on your Lock Screen.

If the test does not appear:
- Make sure the app was opened from the Home Screen, not Safari.
- Check iOS Settings, Notifications, Agent of Empires: banners and Lock Screen allowed.
- Check Focus modes: a Focus mode may be silencing the notification.
- Tap Send test notification again. If you see *delivery failing* in the Settings view, the server's push endpoint is unreachable; check your tunnel.

## Setup on desktop (Chrome, Firefox, Edge, Safari)

1. Open the dashboard URL.
2. Go to Settings. In the Notifications section, click *Enable notifications*.
3. Grant permission when the browser asks.
4. Click *Send test notification*. The server waits a few seconds before firing, so the notification arrives shortly after the click.

Desktop Safari requires macOS 13 or later and does not require the PWA install step.

## Operator kill switch

Operators can disable push notifications server-wide by setting `web.notifications_enabled = false` in the TUI settings (Settings, Web category) or directly in the config file:

```toml
[web]
notifications_enabled = false
```

When disabled:
- `/api/push/*` endpoints return 404.
- The status-change consumer drops events without attempting delivery.
- Clients see a *disabled by the server* state in the Settings UI.
- Existing subscriptions persist; toggling back to `true` resumes delivery.

Changes to this flag require a server restart to take effect.

## How it works

Standard Web Push over VAPID:

- Server generates a long-lived P-256 keypair on first start, stored at `$app_dir/push.vapid.json` with mode 0600.
- Each browser registers a subscription with the push service (Apple's APNs for Safari and iOS, Firebase Cloud Messaging for Chrome and Edge, Mozilla's push service for Firefox). The subscription URL and key material are POSTed to `/api/push/subscribe` and stored at `$app_dir/push.subscriptions.json`.
- When a session transitions to Waiting and the dwell elapses, the server:
  - Generates an ephemeral P-256 keypair per push.
  - Performs ECDH with the subscription's public key and derives a content encryption key via HKDF.
  - Encrypts the payload with AES-128-GCM.
  - Signs a VAPID JWT (ES256, 12-hour expiry).
  - POSTs the encrypted body to the subscription's push endpoint with a 10-second timeout. Up to 8 concurrent sends at once.

Subscriptions are bound to the SHA-256 of the bearer token at subscribe time. On token rotation, subscriptions whose owner-hash no longer matches the current or grace-period token are dropped.

## Threat model

- **Push endpoint URLs are correlatable.** Apple and Google can see that a given device has a subscription on your server. We do not fight this (nothing does); it is inherent to Web Push.
- **Payload is encrypted in transit.** The push relay (Apple, Google, Mozilla) cannot read session titles or URLs.
- **No proxy exposure.** The server's reqwest client is built with `no_proxy()`: corporate MITM proxies do not see endpoint URLs or payloads.
- **Rotation invalidates.** A device that ever held a valid token loses push access when the token rotates past grace (5 minutes by default).
- **Storage on disk is plaintext.** `push.vapid.json` and `push.subscriptions.json` have mode 0600. A host compromise recovers both. Given aoe's single-user-host model, this matches the threat level for `serve.token` stored in the same directory.

## Upgrade note

Upgrading aoe while the PWA is installed replaces `sw.js` but the new service worker does not activate until the next PWA open. If you upgrade and push stops working, open the installed PWA, let it reload, then send a test from Settings.

## Troubleshooting

**"Enable notifications" button does nothing on iPhone.** Open the app from the Home Screen, not Safari. iOS Web Push requires the PWA context.

**Test notification says delivered but nothing appears.** Check iOS Focus modes and Do Not Disturb. Check notification allowances in iOS Settings.

**"Delivery failing" badge on an enabled subscription.** The server cannot reach the push endpoint. Usually means the server does not have outbound HTTPS access, or the subscription's push service is down. Click Diagnose to see the last error.

**"Disabled by the server".** Ask the operator to flip `web.notifications_enabled` or set it in the TUI.

**Notifications stop after a while, and you need to re-enable.** This is token rotation dropping stale subscriptions. If you use `aoe serve --remote`, the token rotates every four hours; grab a fresh dashboard URL and re-enable in the PWA.

**Tapping a notification opens the wrong port or hostname.** Push payloads carry the origin recorded at subscribe time. If you change `--port`, `--host`, move the deployment behind a different reverse proxy, or your `aoe serve --remote` URL changed, the notification still resolves to the old origin until you refresh the subscription. Open Settings, Notifications, and click **Re-subscribe** on the affected device. Subscriptions created before this tracking landed have no recorded origin and are skipped on send; the same Re-subscribe action upgrades them.
