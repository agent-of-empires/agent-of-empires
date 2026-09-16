// Feature-usage telemetry signals (#1881).
//
// #1880 shipped the allowlisted usage_seen registry; this pins the three
// dashboard feature opens that land on it: diff_panel (the diff panel is
// opened for a session), web_terminal (the live terminal connects), and
// the opted-out/read-only short-circuit. diff_comments needs a live structured view
// worker to accept the prompt, so it is covered by the mocked send-flow spec
// (tests/diff-comments.spec.ts) instead.

import { spawnSync } from "node:child_process";
import { join } from "node:path";
import { test, expect } from "../helpers/liveTest";
import { spawnAoeServe, resolveAoeBinary } from "../helpers/aoeServe";
import { commitAll, initWorkingRepo, writeFiles } from "../helpers/gitFixture";

async function captureSeenPings(page: import("@playwright/test").Page) {
  await page.addInitScript(() => {
    const w = window as unknown as { __seenPings: Array<{ surface?: string }> };
    w.__seenPings = [];
    const original = window.fetch;
    window.fetch = (...args) => {
      if (String(args[0]).endsWith("/api/telemetry/seen") && args[1]?.method === "POST") {
        w.__seenPings.push(JSON.parse(String(args[1].body)));
      }
      return original(...args);
    };
  });
  return () => page.evaluate(() => (window as unknown as { __seenPings: Array<{ surface?: string }> }).__seenPings);
}

test("opening a session fires the diff_panel and web_terminal signals", async ({ page }, testInfo) => {
  const serve = await spawnAoeServe({
    authMode: "none",
    workerIndex: testInfo.workerIndex,
    parallelIndex: testInfo.parallelIndex,
    seedFn: ({ home, env }) => {
      const projectDir = join(home, "project");
      initWorkingRepo(projectDir, env);
      writeFiles(projectDir, { "src/a.ts": "export const a = 1;\n" });
      commitAll(projectDir, "baseline", env);
      // Uncommitted edit so the diff endpoint returns a file and the diff
      // panel has something to show.
      writeFiles(projectDir, { "src/a.ts": "export const a = 11;\n" });
      const addRes = spawnSync(resolveAoeBinary(), ["add", projectDir, "-t", "usage-signals", "-c", "claude"], { env });
      if (addRes.status !== 0) {
        throw new Error(`aoe add failed: status=${addRes.status} stderr=${addRes.stderr?.toString() ?? "<none>"}`);
      }
    },
  });
  try {
    const pings = await captureSeenPings(page);
    await page.goto(`${serve.baseUrl}/`);
    const sessionRow = page.getByRole("link").filter({ hasText: "usage-signals" }).first();
    await expect(sessionRow).toBeVisible({ timeout: 10_000 });
    await sessionRow.click();

    // Terminal connects on open -> web_terminal; diff panel mounts for the
    // session -> diff_panel. Both fire without any extra user action.
    await expect
      .poll(async () => (await pings()).some((p) => p.surface === "web_terminal"), {
        timeout: 10_000,
      })
      .toBe(true);
    await expect
      .poll(async () => (await pings()).some((p) => p.surface === "diff_panel"), {
        timeout: 10_000,
      })
      .toBe(true);
  } finally {
    await serve.stop();
  }
});

test("a read-only server fires no feature-usage signals", async ({ serveReadOnly, page }) => {
  // The seen-ping guard skips read-only servers (they cannot persist a
  // snapshot), so none of the feature signals leave the browser.
  const pings = await captureSeenPings(page);

  const aboutPromise = page.waitForResponse((r) => r.url().endsWith("/api/about") && r.status() === 200, {
    timeout: 10_000,
  });
  await page.goto(serveReadOnly.baseUrl);
  await aboutPromise;
  await expect(page.getByText("This dashboard is in read-only mode.")).toBeVisible();

  expect(await pings()).toHaveLength(0);
});
