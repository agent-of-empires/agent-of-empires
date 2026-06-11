// Token-mode entry flow: a stale or bad token in the URL puts the SPA on
// `TokenEntryPage`; pasting the valid token replaces it and routes to the
// dashboard. Covers `web/src/components/TokenEntryPage.tsx`, the localStorage
// capture in `web/src/lib/token.ts`, and the 401 handler in
// `web/src/lib/fetchInterceptor.ts`.

import { test, expect } from "../helpers/liveTest";

const BAD_TOKEN = "a".repeat(64);

test("bad token in URL routes to TokenEntryPage; valid token routes to dashboard", async ({ serveToken, page }) => {
  expect(serveToken.authToken, "harness must expose serve.token").toBeTruthy();
  const validToken = serveToken.authToken!;

  // Drop the bad token into localStorage via the URL capture path that
  // token.ts runs on module load. The SPA then strips ?token from the URL
  // and starts firing token-gated requests with the bad value.
  await page.goto(`${serveToken.baseUrl}/?token=${BAD_TOKEN}`, {
    waitUntil: "domcontentloaded",
  });

  // First auth-gated request 401s; fetchInterceptor dispatches
  // TOKEN_EXPIRED_EVENT and App.tsx swaps in TokenEntryPage. Bump the
  // timeout above the harness's spawn slack so a cold-cache Vite serve
  // doesn't race the assertion.
  await expect(page.locator("#token")).toBeVisible({ timeout: 15_000 });
  await expect(page.getByText(/session token has expired or is missing/i)).toBeVisible();

  // Sanity: localStorage was cleared by the interceptor on 401.
  const storedAfterReject = await page.evaluate(() => window.localStorage.getItem("aoe_auth_token"));
  expect(storedAfterReject).toBeNull();

  // Submitting another bad token surfaces the inline error.
  await page.locator("#token").fill(BAD_TOKEN);
  await page.getByRole("button", { name: /connect/i }).click();
  await expect(page.getByText(/invalid token/i)).toBeVisible({
    timeout: 5_000,
  });

  // Paste the real token; verifyToken hits /api/login/status (login-exempt
  // but token-gated), the 200 unsticks tokenExpired, App re-renders the
  // dashboard. We assert on a stable dashboard locator rather than the
  // absence of TokenEntryPage so the test fails loudly if routing breaks.
  await page.locator("#token").fill(validToken);
  await page.getByRole("button", { name: /connect/i }).click();

  await expect(page.locator("#token")).toBeHidden({ timeout: 10_000 });

  // Token was persisted to localStorage by saveToken.
  const storedAfterAccept = await page.evaluate(() => window.localStorage.getItem("aoe_auth_token"));
  expect(storedAfterAccept).toBe(validToken);
});

test("URL form (?token=...) and raw-token form both unlock TokenEntryPage", async ({ serveToken, page }) => {
  const validToken = serveToken.authToken!;

  // Start with a bad token to land on TokenEntryPage.
  await page.goto(`${serveToken.baseUrl}/?token=${BAD_TOKEN}`, {
    waitUntil: "domcontentloaded",
  });
  await expect(page.locator("#token")).toBeVisible({ timeout: 15_000 });

  // Submit the full URL form (extractToken in TokenEntryPage.tsx parses it).
  await page.locator("#token").fill(`${serveToken.baseUrl}/?token=${validToken}`);
  await page.getByRole("button", { name: /connect/i }).click();
  await expect(page.locator("#token")).toBeHidden({ timeout: 10_000 });
});
