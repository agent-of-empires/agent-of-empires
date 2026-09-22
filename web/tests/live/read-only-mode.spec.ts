// `aoe serve --read-only` advertises the flag and rejects mutations; src/server/tests/read_only.rs covers every endpoint.

import { test, expect } from "../helpers/liveTest";

test("/api/about reports read_only=true", async ({ serveReadOnly }) => {
  const about = await fetch(`${serveReadOnly.baseUrl}/api/about`).then((r) => r.json());
  expect(about?.read_only).toBe(true);
});

test("mutations are rejected with 403 before the body is parsed", async ({ serveReadOnly }) => {
  // #1229: the guard runs before axum's typed extractor, so malformed bodies get 403, not 422.
  const cases: { label: string; path: string; body?: string }[] = [
    {
      label: "valid session",
      path: "/api/sessions",
      body: JSON.stringify({ title: "blocked", path: "/tmp/whatever", tool: "claude" }),
    },
    { label: "wrong-shape JSON", path: "/api/sessions", body: JSON.stringify({ junk: true }) },
    { label: "non-JSON garbage", path: "/api/sessions", body: "not even json" },
    { label: "empty body", path: "/api/sessions" },
    { label: "skills garbage", path: "/api/skills", body: "not even json" },
  ];
  for (const c of cases) {
    const res = await fetch(`${serveReadOnly.baseUrl}${c.path}`, {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: c.body,
    });
    expect(res.status, `case: ${c.label}`).toBe(403);
    if (c.path === "/api/skills") expect(await res.json()).toMatchObject({ error: "read_only" });
  }
});

test("dashboard suppresses mutation UI in read-only", async ({ serveReadOnly, page }) => {
  // The `n` shortcut's guard reads /api/about, so wait for it before pressing.
  const aboutPromise = page.waitForResponse((r) => r.url().endsWith("/api/about") && r.status() === 200, {
    timeout: 10_000,
  });
  await page.goto(serveReadOnly.baseUrl);
  await aboutPromise;
  await expect(page.getByText("This dashboard is in read-only mode.")).toBeVisible();

  await page.locator("body").click();
  await page.keyboard.press("n");
  await expect(page.getByRole("heading", { name: "New session" })).toBeHidden({ timeout: 2_000 });
});
