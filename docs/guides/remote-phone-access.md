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

- **HTTPS end-to-end** via Tailscale or Cloudflare.
- **Two factors at first pairing**: the auth token embedded in the QR URL, plus the passphrase typed on the login page. Either alone is useless. After first pairing, the device is bound and stays signed in across token rotations without re-prompting (see below).
- **Device-bound login session.** Each browser generates a high-entropy secret (`crypto.getRandomValues`) on first load and persists it in `localStorage`. After a successful passphrase login, every authenticated request to that browser must carry both the `aoe_session` cookie AND the device-binding secret. A stolen cookie alone is therefore not enough; the attacker also needs the binding secret. The session is no longer tied to your public IP, so mobile network rotation (Wi-Fi to cellular, Cloudflare CGNAT churn, iCloud Private Relay, VPN reconnect) does not log you out. Clearing site data or reinstalling the PWA generates a new secret and requires re-entering the passphrase once.
- **Idle logout after 30 days.** Every authenticated request slides the session deadline 30 days into the future. An actively-used device stays logged in indefinitely; 30 days with no requests from the device invalidates the session and the next visit hits the passphrase prompt again. GitHub-style, not banking-style.
- **Token rotation is transparent for bound devices.** The auth token in the QR URL rotates every 4 hours (in remote mode) for internal hygiene. A device that has already completed both factors authenticates via its session cookie + binding even when its cached token is stale, and the server attaches the current token in the response so the browser refreshes. You will not see the QR / token-paste prompt again until the session itself expires.
- **Passphrase confirmation when editing settings.** The daily-use structured view and terminal surfaces (sending prompts, cancelling turns, resolving approvals, attaching a session terminal, creating / deleting sessions) do not re-prompt; the device-bound session is sufficient. The narrow exception is the persisted-config endpoints: saving global settings, creating / renaming / deleting a profile, editing a profile's settings, or changing the default profile asks for the passphrase again if it has been more than 15 minutes since the last confirmation. This catches the persisted-tamper attack pattern (an attacker with stolen session + binding plants a malicious Docker image or profile, then waits for the owner to spawn a session) without putting friction on the conversation surface.
- **Push notification on every new login.** When the dashboard accepts a passphrase, every device already subscribed to push notifications receives a "New aoe dashboard login" notice. If you ever see one you did not trigger, restart `aoe serve` with a new `--passphrase` (and re-launch the tunnel so the auth token in the QR URL rotates). Note: this only protects you once a second device has subscribed to push. The first device you log in from has nowhere to send the warning, so the binding secret and idle-logout window are the only protection until at least one device is subscribed.
- **Loopback callers skip the passphrase factor.** The local TUI on the same host as the daemon (e.g. `aoe` after `aoe serve --daemon`) authenticates with the bearer token written to `~/.agent-of-empires/serve.url` (file mode `0600`). Filesystem permissions on that file are already the trust boundary for same-host access, so the passphrase wall adds friction without strengthening the model. The server only applies this carve-out when the resolved client IP is loopback; remote callers proxied through a tunnel resolve to the real remote IP and still need the passphrase. Local TUI attach against a tokenless `--auth=passphrase` daemon is not yet supported; run with token auth (the default) if you want to bridge between the web and the local TUI.
- Tunnel stays up as a background daemon after you close the TUI. Press `R` again anytime to reattach, press `S` to stop, or run `aoe serve --stop` from a shell.

Don't screenshot the QR and passphrase together, and stop the tunnel when you're done.

## Troubleshooting

- **401 or "missing auth token"**: scan the QR, not a screenshot of the URL without the `?token=...` query.
- **QR never appears**: either `tailscale status` should report the daemon is logged in, or `cloudflared --version` should work from the same shell you launched `aoe` from.
- **Tailscale card shows "Funnel not enabled for this node"**: the tailnet ACL doesn't grant the `funnel` nodeAttr to this device. If your node is tagged, `autogroup:member` rules don't apply to it — target the tag instead, or add a rule targeting `*`. Save the ACL and press `[R]` on the Confirm screen to re-check.
- **"Tailscale Funnel is not enabled for this tailnet"**: click the node-specific URL shown in the error to flip the tailnet-wide switch at [login.tailscale.com/f/funnel](https://login.tailscale.com/f/funnel). aoe detects this condition in seconds via `tailscale funnel` stderr, so you won't wait out a 60s timeout.
- **"port 443 is already configured on this node"**: a non-loopback Funnel from another tool is using port 443. Press `[R]` on the Error dialog to run `tailscale funnel reset`, then retry. Stale configs from a prior aoe run are fine and get overwritten automatically.
- **Started `aoe serve` from the CLI instead**: press `R` in the TUI; it attaches to the running daemon.
- **Installed PWA stopped working after aoe restart**: you were on a Cloudflare quick tunnel and the URL rotated. Switch to Tailscale Funnel (or a named Cloudflare tunnel with a stable domain), delete the installed PWA, and reinstall from the new stable URL.
