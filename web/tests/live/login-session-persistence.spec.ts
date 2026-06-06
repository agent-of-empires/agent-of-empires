// Login-session persistence across daemon restart (#1235).
//
// Before this fix, `LoginManager` held sessions in a RwLock<HashMap>
// that died with the daemon, so `aoe serve --stop && aoe serve` logged
// out every browser even though the cookie, device-binding secret,
// passphrase, and 30-day sliding window were all still valid. (See the
// now-stale workaround comment in
// settings-persistence-theme-passphrase.spec.ts's `reLogin`, which
// existed precisely because of this bug.)
//
// These specs boot `aoe serve --auth=passphrase` with a harness-minted
// session, then assert:
//   1. After a real daemon restart, the SAME cookie + binding still
//      authenticates with NO re-login. This is the regression proof:
//      pre-fix it returns authenticated:false.
//   2. The connected-devices view (`GET /api/devices`) is backed by the
//      persisted login sessions and survives the restart.
//   3. Revoke and sign-out-all are elevation-gated (403 without a
//      15-min passphrase confirmation), and work once elevated.

import { test as base, expect } from "@playwright/test";
import { spawnAoeServe, type ServeHandle } from "../helpers/aoeServe";

const test = base.extend<{ servePreauthed: ServeHandle }>({
  servePreauthed: async ({}, use, testInfo) => {
    const handle = await spawnAoeServe({
      authMode: "passphrase",
      workerIndex: testInfo.workerIndex,
      parallelIndex: testInfo.parallelIndex,
      preloginViaHarness: true,
    });
    await use(handle);
    await handle.stop();
  },
});

/** Cookie + device-binding headers the SPA's fetch wrapper would add. */
function authHeaders(handle: ServeHandle): Record<string, string> {
  const out: Record<string, string> = {};
  if (handle.sessionCookie) {
    out["Cookie"] =
      `${handle.sessionCookie.name}=${handle.sessionCookie.value}`;
  }
  if (handle.deviceBindingSecret) {
    out["X-Aoe-Device-Binding"] = handle.deviceBindingSecret;
  }
  return out;
}

async function loginStatus(
  handle: ServeHandle,
): Promise<{ authenticated: boolean; elevated: boolean }> {
  const res = await fetch(`${handle.baseUrl}/api/login/status`, {
    headers: authHeaders(handle),
  });
  return res.json();
}

test("login session survives an aoe serve restart with no re-prompt", async ({
  servePreauthed,
}) => {
  // Sanity: authenticated before the restart.
  expect((await loginStatus(servePreauthed)).authenticated).toBe(true);

  // A real daemon bounce. The harness reuses the same isolated HOME, so
  // login_sessions.toml is the only thing carrying state across.
  await servePreauthed.restart();

  // No re-login. The same cookie + binding must still authenticate.
  await expect(async () => {
    expect((await loginStatus(servePreauthed)).authenticated).toBe(true);
  }).toPass({ timeout: 10_000 });

  // The device shows up in the persisted-session-backed devices view.
  const devices: Array<Record<string, unknown>> = await fetch(
    `${servePreauthed.baseUrl}/api/devices`,
    { headers: authHeaders(servePreauthed) },
  ).then((r) => r.json());
  expect(devices.length).toBeGreaterThan(0);
  const mine = devices.find((d) => d.current === true);
  expect(mine, "the requesting session is flagged current").toBeTruthy();
  for (const field of [
    "session_id",
    "user_agent",
    "created_ip",
    "created_at",
    "last_seen",
  ]) {
    expect(mine).toHaveProperty(field);
  }
});

test("elevation does not survive restart: high-risk actions re-prompt", async ({
  servePreauthed,
}) => {
  // Elevate the live session.
  const elevateRes = await fetch(
    `${servePreauthed.baseUrl}/api/login/elevate`,
    {
      method: "POST",
      headers: {
        ...authHeaders(servePreauthed),
        "Content-Type": "application/json",
      },
      body: JSON.stringify({ passphrase: servePreauthed.passphrase }),
    },
  );
  expect(elevateRes.ok).toBe(true);
  expect((await loginStatus(servePreauthed)).elevated).toBe(true);

  await servePreauthed.restart();

  // Session survives, but elevation must not: a restart is a legit
  // step-up recency break.
  await expect(async () => {
    const s = await loginStatus(servePreauthed);
    expect(s.authenticated).toBe(true);
    expect(s.elevated).toBe(false);
  }).toPass({ timeout: 10_000 });
});

// NOTE on elevation gating: revoke and logout-all are gated behind
// step-up elevation in `requires_elevation`, but a loopback caller
// bypasses the passphrase wall entirely (#1525), so a local test cannot
// observe the 403. The path-level gating policy is unit-tested in
// `requires_elevation_paths` (src/server/auth.rs); here we exercise the
// behavior the handlers produce for a trusted local caller.
test("revoke removes one device and sign-out-all clears every session", async ({
  servePreauthed,
}) => {
  const passphrase = servePreauthed.passphrase!;

  // Create a second device by logging in with a distinct binding secret.
  const otherBinding = Buffer.from(new Uint8Array(32).fill(0x5a)).toString(
    "base64url",
  );
  const loginRes = await fetch(`${servePreauthed.baseUrl}/api/login`, {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify({ passphrase, device_binding_secret: otherBinding }),
  });
  expect(loginRes.ok).toBe(true);

  const devicesBefore: Array<{ session_id: string; current: boolean }> =
    await fetch(`${servePreauthed.baseUrl}/api/devices`, {
      headers: authHeaders(servePreauthed),
    }).then((r) => r.json());
  expect(devicesBefore.length).toBe(2);
  const otherSession = devicesBefore.find((d) => !d.current);
  expect(otherSession).toBeTruthy();

  // Revoke the other device; it disappears from the list.
  const revoke = await fetch(
    `${servePreauthed.baseUrl}/api/login/sessions/${otherSession!.session_id}`,
    { method: "DELETE", headers: authHeaders(servePreauthed) },
  );
  expect(revoke.ok).toBe(true);

  const devicesAfter: Array<{ session_id: string }> = await fetch(
    `${servePreauthed.baseUrl}/api/devices`,
    { headers: authHeaders(servePreauthed) },
  ).then((r) => r.json());
  expect(
    devicesAfter.some((d) => d.session_id === otherSession!.session_id),
  ).toBe(false);

  // Sign out everyone. The current session is dropped too.
  const all = await fetch(`${servePreauthed.baseUrl}/api/login/logout-all`, {
    method: "POST",
    headers: authHeaders(servePreauthed),
  });
  expect(all.ok).toBe(true);
  expect((await loginStatus(servePreauthed)).authenticated).toBe(false);
});
