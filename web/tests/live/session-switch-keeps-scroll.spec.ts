// Regression (Viktor, 10-11 Nov 2026): "clicking a session scrolls away and
// renders wrong". Reading history in one session, opening another, and
// clicking back must land on the same lines, rendered as whole rows, on
// desktop and on a phone. Drives a real `aoe serve` + tmux with two fake
// agents that dump numbered lines into tmux scrollback and idle.
import { devices, type Page, type Locator } from "@playwright/test";
import { join } from "node:path";
import { writeFileSync, chmodSync, mkdirSync } from "node:fs";
import { spawnSync } from "node:child_process";
import { test, expect } from "../helpers/liveTest";
import { spawnAoeServe, resolveAoeBinary } from "../helpers/aoeServe";
import { clickSidebarSession, openMobileSidebar } from "../helpers/sidebar";

const SHOTS = process.env.AOE_REPRO_SHOTS;

function seedSession(e: { shimBin: string; home: string; env: NodeJS.ProcessEnv }, name: string, prefix: string) {
  const tool = join(e.shimBin, `dump-${prefix}`);
  writeFileSync(
    tool,
    `#!/bin/bash
for i in $(seq 1 1500); do echo "${prefix} $i"; done
echo "${prefix} READY"
while true; do sleep 1; done
`,
  );
  chmodSync(tool, 0o755);
  const pd = join(e.home, `project-${prefix}`);
  mkdirSync(pd, { recursive: true });
  spawnSync("git", ["init", "-q"], { cwd: pd });
  const r = spawnSync(resolveAoeBinary(), ["add", pd, "-t", name, "-c", "claude", "--cmd-override", tool], {
    env: e.env,
  });
  if (r.status !== 0) throw new Error(String(r.stderr));
}

/** Rows whose top edge is inside the active pane's viewport. */
async function visibleRows(scroller: Locator): Promise<string[]> {
  return scroller.evaluate((el) => {
    const top = el.scrollTop;
    const bottom = top + el.clientHeight;
    return Array.from(el.querySelectorAll("[data-live-content] > div"))
      .filter((row): row is HTMLElement => row instanceof HTMLElement)
      .filter((row) => row.offsetTop >= top && row.offsetTop + row.offsetHeight <= bottom + 1)
      .map((row) => (row.textContent ?? "").trimEnd())
      .filter((t) => t.length > 0);
  });
}

function activeScroller(page: Page): Locator {
  return page.locator('[aria-hidden="false"] [data-live-terminal] > div, main [data-live-terminal] > div').first();
}

async function openSession(page: Page, title: string, mobile: boolean) {
  if (mobile) await openMobileSidebar(page);
  await clickSidebarSession(page, title);
}

for (const form of ["desktop", "phone"] as const) {
  test(`clicking a session scrolls away and renders wrong (${form})`, async ({ browser }, testInfo) => {
    test.setTimeout(120_000);
    const mobile = form === "phone";
    const serve = await spawnAoeServe({
      authMode: "none",
      workerIndex: testInfo.workerIndex,
      parallelIndex: testInfo.parallelIndex,
      seedFn: (e) => {
        seedSession(e, "alpha-reader", "alpha");
        seedSession(e, "beta-other", "beta");
      },
    });
    try {
      const ctx = await browser.newContext(
        mobile ? { ...devices["iPhone 13"] } : { viewport: { width: 1400, height: 900 } },
      );
      // The default "Active" list drops an idle session once it is no longer
      // the selected one; this test is about the terminal, so list them all.
      await ctx.addInitScript(() => {
        localStorage.setItem("aoe-web-settings", JSON.stringify({ sidebarActiveOnly: false }));
      });
      const page = await ctx.newPage();
      await page.goto(serve.baseUrl);
      const shot = async (label: string) => {
        if (SHOTS) await page.screenshot({ path: join(SHOTS, `${form}-${label}.png`) });
      };

      await openSession(page, "alpha-reader", mobile);
      await page
        .locator("[data-live-content]")
        .filter({ hasText: "alpha READY" })
        .first()
        .waitFor({ state: "attached", timeout: 30_000 });
      await page.waitForTimeout(1200);
      await shot("1-alpha-live");

      // Scroll up to read history, the way a wheel or a swipe would.
      const alpha = activeScroller(page);
      await alpha.evaluate((el) => {
        el.scrollTop = Math.max(0, el.scrollTop - el.clientHeight * 3);
        el.dispatchEvent(new Event("scroll"));
      });
      await page.waitForTimeout(1500);
      const before = await visibleRows(activeScroller(page));
      await shot("2-alpha-reading");
      expect(before.join("|")).not.toContain("alpha READY");
      expect(before.length).toBeGreaterThan(5);

      await openSession(page, "beta-other", mobile);
      await page
        .locator("[data-live-content]")
        .filter({ hasText: "beta READY" })
        .first()
        .waitFor({ state: "attached", timeout: 30_000 });
      await page.waitForTimeout(800);
      await shot("3-beta");

      await openSession(page, "alpha-reader", mobile);
      // The history text may take one capture round trip to arrive (the
      // restored rows are blank until then), but the view must never show the
      // live tail or any other lines, at once or a moment later.
      const samples: string[][] = [];
      const deadline = Date.now() + 5000;
      while (samples.length < 3 && Date.now() < deadline) {
        await page.waitForTimeout(samples.length === 0 ? 100 : 700);
        const rows = await visibleRows(activeScroller(page));
        if (rows.length === 0 && samples.length === 0) continue;
        samples.push(rows);
        await shot(`4-alpha-back-${samples.length}`);
      }
      expect(samples.length, "the reader's lines arrive").toBe(3);
      for (const [i, rows] of samples.entries()) {
        // Renders right: every visible row is one whole numbered line.
        for (const row of rows) expect(row, `sample ${i}: row renders as one whole line`).toMatch(/^alpha \d+$/);
        // Keeps the place: same first line the reader left.
        expect(rows[0], `sample ${i}: back where the reader was (${rows.length} rows)`).toBe(before[0]);
      }
    } finally {
      await serve.stop();
    }
  });
}
