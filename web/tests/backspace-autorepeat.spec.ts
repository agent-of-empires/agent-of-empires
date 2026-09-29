import { test, expect, observeFor } from "./helpers/mockedTest";
import { devices, type Page } from "@playwright/test";
import { mockTerminalApis, type MockHandle } from "./helpers/terminal-mocks";
import { clickSidebarSession, openMobileSidebar } from "./helpers/sidebar";
import { HIDDEN_INPUT_SENTINEL as S } from "../src/lib/hiddenInputDiff";

// A held soft-keyboard Backspace arrives as repeated native deletes, escalating to word deletes on iOS. iOS only
// repeats while the field has text, so the hidden input keeps an unsent sentinel and sends each deletion as DELs.

const INPUT = 'textarea[aria-label="Live terminal input"]';

function textBytes(handle: MockHandle, start: number) {
  return handle.liveMessages
    .slice(start)
    .map((msg) => msg.toString("utf8"))
    .filter((s) => !s.startsWith("{"))
    .join("");
}

const delCount = (handle: MockHandle, start: number) => textBytes(handle, start).split("\x7f").length - 1;

/** Soft-keyboard edits as iOS applies them: beforeinput, the value change, input. Nothing fires on an empty field. */
async function softEdits(page: Page, edits: { inputType: string; data?: string }[], isComposing = false) {
  return page.evaluate(
    ({ selector, edits, isComposing }) => {
      const ta = document.querySelector<HTMLTextAreaElement>(selector);
      if (!ta) throw new Error("live terminal input not found");
      ta.focus();
      let applied = 0;
      for (const { inputType, data } of edits) {
        if (inputType.startsWith("delete") && ta.value === "") break;
        const init = { inputType, data: data ?? null, isComposing, bubbles: true };
        if (!ta.dispatchEvent(new InputEvent("beforeinput", { ...init, cancelable: true }))) continue;
        const end = ta.value.length;
        if (inputType === "insertText") ta.setRangeText(data ?? "", end, end, "end");
        else if (inputType === "deleteWordBackward") {
          const word = /\S*\s*$/.exec(ta.value)?.[0].length || 1;
          ta.setRangeText("", end - word, end, "end");
        } else ta.setRangeText("", end - 1, end, "end");
        ta.dispatchEvent(new InputEvent("input", init));
        applied++;
      }
      return applied;
    },
    { selector: INPUT, edits, isComposing },
  );
}

const backspaces = (n: number) => Array.from({ length: n }, () => ({ inputType: "deleteContentBackward" }));

// Only the iPhone viewport, touch, and UA; the project already pins chromium.
const { defaultBrowserType: _iphoneBrowser, ...iPhone13 } = devices["iPhone 13"];

test.describe("Mobile soft-keyboard Backspace autorepeat", () => {
  test.use(iPhone13);

  async function openSession(page: Page, handle: MockHandle) {
    await page.goto("/");
    await openMobileSidebar(page);
    await clickSidebarSession(page, "pinch-test");
    await page.locator("[data-live-terminal]").waitFor({ state: "visible", timeout: 10_000 });
    await expect.poll(() => handle.liveMessages.length, { timeout: 5_000 }).toBeGreaterThan(0);
  }

  test("a held Backspace keeps deleting past the typed text and word deletes send one DEL per character", async ({
    page,
  }) => {
    const handle = await mockTerminalApis(page);
    await openSession(page, handle);

    const start = handle.liveMessages.length;
    // One tap sends one DEL, never two.
    expect(await softEdits(page, backspaces(1))).toBe(1);
    await expect.poll(() => delCount(handle, start), { timeout: 5_000 }).toBe(1);
    await observeFor(page, 200, async () => {
      expect(delCount(handle, start)).toBe(1);
    });

    // Every repeat tick fires, because the field is never empty.
    expect(await softEdits(page, backspaces(10))).toBe(10);
    await expect.poll(() => delCount(handle, start), { timeout: 5_000 }).toBe(11);
    await expect(page.locator(INPUT)).toHaveValue(S);

    const typed = handle.liveMessages.length;
    await softEdits(
      page,
      [..."git status"].map((data) => ({ inputType: "insertText", data })),
    );
    await softEdits(page, [{ inputType: "deleteWordBackward" }, { inputType: "deleteWordBackward" }]);
    await expect.poll(() => textBytes(handle, typed), { timeout: 5_000 }).toBe("git status" + "\x7f".repeat(10));
  });

  test("a hardware Backspace deletes natively and sends one DEL per keystroke", async ({ page }) => {
    const handle = await mockTerminalApis(page);
    await openSession(page, handle);
    await page.locator(INPUT).focus();

    const start = handle.liveMessages.length;
    await page.keyboard.type("ab");
    for (let i = 0; i < 5; i++) await page.keyboard.press("Backspace");
    await expect.poll(() => textBytes(handle, start), { timeout: 5_000 }).toBe("ab" + "\x7f".repeat(5));
    await expect(page.locator(INPUT)).toHaveValue(S);
  });

  test("deletes inside an IME composition are left to the IME", async ({ page }) => {
    const handle = await mockTerminalApis(page);
    await openSession(page, handle);

    const start = handle.liveMessages.length;
    await page.locator(INPUT).dispatchEvent("compositionstart");
    await softEdits(page, [{ inputType: "insertText", data: "ㅎ" }, ...backspaces(1)], true);
    await page.locator(INPUT).dispatchEvent("compositionend", { data: "" });
    // A later byte on the same ordered socket proves anything the composition could have sent has arrived.
    await page.locator(INPUT).press("ArrowRight");
    await expect.poll(() => handle.liveInput.map((input) => input.toString())).toContain("\x1b[C");

    await observeFor(page, 200, async () => {
      expect(textBytes(handle, start)).toBe("\x1b[C");
    });
  });
});
