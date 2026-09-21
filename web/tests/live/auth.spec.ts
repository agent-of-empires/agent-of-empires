// Authentication modes: no-auth, passphrase login and sessions, token entry and rotation, devices.

import { readFile } from "node:fs/promises";
import { setTimeout as delay } from "node:timers/promises";
import { test, expect, authHeaders, bootDashboard, type ServeHandle } from "../helpers/liveTest";

test("--no-auth skips LoginPage and lands on dashboard", async ({ serve, page }) => {
  await page.goto(serve.baseUrl);
  await expect(page.locator("input#passphrase")).toBeHidden();
  const statusRes = await page.request.get(`${serve.baseUrl}/api/login/status`);
  expect(statusRes.ok()).toBeTruthy();
  expect((await statusRes.json()).required).toBe(false);
});

// `--auth=passphrase` makes /api/login and /api/login/status exempt, so LoginPage renders (#1230).
test.describe("passphrase login page", () => {
  test("wrong passphrase shows an error and stays on LoginPage", async ({ servePassphrase, page }) => {
    await page.goto(servePassphrase.baseUrl);
    await expect(page.locator("input#passphrase")).toBeVisible();
    await page.locator("input#passphrase").fill("definitely-wrong");
    await page.locator("button[type=submit]").click();
    await expect(page.locator("input#passphrase")).toBeVisible();
    await expect(page.getByText(/incorrect passphrase/i)).toBeVisible({ timeout: 5_000 });
  });

  test("correct passphrase logs in and reveals the dashboard", async ({ servePassphrase, page }) => {
    await page.goto(servePassphrase.baseUrl);
    await expect(page.locator("input#passphrase")).toBeVisible();
    await page.locator("input#passphrase").fill(servePassphrase.passphrase!);
    await page.locator("button[type=submit]").click();
    await expect(page.locator("input#passphrase")).toBeHidden({ timeout: 10_000 });
    // Real dashboard chrome, so a blank page after login fails.
    await expect(page.getByRole("button", { name: "Go to dashboard" })).toBeVisible({ timeout: 5_000 });
  });
});

// #1235: login sessions are persisted, so a daemon restart keeps browsers signed in.
test.describe("persisted login sessions", () => {
  const loginStatus = async (handle: ServeHandle): Promise<{ authenticated: boolean; elevated: boolean }> =>
    (await fetch(`${handle.baseUrl}/api/login/status`, { headers: authHeaders(handle) })).json();
  const devices = async (handle: ServeHandle) =>
    (await fetch(`${handle.baseUrl}/api/devices`, { headers: authHeaders(handle) })).json() as Promise<
      Array<{ session_id: string; current: boolean }>
    >;

  test("login session survives an aoe serve restart with no re-prompt", async ({ servePreauthed }) => {
    expect((await loginStatus(servePreauthed)).authenticated).toBe(true);
    await servePreauthed.restart();
    await expect(async () => {
      expect((await loginStatus(servePreauthed)).authenticated).toBe(true);
    }).toPass({ timeout: 10_000 });

    const list = await devices(servePreauthed);
    expect(list.length).toBeGreaterThan(0);
    const mine = list.find((d) => d.current === true);
    expect(mine, "the requesting session is flagged current").toBeTruthy();
    for (const field of ["session_id", "user_agent", "created_ip", "created_at", "last_seen"]) {
      expect(mine).toHaveProperty(field);
    }
  });

  test("elevation does not survive restart: high-risk actions re-prompt", async ({ servePreauthed }) => {
    const elevateRes = await fetch(`${servePreauthed.baseUrl}/api/login/elevate`, {
      method: "POST",
      headers: { ...authHeaders(servePreauthed), "Content-Type": "application/json" },
      body: JSON.stringify({ passphrase: servePreauthed.passphrase }),
    });
    expect(elevateRes.ok).toBe(true);
    expect((await loginStatus(servePreauthed)).elevated).toBe(true);

    await servePreauthed.restart();
    await expect(async () => {
      const s = await loginStatus(servePreauthed);
      expect(s.authenticated).toBe(true);
      expect(s.elevated).toBe(false);
    }).toPass({ timeout: 10_000 });
  });

  // Loopback callers bypass the elevation wall (#1525); the 403 policy is unit-tested in src/server/auth.rs.
  test("revoke removes one device and sign-out-all clears every session", async ({ servePreauthed }) => {
    const otherBinding = Buffer.from(new Uint8Array(32).fill(0x5a)).toString("base64url");
    const loginRes = await fetch(`${servePreauthed.baseUrl}/api/login`, {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ passphrase: servePreauthed.passphrase, device_binding_secret: otherBinding }),
    });
    expect(loginRes.ok).toBe(true);

    const before = await devices(servePreauthed);
    expect(before.length).toBe(2);
    const other = before.find((d) => !d.current);
    expect(other).toBeTruthy();

    const revoke = await fetch(`${servePreauthed.baseUrl}/api/login/sessions/${other!.session_id}`, {
      method: "DELETE",
      headers: authHeaders(servePreauthed),
    });
    expect(revoke.ok).toBe(true);
    expect((await devices(servePreauthed)).some((d) => d.session_id === other!.session_id)).toBe(false);

    const all = await fetch(`${servePreauthed.baseUrl}/api/login/logout-all`, {
      method: "POST",
      headers: authHeaders(servePreauthed),
    });
    expect(all.ok).toBe(true);
    expect((await loginStatus(servePreauthed)).authenticated).toBe(false);
  });

  test("settings -> devices renders the signed-in session as this device", async ({ servePreauthed, page }) => {
    await bootDashboard(page, servePreauthed, "/settings/devices");
    await expect(page.getByRole("heading", { name: /connected devices/i })).toBeVisible({ timeout: 10_000 });
    await expect(page.getByText(/this device/i)).toBeVisible({ timeout: 10_000 });
    await expect(page.getByRole("button", { name: /sign out all devices/i })).toBeVisible();
  });
});

test.describe("token entry", () => {
  const BAD_TOKEN = "a".repeat(64);

  /** Land on TokenEntryPage via a rejected URL token. */
  async function openWithBadToken(serveToken: ServeHandle, page: import("@playwright/test").Page) {
    await page.goto(`${serveToken.baseUrl}/?token=${BAD_TOKEN}`, { waitUntil: "domcontentloaded" });
    await expect(page.locator("#token")).toBeVisible({ timeout: 15_000 });
  }

  test("bad token in URL routes to TokenEntryPage; valid token routes to dashboard", async ({ serveToken, page }) => {
    expect(serveToken.authToken, "harness must expose serve.token").toBeTruthy();
    await openWithBadToken(serveToken, page);
    await expect(page.getByText(/session token has expired or is missing/i)).toBeVisible();
    // The 401 interceptor clears the stored token.
    expect(await page.evaluate(() => window.localStorage.getItem("aoe_auth_token"))).toBeNull();

    await page.locator("#token").fill(BAD_TOKEN);
    await page.getByRole("button", { name: /connect/i }).click();
    await expect(page.getByText(/invalid token/i)).toBeVisible({ timeout: 5_000 });

    await page.locator("#token").fill(serveToken.authToken!);
    await page.getByRole("button", { name: /connect/i }).click();
    await expect(page.locator("#token")).toBeHidden({ timeout: 10_000 });
    expect(await page.evaluate(() => window.localStorage.getItem("aoe_auth_token"))).toBe(serveToken.authToken);
  });

  test("URL form (?token=...) and raw-token form both unlock TokenEntryPage", async ({ serveToken, page }) => {
    await openWithBadToken(serveToken, page);
    await page.locator("#token").fill(`${serveToken.baseUrl}/?token=${serveToken.authToken}`);
    await page.getByRole("button", { name: /connect/i }).click();
    await expect(page.locator("#token")).toBeHidden({ timeout: 10_000 });
  });
});

test("rotated token: old accepted in grace, new accepted, old rejected past grace", async ({ spawnServe }) => {
  // Debug builds honor the lifetime and grace overrides.
  test.setTimeout(60_000);
  const lifetimeSecs = 6;
  const graceSecs = 3;
  const handle = await spawnServe({ authMode: "token", tokenLifetimeSecs: lifetimeSecs, tokenGraceSecs: graceSecs });
  const probe = async (token: string) => {
    const res = await fetch(`${handle.baseUrl}/api/about`, { headers: { Authorization: `Bearer ${token}` } });
    await res.text().catch(() => "");
    return res.status;
  };

  const tokenA = handle.authToken!;
  expect(tokenA).toMatch(/^[0-9a-f]{64}$/);
  expect(await probe(tokenA)).toBe(200);
  expect(await probe("z".repeat(64))).toBe(401);

  let tokenB = "";
  await expect
    .poll(
      async () => {
        // The file can be momentarily absent while the daemon rewrites it.
        tokenB = (await readFile(handle.tokenFile!, "utf8").catch(() => "")).trim();
        return tokenB.length > 0 && tokenB !== tokenA;
      },
      { timeout: (lifetimeSecs + graceSecs + 5) * 1000, intervals: [200] },
    )
    .toBe(true);
  expect(tokenB).toMatch(/^[0-9a-f]{64}$/);
  expect(await probe(tokenA)).toBe(200);
  expect(await probe(tokenB)).toBe(200);

  // Grace expiry is a wall-clock contract; the next rotation is still lifetimeSecs away.
  await delay((graceSecs + 1) * 1000);
  expect(await probe(tokenA)).toBe(401);
  expect(await probe(tokenB)).toBe(200);
});
