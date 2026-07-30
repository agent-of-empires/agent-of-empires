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

  // Nothing may overflow the rail. The footer section labels ("Projects (N)",
  // "Snoozed & archived (N)") are wide-tracked uppercase and used to spill past
  // the edge, which read as a clipped, broken sidebar.
  await expect.poll(async () => panel.evaluate((el) => el.scrollWidth - el.clientWidth)).toBeLessThanOrEqual(1);
  await expect(page.getByTestId("sidebar-projects-toggle")).toBeVisible();
  await expect(page.getByTestId("sidebar-projects-add")).toHaveCount(0);

  // The header wordmark shares the sidebar column, so it stands down rather
  // than sitting flush against the divider; the logo still links home.
  await expect(page.getByRole("button", { name: "Go to dashboard" })).toBeVisible();
  await expect(page.getByText("aoe", { exact: true })).toBeHidden();

  // Persists across a reload.
  await page.reload();
  await expect(page.getByRole("button", { name: "Expand sidebar" })).toBeVisible();
  await expect.poll(async () => (await panel.boundingBox())!.width).toBeLessThan(120);

  // Toggling off restores the full width and the hidden controls.
  await page.getByRole("button", { name: "Expand sidebar" }).click();
  await expect.poll(async () => (await panel.boundingBox())!.width).toBeGreaterThan(200);
  await expect(page.getByTestId("sidebar-group-session-count").first()).toBeVisible();
});

test("entering compact hides an open filter and stops its query narrowing the list", async ({ page }) => {
  await installSidebarMocks(page, { sessions: SESSIONS });
  await page.setViewportSize({ width: 1280, height: 720 });
  await page.goto("/");

  // Open the filter and type a query that hides one of the two sessions. Both
  // sit under /tmp/repo-alpha, so filter on "beta" (a title-only match) rather
  // than "alpha", which would also match the shared project name.
  await page.getByRole("button", { name: "Filter sessions" }).click();
  const filterInput = page.getByTestId("sidebar-filter-input");
  await expect(filterInput).toBeVisible();
  await filterInput.fill("beta");
  await expect(page.getByText("alpha-session")).toHaveCount(0);
  // The matching row must survive, otherwise a filter that hid everything would
  // satisfy the assertion above.
  await expect(page.getByText("beta-session")).toBeVisible();

  // Entering compact hides the panel (it would overflow the rail) and stops the
  // query applying, so nothing filters invisibly once the button is gone.
  await page.getByRole("button", { name: "Compact sidebar" }).click();
  await expect(filterInput).toHaveCount(0);
  await expect(page.getByRole("button", { name: "Filter sessions" })).toHaveCount(0);
  await expect(page.getByText("alpha-session")).toBeVisible();

  // Leaving compact restores the filter exactly as it was, query included.
  await page.getByRole("button", { name: "Expand sidebar" }).click();
  const restoredFilterInput = page.getByTestId("sidebar-filter-input");
  await expect(restoredFilterInput).toBeVisible();
  await expect(restoredFilterInput).toHaveValue("beta");
  await expect(page.getByText("alpha-session")).toHaveCount(0);
});
