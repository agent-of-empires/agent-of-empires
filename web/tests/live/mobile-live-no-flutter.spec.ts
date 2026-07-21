// Regression: the mobile live view must not flutter when a fullscreen agent's
// lowest non-blank row oscillates (spinner / footer redraw). The screen is
// bottom-aligned by trimming trailing blank rows; if that trim count tracked
// the raw last-non-blank row it would change the rendered height every frame
// and bounce the whole block (the jitter #2087 reverted). A debounce keeps the
// height stable. Drives a real `aoe serve` + tmux with a fake agent whose
// bottom row toggles at ~8Hz and asserts a stable top line does not move.
import { devices } from "@playwright/test";
import { join } from "node:path";
import { writeFileSync, chmodSync, mkdirSync } from "node:fs";
import { spawnSync } from "node:child_process";
import { test, expect } from "../helpers/liveTest";
import { spawnAoeServe, resolveAoeBinary } from "../helpers/aoeServe";
import { clickSidebarSession, openMobileSidebar } from "../helpers/sidebar";

test("oscillating bottom row does not flutter the viewport", async ({ browser }, testInfo) => {
  test.setTimeout(90_000);
  const serve = await spawnAoeServe({
    authMode: "none",
    workerIndex: testInfo.workerIndex,
    parallelIndex: testInfo.parallelIndex,
    seedFn: (e) => {
      const tool = join(e.shimBin, "promptbox");
      // Stable header + body, then a "spinner" line that toggles on/off below
      // the input box, moving the lowest non-blank row every 120ms.
      writeFileSync(
        tool,
        `#!/bin/bash
clear
echo "HEADER stable top line"
for i in $(seq 1 16); do echo "body $i"; done
on=1
while true; do
  tput cup 18 0; printf 'INPUTBOX> '
  if [ "$on" = 1 ]; then tput cup 20 0; printf 'spinner working...'; on=0
  else tput cup 20 0; tput el; on=1; fi
  sleep 0.12
done
`,
      );
      chmodSync(tool, 0o755);
      const pd = join(e.home, "project");
      mkdirSync(pd, { recursive: true });
      spawnSync("git", ["init", "-q"], { cwd: pd });
      const r = spawnSync(
        resolveAoeBinary(),
        ["add", pd, "-t", "flutter-test", "-c", "claude", "--cmd-override", tool],
        { env: e.env },
      );
      if (r.status !== 0) throw new Error(String(r.stderr));
    },
  });
  try {
    const ctx = await browser.newContext({ ...devices["iPhone 13"] });
    const page = await ctx.newPage();
    await page.goto(serve.baseUrl);
    await openMobileSidebar(page);
    await clickSidebarSession(page, "flutter-test");
    await page.locator("[data-live-terminal]").waitFor({ state: "visible", timeout: 15_000 });
    await page
      .locator("[data-live-content]")
      .filter({ hasText: "HEADER" })
      .waitFor({ state: "attached", timeout: 15_000 });
    // Keyboard open: focus the input and shrink the viewport like the soft
    // keyboard does, the layout where the bottom-align (and its flutter risk)
    // is in play.
    await page.locator("[data-live-terminal] textarea").focus();
    await page.setViewportSize({ width: 390, height: 380 });

    const headerTop = () =>
      page.evaluate(() => {
        const rows = Array.from(document.querySelectorAll("[data-live-content] > div"));
        const h = rows.find((el) => (el.textContent ?? "").includes("HEADER"));
        return h ? Math.round(h.getBoundingClientRect().top) : null;
      });

    // Wait for the bottom-align to settle before sampling. A fixed wait flaked
    // under CI load: the reflow after the keyboard-shrink was still in flight,
    // so the first few samples caught the HEADER mid-move and inflated the
    // jitter. The real flutter bug never settles (it bounces at ~8Hz), so this
    // loop just runs out its budget on a regression and the sampling below
    // still catches it; a slow CI reflow settles within the cap. A plain loop
    // (not expect.poll) avoids recording a spurious assertion failure on the
    // regression path.
    const settled = async () => {
      const a = await headerTop();
      await page.waitForTimeout(80);
      const b = await headerTop();
      return a != null && a === b;
    };
    const settleDeadline = 5_000;
    for (let waited = 0; waited < settleDeadline; waited += 180) {
      if (await settled()) break;
      await page.waitForTimeout(100);
    }

    const ys: number[] = [];
    for (let i = 0; i < 25; i++) {
      const y = await headerTop();
      if (y != null) ys.push(y);
      await page.waitForTimeout(100);
    }
    // Guard against a false green: if the HEADER row never sampled, the
    // jitter math below would be Math.max(...[]) - Math.min(...[]) = -Infinity,
    // which trivially passes the assertion without measuring anything.
    expect(ys.length).toBeGreaterThan(0);
    const jitter = Math.max(...ys) - Math.min(...ys);
    // Sub-row stability: 1px sampling slop is fine, a row (~10px) bounce is
    // the bug.
    expect(jitter).toBeLessThan(3);
  } finally {
    await serve.stop();
  }
});
