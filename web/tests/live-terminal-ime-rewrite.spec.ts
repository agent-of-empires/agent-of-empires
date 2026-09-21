import { test, expect } from "./helpers/mockedTest";
import { devices, type Page } from "@playwright/test";
import { mockTerminalApis, seedSettings, type MockHandle } from "./helpers/terminal-mocks";
import { clickSidebarSession, openMobileSidebar } from "./helpers/sidebar";

// iOS Korean input (WebKit bug 274700) rewrites the last syllable as deleteContentBackward + insertText with no
// composition events, and emits no delete for an empty textarea, so the hidden input retains typed text.
// Chromium fires no beforeinput for execCommand or CDP deletes, so edits are synthesized and default actions mirrored.

function textBytes(handle: MockHandle, start: number) {
  return handle.liveMessages
    .slice(start)
    .map((msg) => msg.toString("utf8"))
    .filter((s) => !s.startsWith("{"))
    .join("");
}

const INPUT = 'textarea[aria-label="Live terminal input"]';
// App's persistent focus proxy, which survives a session switch.
const PROXY = "textarea[data-keyboard-proxy]";

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
      if (inputType === "deleteContentBackward" && ta.value === "") return;
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
    await softKey(page, "insertText", "ㅎ");
    await softKey(page, "deleteContentBackward");
    await softKey(page, "insertText", "하");
    await softKey(page, "deleteContentBackward");
    await softKey(page, "insertText", "한");

    await expect(page.locator(INPUT)).toHaveValue("한");
    // The PTY sees each rewrite as delete + reinsert.
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

  // #3877: a Ctrl-latched letter never reaches the pane, so it must not stay in the textarea to be deleted later.
  test("a letter the Ctrl latch turned into a control code is not retained", async ({ page }) => {
    const handle = await mockTerminalApis(page);
    await openSession(page, handle);

    const start = handle.liveMessages.length;
    await page.locator('button[aria-label="Ctrl"]').click();
    await softKey(page, "insertText", "c");

    expect(await valueOf(page, INPUT)).toBe("");
    await expect.poll(() => textBytes(handle, start), { timeout: 5_000 }).toBe("\x03");

    await softKey(page, "insertText", "ㅎ");
    await expect.poll(() => textBytes(handle, start), { timeout: 5_000 }).toBe("\x03ㅎ");
  });

  test("out-of-band toolbar input drops the retained syllable before the next rewrite", async ({ page }) => {
    const handle = await mockTerminalApis(page);
    await openSession(page, handle);

    const start = handle.liveMessages.length;
    await softKey(page, "insertText", "한");
    expect(await valueOf(page, INPUT)).toBe("한");

    // Tab bypasses the textarea, so the retained syllable no longer mirrors the line.
    await page.locator('button[aria-label="Tab"]').click();
    expect(await valueOf(page, INPUT)).toBe("");

    await softKey(page, "deleteContentBackward");
    await softKey(page, "insertText", "하");
    expect(await valueOf(page, INPUT)).toBe("하");
    await expect.poll(() => textBytes(handle, start), { timeout: 5_000 }).toBe("한\t하");
  });

  // The proxy persists across a session switch, so it must be cleared there or its syllable leaks into the next PTY.
  test("a session switch drops the syllable retained in the persistent proxy", async ({ page }) => {
    const handle = await mockTerminalApis(page, { extraSessions: [{ id: "other", title: "other" }] });
    await openSession(page, handle);

    await softKey(page, "insertText", "ㅎ", PROXY);
    expect(await valueOf(page, PROXY)).toBe("ㅎ");

    await openMobileSidebar(page);
    await clickSidebarSession(page, "other");
    await page.locator("[data-live-terminal]").waitFor({ state: "visible", timeout: 10_000 });

    expect(await valueOf(page, PROXY)).toBe("");
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

    expect(await valueOf(page, INPUT)).toBe("");
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
    await expect(page.locator(INPUT)).toHaveValue("ㅎ");

    await page.evaluate(() => {
      const w = window as unknown as { releasePasteImage?: () => void };
      w.releasePasteImage?.();
    });
    await expect.poll(() => textBytes(handle, start), { timeout: 5_000 }).toContain("/tmp/paste");
    expect(await valueOf(page, INPUT)).toBe("");
    await expect.poll(() => valueOf(page, PROXY)).toBe("");
  });

  // A late upload from a backgrounded session clears only its own textarea, never the foreground session's proxy.
  test("a late upload from a backgrounded session keeps the foreground proxy", async ({ page }) => {
    const handle = await mockTerminalApis(page, {
      pendingPaste: true,
      extraSessions: [{ id: "other", title: "other" }],
    });
    await page.goto("/");
    await seedSettings(page, { persistentTerminals: true });
    await page.reload();
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
    await openMobileSidebar(page);
    await clickSidebarSession(page, "other");
    await page.locator(`[data-live-terminal]:visible`).waitFor({ state: "visible", timeout: 10_000 });

    await softKey(page, "insertText", "ㅎ", PROXY);
    expect(await valueOf(page, PROXY)).toBe("ㅎ");

    await page.evaluate(() => {
      const w = window as unknown as { releasePasteImage?: () => void };
      w.releasePasteImage?.();
    });
    await expect.poll(() => textBytes(handle, start), { timeout: 5_000 }).toContain("/tmp/paste");
    expect(await valueOf(page, PROXY)).toBe("ㅎ");
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
        input.value = "c";
        input.dispatchEvent(new CompositionEvent("compositionupdate", { data: "c", bubbles: true }));
        input.dispatchEvent(new CompositionEvent("compositionend", { data: "c", bubbles: true }));
      });
      expect(await valueOf(page, selector)).toBe("");
      await softKey(page, "deleteContentBackward", null, selector);
      await softKey(page, "insertText", "ㅎ", selector);
      await expect.poll(() => textBytes(handle, start)).toBe("\x03ㅎ");
    }
  });

  test("only the visible mobile surface owns proxy input after a round trip", async ({ page }) => {
    const writes: Record<string, string> = {};
    const handle = await mockTerminalApis(page, {
      onLiveMessage: (url, message) => {
        const text = message.toString("utf8");
        if (text.startsWith("{")) return;
        const path = new URL(url).pathname;
        writes[path] = (writes[path] ?? "") + text;
      },
    });
    await openSession(page, handle);
    await softKey(page, "insertText", "ㅎ", PROXY);
    await page.getByRole("button", { name: "Toggle panels", exact: true }).click();
    await page.getByTestId("mobile-right-panel-pick-paired").click();
    await expect(page.locator('[data-term="paired"]')).toBeVisible();
    expect(await valueOf(page, PROXY)).toBe("");
    await softKey(page, "deleteContentBackward", null, PROXY);
    await softKey(page, "insertText", "ㅏ", PROXY);
    await page.getByTestId("mobile-back-to-agent").click();
    expect(await valueOf(page, PROXY)).toBe("");
    await softKey(page, "deleteContentBackward", null, PROXY);
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
        const text = message.toString("utf8");
        if (text.startsWith("{")) return;
        const path = new URL(url).pathname;
        writes[path] = (writes[path] ?? "") + text;
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
    expect(await valueOf(page, pairedInput)).toBe("");
    expect(await valueOf(page, PROXY)).toBe("ㅎ");
    await softKey(page, "deleteContentBackward", null, PROXY);
    await softKey(page, "insertText", "하", PROXY);
    await expect.poll(() => writes["/sessions/pinch-test/live-ws"]).toBe("ㅎ\x7f하");
  });
});
