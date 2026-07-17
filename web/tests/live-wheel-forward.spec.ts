import { test, expect } from "./helpers/mockedTest";
import { devices, type Page } from "@playwright/test";
import { clickSidebarSession, openMobileSidebar } from "./helpers/sidebar";
import {
  mockTerminalApis,
  installTerminalSpies,
  seedSettings,
  makeLiveFrame,
  fireTouches,
  type MockHandle,
} from "./helpers/terminal-mocks";

// A full-screen (alternate-screen) mouse agent has no capturable
// scrollback, so the mobile live view forwards the wheel to the app as
// input bytes instead of widening the capture window. This drives the
// real bundle (useLiveTerminal -> wheelMouseBytes -> WebSocket) so the
// forwarded bytes are asserted on the wire, per encoding.
test.use({ ...devices["iPhone 13"] });

async function openSession(page: Page, handle: MockHandle) {
  await openMobileSidebar(page);
  await clickSidebarSession(page, "pinch-test");
  await page.locator("[data-live-terminal]").waitFor({ state: "visible", timeout: 10_000 });
  await expect.poll(() => handle.liveMessages.length, { timeout: 5_000 }).toBeGreaterThan(0);
  await page.waitForTimeout(400);
}

function pushFrame(handle: MockHandle, flags: { altScreen: boolean; mouse: boolean; mouseSgr: boolean }) {
  handle.pushLiveFrame({
    ...makeLiveFrame({ rows: 24, history: 120, window: 24 }),
    ...flags,
  } as Parameters<MockHandle["pushLiveFrame"]>[0]);
}

const scroller = (page: Page) => page.locator("[data-live-terminal] > div").first();
const texts = (h: MockHandle) => h.liveMessages.map((b) => b.toString("latin1"));
const hasLegacyDown = (h: MockHandle) =>
  h.liveMessages.some((b) => b.length >= 4 && b[0] === 0x1b && b[1] === 0x5b && b[2] === 0x4d && b[3] === 0x61);

async function setup(page: Page) {
  await installTerminalSpies(page);
  const handle = await mockTerminalApis(page);
  await page.goto("/");
  await seedSettings(page, { mobileFontSize: 14 });
  await page.reload();
  await openSession(page, handle);
  return handle;
}

async function swipeUp(page: Page) {
  await fireTouches(page, "touchstart", [{ x: 100, y: 300 }]);
  await fireTouches(page, "touchmove", [{ x: 100, y: 220 }]);
  await fireTouches(page, "touchend", [{ x: 100, y: 220 }]);
}

test("swipe over a full-screen SGR-mouse app forwards SGR wheel bytes", async ({ page }) => {
  const handle = await setup(page);
  pushFrame(handle, { altScreen: true, mouse: true, mouseSgr: true });
  await expect.poll(() => scroller(page).getAttribute("class")).toContain("overflow-hidden");
  // touch-action: none is what keeps the drag from panning the whole page:
  // React's delegated touch listeners are passive, so the component cannot
  // preventDefault the native pan (the keyboard-open page-scroll clunk).
  await expect.poll(() => scroller(page).evaluate((el) => getComputedStyle(el).touchAction)).toBe("none");
  await swipeUp(page);
  await expect.poll(() => texts(handle).some((s) => s.includes("\x1b[<65;"))).toBe(true);

  // Downward swipe forwards wheel UP (button 64).
  await fireTouches(page, "touchstart", [{ x: 100, y: 120 }]);
  await fireTouches(page, "touchmove", [{ x: 100, y: 300 }]);
  await fireTouches(page, "touchend", [{ x: 100, y: 300 }]);
  await expect.poll(() => texts(handle).some((s) => s.includes("\x1b[<64;"))).toBe(true);

  // Wheel events in all three deltaModes (px / line / page) + a sub-notch
  // delta (no-op) + a scroll (which must NOT enter reading in forward mode).
  await scroller(page).dispatchEvent("wheel", { deltaY: 120, deltaMode: 0 });
  await scroller(page).dispatchEvent("wheel", { deltaY: 3, deltaMode: 1 });
  await scroller(page).dispatchEvent("wheel", { deltaY: 1, deltaMode: 2 });
  await scroller(page).dispatchEvent("wheel", { deltaY: 1, deltaMode: 0 });
  await scroller(page).dispatchEvent("scroll", {});
  // Still forwarding, still pinned, still no "Back to live" affordance.
  await expect(page.getByRole("button", { name: "Back to live" })).toHaveCount(0);
});

test("swipe over a full-screen LEGACY-mouse app forwards X10 wheel bytes", async ({ page }) => {
  const handle = await setup(page);
  pushFrame(handle, { altScreen: true, mouse: true, mouseSgr: false });
  await expect.poll(() => scroller(page).getAttribute("class")).toContain("overflow-hidden");
  await swipeUp(page);
  await expect.poll(() => hasLegacyDown(handle)).toBe(true);
  expect(texts(handle).some((s) => s.includes("\x1b[<"))).toBe(false);
});

test("normal-screen agent does NOT forward mouse bytes", async ({ page }) => {
  const handle = await setup(page);
  pushFrame(handle, { altScreen: false, mouse: true, mouseSgr: true });
  await expect.poll(() => scroller(page).getAttribute("class")).toContain("overflow-y-auto");
  await swipeUp(page);
  await page.waitForTimeout(300);
  expect(texts(handle).some((s) => s.includes("\x1b[<") || s.includes("\x1b[M"))).toBe(false);
  expect(hasLegacyDown(handle)).toBe(false);
});
