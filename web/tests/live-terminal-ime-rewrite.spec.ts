import { test, expect } from "./helpers/mockedTest";
import { devices, type Page } from "@playwright/test";
import { mockTerminalApis, seedSettings, type MockHandle } from "./helpers/terminal-mocks";
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
    await expect.poll(() => textBytes(handle, start), { timeout: 5_000 }).toBe("한\t하");
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

  // #3885 case 1: the Ctrl latch's refusal must also clear a NON-empty
  // shadow. The chord is transformed into a control code, so the retained
  // syllable no longer mirrors anything the pane has; the next rewrite
  // would open with a delete and eat a character the user did type.
  test("a Ctrl chord over existing retained text drops it", async ({ page }) => {
    const handle = await mockTerminalApis(page);
    await openSession(page, handle);

    const start = handle.liveMessages.length;
    await softKey(page, "insertText", "한");
    await page.locator('button[aria-label="Ctrl"]').click();
    await softKey(page, "insertText", "c");
    await expect.poll(() => textBytes(handle, start), { timeout: 5_000 }).toBe("한\x03");

    // The chord consumed the shadow's job: no stale syllable may survive it.
    expect(await valueOf(page, INPUT)).toBe("");
    // The next rewrite re-arms from an empty line: no leading DEL.
    await softKey(page, "insertText", "ㅎ");
    await expect.poll(() => textBytes(handle, start), { timeout: 5_000 }).toBe("한\x03ㅎ");
  });

  // #3885 case 2: an async image upload's completion invalidates BOTH hidden
  // inputs. The await leaves room for a syllable typed into the local
  // textarea; inserting the paste path afterwards displaces it, so the
  // shadow must not retain what the line will no longer show.
  test("image upload completion drops a syllable typed during the upload", async ({ page }) => {
    const handle = await mockTerminalApis(page, { pendingPaste: true });
    await openSession(page, handle);

    const start = handle.liveMessages.length;
    // Paste an image while the upload is held, then type during the await.
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
    // The pasted path is sent, and the syllable typed during the await is
    // dropped from the shadow: the line shows the path, not the syllable.
    await expect.poll(() => textBytes(handle, start), { timeout: 5_000 }).toContain("/tmp/paste");
    expect(await valueOf(page, INPUT)).toBe("");
    await expect.poll(() => valueOf(page, PROXY)).toBe("");
  });

  // #3885 review: the async upload's completion must not wipe the proxy's
  // retained syllable of the FOREGROUND session. A late upload from a
  // backgrounded session (still mounted in the stack) clears its own local
  // textarea but leaves the shared proxy — which now belongs to the session
  // the user switched to — untouched.
  test("a late upload from a backgrounded session keeps the foreground proxy", async ({ page }) => {
    const handle = await mockTerminalApis(page, {
      pendingPaste: true,
      extraSessions: [{ id: "other", title: "other" }],
    });
    // Keep both sessions mounted across the switch: the scenario is a late
    // upload racing a foreground switch, not a session teardown.
    await page.goto("/");
    await seedSettings(page, { persistentTerminals: true });
    await page.reload();
    await openSession(page, handle);

    // Session A: paste an image, hold the upload.
    const start = handle.liveMessages.length;
    await page.evaluate(() => {
      const ta = document.querySelector<HTMLTextAreaElement>('textarea[aria-label="Live terminal input"]');
      if (!ta) throw new Error("live terminal input not found");
      ta.focus();
      const dt = new DataTransfer();
      dt.items.add(new File(["x"], "shot.png", { type: "image/png" }));
      ta.dispatchEvent(new ClipboardEvent("paste", { clipboardData: dt, bubbles: true, cancelable: true }));
    });
    // Switch to session B (A stays mounted, its upload still pending). The
    // switch itself drops whatever A's proxy held.
    await openMobileSidebar(page);
    await clickSidebarSession(page, "other");
    await page.locator(`[data-live-terminal]:visible`).waitFor({ state: "visible", timeout: 10_000 });

    // The foreground user (session B) retains a syllable in the shared proxy.
    await softKey(page, "insertText", "ㅎ", PROXY);
    expect(await valueOf(page, PROXY)).toBe("ㅎ");

    await page.evaluate(() => {
      const w = window as unknown as { releasePasteImage?: () => void };
      w.releasePasteImage?.();
    });
    // A's continuation sends its path to A's PTY and clears only A's local
    // textarea; B's retained proxy syllable survives.
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
