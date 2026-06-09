import { test, expect } from "./helpers/mockedTest";
import { devices, type Page } from "@playwright/test";
import { clickSidebarSession, openMobileSidebar } from "./helpers/sidebar";
import {
  mockTerminalApis,
  installTerminalSpies,
  seedSettings,
  fireTouches,
  type MockHandle,
} from "./helpers/terminal-mocks";

// Mobile viewport. The scroll-snap-to-live fix is mobile-only: the
// "Back to live" button is rendered only on mobile, and the wheel-down
// clamp in useTerminal only runs when isMobileViewport() is true.
// Desktop keeps tmux's default copy-mode-with-`-e` behavior untouched.
test.use({ ...devices["iPhone 13"] });

// SGR mouse wheel sequences are `\x1b[<64;<col>;<row>M` (up) and
// `\x1b[<65;...M` (down). Coordinates now track the pointer, so match
// by the button-code prefix and count one hit per emitted event.
const WHEEL_UP_SEQ = "\x1b[<64;";
const WHEEL_DOWN_SEQ = "\x1b[<65;";
const ESC = "\x1b";

function countSeq(handle: MockHandle, seq: string): number {
  const needle = Buffer.from(seq);
  let count = 0;
  for (const msg of handle.wsMessages) {
    let idx = 0;
    while ((idx = msg.indexOf(needle, idx)) !== -1) {
      count++;
      idx += needle.length;
    }
  }
  return count;
}

async function openSession(page: Page) {
  await openMobileSidebar(page);
  await clickSidebarSession(page, "pinch-test");
  await page.locator(".xterm").waitFor({ state: "visible", timeout: 10_000 });
}

async function swipeUp(page: Page, travel: number) {
  // Single-finger vertical swipe. A ~300px travel over ~15 frames emits
  // well above the per-gesture wheel threshold.
  const cx = 160;
  let cy = 500;
  await fireTouches(page, "touchstart", [{ x: cx, y: cy }]);
  const steps = 15;
  for (let i = 1; i <= steps; i++) {
    cy = 500 - (i * travel) / steps;
    await fireTouches(page, "touchmove", [{ x: cx, y: cy }]);
  }
  await fireTouches(page, "touchend", []);
}

async function swipeDown(page: Page, travel: number) {
  const cx = 160;
  let cy = 100;
  await fireTouches(page, "touchstart", [{ x: cx, y: cy }]);
  const steps = 15;
  for (let i = 1; i <= steps; i++) {
    cy = 100 + (i * travel) / steps;
    await fireTouches(page, "touchmove", [{ x: cx, y: cy }]);
  }
  await fireTouches(page, "touchend", []);
}

function hasText(handle: MockHandle, needle: string): boolean {
  const buf = Buffer.from(needle);
  return handle.wsMessages.some((m) => m.includes(buf));
}

test.describe("Mobile scrollback exit", () => {
  test("button appears after swipe-up and sends Escape on tap", async ({ page }) => {
    await installTerminalSpies(page);
    const handle = await mockTerminalApis(page);
    await page.goto("/");
    await seedSettings(page, { mobileFontSize: 14 });
    await page.reload();
    await openSession(page);

    await expect(page.getByRole("button", { name: "Back to live" })).toHaveCount(0);

    await swipeUp(page, 300);
    await expect.poll(() => countSeq(handle, WHEEL_UP_SEQ), { timeout: 2_000 }).toBeGreaterThan(0);

    const btn = page.getByRole("button", { name: "Back to live" });
    await expect(btn).toBeVisible();

    const before = handle.wsMessages.length;
    await btn.click();
    await expect(btn).toHaveCount(0);

    await expect.poll(() => handle.wsMessages.length, { timeout: 2_000 }).toBeGreaterThan(before);
    const newMsgs = handle.wsMessages.slice(before);
    const sawEsc = newMsgs.some((m) => m.includes(Buffer.from(ESC)));
    expect(sawEsc).toBe(true);
  });

  test("entering scrollback sends pause_output, exiting sends resume_output", async ({ page }) => {
    await installTerminalSpies(page);
    const handle = await mockTerminalApis(page);
    await page.goto("/");
    await seedSettings(page, { mobileFontSize: 14 });
    await page.reload();
    await openSession(page);

    // No pause sent yet.
    expect(hasText(handle, '"type":"pause_output"')).toBe(false);

    await swipeUp(page, 300);
    await expect.poll(() => hasText(handle, '"type":"pause_output"'), { timeout: 2_000 }).toBe(true);
    // Still no resume until the user exits.
    expect(hasText(handle, '"type":"resume_output"')).toBe(false);

    await page.getByRole("button", { name: "Back to live" }).click();
    await expect.poll(() => hasText(handle, '"type":"resume_output"'), { timeout: 2_000 }).toBe(true);
  });

  test("button stays hidden after tap even with in-flight momentum", async ({ page }) => {
    // Regression: a fast swipe pegs momentum velocity at MAX_VELOCITY,
    // and the requestAnimationFrame decay keeps emitting wheel-ups for
    // hundreds of ms after touchend. If exitScrollback doesn't cancel
    // that momentum, the next decay frame calls sendWheel("up") and
    // re-flips isInScrollback: true, so the button reappears mid-poll.
    await installTerminalSpies(page);
    const handle = await mockTerminalApis(page);
    await page.goto("/");
    await seedSettings(page, { mobileFontSize: 14 });
    await page.reload();
    await openSession(page);

    // Aggressive swipe: large per-step dy (75px) saturates velocity at
    // MAX_VELOCITY=2.0 px/ms even when synthetic touchmoves arrive tens
    // of ms apart (Playwright IPC roundtrip). Without saturation, slow
    // local environments wouldn't generate enough momentum to repro.
    const cx = 160;
    let cy = 600;
    await fireTouches(page, "touchstart", [{ x: cx, y: cy }]);
    const steps = 8;
    const travel = 600;
    for (let i = 1; i <= steps; i++) {
      cy = 600 - (i * travel) / steps;
      await fireTouches(page, "touchmove", [{ x: cx, y: cy }]);
    }
    await fireTouches(page, "touchend", []);

    const btn = page.getByRole("button", { name: "Back to live" });
    await expect(btn).toBeVisible();

    // Click immediately, while momentum is still decaying.
    await btn.click();
    const upsAtClick = countSeq(handle, WHEEL_UP_SEQ);

    // Button must stay gone across the full momentum window (~700ms +
    // slack). toHaveCount with timeout would pass on the first frame; we
    // need to verify it stays at 0 across multiple animation frames.
    for (let i = 0; i < 10; i++) {
      await page.waitForTimeout(100);
      await expect(btn).toHaveCount(0);
    }

    // And no additional wheel-ups should have been emitted after exit
    // (would prove momentum kept running and would re-enter scrollback).
    expect(countSeq(handle, WHEEL_UP_SEQ)).toBe(upsAtClick);
  });

  test("scroll-down clamp: fewer wheel-downs sent than wheel-ups", async ({ page }) => {
    await installTerminalSpies(page);
    const handle = await mockTerminalApis(page);
    await page.goto("/");
    await seedSettings(page, { mobileFontSize: 14 });
    await page.reload();
    await openSession(page);

    await swipeUp(page, 300);
    await expect.poll(() => countSeq(handle, WHEEL_UP_SEQ), { timeout: 2_000 }).toBeGreaterThan(0);
    const ups = countSeq(handle, WHEEL_UP_SEQ);

    // Now swipe down harder — more travel than the up gesture. The
    // clamp should cut off wheel-DOWN emissions before depth hits 0,
    // so the total down count stays strictly less than the up count.
    await swipeDown(page, 600);
    await page.waitForTimeout(200);
    const downs = countSeq(handle, WHEEL_DOWN_SEQ);

    expect(downs).toBeGreaterThan(0);
    expect(downs).toBeLessThan(ups);
  });
});
