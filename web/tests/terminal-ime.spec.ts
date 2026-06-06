import { test, expect } from "./helpers/mockedTest";
import type { Page } from "@playwright/test";
import { mockTerminalApis, type MockHandle } from "./helpers/terminal-mocks";
import { clickSidebarSession } from "./helpers/sidebar";

test.use({ viewport: { width: 1280, height: 800 }, hasTouch: false });

async function openSession(page: Page, handle: MockHandle) {
  await clickSidebarSession(page, "pinch-test");
  await page
    .locator(".xterm")
    .first()
    .waitFor({ state: "visible", timeout: 10_000 });
  await expect
    .poll(() => handle.wsMessages.length, { timeout: 5_000 })
    .toBeGreaterThan(0);
}

function sentText(handle: MockHandle, start: number) {
  return handle.wsMessages.slice(start).map((msg) => msg.toString("utf8"));
}

test.describe("Terminal IME input", () => {
  test("plain printable keys still send text", async ({ page }) => {
    const handle = await mockTerminalApis(page);
    await page.goto("/");
    await openSession(page, handle);

    const start = handle.wsMessages.length;
    await page.locator(".xterm").first().locator("textarea").focus();
    await page.keyboard.type("a");

    await expect
      .poll(() => sentText(handle, start), { timeout: 5_000 })
      .toContain("a");
  });

  test("macOS Chinese composition sends only the committed text", async ({
    page,
  }) => {
    const handle = await mockTerminalApis(page);
    await page.goto("/");
    await openSession(page, handle);

    const start = handle.wsMessages.length;
    await page.evaluate(() => {
      const ta = document.querySelector<HTMLTextAreaElement>(".xterm textarea");
      if (!ta) throw new Error("xterm textarea not found");
      ta.focus();

      ta.dispatchEvent(
        new KeyboardEvent("keydown", {
          key: "n",
          code: "KeyN",
          bubbles: true,
          cancelable: true,
        }),
      );
      ta.dispatchEvent(
        new CompositionEvent("compositionstart", {
          data: "",
          bubbles: true,
          cancelable: true,
        }),
      );
      ta.dispatchEvent(
        new CompositionEvent("compositionupdate", {
          data: "n",
          bubbles: true,
          cancelable: true,
        }),
      );
      ta.dispatchEvent(
        new CompositionEvent("compositionend", {
          data: "你好",
          bubbles: true,
          cancelable: true,
        }),
      );
      // Real browsers populate the textarea value at compositionend and
      // fire an InputEvent with inputType="insertCompositionText" that
      // carries the committed text. xterm.js reads onData from that
      // event, not from compositionend.data alone, so the synthetic
      // sequence needs both.
      ta.value = "你好";
      ta.dispatchEvent(
        new InputEvent("input", {
          data: "你好",
          inputType: "insertCompositionText",
          bubbles: true,
        }),
      );
    });

    await expect
      .poll(() => sentText(handle, start), { timeout: 5_000 })
      .toContain("你好");
    expect(sentText(handle, start)).not.toContain("n");
  });

  test("Shift+Enter sends bracketed paste with newline instead of bare CR", async ({
    page,
  }) => {
    const handle = await mockTerminalApis(page);
    await page.goto("/");
    await openSession(page, handle);

    const start = handle.wsMessages.length;
    await page.locator(".xterm").first().locator("textarea").focus();
    await page.keyboard.down("Shift");
    await page.keyboard.press("Enter");
    await page.keyboard.up("Shift");

    await expect
      .poll(() => sentText(handle, start), { timeout: 5_000 })
      .toContain("\x1b[200~\n\x1b[201~");
    // xterm.js fires both keydown and keypress for Enter; the handler must
    // suppress both, otherwise a stray bare CR leaks through and submits.
    expect(sentText(handle, start)).not.toContain("\r");
  });
});
