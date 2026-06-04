// User story: Escape closes the command palette.
//
// App.tsx's onEscape handler dismisses the palette / about modal /
// selected file (see App.tsx:584-591). The palette must close on a
// single Escape press regardless of focus inside it.

import { test as base, expect } from "@playwright/test";
import { spawnAoeServe } from "../../helpers/aoeServe";

const MOD = process.platform === "darwin" ? "Meta" : "Control";

base("Escape closes an open command palette", async ({ page }, testInfo) => {
  const serve = await spawnAoeServe({
    authMode: "none",
    workerIndex: testInfo.workerIndex,
    parallelIndex: testInfo.parallelIndex,
  });

  try {
    await page.goto(serve.baseUrl);
    await expect(
      page.getByRole("button", { name: "Go to dashboard" }),
    ).toBeVisible({ timeout: 10_000 });

    await page.keyboard.press(`${MOD}+K`);
    const palette = page.getByRole("dialog", { name: "Command palette" });
    await expect(palette).toBeVisible({ timeout: 5_000 });

    await page.keyboard.press("Escape");
    await expect(palette).toBeHidden({ timeout: 5_000 });
  } finally {
    await serve.stop();
  }
});
