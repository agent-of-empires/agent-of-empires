import { test, expect } from "./helpers/mockedTest";
import { Page } from "@playwright/test";
import { openWizard } from "./helpers/wizard";

// Regression for the new-session tooltip rendering above the wizard modal on
// mobile (#2215 follow-up). The shared Tooltip portals a `fixed z-50` span to
// document.body; the wizard overlay used to also be `z-50`, so the tooltip,
// appended later in the DOM, won the equal-z tie and floated over the modal.
// The wizard overlay must outrank the tooltip layer (z-50).

async function mockApis(page: Page) {
  await page.route("**/api/login/status", (r) => r.fulfill({ json: { required: false, authenticated: true } }));
  for (const path of ["settings", "themes", "profiles", "groups", "devices", "about", "system/update-status"]) {
    await page.route(`**/api/${path}`, (r) =>
      r.fulfill({
        json: path === "settings" || path === "about" || path === "system/update-status" ? {} : [],
      }),
    );
  }
  await page.route("**/api/docker/status", (r) => r.fulfill({ json: { available: false, runtime: null } }));
  await page.route("**/api/agents", (r) =>
    r.fulfill({ json: [{ name: "claude", binary: "claude", host_only: false, installed: true, install_hint: "" }] }),
  );
  await page.route("**/api/sessions", (r) => r.fulfill({ json: { sessions: [], workspace_ordering: [] } }));
}

test("wizard overlay outranks the z-50 tooltip layer on mobile", async ({ page }) => {
  await mockApis(page);
  await page.setViewportSize({ width: 390, height: 844 });
  await page.goto("/");

  await openWizard(page);

  const overlayZ = await page
    .getByTestId("session-wizard")
    .evaluate((el) => Number(getComputedStyle(el.parentElement as HTMLElement).zIndex));

  // Tooltip popups are fixed at z-50; the modal must sit above them.
  expect(overlayZ).toBeGreaterThan(50);
});
