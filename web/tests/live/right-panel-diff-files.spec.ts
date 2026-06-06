// Live-backend spec: right panel's diff file list (#1221).
//
// Seeds a working git repo with five committed files on `main`, then
// modifies those same files uncommitted. `aoe add` registers the dir as
// a session whose `project_path` IS the modified working tree; the diff
// endpoint then returns those five files. Exercises file-count rendering,
// the flat/tree view toggle, and keyboard navigation (ArrowDown + Enter)
// against the real backend.

import { spawnSync } from "node:child_process";
import { join } from "node:path";
import { test as base, expect } from "@playwright/test";
import { spawnAoeServe, resolveAoeBinary } from "../helpers/aoeServe";
import { commitAll, initWorkingRepo, writeFiles } from "../helpers/gitFixture";

base(
  "right panel diff list: counts, tree/flat toggle, keyboard select",
  async ({ page }, testInfo) => {
    const serve = await spawnAoeServe({
      authMode: "none",
      workerIndex: testInfo.workerIndex,
      parallelIndex: testInfo.parallelIndex,
      seedFn: ({ home, env }) => {
        const projectDir = join(home, "project");
        initWorkingRepo(projectDir);
        const baseline = {
          "src/a.ts": "export const a = 1;\n",
          "src/b.ts": "export const b = 2;\n",
          "src/nested/c.ts": "export const c = 3;\n",
          "lib/d.ts": "export const d = 4;\n",
          "README.md": "# Old\n",
        };
        writeFiles(projectDir, baseline);
        commitAll(projectDir, "baseline");
        writeFiles(projectDir, {
          "src/a.ts": "export const a = 11;\n",
          "src/b.ts": "export const b = 22;\n",
          "src/nested/c.ts": "export const c = 33;\n",
          "lib/d.ts": "export const d = 44;\n",
          "README.md": "# New\n",
        });
        const addRes = spawnSync(
          resolveAoeBinary(),
          ["add", projectDir, "-t", "rp-files", "-c", "claude"],
          { env },
        );
        if (addRes.status !== 0) {
          throw new Error(
            `aoe add failed: status=${addRes.status} stderr=${addRes.stderr?.toString() ?? "<none>"}`,
          );
        }
      },
    });

    try {
      await page.goto(`${serve.baseUrl}/`);
      const sessionRow = page
        .getByRole("link")
        .filter({ hasText: "rp-files" })
        .first();
      await expect(sessionRow).toBeVisible({ timeout: 10_000 });
      await sessionRow.click();

      // File count chip lives in the right-panel header. The dashboard
      // mounts both a desktop and a mobile copy of the right panel
      // (one hidden via CSS), so the chip appears twice; first() is
      // unambiguous and matches the desktop pane on the test viewport.
      await expect(
        page.getByText("5 files", { exact: true }).first(),
      ).toBeVisible({
        timeout: 15_000,
      });

      // Toggle title flips with current mode. Click whichever variant
      // is currently shown so we land deterministically in tree mode.
      const toTree = page.locator('button[title="Switch to tree view"]');
      const toFlat = page.locator('button[title="Switch to flat list"]');
      if (
        await toTree
          .first()
          .isVisible()
          .catch(() => false)
      ) {
        await toTree.first().click();
      }
      await expect(toFlat.first()).toBeVisible();

      // Tree mode collapses parent dirs into rows. With files under
      // `src/`, `src/nested/`, and `lib/`, expect at least the three
      // top-level dir rows. Match a recognisable one.
      await expect(
        page.getByRole("button", { name: /^src/ }).first(),
      ).toBeVisible();

      // Flip back to flat so subsequent assertions hit the plain
      // index-based list.
      await toFlat.first().click();
      await expect(toTree.first()).toBeVisible();

      // Keyboard nav: focus row 0 via hover (`onMouseEnter` sets
      // focusedIndex), click to open it, then ArrowDown + Enter to
      // advance to row 1 and open that file.
      //
      // The backend sorts diff files by path
      // (`src/git/diff.rs::compute_changed_files`, byte-lexicographic),
      // so for our seed the order is:
      //   row 0 -> README.md   (contains "# Old" / "# New")
      //   row 1 -> lib/d.ts    (contains "export const d = 4")
      // We assert per-file diff content (only rendered in the viewer,
      // never in the file-list rows) so the post-keypress assertion
      // proves a real state change rather than re-checking the
      // already-visible "Modified" status label.
      const firstRow = page.locator('button[data-index="0"]').first();
      await firstRow.hover();
      await firstRow.click();
      await expect(page.getByText("# Old").first()).toBeVisible({
        timeout: 10_000,
      });

      await page.keyboard.press("ArrowDown");
      await page.keyboard.press("Enter");
      await expect(page.getByText(/export const d = 4/).first()).toBeVisible({
        timeout: 10_000,
      });
    } finally {
      await serve.stop();
    }
  },
);
