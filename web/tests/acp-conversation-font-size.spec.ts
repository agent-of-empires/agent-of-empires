import { test, expect } from "./helpers/mockedTest";
import { agentMessageChunk, mockAcpSession, openStructuredSession, stopped } from "./helpers/acpMock";

// User story: the transcript font size is a browser-local preference with a
// separate mobile and desktop value, and crossing 768px must switch between
// them live.
//
// Mocked Playwright rather than Vitest because the whole contract is a CSS
// media query resolving a custom property into a computed font size: jsdom
// evaluates neither, so a unit test could only re-assert the inline variables
// (which StructuredViewRoot.test.tsx already covers). One test walks both
// viewports and the proportional child scaling, per the "one test per
// behavior" rule.
test.describe("structured view conversation font size", () => {
  test("resolves the stored mobile/desktop sizes across the 768px breakpoint and scales markdown with them", async ({
    page,
  }) => {
    await page.addInitScript(() => {
      window.localStorage.setItem(
        "aoe-web-settings",
        JSON.stringify({ structuredMobileFontSize: 11, structuredDesktopFontSize: 20 }),
      );
    });

    const mock = await mockAcpSession(page, {
      title: "story-font-size",
      initialEvents: [agentMessageChunk("# heading\n\nplain paragraph text"), stopped()],
    });
    await page.setViewportSize({ width: 1200, height: 800 });
    await openStructuredSession(page, mock);

    const body = page.locator(".acp-markdown-body").first();
    await expect(body).toBeVisible({ timeout: 10_000 });
    const fontSizeOf = (locator = body) => locator.evaluate((el) => getComputedStyle(el).fontSize);

    expect(await fontSizeOf()).toBe("20px");
    // Headings are `em`, so the whole hierarchy follows the base rather than
    // only paragraphs changing (1.43em * 20px).
    const heading = body.locator("h1").first();
    expect(await fontSizeOf(heading)).toBe("28.6px");

    // Crossing the breakpoint swaps to the mobile value with no reload.
    await page.setViewportSize({ width: 500, height: 800 });
    await expect.poll(() => fontSizeOf()).toBe("11px");
    expect(await fontSizeOf(heading)).toBe("15.73px");

    await page.setViewportSize({ width: 1200, height: 800 });
    await expect.poll(() => fontSizeOf()).toBe("20px");
  });
});
