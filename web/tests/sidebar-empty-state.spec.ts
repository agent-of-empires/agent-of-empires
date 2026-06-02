// With no sessions and no active filter, the sidebar body used to render
// blank (only the filtered "No matches" message existed). It now shows a
// hint plus a New session button wired to the same wizard trigger as the
// top + button. See #1835.

import { test, expect } from "./helpers/mockedTest";
import { installSidebarMocks } from "./helpers/sidebarMocks";

test("empty sidebar shows a hint and opens the wizard from its button", async ({
  page,
}) => {
  await installSidebarMocks(page, { sessions: [] });

  await page.setViewportSize({ width: 1280, height: 720 });
  await page.goto("/");

  const empty = page.getByTestId("sidebar-empty-state");
  await expect(empty).toBeVisible();
  await expect(empty).toContainText("No sessions yet");

  // No rows render when the list is empty.
  await expect(page.getByTestId("sidebar-session-row")).toHaveCount(0);

  // The CTA opens the session wizard (heading is distinct from the
  // button's own "New session" label).
  await empty.getByRole("button", { name: "New session" }).click();
  await expect(
    page.getByRole("heading", { name: "New session" }),
  ).toBeVisible();
});

test("empty-state hint is hidden once a session exists", async ({ page }) => {
  await installSidebarMocks(page, {
    sessions: [
      {
        id: "s-a",
        title: "alpha",
        project_path: "/tmp/repo",
        branch: "feature/a",
      },
    ],
  });

  await page.setViewportSize({ width: 1280, height: 720 });
  await page.goto("/");

  await expect(page.getByTestId("sidebar-session-row")).toHaveCount(1);
  await expect(page.getByTestId("sidebar-empty-state")).toHaveCount(0);
});
