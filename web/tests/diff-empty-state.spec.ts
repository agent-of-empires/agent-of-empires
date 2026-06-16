import { test, expect } from "./helpers/mockedTest";
import { Page } from "@playwright/test";
import { clickSidebarSession } from "./helpers/sidebar";
import { mockTerminalApis } from "./helpers/terminal-mocks";

// Empty changes panel (#2152). When the diff endpoint returns no files,
// the panel names the base instead of a context-free "No changes yet":
// single-repo reads "No changes vs <base>"; multi-repo lists every member
// with its base so the user sees each repo was checked and is clean.

async function setupSession(page: Page, diffJson: unknown) {
  await mockTerminalApis(page);
  await page.route("**/api/sessions/*/diff/files", (r) => r.fulfill({ json: diffJson }));
  await page.goto("/");
  await clickSidebarSession(page, "pinch-test");
}

test.use({ viewport: { width: 1280, height: 720 } });

test.describe("Diff empty state (#2152)", () => {
  test("single-repo empty state names the base", async ({ page }) => {
    await setupSession(page, { files: [], per_repo_bases: [{ base_branch: "origin/develop" }], warning: null });
    // Body line names the base; "origin/develop" also appears in the header
    // chip, so assert against the empty-state paragraph specifically.
    const emptyLine = page.getByText(/No changes vs/);
    await expect(emptyLine).toBeVisible({ timeout: 10000 });
    await expect(emptyLine).toContainText("origin/develop");
  });

  test("multi-repo empty state lists every repo with its base", async ({ page }) => {
    await setupSession(page, {
      files: [],
      per_repo_bases: [
        { repo_name: "taskrunner", base_branch: "origin/develop" },
        { repo_name: "MessageManager", base_branch: "origin/develop" },
        { repo_name: "SmartCaller", base_branch: "origin/main" },
      ],
      warning: null,
    });
    // Multi-repo empty routes through MultiRepoGroups: each member shows a
    // header (name + "vs <base>") and a per-repo "no changes" note.
    await expect(page.getByText("taskrunner")).toBeVisible({ timeout: 10000 });
    await expect(page.getByText("MessageManager")).toBeVisible();
    await expect(page.getByText("SmartCaller")).toBeVisible();
    await expect(page.getByText("vs origin/main")).toBeVisible();
    await expect(page.getByText("No changes in this repo.").first()).toBeVisible();
  });
});
