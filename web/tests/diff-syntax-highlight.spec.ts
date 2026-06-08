import { test, expect } from "./helpers/mockedTest";
import { clickSidebarSession } from "./helpers/sidebar";
import { makePatch } from "./helpers/patch";
import { mockTerminalApis } from "./helpers/terminal-mocks";
import type { Page } from "@playwright/test";

// Diff rendering through @pierre/diffs. Syntax highlighting itself is the
// library's concern (and runs off-thread in a worker); our responsibility is
// to feed it the right file contents and surface the text. These specs assert
// that both sides of a change render for a known language and that an
// unrecognised extension still shows its content (no blank pane, no gate).

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

const DIFF_FILE_RESPONSE = {
  file: {
    path: "src/example.ts",
    old_path: null,
    status: "modified",
    additions: 3,
    deletions: 1,
  },
  old_content:
    'import { useState } from "react";\nconst x = 42;\nexport default x;\n',
  new_content:
    'import { useState } from "react";\n' +
    "const x: number = 42;\n" +
    "function greet(name: string): string {\n" +
    "  return `Hello, ${name}`;\n" +
    "export default x;\n",
  is_binary: false,
  truncated: false,
};
(DIFF_FILE_RESPONSE as { patch?: string }).patch = makePatch(
  "src/example.ts",
  DIFF_FILE_RESPONSE.old_content,
  DIFF_FILE_RESPONSE.new_content,
);

async function setupDiffMocks(page: Page) {
  await mockTerminalApis(page);
  await page.route("**/api/sessions/*/diff/files", (r) =>
    r.fulfill({ json: DIFF_FILES_RESPONSE }),
  );
  await page.route(/\/api\/sessions\/[^/]+\/diff\/file\?/, (r) =>
    r.fulfill({ json: DIFF_FILE_RESPONSE }),
  );
}

async function openSessionAndWaitForDiffList(page: Page) {
  await expect(page.locator("header")).toBeVisible();
  await clickSidebarSession(page, "pinch-test");
  await expect(page.getByText("example.ts").first()).toBeVisible({
    timeout: 10000,
  });
}

test.use({ viewport: { width: 1280, height: 720 } });

test.describe("Diff rendering (@pierre/diffs)", () => {
  test("renders both sides of a TypeScript diff", async ({ page }) => {
    await setupDiffMocks(page);
    await page.goto("/");
    await openSessionAndWaitForDiffList(page);
    await page.getByText("example.ts").first().click();

    // New-side addition and old-side deletion both surface (getByText pierces
    // the renderer's shadow DOM).
    await expect(page.getByText("function greet").first()).toBeVisible({
      timeout: 15000,
    });
    await expect(page.getByText("const x = 42;").first()).toBeVisible();
  });

  test("renders content for an unrecognised extension", async ({ page }) => {
    await mockTerminalApis(page);
    await page.route("**/api/sessions/*/diff/files", (r) =>
      r.fulfill({
        json: {
          files: [
            {
              path: "data.xyz",
              old_path: null,
              status: "added",
              additions: 1,
              deletions: 0,
            },
          ],
          per_repo_bases: [{ base_branch: "main" }],
          warning: null,
        },
      }),
    );
    await page.route(/\/api\/sessions\/[^/]+\/diff\/file\?/, (r) =>
      r.fulfill({
        json: {
          file: {
            path: "data.xyz",
            old_path: null,
            status: "added",
            additions: 1,
            deletions: 0,
          },
          old_content: "",
          new_content: "some unknown format content\n",
          patch: makePatch("data.xyz", "", "some unknown format content\n"),
          is_binary: false,
          truncated: false,
        },
      }),
    );

    await page.goto("/");
    await expect(page.locator("header")).toBeVisible();
    await clickSidebarSession(page, "pinch-test");
    await expect(page.getByText("data.xyz").first()).toBeVisible({
      timeout: 10000,
    });
    await page.getByText("data.xyz").first().click();

    await expect(
      page.getByText("some unknown format content").first(),
    ).toBeVisible({ timeout: 10000 });
  });
});
