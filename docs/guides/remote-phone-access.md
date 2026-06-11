# Remote Access from Your Phone

Start agents on your laptop. Check on them from your phone.

## Four steps

1. **Install `aoe`** (see [Installation](../installation.md)) and one of the two supported tunnel tools on the host:
   - **Tailscale (preferred):** install from [tailscale.com/download](https://tailscale.com/download), run `tailscale up`, then two one-time clicks to unblock Funnel: enable it for the tailnet at [login.tailscale.com/f/funnel](https://login.tailscale.com/f/funnel), and grant the `funnel` nodeAttr to this node in your ACL at [login.tailscale.com/admin/acls/file](https://login.tailscale.com/admin/acls/file). Free, stable URL, no Cloudflare account, **required if you want to install the dashboard as a PWA and have it survive server restarts**.
   - **cloudflared (fallback):** `brew install cloudflared` on macOS, `sudo apt install cloudflared` on Debian/Ubuntu, no Cloudflare account needed. Gives a working URL but it rotates on every restart, which breaks installed PWAs.
2. **Launch the TUI**: `aoe`.
3. **Press `R`**, pick a transport on the Confirm screen (Tailscale Funnel vs Cloudflare Tunnel, cards show each one's readiness), and wait ~10 seconds for the tunnel to come up.
4. **Scan the QR code** with your phone camera, then type the displayed four-word passphrase.

You're in. Tap **Share → Add to Home Screen** (iOS) or **three-dot menu → Install** (Android Chrome) and the dashboard installs as a PWA: launches from your home screen, standalone window, no browser chrome.

**Important if you install the PWA:** use Tailscale for the tunnel. A PWA installed from a Cloudflare quick-tunnel URL will stop working the next time aoe restarts because the URL changes. aoe prints a warning when falling back to the quick tunnel.

## How it's protected

For the full server-side security model and flags, see the [Web Dashboard security section](web-dashboard.md#security). Phone-specific behavior:

- **Two factors at first pairing**: the auth token in the QR URL, plus the passphrase typed on the login page. Either alone is useless. After pairing, the device is bound and stays signed in across token rotations.
- **Device-bound login session.** Each browser generates a high-entropy secret on first load and persists it in `localStorage`. After login, every authenticated request must carry both the `aoe_session` cookie AND that secret, so a stolen cookie alone is not enough. The session is not tied to your public IP, so mobile network rotation (Wi-Fi to cellular, CGNAT churn, iCloud Private Relay, VPN reconnect) does not log you out. Clearing site data or reinstalling the PWA needs one re-login.
- **Idle logout after 30 days.** Every authenticated request slides the deadline 30 days forward; 30 days with no requests invalidates the session. GitHub-style, not banking-style.
- **Sessions survive a daemon restart.** Login sessions are persisted to an owner-only file in the app dir, so restarting `aoe serve` (config edit, `aoe update`, crash) does not log every device out. Changing the passphrase drops every persisted session; set `auth.persist_sessions = false` to force re-authentication on every restart. Step-up elevation is not persisted, so a high-risk action re-prompts for the passphrase after a restart even though the session survives.
- **Token rotation is transparent.** The token in the QR URL rotates every 4 hours; a bound device authenticates via its cookie + binding and the server refreshes the cached token in the response. You won't see the QR / token-paste prompt again until the session expires.
- **Passphrase confirmation when editing settings.** Daily-use actions never re-prompt; saving global settings or creating / editing / deleting a profile asks for the passphrase again if it has been over 15 minutes. See [step-up elevation](web/settings.md#step-up-elevation).
- **Push notification on every new login.** Each device subscribed to push gets a "New aoe dashboard login" notice. If you see one you didn't trigger, open Settings > Web Dashboard > Connected Devices and hit **Sign out all devices**, or restart `aoe serve` with a new `--passphrase` (which also drops every persisted session) and relaunch the tunnel. This only protects you once a second device has subscribed to push.
- **Loopback callers skip the passphrase.** The local TUI on the daemon's host authenticates with the bearer token in `~/.agent-of-empires/serve.url` (mode `0600`); filesystem permissions are already the trust boundary for same-host access. Remote callers through a tunnel resolve to the real remote IP and still need the passphrase. Local TUI attach against a tokenless `--auth=passphrase` daemon is not supported; use token auth (the default) to bridge web and TUI.
- The tunnel stays up as a background daemon after you close the TUI. Press `R` to reattach, `S` to stop, or run `aoe serve --stop`.

Don't screenshot the QR and passphrase together, and stop the tunnel when you're done.

## Troubleshooting

- **401 or "missing auth token"**: scan the QR, not a screenshot of the URL without the `?token=...` query.
- **QR never appears**: either `tailscale status` should report the daemon is logged in, or `cloudflared --version` should work from the same shell you launched `aoe` from.
- **Tailscale card shows "Funnel not enabled for this node"**: the tailnet ACL doesn't grant the `funnel` nodeAttr to this device. If your node is tagged, `autogroup:member` rules don't apply to it; target the tag instead, or add a rule targeting `*`. Save the ACL and press `[R]` on the Confirm screen to re-check.
- **"Tailscale Funnel is not enabled for this tailnet"**: click the node-specific URL shown in the error to flip the tailnet-wide switch at [login.tailscale.com/f/funnel](https://login.tailscale.com/f/funnel). aoe detects this condition in seconds via `tailscale funnel` stderr, so you won't wait out a 60s timeout.
- **"port 443 is already configured on this node"**: a non-loopback Funnel from another tool is using port 443. Press `[R]` on the Error dialog to run `tailscale funnel reset`, then retry. Stale configs from a prior aoe run are fine and get overwritten automatically.
- **Started `aoe serve` from the CLI instead**: press `R` in the TUI; it attaches to the running daemon.
- **Installed PWA stopped working after aoe restart**: you were on a Cloudflare quick tunnel and the URL rotated. Switch to Tailscale Funnel (or a named Cloudflare tunnel with a stable domain), delete the installed PWA, and reinstall from the new stable URL.
- **PWA seems stuck on an old dashboard after updating aoe**: installed PWAs resume the same long-lived page across launches, so new dashboard code only arrives on a reload. The dashboard detects this and shows a "Dashboard updated. Reload now" banner; tap it. If the banner doesn't appear (the page predates it), force-quit the PWA from the app switcher and relaunch.
