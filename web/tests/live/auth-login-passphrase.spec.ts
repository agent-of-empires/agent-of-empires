// Passphrase mode login flow.
//
// The `servePassphrase` fixture boots `aoe serve --auth=passphrase
// --passphrase <fixed>`, which routes the server into
// `run_passphrase_wall` where `/api/login` and `/api/login/status`
// are login-exempt. The spec navigates with no cookie, lets the SPA
// render LoginPage, and drives the form through the browser. See
// #1230 for why this needs `--auth=passphrase` rather than the
// default token+passphrase 2FA setup.

import { test, expect } from "../helpers/liveTest";

test("wrong passphrase shows an error and stays on LoginPage", async ({ servePassphrase, page }) => {
  await page.goto(servePassphrase.baseUrl);

  await expect(page.locator("input#passphrase")).toBeVisible();
  await page.locator("input#passphrase").fill("definitely-wrong");
  await page.locator("button[type=submit]").click();

  // The login lib surfaces server errors via the LoginPage's error
  // state; the field stays focused for retry. Server returns 401 with
  // `message: "Incorrect passphrase"` (src/server/login.rs), which the
  // login() client (web/src/lib/api.ts) maps to LoginPage's local
  // error string.
  await expect(page.locator("input#passphrase")).toBeVisible();
  await expect(page.getByText(/incorrect passphrase/i)).toBeVisible({
    timeout: 5_000,
  });
});

test("correct passphrase logs in and reveals the dashboard", async ({ servePassphrase, page }) => {
  if (!servePassphrase.passphrase) {
    throw new Error("servePassphrase fixture must expose passphrase");
  }

  await page.goto(servePassphrase.baseUrl);
  await expect(page.locator("input#passphrase")).toBeVisible();
  await page.locator("input#passphrase").fill(servePassphrase.passphrase);
  await page.locator("button[type=submit]").click();

  // Once login resolves, the LoginPage unmounts and the dashboard's
  // top bar takes over. Wait for an actual dashboard chrome element
  // (the top bar's dashboard-home button) so an SPA that gets stuck
  // on a blank page after login fails loudly instead of passing on
  // "passphrase field absent".
  await expect(page.locator("input#passphrase")).toBeHidden({
    timeout: 10_000,
  });
  await expect(page.getByRole("button", { name: "Go to dashboard" })).toBeVisible({ timeout: 5_000 });
});
