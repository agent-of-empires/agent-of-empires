import type { Page } from "@playwright/test";
import { test, expect, pressShortcut } from "./helpers/mockedTest";
import { liveTexts, openLiveSession } from "./helpers/liveTerminal";
import { mockTerminalApis, type MockHandle } from "./helpers/terminal-mocks";
import { iPhone13 } from "./helpers/viewports";

// The owner's grid is taller than the room a docked strip leaves on a 375px phone.
const OWNER_ROWS = 40;
const ownerScreen = () => ({
  content:
    ["TOP ROW", ...Array.from({ length: OWNER_ROWS - 2 }, (_, i) => `row ${i + 2}`), "INPUT LINE"].join("\n") + "\n",
  rows: OWNER_ROWS,
  history: 0,
  cursor: { x: 10, y: OWNER_ROWS - 1 },
});

// The resize and claim controls this tab sent, in order.
const sizing = (handle: MockHandle) =>
  liveTexts(handle).flatMap((text) => {
    try {
      const msg = JSON.parse(text) as { type?: string; rows?: number };
      if (msg.type === "claim") return ["claim"];
      return msg.type === "resize" ? [msg.rows!] : [];
    } catch {
      return [];
    }
  });

test.describe("Live terminal control strip at 375px", () => {
  test.use({ ...iPhone13, viewport: { width: 375, height: 667 } });

  test("docks above the key bar, keeps the input line in view, and Take control hands over", async ({ page }) => {
    const handle = await mockTerminalApis(page, { viewOnly: { owner: "tui", transportReason: "pipe_owned" } });
    await openLiveSession(page, handle, { mobile: true });
    await handle.pushLiveFrame(ownerScreen());

    const strip = page.locator("[data-live-control-strip]");
    await expect(strip.getByRole("status")).toHaveText("Open in the TUI. Take control to type. · Updates may lag");
    const stripBox = (await strip.boundingBox())!;
    const toolbarBox = (await page.locator("[data-terminal-toolbar]").boundingBox())!;
    expect(stripBox.y + stripBox.height).toBeLessThanOrEqual(toolbarBox.y + 1);
    expect(stripBox.x).toBeGreaterThanOrEqual(0);
    expect(stripBox.x + stripBox.width).toBeLessThanOrEqual(375);
    expect(await page.evaluate(() => document.documentElement.scrollWidth)).toBeLessThanOrEqual(375);

    // Crop from the top: the owner's input line stays visible just above the strip.
    const scrollerBox = (await page.locator("[data-live-terminal] > div").first().boundingBox())!;
    const input = (await page.locator("[data-live-content] > *", { hasText: "INPUT LINE" }).boundingBox())!;
    const top = (await page.locator("[data-live-content] > *", { hasText: "TOP ROW" }).boundingBox())!;
    expect(input.y).toBeGreaterThanOrEqual(scrollerBox.y);
    expect(input.y + input.height).toBeLessThanOrEqual(Math.min(scrollerBox.y + scrollerBox.height, stripBox.y) + 1);
    expect(top.y).toBeLessThan(scrollerBox.y);

    await expect(strip.getByRole("button", { name: "Take control" })).toHaveCSS("height", "32px");
    await strip.getByRole("button", { name: "Take control" }).tap();
    await expect(strip).toHaveCount(0);
    // The claim follows the resize the fit sends once the strip is gone.
    await expect.poll(() => liveTexts(handle).filter((t) => t.includes('"claim"')).length).toBe(1);
  });

  const takeControlPaths = {
    "the strip's button": (page: Page) => page.locator("[data-live-take-control]").tap(),
    "the command palette": async (page: Page) => {
      await pressShortcut(page, "ControlOrMeta+k");
      await page.getByPlaceholder("Search actions, sessions, settings…").fill("Take control");
      await page.keyboard.press("Enter");
    },
  };
  for (const [path, takeControl] of Object.entries(takeControlPaths)) {
    test(`Take control from ${path} resizes the pane once, to the grid without the strip`, async ({ page }) => {
      const handle = await mockTerminalApis(page, { viewOnly: { owner: "tui" } });
      await openLiveSession(page, handle, { mobile: true });
      await expect(page.locator("[data-live-control-strip]")).toBeVisible();
      const docked = sizing(handle)
        .filter((x): x is number => typeof x === "number")
        .at(-1)!;

      await takeControl(page);
      await expect(page.locator("[data-live-control-strip]")).toHaveCount(0);
      await expect.poll(() => sizing(handle).filter((x) => x === "claim").length).toBe(1);
      const sent = sizing(handle);
      const claimAt = sent.indexOf("claim");
      const full = sent.slice(0, claimAt).at(-1);
      // The claim resizes the pane to the last grid before it: the one without the strip.
      expect(typeof full === "number" && full > docked, JSON.stringify(sent)).toBe(true);
      await expect
        .poll(() =>
          sizing(handle)
            .slice(claimAt + 1)
            .filter((rows) => rows !== full),
        )
        .toEqual([]);
    });
  }

  test("after Take control, the top bar's slower-updates notice leaves the header's controls in view", async ({
    page,
  }) => {
    const handle = await mockTerminalApis(page, { viewOnly: { owner: "tui", transportReason: "pipe_owned" } });
    await openLiveSession(page, handle, { mobile: true });
    await page.locator("[data-live-take-control]").tap();
    const notice = page.locator("[data-live-slow-notice]");
    await expect(notice.getByRole("status")).toHaveText("Updates are slower.");
    await expect(notice.getByRole("button", { name: "Reconnect" })).toBeInViewport({ ratio: 1 });
    // The header clips overflow, so a crowded notice would push its own controls off screen.
    for (const name of ["Dismiss slower updates notice", "More options"]) {
      await expect(page.locator("header").getByRole("button", { name })).toBeInViewport({ ratio: 1 });
    }
  });
});
