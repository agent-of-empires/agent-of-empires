// Live-backend spec: the Files pane + provenance file viewer (#3088).
//
// Registers a NON-git scratch directory as a session (the case from the
// original report: a scratch session has no git diff, so its files never
// appeared in the diff list). Opens the Files pane from the activity bar,
// clicks a Markdown file, and asserts it renders as formatted HTML rather than
// raw source. Exercises the git-agnostic /acp/files listing and the
// provenance-confined /file read end to end.
//
// A second case opens a non-Markdown file and asserts the real line-number
// gutter (#4003). It seeds its own session rather than navigating back from
// the first: the viewer's back control and the activity-bar pane toggle both
// match an accessible name containing "Files", so a back-click is ambiguous
// and closed the pane instead.

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
  } finally {
    await serve.stop();
  }
});

base("files pane numbers the lines of a source file", async ({ page }, testInfo) => {
  const serve = await spawnAoeServe({
    authMode: "none",
    workerIndex: testInfo.workerIndex,
    parallelIndex: testInfo.parallelIndex,
    seedFn: ({ home, env }) => {
      const projectDir = join(home, "gutter-project");
      writeFiles(projectDir, { "notes.ts": "const a = 1;\nconst b = 2;\nconst c = 3;\n" });
      const addRes = spawnSync(resolveAoeBinary(), ["add", projectDir, "-t", "rp-files-gutter", "-c", "claude"], {
        env,
      });
      if (addRes.status !== 0) {
        throw new Error(`aoe add failed: status=${addRes.status} stderr=${addRes.stderr?.toString() ?? "<none>"}`);
      }
    },
  });

  try {
    await page.goto(`${serve.baseUrl}/`);
    const sessionRow = page.getByRole("link").filter({ hasText: "rp-files-gutter" }).first();
    await expect(sessionRow).toBeVisible({ timeout: 10_000 });
    await sessionRow.click();

    await page.getByRole("button", { name: "Toggle Files pane" }).first().click();

    const notesRow = page.getByRole("button", { name: "notes.ts" }).first();
    await expect(notesRow).toBeVisible({ timeout: 10_000 });
    await notesRow.click();

    // The renderer marks each gutter cell with `data-line-number-content`, so
    // assert the numbers themselves, in order, rather than that a gutter
    // merely exists. Four cells for three lines of code: the renderer splits
    // on line offsets, so a file ending in a newline carries a final empty
    // line and numbers it, the way an editor shows it.
    const gutter = page.locator("[data-line-number-content]");
    await expect(gutter).toHaveCount(4, { timeout: 10_000 });
    await expect(gutter).toHaveText(["1", "2", "3", "4"]);
  } finally {
    await serve.stop();
  }
});
