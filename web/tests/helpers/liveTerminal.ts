// Shared setup for the mocked live-terminal specs: the session boot, and the
// locators and byte readers the assertions key on.

import { expect, type Page } from "@playwright/test";
import { clickSidebarSession, openMobileSidebar } from "./sidebar";
import { makeLiveFrame, mockTerminalApis, seedSettings, type MockHandle } from "./terminal-mocks";

export const scroller = (page: Page) => page.locator("[data-live-terminal] > div").first();
export const liveContent = (page: Page) => page.locator("[data-live-content]");

export const liveTexts = (h: MockHandle) => h.liveMessages.map((b) => b.toString("latin1"));
export const liveMatches = (h: MockHandle, re: RegExp) => liveTexts(h).some((s) => re.test(s));

/** Open the live view of a seeded session on an already-mocked page. */
export async function openLiveSession(
  page: Page,
  handle: MockHandle,
  opts: {
    mobile?: boolean;
    title?: string;
    settings?: Parameters<typeof seedSettings>[1] | null;
    /** False for a structured-view session, which mounts no live terminal. */
    waitForLive?: boolean;
  } = {},
): Promise<MockHandle> {
  await page.goto("/");
  if (opts.settings !== null) {
    await seedSettings(page, opts.settings ?? { mobileFontSize: 14 });
    await page.reload();
  }
  if (opts.mobile) await openMobileSidebar(page);
  await clickSidebarSession(page, opts.title ?? "pinch-test");
  if (opts.waitForLive === false) return handle;
  await page.locator("[data-live-terminal]").first().waitFor({ state: "visible", timeout: 10_000 });
  await handle.waitForLiveReady();
  return handle;
}

/** Mock the terminal APIs and open the live view of the `pinch-test` session. */
export async function openLiveTerminal(page: Page, opts: Parameters<typeof openLiveSession>[2] = {}) {
  return openLiveSession(page, await mockTerminalApis(page), opts);
}

/** A standard 24-row frame carrying the alt-screen / mouse flags under test. */
export async function pushModeFrame(
  handle: MockHandle,
  flags: { altScreen: boolean; mouse: boolean; mouseSgr: boolean },
) {
  await handle.pushLiveFrame({ ...makeLiveFrame({ rows: 24, history: 120, window: 24 }), ...flags });
}

/** Wait for the scroller to settle into forwarding (pinned) or reading mode. */
export async function expectScrollMode(page: Page, mode: "forward" | "read") {
  await expect
    .poll(() => scroller(page).getAttribute("class"))
    .toContain(mode === "forward" ? "overflow-hidden" : "overflow-y-auto");
}
