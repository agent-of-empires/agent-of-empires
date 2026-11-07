import { test, expect } from "./helpers/mockedTest";
import type { Page } from "@playwright/test";

// #5344: a linked toast (an in-app session push, or a plugin `ui.open_url` link) opens from the keyboard, once,
// and its Dismiss button never navigates.

type Opened = [kind: "session" | "link", target: string][];

async function recordOpens(page: Page) {
  await page.addInitScript(() => {
    const w = window as unknown as { __opened: unknown[] };
    w.__opened = [];
    window.open = ((url?: string | URL) => {
      w.__opened.push(["link", String(url)]);
      return null;
    }) as typeof window.open;
    window.addEventListener("aoe-open-session", (e) =>
      w.__opened.push(["session", (e as CustomEvent<{ sessionId: string }>).detail.sessionId]),
    );
  });
}

const opened = (page: Page) => page.evaluate(() => (window as unknown as { __opened: Opened }).__opened);

// The plugin poll's first snapshot only seeds the notification sequence, so a link is armed only after it was served.
async function mockPluginNotifications(page: Page) {
  let seq = 0;
  let resolveSeeded: () => void = () => {};
  const seeded = new Promise<void>((resolve) => (resolveSeeded = resolve));
  await page.route("**/api/plugins/ui-state", async (r) => {
    const notifications =
      seq === 0
        ? []
        : [{ seq, plugin_id: "acme.kit", tone: "info", title: "Review PR", href: "https://example.com/pr/1" }];
    await r.fulfill({ json: { entries: [], notifications } });
    resolveSeeded();
  });
  return async () => {
    await seeded;
    seq += 1;
  };
}

async function pushSessionToast(page: Page) {
  await page.evaluate(() =>
    navigator.serviceWorker.dispatchEvent(
      new MessageEvent("message", { data: { type: "aoe-push", payload: { title: "Done", session_id: "s-1" } } }),
    ),
  );
}

// Starts the keyboard at the toast column, then Tabs: the opener comes first in each toast, Dismiss last. The column
// is made focusable only for this, as a fixed starting point that doesn't depend on the rest of the page.
async function tabTo(page: Page, name: string) {
  await page.getByTestId("toast-list").evaluate((list) => {
    list.tabIndex = -1;
    list.focus();
  });
  for (let i = 0; i < 4; i++) {
    await page.keyboard.press("Tab");
    const reached = await page.evaluate((n) => {
      const el = document.activeElement;
      return !!el?.closest("[data-testid=toast-list]") && (el.getAttribute("aria-label") ?? el.textContent) === n;
    }, name);
    if (reached) return i + 1;
  }
  throw new Error(`keyboard never reached ${name}`);
}

test.describe("linked toasts from the keyboard (#5344)", () => {
  test.beforeEach(async ({ page }) => {
    await recordOpens(page);
  });

  test("a session toast opens once with Enter; Dismiss from the keyboard doesn't navigate", async ({ page }) => {
    await page.goto("/");
    const list = page.getByTestId("toast-list");

    await pushSessionToast(page);
    await expect(list.getByRole("button", { name: "Done" })).toBeVisible();
    expect(await tabTo(page, "Done")).toBe(1);
    await page.keyboard.press("Enter");
    await expect(list.getByText("Done")).toHaveCount(0);
    expect(await opened(page)).toEqual([["session", "s-1"]]);

    await pushSessionToast(page);
    expect(await tabTo(page, "Dismiss")).toBe(2);
    await page.keyboard.press("Enter");
    await expect(list.getByText("Done")).toHaveCount(0);
    expect(await opened(page)).toEqual([["session", "s-1"]]);
  });

  test("a plugin link toast opens once with Space; Dismiss from the keyboard doesn't open it", async ({ page }) => {
    const arm = await mockPluginNotifications(page);
    await page.goto("/");
    const list = page.getByTestId("toast-list");

    await arm();
    await expect(list.getByRole("button", { name: "Review PR" })).toBeVisible({ timeout: 10_000 });
    expect(await tabTo(page, "Review PR")).toBe(1);
    await page.keyboard.press("Space");
    await expect(list.getByText("Review PR")).toHaveCount(0);
    expect(await opened(page)).toEqual([["link", "https://example.com/pr/1"]]);

    await arm();
    await expect(list.getByRole("button", { name: "Review PR" })).toBeVisible({ timeout: 10_000 });
    expect(await tabTo(page, "Dismiss")).toBe(2);
    await page.keyboard.press("Space");
    await expect(list.getByText("Review PR")).toHaveCount(0);
    expect(await opened(page)).toEqual([["link", "https://example.com/pr/1"]]);
  });
});
