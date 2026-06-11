// Regression for the hidden-mobile-slide-in bug fixed in the wterm ->
// xterm.js swap. ContentSplit renders the paired-terminal `{right}`
// block twice on every viewport: the desktop inline copy and the
// mobile slide-in overlay. On desktop the slide-in's wrapper is
// `md:hidden` so it never paints, but its useTerminal hook still
// runs, mounts an xterm.js Terminal inside a 0-size container, and
// historically measured the hidden grid as ~10x4. That resize message
// then dragged the shared tmux session down to 10x4, which the
// visible inline terminal rendered as DEC line-drawing border chars
// surrounding a tiny pane.
//
// The fix in useTerminal.ts skips sending resize messages whose
// dimensions look like they came from measuring a hidden container.
// This spec asserts the visible inline paired terminal sends a real
// resize while the hidden mobile slide-in copy does not.

import { test, expect } from "./helpers/mockedTest";
import { mockTerminalApis, type MockHandle } from "./helpers/terminal-mocks";
import { clickSidebarSession } from "./helpers/sidebar";

test.use({ viewport: { width: 1400, height: 900 }, hasTouch: false });

interface ResizeMsg {
  type: "resize";
  cols: number;
  rows: number;
}

function resizesFor(handle: MockHandle): ResizeMsg[] {
  const out: ResizeMsg[] = [];
  for (const msg of handle.wsMessages) {
    const s = msg.toString("utf8");
    if (!s.startsWith("{")) continue;
    try {
      const parsed = JSON.parse(s);
      if (parsed?.type === "resize") out.push(parsed);
    } catch {
      // not json
    }
  }
  return out;
}

test.describe("Paired terminal hidden-container resize gating", () => {
  test("the hidden mobile slide-in's paired terminal does NOT ship a tiny resize", async ({ page }) => {
    const handle = await mockTerminalApis(page);
    await page.goto("/");
    await clickSidebarSession(page, "pinch-test");
    // Wait for both .xterm panels to mount (agent + the two paired
    // copies the ContentSplit + RightPanel chain renders).
    await page.locator(".xterm").first().waitFor({ state: "visible", timeout: 10_000 });
    await page.waitForTimeout(1500);

    const allResizes = resizesFor(handle);
    // Every resize that the dashboard ships has to be plausible.
    // The visible agent + visible paired both clear 20 cols, 5 rows
    // at this viewport. A 10x4 or smaller message means a hidden
    // container's measurement leaked through.
    const tiny = allResizes.filter((r) => r.cols < 20 || r.rows < 5);
    expect(
      tiny,
      `Hidden-container measurement leaked: ${JSON.stringify(tiny)}. ` + `Full list: ${JSON.stringify(allResizes)}`,
    ).toHaveLength(0);
    // At least one real resize must have shipped so we know the
    // visible terminals connected.
    expect(allResizes.length).toBeGreaterThan(0);
  });
});
