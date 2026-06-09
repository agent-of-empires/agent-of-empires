// User story: open the Projects view from the sidebar.
//
// WorkspaceSidebar.tsx exposes a Projects button (aria-label="Projects")
// that navigates to /projects. ProjectsView mounts with an h1 "Projects".

import { test as base, expect } from "@playwright/test";
import { spawnAoeServe } from "../../helpers/aoeServe";

base("sidebar Projects button opens the Projects view", async ({ page }, testInfo) => {
  const serve = await spawnAoeServe({
    authMode: "none",
    workerIndex: testInfo.workerIndex,
    parallelIndex: testInfo.parallelIndex,
  });

  try {
    await page.goto(serve.baseUrl);

    await page.getByRole("button", { name: "Projects", exact: true }).click();

    await expect(page).toHaveURL(/\/projects/, { timeout: 5_000 });
    await expect(page.getByRole("heading", { name: "Projects", exact: true })).toBeVisible({ timeout: 5_000 });
    await expect(page.getByText(/Saved repositories you can multi-select/i)).toBeVisible();
  } finally {
    await serve.stop();
  }
});
