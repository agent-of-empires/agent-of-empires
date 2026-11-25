// The right dock as the Session's review panel: a picked file opens as a main-area tab beside the Agent, the dock
// maximizes over the main area, and between md and 1100px it overlays the main area as a sheet.
// Tab and width logic: MainViewTabs.test.tsx, rightDockMode.test.ts.

import { test, expect, pressShortcut } from "./helpers/mockedTest";
import {
  diffFileResponse,
  diffFilesResponse,
  openDiffFile,
  openDiffSession,
  setupDiffSession,
} from "./helpers/diffMocks";

const FILES = diffFilesResponse([{ path: "src/example.ts" }, { path: "README.md" }]);
const CONTENTS = diffFileResponse({ path: "src/example.ts" }, "const x = 1;\n", "const x = 2;\n");

test.beforeEach(async ({ page }) => {
  await setupDiffSession(page, { files: FILES, contents: CONTENTS });
});

test("a picked file opens as a tab beside the Agent, which keeps it until closed", async ({ page }) => {
  await page.setViewportSize({ width: 1280, height: 720 });
  await openDiffSession(page);
  await expect(page.getByTestId("main-view-tabs")).toHaveCount(0);
  await openDiffFile(page, "example.ts");

  const agentTab = page.getByTestId("main-view-tab-agent");
  const fileTab = page.getByTestId("main-view-tab-file");
  await expect(fileTab).toHaveText("example.ts");
  await expect(fileTab).toHaveAttribute("aria-selected", "true");
  await expect(page.getByText("const x = 2;").first()).toBeVisible({ timeout: 15000 });

  await agentTab.click();
  await expect(agentTab).toHaveAttribute("aria-selected", "true");
  await expect(page.locator('[data-term="agent"]').first()).toBeVisible();
  await expect(page.getByText("const x = 2;").first()).toBeHidden();

  await fileTab.click();
  await expect(page.getByText("const x = 2;").first()).toBeVisible();

  await page.getByRole("button", { name: "Close example.ts" }).click();
  await expect(page.getByTestId("main-view-tabs")).toHaveCount(0);
  await expect(page.locator('[data-term="agent"]').first()).toBeVisible();
});

test("the right dock maximizes over the main area and restores", async ({ page }) => {
  await page.setViewportSize({ width: 1280, height: 720 });
  await openDiffSession(page);
  const agent = page.locator('[data-term="agent"]').first();
  await expect(agent).toBeVisible();

  await page.getByRole("button", { name: "Maximize panel" }).click();
  await expect(agent).toBeHidden();
  await expect(page.getByTestId("content-split-resize-handle")).toHaveCount(0);

  await page.getByRole("button", { name: "Restore panel" }).click();
  await expect(agent).toBeVisible();
  await expect(page.getByTestId("content-split-resize-handle")).toBeVisible();
});

test("between md and 1100px the dock is a sheet that Escape and the backdrop close", async ({ page }) => {
  await page.setViewportSize({ width: 1000, height: 720 });
  await openDiffSession(page);
  const sheet = page.getByRole("dialog", { name: "Panels" });
  await expect(sheet).toBeVisible();
  await expect(page.getByTestId("content-split-resize-handle")).toHaveCount(0);
  await expect(page.getByRole("button", { name: "Maximize panel" })).toHaveCount(0);
  await expect.poll(() => sheet.evaluate((el) => el.contains(document.activeElement))).toBe(true);

  await page.keyboard.press("Escape");
  await expect(sheet).toHaveCount(0);
  await expect(page.getByTestId("right-dock-toggle")).toBeFocused();

  await page.getByTestId("right-dock-toggle").click();
  await expect(sheet).toBeVisible();
  await page.getByTestId("right-dock-sheet-backdrop").click({ position: { x: 20, y: 200 } });
  await expect(sheet).toHaveCount(0);

  // Picking a file from the sheet closes it and shows the file in the main area.
  await page.getByTestId("right-dock-toggle").click();
  await openDiffFile(page, "example.ts");
  await expect(sheet).toHaveCount(0);
  await expect(page.getByTestId("main-view-tab-file")).toHaveAttribute("aria-selected", "true");
});

test("the sheet keeps Tab inside it, and Escape closes Help over it before the sheet", async ({ page }) => {
  await page.setViewportSize({ width: 1000, height: 720 });
  await openDiffSession(page);
  const sheet = page.getByRole("dialog", { name: "Panels" });
  await expect.poll(() => sheet.evaluate((el) => el.contains(document.activeElement))).toBe(true);
  const inSheet = () => sheet.evaluate((el) => el.contains(document.activeElement));
  for (let i = 0; i < 12; i++) {
    await page.keyboard.press("Tab");
    expect(await inSheet()).toBe(true);
  }
  for (let i = 0; i < 12; i++) {
    await page.keyboard.press("Shift+Tab");
    expect(await inSheet()).toBe(true);
  }

  await page.getByTestId("right-dock-sheet").focus();
  await pressShortcut(page, "?");
  await expect(page.getByRole("heading", { name: "Help" })).toBeVisible();
  await page.keyboard.press("Escape");
  await expect(page.getByRole("heading", { name: "Help" })).not.toBeVisible();
  await expect(sheet).toBeVisible();
});
