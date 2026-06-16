// The sidebar has two new-session buttons: the global toolbar button (no
// project preselected) and the per-project header button (prefills that
// project). They used to share the literal "New session" hover tooltip, so
// nothing distinguished them on hover. They now read differently. See #2205.

import { test, expect } from "./helpers/mockedTest";
import { installSidebarMocks } from "./helpers/sidebarMocks";

test("the two new-session buttons have distinct tooltips and labels", async ({ page }) => {
  await installSidebarMocks(page, {
    sessions: [{ id: "s-a", title: "alpha-session", project_path: "/tmp/repo-alpha", branch: "feat/a" }],
  });

  await page.setViewportSize({ width: 1280, height: 720 });
  await page.goto("/");

  // Global toolbar button: scoped to no project.
  await expect(page.getByRole("button", { name: "New session (choose project)" })).toBeVisible();
  await expect(page.getByText("New session (choose project)")).toBeAttached();

  // Per-project header button: scoped to the repo's display name.
  await expect(page.getByRole("button", { name: "New session in repo-alpha" })).toBeVisible();
  await expect(page.getByText("New session in repo-alpha")).toBeAttached();
});
