// DisconnectBanner recovery flow.
//
// `useSessions` polls /api/sessions every 3s and flips
// `setServerDown(true/false)` based on success. When the server is
// down, `DisconnectBanner` renders `role=alert` "Server unreachable".
// When the server comes back, a `role=status` "Reconnected" flash
// shows for ~3s and then auto-dismisses.
//
// Spec drives the full cycle:
//   1. Navigate to the dashboard in no-auth mode.
//   2. SIGTERM the live `aoe serve` proc.
//   3. Wait for the alert banner.
//   4. Re-spawn the server on the same port via `serve.restart()`.
//   5. Wait for the reconnected flash.
//   6. Assert the flash auto-dismisses inside its 3s timer.

import { test, expect } from "../helpers/liveTest";

test("SIGTERM surfaces the alert; restart() clears it and flashes reconnected", async ({ serve, page }) => {
  // Narrow role queries by text content. The dashboard has an unrelated
  // dnd-kit live region (`<div role="status" aria-live="assertive">`)
  // that would otherwise match `getByRole("status")` and false-trigger
  // the reconnected assertion.
  const alertBanner = page.getByRole("alert").filter({ hasText: /server unreachable/i });
  const reconnectedBanner = page.getByRole("status").filter({ hasText: /reconnected/i });

  await page.goto(serve.baseUrl, { waitUntil: "domcontentloaded" });

  // Let the first /api/sessions poll land so `useSessions` has set
  // serverDown = false. Without this gate, the SIGTERM can race the
  // initial fetch and the "Reconnected" assertion comes too early.
  await page.waitForResponse((r) => r.url().endsWith("/api/sessions") && r.status() === 200, { timeout: 10_000 });

  await expect(alertBanner).toBeHidden();
  await expect(reconnectedBanner).toBeHidden();

  // SIGTERM directly so we can observe the disconnect banner BEFORE
  // the harness respawns. `restart()` would kill + respawn in one
  // shot and the banner would never get its 3s poll window to render.
  serve.proc.kill("SIGTERM");

  // The poll interval is 3s; allow up to 8s for the first failed
  // tick to land + React commit to render the alert.
  await expect(alertBanner).toBeVisible({ timeout: 8_000 });

  // Bring the server back on the same port. `restart()` is a no-op
  // SIGTERM on the dead proc, then a fresh spawn with the captured args.
  await serve.restart();

  // /api/about responds on the new boot.
  await expect
    .poll(
      async () => {
        try {
          const r = await fetch(`${serve.baseUrl}/api/about`);
          return r.status;
        } catch {
          return -1;
        }
      },
      { timeout: 10_000 },
    )
    .toBe(200);

  // Reconnected flash renders inside its 3s window. The poll interval
  // is 3s, so worst-case we wait ~3s before the next /api/sessions
  // succeeds + setServerDown(false) fires.
  await expect(reconnectedBanner).toBeVisible({ timeout: 8_000 });

  // 3s timer in DisconnectBanner auto-clears the flash. Wait past it
  // with a small margin and confirm both banners are gone.
  await expect(reconnectedBanner).toBeHidden({ timeout: 6_000 });
  await expect(alertBanner).toBeHidden();
});
