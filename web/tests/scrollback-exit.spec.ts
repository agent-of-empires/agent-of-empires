import { test, expect } from "./helpers/mockedTest";
import { devices, type Page } from "@playwright/test";
import { clickSidebarSession, openMobileSidebar } from "./helpers/sidebar";
import { mockTerminalApis, installTerminalSpies, seedSettings, type MockHandle } from "./helpers/terminal-mocks";

// Mobile scrollback on the capture-snapshot live view. Scrolling is the
// browser's NATIVE scroll over rendered history lines (no tmux copy-mode,
// no SGR wheel synthesis, no pause/resume SIGSTOP): the spec asserts the
// live-view contract instead of the old copy-mode one.
test.use({ ...devices["iPhone 13"] });

async function openSession(page: Page, handle: MockHandle) {
  await openMobileSidebar(page);
  await clickSidebarSession(page, "pinch-test");
  await page.locator("[data-live-terminal]").waitFor({ state: "visible", timeout: 10_000 });
  await expect.poll(() => handle.liveMessages.length, { timeout: 5_000 }).toBeGreaterThan(0);
  // Let the first frame land + the sizing effect settle.
  await page.waitForTimeout(400);
}

function scroller(page: Page) {
  return page.locator("[data-live-terminal] > div").first();
}

function textMessages(handle: MockHandle): string[] {
  return handle.liveMessages.map((m) => m.toString("utf8"));
}

test.describe("Mobile live-view scrollback", () => {
  test("scrolling up shows Back to live; tapping it returns to the bottom", async ({ page }) => {
    await installTerminalSpies(page);
    const handle = await mockTerminalApis(page);
    await page.goto("/");
    await seedSettings(page, { mobileFontSize: 14 });
    await page.reload();
    await openSession(page, handle);

    await expect(page.getByRole("button", { name: "Back to live" })).toHaveCount(0);

    await scroller(page).evaluate((el) => {
      el.scrollTop = 0;
    });
    const btn = page.getByRole("button", { name: "Back to live" });
    await expect(btn).toBeVisible();

    // History content rendered as real DOM text.
    await expect.poll(() => page.locator("[data-live-content]").innerText()).toContain("history line");

    await btn.tap();
    await expect(btn).toHaveCount(0);
    const distance = await scroller(page).evaluate((el) => el.scrollHeight - el.scrollTop - el.clientHeight);
    expect(distance).toBeLessThan(30);
  });

  test("scrolling requests a bigger capture window instead of wheel escapes", async ({ page }) => {
    await installTerminalSpies(page);
    const handle = await mockTerminalApis(page);
    await page.goto("/");
    await seedSettings(page, { mobileFontSize: 14 });
    await page.reload();
    await openSession(page, handle);

    const before = textMessages(handle).filter((m) => m.includes('"type":"window"')).length;
    await scroller(page).evaluate((el) => {
      el.scrollTop = 0;
    });
    await expect
      .poll(() => textMessages(handle).filter((m) => m.includes('"type":"window"')).length, { timeout: 3_000 })
      .toBeGreaterThan(before);

    // The copy-mode machinery must stay retired on mobile: no SGR wheel
    // bytes, no pause/resume control messages, ever.
    const all = textMessages(handle).join("");
    expect(all).not.toContain("\x1b[<64;");
    expect(all).not.toContain("\x1b[<65;");
    expect(all).not.toContain("pause_output");
    expect(all).not.toContain("resume_output");
  });

  test("reading freezes the stream via hold; returning releases it", async ({ page }) => {
    await installTerminalSpies(page);
    const handle = await mockTerminalApis(page);
    await page.goto("/");
    await seedSettings(page, { mobileFontSize: 14 });
    await page.reload();
    await openSession(page, handle);

    // Scrolling up requests the full history; once the covering frame
    // arrives the client holds the server's pushes (zero bandwidth, a
    // perfectly still reading surface, agent untouched).
    await scroller(page).evaluate((el) => {
      el.scrollTop = 0;
    });
    await expect
      .poll(() => textMessages(handle).filter((m) => m.includes('"hold":true')).length, { timeout: 3_000 })
      .toBeGreaterThan(0);

    // Returning to live releases the hold so a fresh frame repaints.
    await page.getByRole("button", { name: "Back to live" }).tap();
    await expect
      .poll(() => {
        const msgs = textMessages(handle).filter((m) => m.includes('"type":"hold"'));
        return msgs[msgs.length - 1] ?? "";
      })
      .toContain('"hold":false');
  });
});
