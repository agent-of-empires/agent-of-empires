import { test, expect } from "./helpers/mockedTest";
import { devices, type Page } from "@playwright/test";
import { agentMessageChunk, mockAcpSession, openStructuredSession, stopped } from "./helpers/acpMock";

// User story: the transcript font size is a dashboard preference with a
// separate mobile and desktop value, and the dashboard's own mobile classifier
// (coarse primary pointer AND a viewport below 768px, the rule
// `clientFormFactor()` uses) decides which one applies.
//
// Mocked Playwright rather than Vitest because the whole contract is a media
// query resolving a custom property into a computed font size: jsdom evaluates
// neither pointer capability nor `rem`, so a unit test could only re-assert the
// inline variables (which StructuredViewRoot.test.tsx already covers).
//
// Split into two describes because pointer capability is fixed per browser
// context: Playwright cannot flip `(pointer: coarse)` mid-page the way it can
// resize a viewport. Each context still exercises a live viewport change, which
// is the resize case users actually hit.

const MOBILE_SIZE = 11;
const DESKTOP_SIZE = 20;

async function openTranscript(page: Page) {
  await page.addInitScript(
    ([mobile, desktop]) => {
      window.localStorage.setItem(
        "aoe-web-settings",
        JSON.stringify({ structuredMobileFontSize: mobile, structuredDesktopFontSize: desktop }),
      );
    },
    [MOBILE_SIZE, DESKTOP_SIZE],
  );

  const mock = await mockAcpSession(page, {
    title: "story-font-size",
    initialEvents: [agentMessageChunk("# heading\n\nplain paragraph text\n\n```\nfenced code\n```"), stopped()],
  });
  await openStructuredSession(page, mock);

  const body = page.locator(".acp-markdown-body").first();
  await expect(body).toBeVisible({ timeout: 10_000 });
  return body;
}

const fontSizeOf = (locator: ReturnType<Page["locator"]>) => locator.evaluate((el) => getComputedStyle(el).fontSize);

/** Computed line-height as a ratio of the element's own font size, so the
 *  assertion holds at any base size. */
const leadingRatioOf = (locator: ReturnType<Page["locator"]>) =>
  locator.evaluate((el) => {
    const cs = getComputedStyle(el);
    return Number.parseFloat(cs.lineHeight) / Number.parseFloat(cs.fontSize);
  });

test.describe("structured view conversation font size (fine pointer)", () => {
  test.use({ viewport: { width: 1200, height: 800 }, hasTouch: false });

  test("uses the desktop size at any width and scales it with the browser root font size", async ({ page }) => {
    const body = await openTranscript(page);
    const heading = body.locator("h1").first();

    expect(await fontSizeOf(body)).toBe("20px");
    // Headings are `em`, so the whole hierarchy follows the base rather than
    // only paragraphs changing (1.43em * 20px).
    expect(await fontSizeOf(heading)).toBe("28.6px");

    // Fenced code scales with the base (0.86em) but keeps the tight leading the
    // old `text-xs` gave it: Tailwind's `--tw-leading` is `inherits: false`, so
    // an em-sized block with no leading of its own would silently pick up the
    // root's `leading-relaxed` (1.625) and render code much looser than before.
    const codeBlock = body.locator("pre").first();
    expect(await fontSizeOf(codeBlock)).toBe("17.2px");
    expect(await leadingRatioOf(codeBlock)).toBeCloseTo(1.3333, 3);

    // A narrow desktop window is not mobile: without the pointer term this
    // would wrongly drop to the mobile size.
    await page.setViewportSize({ width: 500, height: 800 });
    await expect.poll(() => fontSizeOf(body)).toBe("20px");

    // The size is published as `rem`, so raising the browser/root font size
    // scales the transcript proportionally (20 setting = 1.25rem = 25px at a
    // 20px root) instead of pinning it to the authored px.
    await page.evaluate(() => {
      document.documentElement.style.fontSize = "20px";
    });
    await expect.poll(() => fontSizeOf(body)).toBe("25px");
  });
});

// iPhone 13 gives width 390 (< 768), pointer:coarse and hasTouch. Drop
// `defaultBrowserType`: Playwright forbids it in a describe-level `test.use`
// (it would force a new worker) and the project already pins chromium.
const { defaultBrowserType: _iphoneBrowser, ...iPhone13 } = devices["iPhone 13"];

test.describe("structured view conversation font size (coarse pointer)", () => {
  test.use(iPhone13);

  test("uses the mobile size when narrow and the desktop size once the viewport widens", async ({ page }) => {
    const body = await openTranscript(page);
    const heading = body.locator("h1").first();

    expect(await fontSizeOf(body)).toBe("11px");
    expect(await fontSizeOf(heading)).toBe("15.73px");

    // Still a coarse pointer, but a tablet-width viewport is classified
    // desktop, and the swap happens live with no reload.
    await page.setViewportSize({ width: 900, height: 800 });
    await expect.poll(() => fontSizeOf(body)).toBe("20px");
    expect(await fontSizeOf(heading)).toBe("28.6px");
  });
});
