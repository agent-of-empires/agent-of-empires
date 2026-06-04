import { test, expect } from "./helpers/mockedTest";
import type { Page } from "@playwright/test";
import { clickSidebarSession } from "./helpers/sidebar";
import {
  mockTerminalApis,
  installTerminalSpies,
  seedSettings,
  type MockHandle,
} from "./helpers/terminal-mocks";

// Desktop viewport: exercises the mouse-wheel → SGR scroll path that only
// exists for non-touch (pointer: fine) users.
test.use({ viewport: { width: 1280, height: 800 }, hasTouch: false });

// SGR mouse-wheel escape sequences as raw bytes:
//   wheel up:   ESC [ < 64 ; 1 ; 1 M
//   wheel down: ESC [ < 65 ; 1 ; 1 M
const WHEEL_UP_SEQ = "\x1b[<64;1;1M";
const WHEEL_DOWN_SEQ = "\x1b[<65;1;1M";

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

test.describe("Terminal mouse-wheel scroll (desktop)", () => {
  async function openSession(page: Page, handle: MockHandle) {
    await clickSidebarSession(page, "pinch-test");
    await page
      .locator(".xterm")
      .first()
      .waitFor({ state: "visible", timeout: 10_000 });
    // Wait for the WebSocket to deliver at least one message (the app sends
    // resize/activate on connect). Until readyState is OPEN, sendWheel
    // silently drops messages.
    await expect
      .poll(() => handle.wsMessages.length, { timeout: 10_000 })
      .toBeGreaterThan(0);
  }

  async function fireWheel(
    page: Page,
    opts: { deltaY: number; ctrlKey?: boolean; times?: number },
  ) {
    await page.evaluate(
      ({ deltaY, ctrlKey, times }) => {
        const target = document.querySelector<HTMLElement>(".xterm");
        if (!target) throw new Error(".xterm not mounted");
        for (let i = 0; i < (times ?? 1); i++) {
          target.dispatchEvent(
            new WheelEvent("wheel", {
              bubbles: true,
              cancelable: true,
              deltaY,
              ctrlKey: ctrlKey ?? false,
            }),
          );
        }
      },
      opts,
    );
  }

  // Re-fire the wheel burst on every poll tick until the expected side
  // effect lands. A single synthetic burst can arrive in a transient
  // not-ready window (renderer init / socket-open timing under heavy CI
  // parallelism), where the custom wheel handler never forwards it and the
  // SGR bytes are silently dropped. Polling only the observation can't
  // recover from a dropped burst, so it would wait out the full timeout and
  // fail with "Received: 0". Re-firing recovers as soon as the handler is
  // live. Only sound for monotonic effects: the wheel direction is fixed, so
  // extra bursts can only push the observed value further past the threshold,
  // never back across it.
  async function fireUntil(
    page: Page,
    opts: { deltaY: number; ctrlKey?: boolean; times?: number },
    predicate: () => boolean | Promise<boolean>,
    timeout = 10_000,
  ) {
    await expect
      .poll(
        async () => {
          await fireWheel(page, opts);
          return await predicate();
        },
        { timeout },
      )
      .toBe(true);
  }

  test("scroll down sends SGR wheel-down escape sequences", async ({
    page,
  }) => {
    await installTerminalSpies(page);
    const handle = await mockTerminalApis(page);
    await page.goto("/");
    await seedSettings(page, { desktopFontSize: 14 });
    await page.reload();
    await openSession(page, handle);
    handle.wsMessages.length = 0;

    // deltaY > 0 = scroll down. Fire enough events to exceed pxPerWheel
    // threshold (fontSize 14 * LINES_PER_WHEEL 2 = 28px per wheel tick).
    // deltaY=120 is a typical single mouse wheel notch on most browsers.
    await fireUntil(
      page,
      { deltaY: 120, times: 3 },
      () => countSeq(handle, WHEEL_DOWN_SEQ) > 0,
    );
    expect(countSeq(handle, WHEEL_UP_SEQ)).toBe(0);
  });

  test("scroll up sends SGR wheel-up escape sequences", async ({ page }) => {
    await installTerminalSpies(page);
    const handle = await mockTerminalApis(page);
    await page.goto("/");
    await seedSettings(page, { desktopFontSize: 14 });
    await page.reload();
    await openSession(page, handle);
    handle.wsMessages.length = 0;

    // deltaY < 0 = scroll up
    await fireUntil(
      page,
      { deltaY: -120, times: 3 },
      () => countSeq(handle, WHEEL_UP_SEQ) > 0,
    );
    expect(countSeq(handle, WHEEL_DOWN_SEQ)).toBe(0);
  });

  test("scroll does not change font size", async ({ page }) => {
    await installTerminalSpies(page);
    const handle = await mockTerminalApis(page);
    await page.goto("/");
    await seedSettings(page, { desktopFontSize: 14 });
    await page.reload();
    await openSession(page, handle);

    await page.evaluate(() => {
      (window as unknown as { __LS_WRITES__: string[] }).__LS_WRITES__ = [];
    });

    await fireWheel(page, { deltaY: -120, times: 5 });

    // Wait longer than the 400ms debounce to confirm no font size change leaked
    await page.waitForTimeout(500);
    const writes = await page.evaluate(() =>
      (window as unknown as { __LS_WRITES__: string[] }).__LS_WRITES__.filter(
        (w) => w.includes("desktopFontSize"),
      ),
    );
    expect(writes).toEqual([]);

    const fontSize = await page.evaluate(() => {
      const raw = localStorage.getItem("aoe-web-settings");
      return raw ? JSON.parse(raw).desktopFontSize : null;
    });
    expect(fontSize).toBe(14);
  });

  test("Ctrl+wheel still zooms (not scroll)", async ({ page }) => {
    await installTerminalSpies(page);
    const handle = await mockTerminalApis(page);
    await page.goto("/");
    await seedSettings(page, { desktopFontSize: 14 });
    await page.reload();
    await openSession(page, handle);
    handle.wsMessages.length = 0;

    // Ctrl+wheel should zoom, not scroll. Re-fire until the zoom lands:
    // extra ctrl+wheel bursts only zoom further in, never back, and never
    // emit scroll sequences, so the scroll-count assertion below still holds.
    await fireUntil(
      page,
      { deltaY: -60, ctrlKey: true, times: 2 },
      async () => {
        const size = await page.evaluate(() => {
          const raw = localStorage.getItem("aoe-web-settings");
          return raw ? JSON.parse(raw).desktopFontSize : null;
        });
        return typeof size === "number" && size > 14;
      },
    );

    // No SGR scroll sequences should have been sent
    const scrollCount =
      countSeq(handle, WHEEL_UP_SEQ) + countSeq(handle, WHEEL_DOWN_SEQ);
    expect(scrollCount).toBe(0);
  });

  // Pause/resume roundtrip on desktop: scrolling up into scrollback
  // sends pause_output so claude stops emitting output that would
  // shift scrollback; scrolling back down past the starting depth
  // sends resume_output (tmux's -e flag has by then auto-exited
  // copy-mode). No "Back to live" button is rendered on desktop.
  test("desktop: wheel-up sends pause_output, wheel-down back to live sends resume_output", async ({
    page,
  }) => {
    await installTerminalSpies(page);
    const handle = await mockTerminalApis(page);
    await page.goto("/");
    await seedSettings(page, { desktopFontSize: 14 });
    await page.reload();
    await openSession(page, handle);

    // No button should ever appear on desktop.
    await expect(
      page.getByRole("button", { name: "Back to live" }),
    ).toHaveCount(0);

    const hasText = (needle: string) =>
      handle.wsMessages.some((m) => m.includes(Buffer.from(needle)));

    // Wheel-up enters scrollback (pause_output). Re-firing up only deepens
    // scrollback, so the pause transition is monotonic and re-fire-safe.
    await fireUntil(
      page,
      { deltaY: -120, times: 3 },
      () => hasText('"type":"pause_output"'),
    );
    expect(hasText('"type":"resume_output"')).toBe(false);

    // Scroll back down to zero the depth; on desktop tmux auto-exits
    // copy-mode and the client emits resume_output. Re-fire down until that
    // lands: each burst drives depth toward 0 and stops at 0, so re-firing
    // can only reach (never overshoot past) the resume transition.
    await fireUntil(
      page,
      { deltaY: 120, times: 5 },
      () => hasText('"type":"resume_output"'),
    );

    // Still no button on desktop at any point.
    await expect(
      page.getByRole("button", { name: "Back to live" }),
    ).toHaveCount(0);
  });
});
