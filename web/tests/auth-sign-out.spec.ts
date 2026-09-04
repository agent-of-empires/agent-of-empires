// User story (ported from live acp-stories/auth-sign-out): a logged-in
// user can sign out via the topbar overflow menu, and the LoginPage
// re-appears afterward.
//
// The "Sign out" entry only renders when `loginRequired` is true on
// the TopBar (TopBar.tsx:52). The live spec drove a real passphrase
// login; here `/api/login/status` is stubbed to report an already
// authenticated passphrase session, and `POST /api/logout` is stubbed
// so App.tsx's handleLogout resolves and flips `loginAuthenticated`
// back to false, which is what re-renders the LoginPage.

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
