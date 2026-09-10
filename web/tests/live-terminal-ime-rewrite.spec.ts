import { test, expect } from "./helpers/mockedTest";
import { devices, type Page } from "@playwright/test";
import { mockTerminalApis, type MockHandle } from "./helpers/terminal-mocks";
import { clickSidebarSession, openMobileSidebar } from "./helpers/sidebar";

// iOS WebKit fires no composition events for the Korean keyboard (WebKit bug
// 274700). Every keystroke rewrites the trailing syllable through the plain
// editing path instead: `deleteContentBackward` for the previous state, then
// `insertText` with the new one ("ㅎ" -> "하" -> "한"). WebKit dispatches no
// delete event at all when the textarea has nothing before the caret, so the
// hidden input has to retain typed text for those deletes to be observable.
//
// Chromium fires `beforeinput` neither for execCommand nor for CDP-driven
// deletes, so the keystrokes are synthesized the way backspace-autorepeat.spec
// does, with one addition: the browser's default action (mutating the
// textarea) is mirrored whenever the handler leaves the event uncancelled.

// Text bytes only: JSON control frames (resize / window / cadence) share the WS.
function textBytes(handle: MockHandle, start: number) {
  return handle.liveMessages
    .slice(start)
    .map((msg) => msg.toString("utf8"))
    .filter((s) => !s.startsWith("{"))
    .join("");
}

const INPUT = 'textarea[aria-label="Live terminal input"]';
// App's persistent proxy, which holds iOS focus authorized by a sidebar tap
// and, unlike INPUT, survives a session switch.
const PROXY = "textarea[data-keyboard-proxy]";

// Emit one soft-keyboard edit on a hidden terminal input and apply the
// default action if the page did not preventDefault it.
async function softKey(
  page: Page,
  inputType: "insertText" | "deleteContentBackward",
  data: string | null = null,
  selector = INPUT,
) {
  await page.evaluate(
    ({ selector, inputType, data }) => {
      const ta = document.querySelector<HTMLTextAreaElement>(selector);
      if (!ta) throw new Error("live terminal input not found");
      ta.focus();
      const ev = new InputEvent("beforeinput", { inputType, data, bubbles: true, cancelable: true });
      if (!ta.dispatchEvent(ev)) return;
      const end = ta.value.length;
      if (inputType === "insertText") ta.setRangeText(data ?? "", end, end, "end");
      else ta.setRangeText("", Math.max(0, end - 1), end, "end");
    },
    { selector, inputType, data },
  );
}

function valueOf(page: Page, selector: string) {
  return page.evaluate((s) => document.querySelector<HTMLTextAreaElement>(s)?.value ?? null, selector);
}

const { defaultBrowserType: _iphoneBrowser, ...iPhone13 } = devices["iPhone 13"];

test.describe("Live terminal IME syllable rewrite", () => {
  test.use(iPhone13);

  async function openSession(page: Page, handle: MockHandle) {
    await page.goto("/");
    await openMobileSidebar(page);
    await clickSidebarSession(page, "pinch-test");
    await page.locator("[data-live-terminal]").waitFor({ state: "visible", timeout: 10_000 });
    await expect.poll(() => handle.liveMessages.length, { timeout: 5_000 }).toBeGreaterThan(0);
  }

  test("delete + reinsert of the trailing syllable reaches the PTY as DEL + text", async ({ page }) => {
    const handle = await mockTerminalApis(page);
    await openSession(page, handle);

    const start = handle.liveMessages.length;
    // What the iOS Korean keyboard emits for the keystrokes ㅎ, ㅏ, ㄴ.
    await softKey(page, "insertText", "ㅎ");
    await softKey(page, "deleteContentBackward");
    await softKey(page, "insertText", "하");
    await softKey(page, "deleteContentBackward");
    await softKey(page, "insertText", "한");

    // The typed text stays in the hidden input as IME context...
    await expect(page.locator(INPUT)).toHaveValue("한");
    // ...and the PTY sees each rewrite as delete + reinsert, ending on 한.
    await expect.poll(() => textBytes(handle, start), { timeout: 5_000 }).toBe("ㅎ\x7f하\x7f한");
  });

  test("Enter submits and drops the retained IME context", async ({ page }) => {
    const handle = await mockTerminalApis(page);
    await openSession(page, handle);

    const start = handle.liveMessages.length;
    await softKey(page, "insertText", "한");
    await page
      .locator(INPUT)
      .dispatchEvent("keydown", { key: "Enter", code: "Enter", bubbles: true, cancelable: true });

    await expect(page.locator(INPUT)).toHaveValue("");
    await expect.poll(() => textBytes(handle, start), { timeout: 5_000 }).toBe("한\r");
  });

  // #3877's repro. The Ctrl latch turns the next letter into a control code,
  // so `sendKeys` returns false and the pane never receives "c". If the
  // textarea kept it anyway, the following Korean rewrite would open with a
  // delete and eat a character of the post-SIGINT prompt.
  test("a letter the Ctrl latch turned into a control code is not retained", async ({ page }) => {
    const handle = await mockTerminalApis(page);
    await openSession(page, handle);

    const start = handle.liveMessages.length;
    await page.locator('button[aria-label="Ctrl"]').click();
    await softKey(page, "insertText", "c");

    expect(await valueOf(page, INPUT)).toBe("");
    await expect.poll(() => textBytes(handle, start), { timeout: 5_000 }).toBe("\x03");

    // The next rewrite therefore re-arms from an empty line: no leading DEL.
    await softKey(page, "insertText", "ㅎ");
    await expect.poll(() => textBytes(handle, start), { timeout: 5_000 }).toBe("\x03ㅎ");
  });

  test("out-of-band toolbar input drops the retained syllable before the next rewrite", async ({ page }) => {
    const handle = await mockTerminalApis(page);
    await openSession(page, handle);

    const start = handle.liveMessages.length;
    await softKey(page, "insertText", "한");
    expect(await valueOf(page, INPUT)).toBe("한");

    // Tab bypasses the textarea: once it reaches the PTY the retained
    // syllable no longer mirrors the line.
    await page.locator('button[aria-label="Tab"]').click();
    expect(await valueOf(page, INPUT)).toBe("");

    // So the rewrite re-arms from an empty shadow, mirroring the keyboard.
    await softKey(page, "deleteContentBackward");
    await softKey(page, "insertText", "하");
    expect(await valueOf(page, INPUT)).toBe("하");
    await expect.poll(() => textBytes(handle, start), { timeout: 5_000 }).toBe("한\t\x7f하");
  });

  // The proxy is the element under test, not INPUT: a session switch unmounts
  // and remounts the live terminal, so INPUT is empty afterwards either way.
  // The proxy persists across the switch, so only clearing it on the session
  // boundary keeps the retained syllable out of the next session's PTY.
  test("a session switch drops the syllable retained in the persistent proxy", async ({ page }) => {
    const handle = await mockTerminalApis(page, { extraSessions: [{ id: "other", title: "other" }] });
    await openSession(page, handle);

    await softKey(page, "insertText", "ㅎ", PROXY);
    expect(await valueOf(page, PROXY)).toBe("ㅎ");

    await openMobileSidebar(page);
    await clickSidebarSession(page, "other");
    await page.locator("[data-live-terminal]").waitFor({ state: "visible", timeout: 10_000 });

    // Without the clear this still holds "ㅎ", and the next Korean keystroke
    // rewrites it as DEL + replacement into the newly selected session.
    expect(await valueOf(page, PROXY)).toBe("");
  });
});
