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

test("scrollback remains available through the web capture limit", async ({ browser }, testInfo) => {
  test.setTimeout(90_000);
  const serve = await spawnAoeServe({
    authMode: "none",
    workerIndex: testInfo.workerIndex,
    parallelIndex: testInfo.parallelIndex,
    seedFn: (e) => {
      const tool = join(e.shimBin, "dumper");
      // Let the test raise this pane's tmux history limit before emitting more
      // than the VT channel's 2,000-line seed cache. The web transport itself
      // promises up to 4,000 captured lines, so line 1 must remain reachable.
      writeFileSync(
        tool,
        `#!/bin/bash
sleep 5
for i in $(seq 1 2500); do echo "scrollline $i"; done
echo "PROMPT_READY"
while true; do sleep 1; done
`,
      );
      chmodSync(tool, 0o755);
      const pd = join(e.home, "project");
      mkdirSync(pd, { recursive: true });
      spawnSync("git", ["init", "-q"], { cwd: pd });
      const bootstrap = spawnSync(
        "tmux",
        ["-S", e.env.AOE_TMUX_SOCKET!, "new-session", "-d", "-s", "history-bootstrap", "sleep 30"],
        { env: e.env },
      );
      if (bootstrap.status !== 0) throw new Error(String(bootstrap.stderr));
      const historyLimit = spawnSync(
        "tmux",
        ["-S", e.env.AOE_TMUX_SOCKET!, "set-option", "-g", "history-limit", "4000"],
        { env: e.env },
      );
      if (historyLimit.status !== 0) throw new Error(String(historyLimit.stderr));
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
      // The seed idles 5s before flooding 2,500 lines, so PROMPT_READY lands
      // late; on a loaded CI runner (two live-serve workers per shard) the
      // stream + render can outlast a 15s budget. 30s (well within the 90s
      // test cap) keeps this deterministic without masking a real hang.
      .filter({ hasText: "PROMPT_READY" })
      .waitFor({ state: "attached", timeout: 30_000 });
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

    // Jump to the oldest retained row. The fast VT path caches 2,000 lines,
    // but the browser's advertised capture limit is 4,000; its fallback must
    // therefore retrieve the older 500 lines from tmux instead of treating
    // them as permanently lost history.
    await scroller.evaluate((el) => {
      el.scrollTop = 0;
      el.dispatchEvent(new Event("scroll"));
    });
    await expect
      .poll(
        () =>
          scroller.evaluate((el) => {
            const top = el.scrollTop;
            const bottom = top + el.clientHeight;
            return Array.from(el.querySelectorAll("[data-live-content] > div"))
              .filter((row): row is HTMLElement => row instanceof HTMLElement)
              .filter((row) => row.offsetTop >= top && row.offsetTop < bottom)
              .map((row) => row.textContent ?? "")
              .join("|");
          }),
        { timeout: 10_000 },
      )
      .toContain("scrollline 1");
  } finally {
    await serve.stop();
  }
});
