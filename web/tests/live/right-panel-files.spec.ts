// Live-backend spec: the Files pane + provenance file viewer (#3088).
//
// Registers a NON-git scratch directory as a session (the case from the
// original report: a scratch session has no git diff, so its files never
// appeared in the diff list). Opens the Files pane from the activity bar,
// clicks a Markdown file, and asserts it renders as formatted HTML rather than
// raw source. Exercises the git-agnostic /acp/files listing and the
// provenance-confined /file read end to end.
//
// Also opens a non-Markdown file and asserts the real line-number gutter
// (#4003): the viewer renders through @pierre/diffs, which cannot paint under
// jsdom, so the numbers themselves are only observable in a browser.

import { spawnSync } from "node:child_process";
import { join } from "node:path";
import { test as base, expect } from "@playwright/test";
import { spawnAoeServe, resolveAoeBinary } from "../helpers/aoeServe";
import { writeFiles } from "../helpers/gitFixture";

base("files pane renders a Markdown file in a scratch session", async ({ page }, testInfo) => {
  const serve = await spawnAoeServe({
    authMode: "none",
    workerIndex: testInfo.workerIndex,
    parallelIndex: testInfo.parallelIndex,
    seedFn: ({ home, env }) => {
      // A plain (non-git) directory: no `git init`, so there is no diff.
      const projectDir = join(home, "scratch-project");
      writeFiles(projectDir, {
        "plan.md": "# The Plan\n\n- step one\n- step two\n",
        "readme.txt": "not markdown\n",
        "notes.ts": "const a = 1;\nconst b = 2;\nconst c = 3;\n",
      });
      const addRes = spawnSync(resolveAoeBinary(), ["add", projectDir, "-t", "rp-files-md", "-c", "claude"], { env });
      if (addRes.status !== 0) {
        throw new Error(`aoe add failed: status=${addRes.status} stderr=${addRes.stderr?.toString() ?? "<none>"}`);
      }
    },
  });

  try {
    await page.goto(`${serve.baseUrl}/`);
    const sessionRow = page.getByRole("link").filter({ hasText: "rp-files-md" }).first();
    await expect(sessionRow).toBeVisible({ timeout: 10_000 });
    await sessionRow.click();

    // Open the Files pane from the activity bar (not auto-opened).
    await page.getByRole("button", { name: "Toggle Files pane" }).first().click();

    // The scratch dir's files list even though there is no git diff.
    const planRow = page.getByRole("button", { name: "plan.md" }).first();
    await expect(planRow).toBeVisible({ timeout: 10_000 });
    await planRow.click();

    // Rendered by default: "# The Plan" becomes an <h1>The Plan</h1>, and the
    // list items render as a real <ul>, not raw "- step one" text.
    await expect(page.getByRole("heading", { name: "The Plan" }).first()).toBeVisible({
      timeout: 10_000,
    });
    await expect(page.getByRole("listitem").filter({ hasText: "step one" }).first()).toBeVisible();

    // A non-Markdown file renders as source with a line-number gutter. The
    // renderer marks each gutter cell with `data-line-number-content`, so
    // assert one number per seeded line, in order, rather than that a gutter
    // merely exists.
    await page.getByRole("button", { name: "Files" }).first().click();
    const notesRow = page.getByRole("button", { name: "notes.ts" }).first();
    await expect(notesRow).toBeVisible({ timeout: 10_000 });
    await notesRow.click();

    const gutter = page.locator("[data-line-number-content]");
    await expect(gutter).toHaveCount(3, { timeout: 10_000 });
    await expect(gutter).toHaveText(["1", "2", "3"]);
    await expect(page.getByText("const b = 2;").first()).toBeVisible();
  } finally {
    await serve.stop();
  }
});
