import { test, expect } from "./helpers/mockedTest";
import { mockTerminalApis } from "./helpers/terminal-mocks";
import { clickSidebarSession } from "./helpers/sidebar";

// #3342: rows rendered as one `white-space: pre` run take their column
// positions from whatever font supplied each glyph. A glyph missing from the
// configured font (CJK under Geist Mono, braille under D2Coding NF) falls
// back to a font whose advance is not 1 cell and every later column on that
// row shifts. The renderer must enforce per-cell width so a row of N terminal
// cells always lays out N x cellWidth pixels, regardless of fallback fonts.

const ASCII_ROW = "AAAAAAAAAAAAAAAAAAAA"; // exactly 20 cells
const MIXED_ROW = "\uD55C\uAE00\uD14C\uC2A4\uD2B8" + "AAAA" + "\u280B\u280D" + "AAAA"; // 5x2 CJK + 4 + 2 braille + 4 = 20 cells

test.describe("Live terminal glyph cell-width enforcement", () => {
  test.use({ viewport: { width: 1280, height: 800 }, hasTouch: false });

  test("a row with glyphs missing from the terminal font is exactly cells x cellWidth wide", async ({ page }) => {
    const handle = await mockTerminalApis(page);
    await page.goto("/");
    await clickSidebarSession(page, "pinch-test");
    const term = page.locator("[data-live-terminal]").first();
    await term.waitFor({ state: "visible", timeout: 10_000 });
    // Let the mount settle: webfonts load, the component re-measures charW,
    // and its final resize lands. The mocked live-ws answers every resize
    // with a fresh default frame, so pushing before that settle races the
    // overwrite.
    await page.waitForFunction(() => document.fonts.status === "loaded", undefined, { timeout: 10_000 });
    await term.locator("[data-live-content]").innerText(); // default frame rendered
    await page.waitForTimeout(250);

    // Two lines with the SAME terminal cell count (20): one pure ASCII, one
    // mixing CJK + braille that no default stack covers. Pre-fix the mixed
    // line renders narrower because each missing glyph falls back to a
    // non-1-cell advance.
    handle.pushLiveFrame({
      content: [ASCII_ROW, MIXED_ROW, ""].join("\n"),
      rows: 6,
      history: 0,
      cursor: null,
    });
    const content = term.locator("[data-live-content]");
    await expect.poll(() => content.innerText()).toContain("테스트");

    const metrics = await page.evaluate(() => {
      const grid = document.querySelector<HTMLElement>("[data-live-content]")!;
      // Mirror the component's own cell measure: a 20-char M run in the same
      // styles, inside the grid so it inherits font family and size.
      const probe = document.createElement("span");
      probe.textContent = "M".repeat(20);
      probe.setAttribute("aria-hidden", "true");
      probe.style.whiteSpace = "pre";
      probe.style.position = "absolute";
      probe.style.visibility = "hidden";
      grid.appendChild(probe);
      const cellW = probe.getBoundingClientRect().width / 20;
      probe.remove();
      // Row divs are full-width blocks; their INLINE content extent is the
      // laid-out text width, which is what the grid invariant constrains.
      const inlineWidth = (el: HTMLElement) => {
        const range = document.createRange();
        range.selectNodeContents(el);
        return range.getBoundingClientRect().width;
      };
      const rows = [...grid.children]
        .filter((el): el is HTMLElement => el instanceof HTMLElement && el.tagName === "DIV")
        .map((el) => ({ text: el.textContent ?? "", width: inlineWidth(el) }));
      return { cellW, rows };
    });

    const ascii = metrics.rows.find((r) => r.text.startsWith(ASCII_ROW.slice(0, 10)));
    const mixed = metrics.rows.find((r) => r.text.includes("테스트"));
    expect(ascii).toBeDefined();
    expect(mixed).toBeDefined();
    expect(metrics.cellW).toBeGreaterThan(0);
    const expected = 20 * metrics.cellW;
    // Pure-ASCII line already honors the grid; it anchors the measurement.
    expect(Math.abs(ascii!.width - expected)).toBeLessThan(0.75);
    // The mixed line must be exactly as wide as its cell count demands.
    expect(Math.abs(mixed!.width - expected)).toBeLessThan(0.75);
  });
});
