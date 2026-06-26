import { test, expect } from "./helpers/mockedTest";
import { devices, type Page } from "@playwright/test";
import { clickSidebarSession, openMobileSidebar } from "./helpers/sidebar";
import {
  mockTerminalApis,
  installTerminalSpies,
  seedSettings,
  makeLiveFrame,
  type MockHandle,
} from "./helpers/terminal-mocks";

// A full-screen (alternate-screen) mouse agent receives forwarded mouse
// BUTTON events (press / drag / release), not just the wheel. This drives
// the real bundle (pointer handlers -> buttonMouseBytes -> WebSocket) in a
// real browser, where pointerCell maps to actual measured cells (jsdom can't,
// so the byte assertions for the drag path live here, not in Vitest). The
// SGR encodings themselves are also unit-tested in src/lib/__tests__.
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
const anyMatch = (h: MockHandle, re: RegExp) => texts(h).some((s) => re.test(s));

async function setup(page: Page) {
  await installTerminalSpies(page);
  const handle = await mockTerminalApis(page);
  await page.goto("/");
  await seedSettings(page, { mobileFontSize: 14 });
  await page.reload();
  await openSession(page, handle);
  return handle;
}

async function pointer(page: Page, type: string, x: number, y: number, init: Record<string, unknown> = {}) {
  await scroller(page).dispatchEvent(type, { pointerType: "mouse", button: 0, clientX: x, clientY: y, ...init });
}

test("a left click on a full-screen SGR-mouse app forwards press + release", async ({ page }) => {
  const handle = await setup(page);
  pushFrame(handle, { altScreen: true, mouse: true, mouseSgr: true });
  await expect.poll(() => scroller(page).getAttribute("class")).toContain("overflow-hidden");
  const box = (await scroller(page).boundingBox())!;
  await pointer(page, "pointerdown", box.x + 30, box.y + 20);
  await pointer(page, "pointerup", box.x + 30, box.y + 20);
  // Press: SGR left button (0), `M`. Release: same button, lowercase `m`.
  await expect.poll(() => anyMatch(handle, /\x1b\[<0;\d+;\d+M/)).toBe(true);
  await expect.poll(() => anyMatch(handle, /\x1b\[<0;\d+;\d+m/)).toBe(true);
});

test("dragging forwards a motion report (button + 32) per new cell", async ({ page }) => {
  const handle = await setup(page);
  pushFrame(handle, { altScreen: true, mouse: true, mouseSgr: true });
  await expect.poll(() => scroller(page).getAttribute("class")).toContain("overflow-hidden");
  const box = (await scroller(page).boundingBox())!;
  await pointer(page, "pointerdown", box.x + 20, box.y + 20);
  await pointer(page, "pointermove", box.x + 160, box.y + 20); // far enough to cross cells
  await pointer(page, "pointerup", box.x + 160, box.y + 20);
  // Motion (drag) rides at +32 in SGR; the press and release bracket it.
  await expect.poll(() => anyMatch(handle, /\x1b\[<32;\d+;\d+M/)).toBe(true);
  await expect.poll(() => anyMatch(handle, /\x1b\[<0;\d+;\d+m/)).toBe(true);
});

test("a legacy-mouse app forwards X10 button bytes (ESC [ M), not SGR", async ({ page }) => {
  const handle = await setup(page);
  pushFrame(handle, { altScreen: true, mouse: true, mouseSgr: false });
  await expect.poll(() => scroller(page).getAttribute("class")).toContain("overflow-hidden");
  const box = (await scroller(page).boundingBox())!;
  await pointer(page, "pointerdown", box.x + 30, box.y + 20);
  await pointer(page, "pointerup", box.x + 30, box.y + 20);
  // X10: `ESC [ M` + bytes; the left-press button byte is 0 + 32 = 0x20.
  await expect.poll(() => texts(handle).some((s) => s.startsWith("\x1b[M"))).toBe(true);
  expect(anyMatch(handle, /\x1b\[</)).toBe(false);
});

test("Shift+click is NOT forwarded (keeps local text selection)", async ({ page }) => {
  const handle = await setup(page);
  pushFrame(handle, { altScreen: true, mouse: true, mouseSgr: true });
  await expect.poll(() => scroller(page).getAttribute("class")).toContain("overflow-hidden");
  const box = (await scroller(page).boundingBox())!;
  await pointer(page, "pointerdown", box.x + 30, box.y + 20, { shiftKey: true });
  await page.waitForTimeout(200);
  expect(anyMatch(handle, /\x1b\[</)).toBe(false);
});

test("a normal-screen agent does NOT forward a click", async ({ page }) => {
  const handle = await setup(page);
  pushFrame(handle, { altScreen: false, mouse: true, mouseSgr: true });
  await expect.poll(() => scroller(page).getAttribute("class")).toContain("overflow-y-auto");
  const box = (await scroller(page).boundingBox())!;
  await pointer(page, "pointerdown", box.x + 30, box.y + 20);
  await pointer(page, "pointerup", box.x + 30, box.y + 20);
  await page.waitForTimeout(200);
  expect(anyMatch(handle, /\x1b\[</) || texts(handle).some((s) => s.startsWith("\x1b[M"))).toBe(false);
});
