// Sidebar Projects section (#2212): add, edit base branch, remove.

import { spawnSync } from "node:child_process";
import { join } from "node:path";
import type { Page } from "@playwright/test";
import { test, expect, type ServeHandle, type ServeOptions } from "../../helpers/liveTest";
import { resolveAoeBinary } from "../../helpers/aoeServe";
import { initWorkingRepo } from "../../helpers/gitFixture";

const BASE_BRANCH_PLACEHOLDER = "blank = inherit global default, then auto-detect";

async function serveWithRepo(
  spawnServe: (opts?: ServeOptions) => Promise<ServeHandle>,
  name: string,
  register = false,
): Promise<{ serve: ServeHandle; projectPath: string }> {
  let projectPath = "";
  const serve = await spawnServe({
    seedFn: ({ home, env }) => {
      projectPath = initWorkingRepo(join(home, name), env).path;
      if (!register) return;
      const res = spawnSync(resolveAoeBinary(), ["project", "add", projectPath], { env });
      if (res.status !== 0) {
        throw new Error(`aoe project add failed: status=${res.status} stderr=${res.stderr?.toString() ?? "<none>"}`);
      }
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

test("add a project from the sidebar Projects section", async ({ page, spawnServe }) => {
  const { serve, projectPath } = await serveWithRepo(spawnServe, "story-projects-add");
  await page.goto(`${serve.baseUrl}/`);
  await addProject(page, projectPath, "story-projects-add");
});

test("edit a project's base branch from the sidebar Projects section", async ({ page, spawnServe }) => {
  const { serve, projectPath } = await serveWithRepo(spawnServe, "story-projects-edit");
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
  await expect(projectRow(page, "story-projects-edit").getByText("release", { exact: false })).toBeVisible({
    timeout: 10_000,
  });
});

test("remove a project from the sidebar Projects section", async ({ page, spawnServe }) => {
  const { serve } = await serveWithRepo(spawnServe, "story-projects-remove", true);
  page.on("dialog", (d) => void d.accept());
  await page.goto(`${serve.baseUrl}/`);
  const row = projectRow(page, "story-projects-remove");
  await expect(row).toBeVisible({ timeout: 10_000 });

  await row.click({ button: "right" });
  await page.getByTestId("sidebar-project-context-menu-remove").click();
  await expect(row).toHaveCount(0, { timeout: 5_000 });
});
