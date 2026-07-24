// Compact (slim) sidebar mode (#2288). A header toggle shrinks the sidebar to
// a fixed ~88px rail: status glyph + truncated title, project icon + truncated
// name. Trailing badges, the session count, and the per-project New-session
// button drop away; sessions stay tappable and the choice persists (localStorage
// `aoe-web-settings.sidebarCompact`, so it survives reload). Desktop viewport so
// the sidebar is in-flow (`md:static`) rather than a mobile overlay drawer.

import { test, expect } from "./helpers/mockedTest";
import { installSidebarMocks } from "./helpers/sidebarMocks";

const SESSIONS = [
  { id: "s-a", title: "alpha-session", project_path: "/tmp/repo-alpha", branch: "feat/a" },
  { id: "s-b", title: "beta-session", project_path: "/tmp/repo-alpha", branch: "feat/b" },
];

test("compact toggle slims the sidebar, hides extras, stays tappable, and persists", async ({ page }) => {
  await installSidebarMocks(page, { sessions: SESSIONS });
  await page.setViewportSize({ width: 1280, height: 720 });
  await page.goto("/");

  const panel = page.locator('[data-tour="sidebar"]');
  await expect(panel).toBeVisible();
  const fullWidth = (await panel.boundingBox())!.width;
  expect(fullWidth).toBeGreaterThan(200);

  // Full mode shows the per-project session count and New-session button.
  await expect(page.getByTestId("sidebar-group-session-count").first()).toBeVisible();
  await expect(page.getByRole("button", { name: "New session in repo-alpha" })).toBeVisible();

  // Enter compact mode.
  await page.getByRole("button", { name: "Compact sidebar" }).click();

  // Rail shrinks well below the 200px minimum; count + New-session drop away;
  // the toggle flips to "Expand sidebar".
  await expect.poll(async () => (await panel.boundingBox())!.width).toBeLessThan(120);
  await expect(page.getByTestId("sidebar-group-session-count")).toHaveCount(0);
  await expect(page.getByRole("button", { name: "New session in repo-alpha" })).toHaveCount(0);
  await expect(page.getByRole("button", { name: "Expand sidebar" })).toBeVisible();

  // Session titles remain in the DOM (CSS-truncated) and tappable.
  await expect(page.getByText("alpha-session")).toBeVisible();
  await page.getByText("alpha-session").click();

  // Persists across a reload.
  await page.reload();
  await expect(page.getByRole("button", { name: "Expand sidebar" })).toBeVisible();
  await expect.poll(async () => (await panel.boundingBox())!.width).toBeLessThan(120);

  // Toggling off restores the full width and the hidden controls.
  await page.getByRole("button", { name: "Expand sidebar" }).click();
  await expect.poll(async () => (await panel.boundingBox())!.width).toBeGreaterThan(200);
  await expect(page.getByTestId("sidebar-group-session-count").first()).toBeVisible();
});
