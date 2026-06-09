// Read-only mode coverage.
//
// `aoe serve --read-only` must (1) advertise read_only=true on /api/about
// so the frontend can suppress mutation UI, and (2) reject mutating HTTP
// requests with 403. Both layers must hold; src/server/tests/read_only.rs
// (Rust side) does the full mutation-endpoint table.

import { test, expect } from "../helpers/liveTest";

test("/api/about reports read_only=true", async ({ serveReadOnly }) => {
  const about = await fetch(`${serveReadOnly.baseUrl}/api/about`).then((r) => r.json());
  expect(about?.read_only).toBe(true);
});

test("POST /api/sessions is rejected with 403", async ({ serveReadOnly }) => {
  // The right-shape body trips the read-only guard at the top of
  // `create_session` and returns 403.
  const res = await fetch(`${serveReadOnly.baseUrl}/api/sessions`, {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify({
      title: "blocked",
      path: "/tmp/whatever",
      tool: "claude",
    }),
  });
  expect(res.status).toBe(403);
});

test("POST /api/sessions with malformed body still returns 403", async ({ serveReadOnly }) => {
  // Regression for #1229: the read-only check runs BEFORE axum's typed
  // body extractor, so any body shape (including intentionally
  // malformed) must be rejected with 403, not 422.
  const cases: { label: string; init: RequestInit }[] = [
    {
      label: "wrong-shape JSON",
      init: {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({ junk: true }),
      },
    },
    {
      label: "non-JSON garbage",
      init: {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: "not even json",
      },
    },
    {
      label: "empty body",
      init: {
        method: "POST",
        headers: { "Content-Type": "application/json" },
      },
    },
  ];
  for (const c of cases) {
    const res = await fetch(`${serveReadOnly.baseUrl}/api/sessions`, c.init);
    expect(res.status, `case: ${c.label}`).toBe(403);
  }
});

test("dashboard suppresses mutation UI in read-only", async ({ serveReadOnly, page }) => {
  // Wait for /api/about to land before driving keyboard shortcuts. The
  // "n" shortcut handler reads `serverAbout?.read_only`; if /api/about
  // hasn't resolved yet, `serverAbout` is `null` and the read-only guard
  // is bypassed. Wait for the response so the React state has the flag
  // before the keypress fires.
  const aboutPromise = page.waitForResponse((r) => r.url().endsWith("/api/about") && r.status() === 200, {
    timeout: 10_000,
  });
  await page.goto(serveReadOnly.baseUrl);
  await aboutPromise;
  // Small settle so React commits the serverAbout state from the response.
  await page.waitForTimeout(200);

  await page.locator("body").click();
  await page.keyboard.press("n");
  await expect(page.getByRole("heading", { name: "New session" })).toBeHidden({
    timeout: 2_000,
  });
});
