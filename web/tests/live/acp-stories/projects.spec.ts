// Sidebar Projects section (#2212): add, edit base branch, remove.

import { join } from "node:path";
import type { Page } from "@playwright/test";
import { test, expect, type ServeHandle, type ServeOptions } from "../../helpers/liveTest";
import { initWorkingRepo } from "../../helpers/gitFixture";

const BASE_BRANCH_PLACEHOLDER = "blank = inherit global default, then auto-detect";

async function serveWithRepo(
  spawnServe: (opts?: ServeOptions) => Promise<ServeHandle>,
  name: string,
): Promise<{ serve: ServeHandle; projectPath: string }> {
  let projectPath = "";
  const serve = await spawnServe({
    seedFn: ({ home, env }) => {
      projectPath = initWorkingRepo(join(home, name), env).path;
    },
  });
  return { serve, projectPath };
}

const projectRow = (page: Page, name: string) =>
  page.getByTestId("sidebar-projects-section").locator("[data-testid='sidebar-project-row']").filter({ hasText: name });

async function addProject(page: Page, projectPath: string, name: string) {
  await page.getByTestId("sidebar-projects-add").click();
  await page.getByPlaceholder("/path/to/repo").fill(projectPath);
  await page.getByPlaceholder(BASE_BRANCH_PLACEHOLDER).fill("develop");
  await page.getByRole("button", { name: "Add", exact: true }).click();
  const row = projectRow(page, name);
  await expect(row).toBeVisible({ timeout: 10_000 });
  await expect(row.getByText("develop", { exact: false })).toBeVisible();
  return row;
}

test("add, edit the base branch of, and remove a project from the sidebar Projects section", async ({
  page,
  spawnServe,
}) => {
  const { serve, projectPath } = await serveWithRepo(spawnServe, "story-projects-edit");
  page.on("dialog", (d) => void d.accept());
  await page.goto(`${serve.baseUrl}/`);
  const row = await addProject(page, projectPath, "story-projects-edit");

  await row.click({ button: "right" });
  await page.getByTestId("sidebar-project-context-menu-edit").click();
  const editor = page.getByPlaceholder(BASE_BRANCH_PLACEHOLDER);
  await expect(editor).toHaveValue("develop");
  await editor.fill("release");
  await page.getByRole("button", { name: "Save", exact: true }).click();
  await expect(row.getByText("release", { exact: false })).toBeVisible({ timeout: 5_000 });

  // Survives a reload, so the PATCH reached the registry.
  await page.reload();
  const reloaded = projectRow(page, "story-projects-edit");
  await expect(reloaded.getByText("release", { exact: false })).toBeVisible({ timeout: 10_000 });

  await reloaded.click({ button: "right" });
  await page.getByTestId("sidebar-project-context-menu-remove").click();
  await expect(reloaded).toHaveCount(0, { timeout: 5_000 });
});
