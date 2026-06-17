// The sidebar has two new-session buttons: the global toolbar button (opens
// the project-first wizard) and the per-project header button (prefills that
// project). They used to share the literal "New session" hover tooltip, so
// nothing distinguished them on hover. The global one now reads "New project
// session"; the per-project one stays a short "New session". See #2205.

import { test, expect } from "./helpers/mockedTest";
import { installSidebarMocks } from "./helpers/sidebarMocks";

test("the two new-session buttons have distinct tooltips and labels", async ({ page }) => {
  await installSidebarMocks(page, {
    sessions: [{ id: "s-a", title: "alpha-session", project_path: "/tmp/repo-alpha", branch: "feat/a" }],
  });

  await page.setViewportSize({ width: 1280, height: 720 });
  await page.goto("/");

  // Global toolbar button: project-first wizard.
  await expect(page.getByRole("button", { name: "New project session" })).toBeVisible();
  await expect(page.getByText("New project session")).toBeAttached();

  // Per-project header button: short tooltip, but its accessible name stays
  // scoped to the project. The project name is kept out of the always-rendered
  // tooltip text so it does not collide with getByText(projectName) elsewhere.
  await expect(page.getByRole("button", { name: "New session in repo-alpha" })).toBeVisible();
  const header = page.locator("[data-testid='sidebar-group-header']");
  await expect(header.getByText("New session", { exact: true })).toBeAttached();
});
