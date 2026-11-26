// iOS 26 lays a scroll-edge blur over an installed app's top and bottom edges unless the box hit at that edge sits
// in a fixed or sticky container painting one opaque colour, with no backdrop filter (WebKit FixedContainerEdges).
import { test, expect } from "./helpers/mockedTest";
import type { Page } from "@playwright/test";
import { openLiveTerminal } from "./helpers/liveTerminal";
import { iPhone13 } from "./helpers/viewports";

/** The fixed or sticky container hit at `y`, with what iOS samples from it. */
function edgeContainer(page: Page, y: "top" | "bottom") {
  return page.evaluate((edge) => {
    const at = edge === "top" ? 4 : window.innerHeight - 4;
    for (let el = document.elementFromPoint(window.innerWidth / 2, at); el; el = el.parentElement) {
      const style = getComputedStyle(el);
      if (style.position !== "fixed" && style.position !== "sticky") continue;
      const width = el.getBoundingClientRect().width / window.innerWidth;
      return { background: style.backgroundColor, backdrop: style.backdropFilter, wide: width >= 0.9 };
    }
    return null;
  }, y);
}

test.describe("edges an iOS 26 installed app can tint", () => {
  test.use(iPhone13);

  test("the session list and an open session paint opaque edges", async ({ page }) => {
    await openLiveTerminal(page, { mobile: true });
    for (const screen of ["session", "list"] as const) {
      if (screen === "list") await page.goto("/");
      for (const edge of ["top", "bottom"] as const) {
        const box = await edgeContainer(page, edge);
        expect(box, `${screen} ${edge}`).not.toBeNull();
        expect(box!.wide, `${screen} ${edge}`).toBe(true);
        expect(box!.backdrop, `${screen} ${edge}`).toBe("none");
        expect(box!.background, `${screen} ${edge}`).toMatch(/^rgb\(/);
      }
    }
  });
});
