// Regression: the mobile live view keeps a buffer of recent scrollback loaded
// ABOVE the live screen, so a scroll-up lands on real content instead of the
// blank history spacer that only fills on a capture round-trip. Drives a real
// `aoe serve` + tmux with a fake agent that dumps numbered lines into tmux
// scrollback and then idles, and asserts that at the live edge MORE than one
// screenful of those lines is already rendered (the overscan window), and that
// scrolling up a viewport lands on real text rather than blank rows.
import { devices } from "@playwright/test";
import { join } from "node:path";
import { writeFileSync, chmodSync, mkdirSync } from "node:fs";
import { spawnSync } from "node:child_process";
import { test, expect } from "../helpers/liveTest";
import { spawnAoeServe, resolveAoeBinary } from "../helpers/aoeServe";
import { clickSidebarSession, openMobileSidebar } from "../helpers/sidebar";

test("recent scrollback is kept loaded above the live screen", async ({ browser }, testInfo) => {
  test.setTimeout(90_000);
  const serve = await spawnAoeServe({
    authMode: "none",
    workerIndex: testInfo.workerIndex,
    parallelIndex: testInfo.parallelIndex,
    seedFn: (e) => {
      const tool = join(e.shimBin, "dumper");
      // Print 400 numbered lines into tmux scrollback, then idle at a prompt so
      // the pane stays alive with a deep history above the live screen.
      writeFileSync(
        tool,
        `#!/bin/bash
for i in $(seq 1 400); do echo "scrollline $i"; done
echo "PROMPT_READY"
while true; do sleep 1; done
`,
      );
      chmodSync(tool, 0o755);
      const pd = join(e.home, "project");
      mkdirSync(pd, { recursive: true });
      spawnSync("git", ["init", "-q"], { cwd: pd });
      const r = spawnSync(
        resolveAoeBinary(),
        ["add", pd, "-t", "scrollback-test", "-c", "claude", "--cmd-override", tool],
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
    await clickSidebarSession(page, "scrollback-test");
    await page.locator("[data-live-terminal]").waitFor({ state: "visible", timeout: 15_000 });
    await page
      .locator("[data-live-content]")
      .filter({ hasText: "PROMPT_READY" })
      .waitFor({ state: "attached", timeout: 15_000 });
    // Let the sizing effect settle the grid + the buffered window land.
    await page.waitForTimeout(1200);

    const scroller = page.locator("[data-live-terminal] > div").first();
    const m = await scroller.evaluate((el) => {
      const rows = Array.from(el.querySelectorAll("[data-live-content] > div")) as HTMLElement[];
      const h = rows.length >= 2 ? rows[rows.length - 1]!.getBoundingClientRect().height : 16;
      const nums = rows
        .map((r) => /scrollline (\d+)/.exec(r.textContent ?? "")?.[1])
        .filter((x): x is string => !!x)
        .map(Number);
      return {
        screenRows: Math.round(el.clientHeight / h),
        min: nums.length ? Math.min(...nums) : null,
        max: nums.length ? Math.max(...nums) : null,
      };
    });
    expect(m.min, "scrollback lines are rendered at the live edge").not.toBeNull();
    // More than one screenful of distinct scrollback lines is loaded (the
    // visible screen PLUS the overscan buffer above it). With only the screen
    // captured the span would be ~one screen.
    expect(m.max! - m.min!, "buffered scrollback spans more than one screen").toBeGreaterThan(m.screenRows);

    // Scroll up one viewport and confirm the revealed rows are real text,
    // already loaded (not the blank spacer).
    await scroller.evaluate((el) => {
      el.scrollTop = Math.max(0, el.scrollHeight - 2 * el.clientHeight);
    });
    const visible = await scroller.evaluate((el) => {
      const rows = Array.from(el.querySelectorAll("[data-live-content] > div")) as HTMLElement[];
      const top = el.scrollTop;
      const bottom = top + el.clientHeight;
      return rows
        .filter((r) => r.offsetTop >= top && r.offsetTop < bottom)
        .map((r) => r.textContent ?? "")
        .join("|");
    });
    expect(visible, "a scroll-up lands on loaded scrollback, not blank").toContain("scrollline");
  } finally {
    await serve.stop();
  }
});
