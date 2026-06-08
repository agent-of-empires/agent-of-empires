// Live-backend spec: wizard `Clone URL` tab end-to-end against a real
// `aoe serve`. Covers the happy path (file:// clone of a throwaway bare
// repo into the isolated HOME) and the validation-failure path
// (unrecognised URL scheme surfaces the server's error banner).
//
// Pairs with the matching matrix entry `git-clone` in
// `web/tests/coverage-matrix.json`. Wizard interaction patterns mirror
// the directory-browser live spec.

import { existsSync } from "node:fs";
import { join } from "node:path";
import { test as base, expect } from "@playwright/test";
import { spawnAoeServe } from "../helpers/aoeServe";
import { createBareRepo } from "../helpers/gitFixture";

base(
  "clone happy path: file:// URL clones into HOME and the wizard advances",
  async ({ page }, testInfo) => {
    const serve = await spawnAoeServe({
      authMode: "none",
      workerIndex: testInfo.workerIndex,
      parallelIndex: testInfo.parallelIndex,
    });

    try {
      const bare = createBareRepo(serve.home);

      await page.goto(serve.baseUrl);
      await page.locator("body").click();
      await page.keyboard.press("n");
      await expect(
        page.getByRole("heading", { name: "New session" }),
      ).toBeVisible({
        timeout: 10_000,
      });

      await page
        .getByRole("button", { name: "Clone URL", exact: true })
        .click();

      const cloneBtn = page.getByRole("button", { name: "Clone repository" });
      await expect(cloneBtn).toBeDisabled();

      const urlInput = page.locator("#clone-url");
      await urlInput.fill(bare.url);
      await expect(cloneBtn).toBeEnabled();

      // Pin the destination to a known path under the isolated HOME so the
      // assertion isn't sensitive to the repo-name derivation in the server.
      await page.getByRole("button", { name: /Advanced/ }).click();
      const destInput = page.locator("#clone-dest");
      const dest = join(serve.home, "cloned-repo");
      await destInput.fill(dest);

      await cloneBtn.click();

      // The wizard switches back to the Recent tab and renders the selected
      // path block; the cloned dir exists on disk; the wizard `Next`
      // button becomes enabled now that `data.path` is set.
      await expect(page.getByText("Selected project")).toBeVisible({
        timeout: 30_000,
      });
      await expect(page.getByText(dest, { exact: false })).toBeVisible();
      expect(existsSync(join(dest, ".git"))).toBe(true);
      await expect(page.getByRole("button", { name: "Next" })).toBeEnabled();
    } finally {
      await serve.stop();
    }
  },
);

base(
  "clone failure path: unrecognised URL scheme surfaces a server error",
  async ({ page }, testInfo) => {
    const serve = await spawnAoeServe({
      authMode: "none",
      workerIndex: testInfo.workerIndex,
      parallelIndex: testInfo.parallelIndex,
    });

    try {
      await page.goto(serve.baseUrl);
      await page.locator("body").click();
      await page.keyboard.press("n");
      await expect(
        page.getByRole("heading", { name: "New session" }),
      ).toBeVisible({
        timeout: 10_000,
      });

      await page
        .getByRole("button", { name: "Clone URL", exact: true })
        .click();
      await page.locator("#clone-url").fill("not-a-url");
      await page.getByRole("button", { name: "Clone repository" }).click();

      // The server's `validation_failed` message reaches the UI banner.
      await expect(
        page.getByText("URL does not look like a git repository URL"),
      ).toBeVisible({ timeout: 10_000 });

      // No path got selected, so the wizard can't advance.
      await expect(page.getByText("Selected project")).toHaveCount(0);
      await expect(page.getByRole("button", { name: "Next" })).toBeDisabled();
    } finally {
      await serve.stop();
    }
  },
);
