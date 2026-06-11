// No-auth mode: navigating to / should skip LoginPage entirely and land
// on the dashboard. Proves that `aoe serve --no-auth` flips
// `/api/login/status.required` to false and the React app honors it.

import { test, expect } from "../helpers/liveTest";

test("--no-auth skips LoginPage and lands on dashboard", async ({ serve, page }) => {
  await page.goto(serve.baseUrl);

  // LoginPage renders the passphrase field. In no-auth mode it must
  // never appear.
  await expect(page.locator("input#passphrase")).toBeHidden();

  // /api/login/status from the live serve should advertise the no-auth
  // shape so the client unconditionally renders the dashboard.
  const statusRes = await page.request.get(`${serve.baseUrl}/api/login/status`);
  expect(statusRes.ok()).toBeTruthy();
  const status = await statusRes.json();
  expect(status.required).toBe(false);
});
