import { test, expect } from "./helpers/mockedTest";
import { Page } from "@playwright/test";
import { clickSidebarSession } from "./helpers/sidebar";
import { mockTerminalApis } from "./helpers/terminal-mocks";

// Per-session diff-base override (#970). The `vs <ref>` chip in the
// diff file-list header is a button that opens a typeahead popover.
// Selecting a branch PATCHes /diff-base; the chip styling toggles
// when an override is active; a "Reset to auto-detected" affordance
// clears it.

const DIFF_FILES_RESPONSE = {
  files: [
    {
      path: "src/example.ts",
      old_path: null,
      status: "modified",
      additions: 3,
      deletions: 1,
    },
  ],
  per_repo_bases: [{ base_branch: "main" }],
  warning: null,
};

async function setupSession(page: Page) {
  await mockTerminalApis(page);
  await page.route("**/api/sessions/*/diff/files", (r) =>
    r.fulfill({ json: DIFF_FILES_RESPONSE }),
  );
  await page.route("**/api/git/branches**", (r) =>
    r.fulfill({
      json: [
        { name: "main", is_current: true },
        { name: "develop", is_current: false },
        { name: "upstream/main", is_current: false, remote_only: true },
      ],
    }),
  );
}

test.use({ viewport: { width: 1280, height: 720 } });

test.describe("Diff base override (#970)", () => {
  test("clicking the chip opens a typeahead populated from /api/git/branches", async ({
    page,
  }) => {
    await setupSession(page);
    await page.goto("/");
    await expect(page.locator("header")).toBeVisible();
    await clickSidebarSession(page, "pinch-test");
    const chip = page.getByRole("button", {
      name: /Change diff base \(current: main\)/,
    });
    await expect(chip).toBeVisible({ timeout: 10000 });
    await chip.click();
    await expect(page.getByPlaceholder("Search branches...")).toBeVisible();
    await expect(page.getByRole("option", { name: /^main/ })).toBeVisible();
    await expect(
      page.getByRole("option", { name: /upstream\/main/ }),
    ).toBeVisible();
  });

  test("selecting a branch PATCHes diff-base and the chip reflects the new value", async ({
    page,
  }) => {
    await setupSession(page);
    let patched: { base_branch?: string | null } | null = null;
    let getCount = 0;
    await page.route("**/api/sessions/*/diff-base", (r) => {
      patched = JSON.parse(r.request().postData() || "{}");
      return r.fulfill({ json: { id: "pinch-test" } });
    });
    // After the PATCH the client refetches diff/files. Flip both the
    // file list and the base on the second response — `useDiffFiles`
    // skips state updates when the files fingerprint is unchanged, so
    // we mutate the files too to trigger the per-repo-bases swap.
    await page.unroute("**/api/sessions/*/diff/files");
    await page.route("**/api/sessions/*/diff/files", (r) => {
      getCount += 1;
      if (getCount === 1) {
        return r.fulfill({ json: DIFF_FILES_RESPONSE });
      }
      return r.fulfill({
        json: {
          files: [
            {
              ...DIFF_FILES_RESPONSE.files[0],
              additions: 4,
            },
          ],
          per_repo_bases: [{ base_branch: "develop" }],
          warning: null,
        },
      });
    });

    await page.goto("/");
    await clickSidebarSession(page, "pinch-test");
    const chip = page.getByRole("button", {
      name: /Change diff base \(current: main\)/,
    });
    await expect(chip).toBeVisible({ timeout: 10000 });
    await chip.click();
    await page.getByRole("option", { name: /develop/ }).click();
    await expect.poll(() => patched?.base_branch).toBe("develop");
    await expect(
      page.getByRole("button", {
        name: /Change diff base \(current: develop\)/,
      }),
    ).toBeVisible({ timeout: 5000 });
  });

  test("reset clears the override (PATCH with null)", async ({ page }) => {
    await setupSession(page);
    // First mount: session has an override set.
    await page.unroute("**/api/sessions");
    await page.route("**/api/sessions", (r) => {
      if (r.request().method() === "POST") return r.fulfill({ status: 400 });
      return r.fulfill({
        json: {
          sessions: [
            {
              id: "pinch-test",
              title: "pinch-test",
              project_path: "/tmp/pinch-test",
              group_path: "/tmp",
              tool: "claude",
              status: "Running",
              yolo_mode: false,
              created_at: new Date().toISOString(),
              last_accessed_at: null,
              last_error: null,
              branch: null,
              main_repo_path: null,
              base_branch_override: "upstream/main",
              is_sandboxed: false,
              has_terminal: true,
              profile: "default",
              workspace_repos: [],
            },
          ],
          workspace_ordering: [],
        },
      });
    });
    let patched: { base_branch?: string | null } | null = null;
    await page.route("**/api/sessions/*/diff-base", (r) => {
      patched = JSON.parse(r.request().postData() || "{}");
      return r.fulfill({ json: { id: "pinch-test" } });
    });

    await page.goto("/");
    await clickSidebarSession(page, "pinch-test");
    const chip = page.getByRole("button", {
      name: /Change diff base/,
    });
    await expect(chip).toBeVisible({ timeout: 10000 });
    await chip.click();
    const reset = page.getByRole("button", { name: /Reset to auto-detected/ });
    await expect(reset).toBeVisible();
    await reset.click();
    await expect.poll(() => patched?.base_branch).toBeNull();
  });
});
