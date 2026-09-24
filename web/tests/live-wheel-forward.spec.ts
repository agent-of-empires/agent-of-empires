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
// scrollback, so the mobile live view forwards the wheel to the app instead
// of widening the capture window. This drives the real bundle
// (useLiveTerminal -> WebSocket) so the forwarded message is asserted on the
// wire. The client no longer picks the encoding: it sends a `wheel` control
// message and the daemon encodes it for the pane's modes, which is also the
// only form a viewer without the size lock may send. The encodings
// themselves are covered by `tmux::mouse`.
test.use({ ...devices["iPhone 13"] });

async function openSession(page: Page, handle: MockHandle) {
  await openMobileSidebar(page);
  await clickSidebarSession(page, "pinch-test");
  await page.locator("[data-live-terminal]").waitFor({ state: "visible", timeout: 10_000 });
  await handle.waitForLiveReady();
}

async function pushFrame(handle: MockHandle, flags: { altScreen: boolean; mouse: boolean; mouseSgr: boolean }) {
  await handle.pushLiveFrame({
    ...makeLiveFrame({ rows: 24, history: 120, window: 24 }),
    ...flags,
  } as Parameters<MockHandle["pushLiveFrame"]>[0]);
}

const scroller = (page: Page) => page.locator("[data-live-terminal] > div").first();
const texts = (h: MockHandle) => h.liveMessages.map((b) => b.toString("latin1"));
const wheels = (h: MockHandle) =>
  texts(h)
    .filter((s) => s.startsWith("{"))
    .map((s) => {
      try {
        return JSON.parse(s) as { type?: string; up?: boolean; count?: number };
      } catch {
        return {};
      }
    })
    .filter((m) => m.type === "wheel");
/// Any raw mouse report, SGR or legacy X10. None may be sent for a wheel.
const hasMouseBytes = (h: MockHandle) =>
  texts(h).some((s) => s.includes("\x1b[<")) ||
  h.liveMessages.some((b) => b.length >= 3 && b[0] === 0x1b && b[1] === 0x5b && b[2] === 0x4d);

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

test("swipe over a full-screen mouse app forwards the wheel to the daemon", async ({ page }) => {
  const handle = await setup(page);
  await pushFrame(handle, { altScreen: true, mouse: true, mouseSgr: true });
  await expect.poll(() => scroller(page).getAttribute("class")).toContain("overflow-hidden");
  // touch-action: none is what keeps the drag from panning the whole page:
  // React's delegated touch listeners are passive, so the component cannot
  // preventDefault the native pan (the keyboard-open page-scroll clunk).
  await expect.poll(() => scroller(page).evaluate((el) => getComputedStyle(el).touchAction)).toBe("none");
  // A direct, non-passive listener backs this up if WebKit decided the
  // gesture's touch-action before the frame switched into forward mode.
  await expect
    .poll(() =>
      scroller(page).evaluate((el) => {
        const move = new Event("touchmove", { cancelable: true });
        return el.dispatchEvent(move);
      }),
    )
    .toBe(false);
  await swipeUp(page);
  await expect.poll(() => wheels(handle).some((m) => m.up === false)).toBe(true);

  // Downward swipe forwards wheel UP.
  await fireTouches(page, "touchstart", [{ x: 100, y: 120 }]);
  await fireTouches(page, "touchmove", [{ x: 100, y: 300 }]);
  await fireTouches(page, "touchend", [{ x: 100, y: 300 }]);
  await expect.poll(() => wheels(handle).some((m) => m.up === true)).toBe(true);
  // Never as raw input, which the daemon drops for a non-owner viewer.
  expect(hasMouseBytes(handle)).toBe(false);

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

test("a flick coasts: wheel messages keep arriving after the finger lifts, and a touch stops it", async ({ page }) => {
  const handle = await setup(page);
  await pushFrame(handle, { altScreen: true, mouse: true, mouseSgr: true });
  await expect.poll(() => scroller(page).getAttribute("class")).toContain("overflow-hidden");
  // Fast multi-move swipe. Synthetic touchmoves land with ~1ms deltas, so the
  // raw release velocity is absurd; the component's velocity cap is what makes
  // this coast at the same bounded rate a real flick would (the AGENTS.md
  // synthetic-touch gotcha).
  await fireTouches(page, "touchstart", [{ x: 100, y: 300 }]);
  for (const y of [280, 260, 240, 220]) {
    await fireTouches(page, "touchmove", [{ x: 100, y }]);
  }
  await fireTouches(page, "touchend", [{ x: 100, y: 220 }]);
  // Let the drag's own messages drain, then require NEW ones with no input at
  // all: only the momentum loop can be producing them.
  await page.waitForTimeout(150);
  const atLift = handle.liveMessages.length;
  await expect.poll(() => handle.liveMessages.length, { timeout: 3_000 }).toBeGreaterThan(atLift);
  // A touch lands mid-coast: the coast must stop (a tap forwards no wheel).
  await fireTouches(page, "touchstart", [{ x: 100, y: 200 }]);
  await fireTouches(page, "touchend", [{ x: 100, y: 200 }]);
  await page.waitForTimeout(150);
  const afterStop = handle.liveMessages.length;
  await page.waitForTimeout(500);
  expect(handle.liveMessages.length).toBe(afterStop);
});

test("a legacy-mouse app forwards the same wheel message, not X10 bytes", async ({ page }) => {
  const handle = await setup(page);
  // The app's encoding is the daemon's business now, so an X10-only app must
  // still receive the message an SGR app does. Buttons are unaffected and keep
  // choosing an encoding client-side; see `live-terminal-input.spec.ts`.
  await pushFrame(handle, { altScreen: true, mouse: true, mouseSgr: false });
  await swipeUp(page);
  await expect.poll(() => wheels(handle).some((m) => m.up === false)).toBe(true);
  expect(hasMouseBytes(handle)).toBe(false);
});

test("normal-screen agent does NOT forward the wheel", async ({ page }) => {
  const handle = await setup(page);
  await pushFrame(handle, { altScreen: false, mouse: true, mouseSgr: true });
  await expect.poll(() => scroller(page).getAttribute("class")).toContain("overflow-y-auto");
  await swipeUp(page);
  await page.waitForTimeout(300);
  expect(wheels(handle)).toEqual([]);
  expect(hasMouseBytes(handle)).toBe(false);
});
