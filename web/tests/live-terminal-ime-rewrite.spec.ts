import { test, expect } from "./helpers/mockedTest";
import { devices, type Page } from "@playwright/test";
import { mockTerminalApis, type MockHandle } from "./helpers/terminal-mocks";
import { clickSidebarSession, openMobileSidebar } from "./helpers/sidebar";
import { HIDDEN_INPUT_SENTINEL as S } from "../src/lib/hiddenInputDiff";

// iOS Korean input (WebKit bug 274700) rewrites the last syllable as deleteContentBackward + insertText with no
// composition events, and iOS dictation replaces its hypothesis in place, so the hidden input retains typed text and
// sends the diff of its value. Edits are synthesized as the browser applies them: beforeinput, the value change, input.

/** What the PTY receives for one socket message: input bytes, a paste as the server brackets it, or nothing. */
function ptyBytes(message: Buffer) {
  const text = message.toString("utf8");
  if (!text.startsWith("{")) return text;
  const msg = JSON.parse(text) as { type?: string; text?: string };
  return msg.type === "paste" ? `\x1b[200~${msg.text}\x1b[201~` : "";
}

function textBytes(handle: MockHandle, start: number) {
  return handle.liveMessages.slice(start).map(ptyBytes).join("");
}

const INPUT = 'textarea[aria-label="Live terminal input"]';
// App's persistent focus proxy, which survives a session switch.
const PROXY = "textarea[data-keyboard-proxy]";

async function softKey(
  page: Page,
  inputType: "insertText" | "deleteContentBackward" | "insertReplacementText",
  data: string | null = null,
  selector = INPUT,
  /** For insertReplacementText: how many trailing characters the replacement covers. */
  replaces = 0,
) {
  await page.evaluate(
    ({ selector, inputType, data, replaces }) => {
      const ta = document.querySelector<HTMLTextAreaElement>(selector);
      if (!ta) throw new Error("live terminal input not found");
      ta.focus();
      if (inputType === "deleteContentBackward" && ta.value === "") return;
      const ev = new InputEvent("beforeinput", { inputType, data, bubbles: true, cancelable: true });
      if (!ta.dispatchEvent(ev)) return;
      const end = ta.value.length;
      if (inputType === "deleteContentBackward") ta.setRangeText("", Math.max(0, end - 1), end, "end");
      else ta.setRangeText(data ?? "", end - replaces, end, "end");
      ta.dispatchEvent(new InputEvent("input", { inputType, data, bubbles: true }));
    },
    { selector, inputType, data, replaces },
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
    await softKey(page, "insertText", "ㅎ");
    await softKey(page, "deleteContentBackward");
    await softKey(page, "insertText", "하");
    await softKey(page, "deleteContentBackward");
    await softKey(page, "insertText", "한");

    await expect(page.locator(INPUT)).toHaveValue(S + "한");
    // The PTY sees each rewrite as delete + reinsert.
    await expect.poll(() => textBytes(handle, start), { timeout: 5_000 }).toBe("ㅎ\x7f하\x7f한");
  });

  test("dictation hypotheses replaced in place reach the PTY once", async ({ page }) => {
    const handle = await mockTerminalApis(page);
    await openSession(page, handle);

    const start = handle.liveMessages.length;
    let previous = 0;
    for (const hypothesis of ["thi", "this", "this is", "this is a test", "This is a test."]) {
      await softKey(page, "insertReplacementText", hypothesis, INPUT, previous);
      previous = hypothesis.length;
    }

    await expect(page.locator(INPUT)).toHaveValue(S + "This is a test.");
    await expect
      .poll(() => textBytes(handle, start), { timeout: 5_000 })
      .toBe("this is a test" + "\x7f".repeat(14) + "This is a test.");
  });

  test("Enter submits and drops the retained IME context", async ({ page }) => {
    const handle = await mockTerminalApis(page);
    await openSession(page, handle);

    const start = handle.liveMessages.length;
    await softKey(page, "insertText", "한");
    await page
      .locator(INPUT)
      .dispatchEvent("keydown", { key: "Enter", code: "Enter", bubbles: true, cancelable: true });

    await expect(page.locator(INPUT)).toHaveValue(S);
    await expect.poll(() => textBytes(handle, start), { timeout: 5_000 }).toBe("한\r");
  });

  // #3877: a Ctrl-latched letter never reaches the pane, so it must not stay in the textarea to be deleted later.
  test("a letter the Ctrl latch turned into a control code is not retained", async ({ page }) => {
    const handle = await mockTerminalApis(page);
    await openSession(page, handle);

    const start = handle.liveMessages.length;
    await page.locator('button[aria-label="Ctrl"]').click();
    await softKey(page, "insertText", "c");

    expect(await valueOf(page, INPUT)).toBe(S);
    await expect.poll(() => textBytes(handle, start), { timeout: 5_000 }).toBe("\x03");

    await softKey(page, "insertText", "ㅎ");
    await expect.poll(() => textBytes(handle, start), { timeout: 5_000 }).toBe("\x03ㅎ");
  });

  // The proxy persists across a session switch, so it must be cleared there or its syllable leaks into the next PTY.
  test("a session switch drops the syllable retained in the persistent proxy", async ({ page }) => {
    const handle = await mockTerminalApis(page, { extraSessions: [{ id: "other", title: "other" }] });
    await openSession(page, handle);

    await softKey(page, "insertText", "ㅎ", PROXY);
    expect(await valueOf(page, PROXY)).toBe(S + "ㅎ");

    await openMobileSidebar(page);
    await clickSidebarSession(page, "other");
    await page.locator("[data-live-terminal]").waitFor({ state: "visible", timeout: 10_000 });

    expect(await valueOf(page, PROXY)).toBe(S);
  });

  // #3885: a Ctrl-latched chord also clears a non-empty shadow.
  test("a Ctrl chord over existing retained text drops it", async ({ page }) => {
    const handle = await mockTerminalApis(page);
    await openSession(page, handle);

    const start = handle.liveMessages.length;
    await softKey(page, "insertText", "한");
    await page.locator('button[aria-label="Ctrl"]').click();
    await softKey(page, "insertText", "c");
    await expect.poll(() => textBytes(handle, start), { timeout: 5_000 }).toBe("한\x03");

    expect(await valueOf(page, INPUT)).toBe(S);
    await softKey(page, "insertText", "ㅎ");
    await expect.poll(() => textBytes(handle, start), { timeout: 5_000 }).toBe("한\x03ㅎ");
  });

  // #3885: a completed image upload displaces text typed during the await, so both hidden inputs are cleared.
  test("image upload completion drops a syllable typed during the upload", async ({ page }) => {
    const handle = await mockTerminalApis(page, { pendingPaste: true });
    await openSession(page, handle);

    const start = handle.liveMessages.length;
    await page.evaluate(() => {
      const ta = document.querySelector<HTMLTextAreaElement>('textarea[aria-label="Live terminal input"]');
      if (!ta) throw new Error("live terminal input not found");
      ta.focus();
      const dt = new DataTransfer();
      dt.items.add(new File(["x"], "shot.png", { type: "image/png" }));
      ta.dispatchEvent(new ClipboardEvent("paste", { clipboardData: dt, bubbles: true, cancelable: true }));
    });
    await softKey(page, "insertText", "ㅎ");
    await expect(page.locator(INPUT)).toHaveValue(S + "ㅎ");

    await page.evaluate(() => {
      const w = window as unknown as { releasePasteImage?: () => void };
      w.releasePasteImage?.();
    });
    await expect.poll(() => textBytes(handle, start), { timeout: 5_000 }).toContain("/tmp/paste");
    expect(await valueOf(page, INPUT)).toBe(S);
    await expect.poll(() => valueOf(page, PROXY)).toBe(S);
  });

  test("refused composition commits cannot seed the next rewrite", async ({ page }) => {
    const handle = await mockTerminalApis(page);
    await openSession(page, handle);
    for (const selector of [INPUT, PROXY]) {
      const start = handle.liveMessages.length;
      await page.locator('button[aria-label="Ctrl"]').click();
      await page.locator(selector).evaluate((element) => {
        const input = element as HTMLTextAreaElement;
        input.focus();
        input.dispatchEvent(new CompositionEvent("compositionstart", { bubbles: true }));
        input.value += "c";
        input.dispatchEvent(new CompositionEvent("compositionupdate", { data: "c", bubbles: true }));
        input.dispatchEvent(new CompositionEvent("compositionend", { data: "c", bubbles: true }));
      });
      // Nothing is left for the IME to rewrite, so the next syllable is a plain insert.
      expect(await valueOf(page, selector)).toBe(S);
      await softKey(page, "insertText", "ㅎ", selector);
      await expect.poll(() => textBytes(handle, start)).toBe("\x03ㅎ");
    }
  });

  test("only the visible mobile surface owns proxy input after a round trip", async ({ page }) => {
    const writes: Record<string, string> = {};
    const handle = await mockTerminalApis(page, {
      onLiveMessage: (url, message) => {
        const bytes = ptyBytes(message);
        const path = new URL(url).pathname;
        if (bytes) writes[path] = (writes[path] ?? "") + bytes;
      },
    });
    await openSession(page, handle);
    await softKey(page, "insertText", "ㅎ", PROXY);
    await page.getByRole("button", { name: "Toggle panels", exact: true }).click();
    await page.getByTestId("mobile-right-panel-pick-paired").click();
    await expect(page.locator('[data-term="paired"]')).toBeVisible();
    expect(await valueOf(page, PROXY)).toBe(S);
    await softKey(page, "insertText", "ㅏ", PROXY);
    await page.getByTestId("mobile-back-to-agent").click();
    expect(await valueOf(page, PROXY)).toBe(S);
    await softKey(page, "insertText", "ㄴ", PROXY);
    await page.locator(PROXY).dispatchEvent("keydown", { key: "Enter", bubbles: true, cancelable: true });
    await expect
      .poll(() => writes)
      .toEqual({
        "/sessions/pinch-test/live-ws": "ㅎㄴ\r",
        "/sessions/pinch-test/terminal/live-ws": "ㅏ",
      });
    await expect(page.locator('[data-term="paired"]')).toHaveCount(1);
  });

  test("a hidden paired terminal upload preserves the agent proxy", async ({ page }) => {
    const writes: Record<string, string> = {};
    const handle = await mockTerminalApis(page, {
      pendingPaste: true,
      onLiveMessage: (url, message) => {
        const bytes = ptyBytes(message);
        const path = new URL(url).pathname;
        if (bytes) writes[path] = (writes[path] ?? "") + bytes;
      },
    });
    await openSession(page, handle);
    await page.getByRole("button", { name: "Toggle panels", exact: true }).click();
    await page.getByTestId("mobile-right-panel-pick-paired").click();
    await expect(page.locator('[data-term="paired"]')).toBeVisible();
    const pairedInput = `[data-term="paired"] ${INPUT}`;
    await page.locator(pairedInput).evaluate((element) => {
      const clipboardData = new DataTransfer();
      clipboardData.items.add(new File(["x"], "shot.png", { type: "image/png" }));
      element.dispatchEvent(new ClipboardEvent("paste", { clipboardData, bubbles: true, cancelable: true }));
    });
    await softKey(page, "insertText", "ㄱ", pairedInput);
    await page.getByTestId("mobile-back-to-agent").click();
    await softKey(page, "insertText", "ㅎ", PROXY);
    await page.evaluate(() => (window as unknown as { releasePasteImage: () => void }).releasePasteImage());
    await expect
      .poll(() => writes["/sessions/pinch-test/terminal/live-ws"])
      .toBe("ㄱ\x1b[200~ /tmp/paste/shot.png \x1b[201~");
    expect(await valueOf(page, pairedInput)).toBe(S);
    expect(await valueOf(page, PROXY)).toBe(S + "ㅎ");
    await softKey(page, "deleteContentBackward", null, PROXY);
    await softKey(page, "insertText", "하", PROXY);
    await expect.poll(() => writes["/sessions/pinch-test/live-ws"]).toBe("ㅎ\x7f하");
  });
});
