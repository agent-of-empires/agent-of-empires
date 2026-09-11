import { test, expect } from "./helpers/mockedTest";
import { devices, type Page } from "@playwright/test";
import { clickSidebarSession, openMobileSidebar } from "./helpers/sidebar";
import { mockTerminalApis, seedSettings, type MockHandle } from "./helpers/terminal-mocks";

// A URL an agent prints is rendered as a target=_blank anchor. Under a
// full-screen mouse app the scroller preventDefaults the press and takes
// pointer capture so it can forward press/drag/release; capture retargets
// pointerup, and so click, to the scroller, so the anchor never navigated
// (#3918). This needs a real browser driven by real input: jsdom implements
// neither pointer capture nor the retargeting, and dispatchEvent skips the
// gesture entirely.

const LINK = "https://example.com/pull/1375";
const PROMPT = "$ open the PR";
const scroller = (page: Page) => page.locator("[data-live-terminal] > div").first();
const anchor = (page: Page) => page.locator(`a[href="${LINK}"]`).first();
const mouseBytes = (h: MockHandle) => h.liveMessages.map((b) => b.toString("latin1")).filter((s) => /\x1b\[</.test(s));

/** A full-screen SGR-mouse frame whose output contains a URL. */
function pushLinkFrame(handle: MockHandle) {
  const lines = [PROMPT, `see ${LINK} for details`, ...Array<string>(22).fill("")];
  handle.pushLiveFrame({
    content: `${lines.join("\n")}\n`,
    rows: 24,
    history: 0,
    altScreen: true,
    mouse: true,
    mouseSgr: true,
  });
}

/** Centre of `locator`, once the point actually hits it. The mobile sidebar
 *  is an overlay with a 300ms slide-out, and `toBeVisible` does not hit-test,
 *  so without this a tap lands on the closing sidebar. */
async function hittableCentre(page: Page, selector: string) {
  const box = (await page.locator(selector).first().boundingBox())!;
  const point = [box.x + box.width / 2, box.y + box.height / 2] as const;
  await expect
    .poll(
      () =>
        page.evaluate(([x, y, sel]) => !!document.elementFromPoint(x as number, y as number)?.closest(sel as string), [
          point[0],
          point[1],
          selector,
        ] as const),
      { timeout: 5_000 },
    )
    .toBe(true);
  return point;
}

/** Wait until the client stops sending control messages. The mock answers
 *  every resize/window with its own default frame, so pushing the link frame
 *  before the layout settles (mobile sends extra resizes) loses it. */
async function quiesce(handle: MockHandle) {
  let last = -1;
  await expect
    .poll(
      async () => {
        const seen = handle.liveMessages.length;
        const settled = seen === last;
        last = seen;
        return settled;
      },
      { timeout: 10_000, intervals: [300] },
    )
    .toBe(true);
}

async function setup(page: Page, mobile: boolean) {
  // Route on the CONTEXT, not the page: the link opens a new tab, and a
  // page-scoped route would leave that tab to hit the real network.
  await page
    .context()
    .route("https://example.com/**", (route) =>
      route.fulfill({ contentType: "text/html", body: "<title>linked page</title>ok" }),
    );
  const handle = await mockTerminalApis(page);
  await page.goto("/");
  await seedSettings(page, { mobileFontSize: 14, desktopFontSize: 14 });
  await page.reload();
  if (mobile) await openMobileSidebar(page);
  await clickSidebarSession(page, "pinch-test");
  await page.locator("[data-live-terminal]").first().waitFor({ state: "visible", timeout: 10_000 });
  await expect.poll(() => handle.liveMessages.length, { timeout: 5_000 }).toBeGreaterThan(0);
  await quiesce(handle);
  pushLinkFrame(handle);
  await expect(scroller(page)).toHaveClass(/overflow-hidden/);
  await expect(anchor(page)).toBeVisible();
  return handle;
}

test.describe("Live terminal link clicks (desktop)", () => {
  test.use({ viewport: { width: 1280, height: 800 }, hasTouch: false });

  test("a real click on a printed URL opens it in a new tab", async ({ page }) => {
    const handle = await setup(page, false);
    const [x, y] = await hittableCentre(page, `a[href="${LINK}"]`);
    // Real mouse input, not dispatchEvent: only the browser's own
    // press/capture/release sequence reproduces the retargeting.
    const [popup] = await Promise.all([page.context().waitForEvent("page"), page.mouse.click(x, y)]);
    await popup.waitForLoadState();
    expect(popup.url()).toBe(LINK);
    // The press that opened the link is the browser's, not the app's.
    expect(mouseBytes(handle)).toHaveLength(0);
  });

  test("a click on output beside the link still forwards to the app", async ({ page }) => {
    const handle = await setup(page, false);
    const box = (await page.getByText(PROMPT, { exact: true }).boundingBox())!;
    await page.mouse.click(box.x + 4, box.y + box.height / 2);
    // Press (`M`) then release (`m`) for SGR left button 0.
    await expect.poll(() => mouseBytes(handle).some((s) => /\x1b\[<0;\d+;\d+M/.test(s))).toBe(true);
    await expect.poll(() => mouseBytes(handle).some((s) => /\x1b\[<0;\d+;\d+m/.test(s))).toBe(true);
  });

  test("a right-click on the link still reaches the app", async ({ page }) => {
    const handle = await setup(page, false);
    const [x, y] = await hittableCentre(page, `a[href="${LINK}"]`);
    await page.mouse.click(x, y, { button: "right" });
    // SGR right button is 2.
    await expect.poll(() => mouseBytes(handle).some((s) => /\x1b\[<2;\d+;\d+M/.test(s))).toBe(true);
  });
});

test.describe("Live terminal link taps (mobile)", () => {
  // `defaultBrowserType` would force a new worker, which test.use inside a
  // group forbids; the rest of the descriptor is what matters here.
  const { defaultBrowserType: _browser, ...iPhone13 } = devices["iPhone 13"];
  test.use(iPhone13);

  test("a real tap on a printed URL opens it in a new tab", async ({ page }) => {
    // Touch never enters the forwarding path (it is gated to pointerType
    // "mouse"), but the same anchor has to work under a finger.
    await setup(page, true);
    const [x, y] = await hittableCentre(page, `a[href="${LINK}"]`);
    const [popup] = await Promise.all([page.context().waitForEvent("page"), page.touchscreen.tap(x, y)]);
    await popup.waitForLoadState();
    expect(popup.url()).toBe(LINK);
  });

  test("a tap on output beside the link opens nothing", async ({ page }) => {
    await setup(page, true);
    const [x, y] = await hittableCentre(page, "[data-live-terminal]");
    await page.touchscreen.tap(x, y);
    await page.waitForTimeout(500);
    expect(page.context().pages()).toHaveLength(1);
  });
});
