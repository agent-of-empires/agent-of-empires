// Signing out from the topbar overflow menu brings the LoginPage back. The
// entry renders only when `loginRequired` is true, so /api/login/status reports
// an authenticated passphrase session and POST /api/logout is stubbed for
// App.tsx's handleLogout.

import { test, expect } from "./helpers/mockedTest";

test("topbar overflow menu signs the user out and returns to LoginPage", async ({ page }) => {
  await page.route("**/api/login/status", (r) =>
    r.fulfill({
      json: { required: true, authenticated: true, elevated: true, elevated_until_secs: 600 },
    }),
  );
  await page.route("**/api/logout", (r) => r.fulfill({ json: { ok: true } }));
  await page.route("**/api/sessions", (r) => r.fulfill({ json: { sessions: [], workspace_ordering: [] } }));

  await page.setViewportSize({ width: 1280, height: 720 });
  await page.goto("/");
  await expect(page.getByRole("button", { name: "Go to dashboard" })).toBeVisible();

  await page.getByRole("button", { name: "More options" }).click();
  await page.getByRole("menuitem", { name: "Sign out" }).click();

  await expect(page.locator("input#passphrase")).toBeVisible();
});
