// User story: Cmd/Ctrl+K opens the command palette anywhere on the
// dashboard. The shortcut binds globally in useKeyboardShortcuts.ts
// (fires regardless of focus) and toggles the CommandPalette dialog.

import { test as base, expect } from "@playwright/test";
import { spawnAoeServe } from "../../helpers/aoeServe";

const MOD = process.platform === "darwin" ? "Meta" : "Control";

base("Cmd/Ctrl+K opens the command palette", async ({ page }, testInfo) => {
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
    await expect(
      palette.getByPlaceholder("Search actions, sessions, settings…"),
    ).toBeFocused();
  } finally {
    await serve.stop();
  }
});
