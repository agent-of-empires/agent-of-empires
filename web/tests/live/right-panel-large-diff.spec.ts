// Live-backend spec: right panel's diff viewer rendering a 1000-line
// file and a binary file (#1221).
//
// The diff viewer virtualizes via @pierre/diffs: off-screen rows are not in
// the DOM until scrolled near, so we assert a deep row is absent on first
// paint and present after scrolling the viewer to the bottom. The binary-file
// path renders the "Binary file changed" placeholder instead of a diff.

import { spawnSync } from "node:child_process";
import { join } from "node:path";
import { test as base, expect } from "@playwright/test";
import { spawnAoeServe, resolveAoeBinary } from "../helpers/aoeServe";
import {
  commitAll,
  generateLargeFileContent,
  initWorkingRepo,
  pngStubBytes,
  writeBinaryFile,
  writeFiles,
} from "../helpers/gitFixture";

base("right panel diff viewer: 1000-line file scrolls, binary file shows placeholder", async ({ page }, testInfo) => {
  const serve = await spawnAoeServe({
    authMode: "none",
    workerIndex: testInfo.workerIndex,
    parallelIndex: testInfo.parallelIndex,
    seedFn: ({ home, env }) => {
      const projectDir = join(home, "project");
      initWorkingRepo(projectDir);
      // Baseline on main: same large file content the modified version
      // will diverge from. Committing first means the diff lands on
      // the "modified" path with hunks of changes, not "added" with
      // one giant +1000.
      writeFiles(projectDir, {
        "big.txt": generateLargeFileContent(1000, "base"),
      });
      commitAll(projectDir, "baseline");
      // Now replace with a deterministic prefix swap so every line
      // shows in the diff. `generateLargeFileContent(1000, "edit")`
      // produces 1000 lines all distinct from the baseline.
      writeFiles(projectDir, {
        "big.txt": generateLargeFileContent(1000, "edit"),
      });
      // Binary file: not committed in baseline, so it shows as added.
      writeBinaryFile(projectDir, "image.png", pngStubBytes());
      const addRes = spawnSync(resolveAoeBinary(), ["add", projectDir, "-t", "rp-large", "-c", "claude"], { env });
      if (addRes.status !== 0) {
        throw new Error(`aoe add failed: status=${addRes.status} stderr=${addRes.stderr?.toString() ?? "<none>"}`);
      }
    },
  });

  try {
    await page.goto(`${serve.baseUrl}/`);
    const sessionRow = page.getByRole("link").filter({ hasText: "rp-large" }).first();
    await expect(sessionRow).toBeVisible({ timeout: 10_000 });
    await sessionRow.click();

    // Two files surface: big.txt (modified) and image.png (added).
    // The dashboard renders both a desktop and a mobile right panel
    // (one hidden via CSS); first() picks the desktop copy.
    await expect(page.getByText("2 files", { exact: true }).first()).toBeVisible({
      timeout: 15_000,
    });

    // Click big.txt; DiffFileViewer mounts. The viewer's left content
    // pane shows the path; assert via the path label.
    await page
      .getByRole("button", { name: /big\.txt/ })
      .first()
      .click();
    await expect(page.getByText("big.txt").first()).toBeVisible({
      timeout: 10_000,
    });
    // The renderer mounts.
    await expect(page.locator("diffs-container").first()).toBeVisible({
      timeout: 10_000,
    });

    // Virtualization: a row near the end of this 1000-line replacement
    // ("edit 999: lorem ...") is not in the DOM on first paint.
    await expect(page.getByText("edit 999:", { exact: false })).toHaveCount(0);

    // Scroll the diff viewer to the bottom; the virtualizer mounts rows as
    // they approach the viewport.
    await page.evaluate(() => {
      const host = document.querySelector("diffs-container");
      let el = host?.parentElement as HTMLElement | null;
      while (el && el.scrollHeight <= el.clientHeight) el = el.parentElement;
      if (el) el.scrollTop = el.scrollHeight;
    });
    await expect(page.getByText("edit 999:", { exact: false }).first()).toBeVisible({ timeout: 15_000 });

    // Switch to the binary file. Click via the row text.
    await page
      .getByRole("button", { name: /image\.png/ })
      .first()
      .click();
    await expect(page.getByText("Binary file changed", { exact: true }).first()).toBeVisible({ timeout: 10_000 });
  } finally {
    await serve.stop();
  }
});
