import type { Page } from "@playwright/test";
import { test, expect, observeFor, pressShortcut } from "./helpers/mockedTest";
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

const resizes = (handle: MockHandle) =>
  liveTexts(handle).flatMap((text) => {
    try {
      const msg = JSON.parse(text) as { type?: string; cols?: number; rows?: number };
      return msg.type === "resize" ? [{ cols: msg.cols!, rows: msg.rows! }] : [];
    } catch {
      return [];
    }
  });

// Only an owner's input reaches the pane, so a key arriving shows the claim was granted. `refocus` puts focus back
// on App's keyboard proxy first, for paths that leave it elsewhere.
const sendUntilDelivered = (page: Page, handle: MockHandle, key: string, bytes: string, refocus: boolean) =>
  expect(async () => {
    if (refocus) await page.locator("[data-keyboard-proxy]").focus();
    await page.keyboard.press(key);
    expect(handle.liveInput.map(String)).toContain(bytes);
  }).toPass();

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
      const strip = page.locator("[data-live-control-strip]");
      await expect(strip).toBeVisible();
      // The fit keeps its tallest height until the width changes by more than 1px, so a report sent before the
      // strip docked would keep the full grid. A narrower width, reported, then the original one resets it to the
      // docked height.
      const lastResize = () => resizes(handle).at(-1);
      const { width, height } = page.viewportSize()!;
      const wideCols = lastResize()!.cols;
      await page.setViewportSize({ width: width - 15, height });
      await expect.poll(() => lastResize()!.cols).toBeLessThan(wideCols);
      await page.setViewportSize({ width, height });
      await expect.poll(() => lastResize()!.cols).toBe(wideCols);
      await expect(strip).toBeVisible();
      // Settled once two polls past the fit's 150ms debounce agree.
      let previous: string | undefined;
      await expect
        .poll(
          () => {
            const current = JSON.stringify(lastResize());
            const settled = current === previous;
            previous = current;
            return settled;
          },
          { intervals: [250] },
        )
        .toBe(true);
      const docked = lastResize()!.rows;

      await takeControl(page);
      await expect(page.locator("[data-live-control-strip]")).toHaveCount(0);
      await expect.poll(() => sizing(handle).filter((x) => x === "claim").length).toBe(1);
      const sent = sizing(handle);
      const claimAt = sent.indexOf("claim");
      const full = sent.slice(0, claimAt).at(-1);
      // The claim resizes the pane to the last grid before it: the one without the strip.
      expect(typeof full === "number" && full > docked, JSON.stringify(sent)).toBe(true);
      await sendUntilDelivered(page, handle, "Enter", "\r", true);
      // Well past the 150ms fit debounce the grant's layout change triggers.
      await observeFor(page, 1000, async () => {
        expect(
          sizing(handle)
            .slice(claimAt + 1)
            .filter((rows) => rows !== full),
        ).toEqual([]);
      });
    });
  }

  test("a fit still queued when Take control is clicked claims with the grid without the strip", async ({ page }) => {
    const handle = await mockTerminalApis(page, { viewOnly: { owner: "tui" } });
    await openLiveSession(page, handle, { mobile: true });
    await expect(page.locator("[data-live-control-strip]")).toBeVisible();
    const scroller = page.locator("[data-live-terminal] > div").first();
    const wideCols = resizes(handle).at(-1)!.cols;
    // A narrower pane resets the fit's height latch, so it reports the docked grid at that width.
    await scroller.evaluate((el) => {
      el.style.width = `${el.clientWidth - 15}px`;
    });
    await expect.poll(() => resizes(handle).at(-1)!.cols).toBeLessThan(wideCols);
    const docked = resizes(handle).at(-1)!.rows;

    // Widen it back and click from an observer registered after the fit's: it runs once the fit has queued its
    // debounced report, which resets the latch again and measures whatever the pane holds when it fires.
    await scroller.evaluate(
      (el) =>
        new Promise<void>((resolve) => {
          const ro = new ResizeObserver(() => {
            ro.disconnect();
            document.querySelector<HTMLButtonElement>("[data-live-take-control]")!.click();
            resolve();
          });
          el.style.width = "";
          ro.observe(el);
        }),
    );
    await expect.poll(() => sizing(handle).filter((x) => x === "claim").length).toBe(1);
    const sent = liveTexts(handle).flatMap((text) => {
      try {
        const msg = JSON.parse(text) as { type?: string; cols?: number; rows?: number };
        if (msg.type === "claim") return ["claim" as const];
        return msg.type === "resize" ? [{ cols: msg.cols!, rows: msg.rows! }] : [];
      } catch {
        return [];
      }
    });
    const claimAt = sent.indexOf("claim");
    const carrier = sent[claimAt - 1];
    // The queued report carried the claim, at the restored width and without the strip.
    expect(
      typeof carrier === "object" && carrier.cols === wideCols && carrier.rows > docked,
      JSON.stringify({ docked, sent }),
    ).toBe(true);
    await sendUntilDelivered(page, handle, "Enter", "\r", true);
    await observeFor(page, 1000, async () => {
      expect(
        sizing(handle)
          .slice(claimAt + 1)
          .filter((rows) => typeof carrier !== "object" || rows !== carrier.rows),
      ).toEqual([]);
    });
  });

  test("Tab on App's keyboard proxy reaches Take control only during a blocked key's hold", async ({ page }) => {
    const handle = await mockTerminalApis(page, { viewOnly: { owner: "tui" } });
    await openLiveSession(page, handle, { mobile: true });
    const strip = page.locator("[data-live-control-strip]");
    await expect(strip.getByRole("status")).toHaveText("Open in the TUI. Take control to type.");
    const proxy = page.locator("[data-keyboard-proxy]");
    await proxy.focus();
    // Outside the hold Tab is terminal input: view only blocks it, which starts the hold.
    await page.keyboard.press("Tab");
    await expect(strip.getByRole("status")).toHaveText("Not sent. Take control to type.");
    await expect(proxy).toBeFocused();
    await page.keyboard.press("Tab");
    await expect(page.locator("[data-live-take-control]")).toBeFocused();
    await page.keyboard.press("Enter");
    await expect(strip).toHaveCount(0);
    // The button is gone; focus is back on the proxy, so typing goes on without a tap.
    await expect(proxy).toBeFocused();
    // Holding control, Tab goes to the pane again.
    await sendUntilDelivered(page, handle, "Tab", "\t", false);
    await expect(proxy).toBeFocused();
  });

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
