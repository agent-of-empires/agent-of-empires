// Mocked port of the live acp-telemetry-seen spec.
//
// structured_view_seen telemetry activation (#1882). The backend path
// for `structured_view_seen` shipped with #1863 (endpoint, AtomicBool,
// snapshot swap-on-read) and the `reportTelemetrySeen` helper accepts
// `surface: "structured_view"`, but no frontend caller ever passed
// `"structured_view"`, so the flag was always false. These specs pin
// the activated frontend behavior: opening a structured view session
// fires the structured view seen-ping, and a read-only server
// short-circuits the shared guard so no ping is sent. (The backend's
// 403-on-read-only enforcement stays covered by Rust tests; the
// frontend should never get that far.)
//
// Pure page.route stubs; no WS frames needed. The mock helper captures
// every `POST /api/telemetry/seen` body into `mock.telemetryPings`.

import { test, expect } from "./helpers/mockedTest";
import { mockAcpSession, openStructuredSession } from "./helpers/acpMock";

test("opening a structured view session fires the structured view seen-ping", async ({ page }) => {
  const mock = await mockAcpSession(page, { title: "acp-seen-ping" });
  await openStructuredSession(page, mock);

  // The structured view mount fires `reportTelemetrySeen("structured_view")`.
  // Pre-fix no caller passed `"structured_view"`, so this poll timed out
  // (the bug).
  await expect
    .poll(() => mock.telemetryPings.some((p) => p.surface === "structured_view"), {
      timeout: 10_000,
    })
    .toBe(true);

  // The on-load `"web"` ping still fires too; the structured view ping is
  // additive, not a replacement.
  expect(mock.telemetryPings.some((p) => p.surface === "web")).toBe(true);
});

test("a read-only server sends no telemetry seen-ping", async ({ page }) => {
  // The seen-ping effects (both `"web"` and `"structured_view"`) share the
  // same guard: skip on read-only servers, which can't persist a snapshot.
  const mock = await mockAcpSession(page, { about: { read_only: true } });

  await page.goto("/");
  await expect(page.locator("header")).toBeVisible();
  // Settle so React commits the read-only serverAbout state and any effect
  // that was going to fire would have fired.
  await page.waitForTimeout(500);

  expect(mock.telemetryPings).toHaveLength(0);
});
