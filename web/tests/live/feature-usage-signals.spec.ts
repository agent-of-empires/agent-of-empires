// Feature-usage telemetry signals (#1881).
//
// #1880 shipped the allowlisted usage_seen registry; this pins the three
// dashboard feature opens that land on it: diff_panel (the diff panel is
// opened for a session), web_terminal (the xterm.js terminal connects), and
// the opted-out/read-only short-circuit. diff_comments needs a live structured view
// worker to accept the prompt, so it is covered by the mocked send-flow spec
// (tests/diff-comments.spec.ts) instead.

import { spawnSync } from "node:child_process";
import { join } from "node:path";
import { test, expect } from "../helpers/liveTest";
import { spawnAoeServe, resolveAoeBinary } from "../helpers/aoeServe";
import { commitAll, initWorkingRepo, writeFiles } from "../helpers/gitFixture";

/** Capture every `POST /api/telemetry/seen` body, parsed into `{ surface }`.
 *  Attach before `page.goto` so the on-load pings are observed. */
function captureSeenPings(
  page: import("@playwright/test").Page,
): Array<{ surface?: string }> {
  const pings: Array<{ surface?: string }> = [];
  page.on("request", (req) => {
    if (req.method() === "POST" && req.url().includes("/api/telemetry/seen")) {
      const body = req.postData();
      if (!body) return;
      try {
        pings.push(JSON.parse(body));
      } catch {
        // Ignore unparseable bodies.
      }
    }
  });
  return pings;
}

test("opening a session fires the diff_panel and web_terminal signals", async ({
  page,
}, testInfo) => {
  const serve = await spawnAoeServe({
    authMode: "none",
    workerIndex: testInfo.workerIndex,
    parallelIndex: testInfo.parallelIndex,
    seedFn: ({ home, env }) => {
      const projectDir = join(home, "project");
      initWorkingRepo(projectDir);
      writeFiles(projectDir, { "src/a.ts": "export const a = 1;\n" });
      commitAll(projectDir, "baseline");
      // Uncommitted edit so the diff endpoint returns a file and the diff
      // panel has something to show.
      writeFiles(projectDir, { "src/a.ts": "export const a = 11;\n" });
      const addRes = spawnSync(
        resolveAoeBinary(),
        ["add", projectDir, "-t", "usage-signals", "-c", "claude"],
        { env },
      );
      if (addRes.status !== 0) {
        throw new Error(
          `aoe add failed: status=${addRes.status} stderr=${addRes.stderr?.toString() ?? "<none>"}`,
        );
      }
    },
  });
  try {
    const pings = captureSeenPings(page);
    await page.goto(`${serve.baseUrl}/`);
    const sessionRow = page
      .getByRole("link")
      .filter({ hasText: "usage-signals" })
      .first();
    await expect(sessionRow).toBeVisible({ timeout: 10_000 });
    await sessionRow.click();

    // Terminal connects on open -> web_terminal; diff panel mounts for the
    // session -> diff_panel. Both fire without any extra user action.
    await expect
      .poll(() => pings.some((p) => p.surface === "web_terminal"), {
        timeout: 10_000,
      })
      .toBe(true);
    await expect
      .poll(() => pings.some((p) => p.surface === "diff_panel"), {
        timeout: 10_000,
      })
      .toBe(true);
  } finally {
    await serve.stop();
  }
});

test("a read-only server fires no feature-usage signals", async ({
  serveReadOnly,
  page,
}) => {
  // The seen-ping guard skips read-only servers (they cannot persist a
  // snapshot), so none of the feature signals leave the browser.
  const pings = captureSeenPings(page);

  const aboutPromise = page.waitForResponse(
    (r) => r.url().endsWith("/api/about") && r.status() === 200,
    { timeout: 10_000 },
  );
  await page.goto(serveReadOnly.baseUrl);
  await aboutPromise;
  await page.waitForTimeout(500);

  expect(pings).toHaveLength(0);
});
