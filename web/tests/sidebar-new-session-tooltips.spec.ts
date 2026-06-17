// The sidebar has two new-session buttons: the global toolbar button (opens
// the new-session wizard) and the per-project header button (prefills that
// project). They used to share the literal "New session" hover tooltip, so
// nothing distinguished them on hover. The global one now reads "New project
// session"; the per-project one stays a short "New session". See #2205.
//
// The shared Tooltip (Tooltip.tsx) renders its text into a portaled
// role="tooltip" span only while the trigger is hovered/focused (#2214), so
// each tooltip is asserted after hovering its button rather than from the
// static DOM.

import { test, expect } from "./helpers/mockedTest";
import { installSidebarMocks } from "./helpers/sidebarMocks";

test("the two new-session buttons have distinct tooltips and labels", async ({ page }) => {
  await installSidebarMocks(page, {
    sessions: [{ id: "s-a", title: "alpha-session", project_path: "/tmp/repo-alpha", branch: "feat/a" }],
  });

  await page.setViewportSize({ width: 1280, height: 720 });
  await page.goto("/");

  // Global toolbar button: project-first wizard. The accessible name is always
  // present; the visible tooltip shows on hover.
  const globalBtn = page.getByRole("button", { name: "New project session" });
  await expect(globalBtn).toBeVisible();
  await globalBtn.hover();
  await expect(page.getByRole("tooltip")).toHaveText("New project session");

  // Per-project header button: short tooltip, but its accessible name stays
  // scoped to the project. The project name is kept out of the tooltip text so
  // it does not collide with getByText(projectName) elsewhere.
  const projectBtn = page.getByRole("button", { name: "New session in repo-alpha" });
  await expect(projectBtn).toBeVisible();
  await projectBtn.hover();
  await expect(page.getByRole("tooltip")).toHaveText("New session");
});
