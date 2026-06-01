// User story: a cockpit session hits a provider rate limit and gets
// parked. The server maps the rate-limited stop to Idle, so the sidebar
// status glyph alone can't tell it apart from a normal idle session.
// After navigating back to the dashboard, the session row must carry a
// rate-limited indicator (Hourglass + reset time) sourced from the same
// persisted cockpit-state mirror the queued-prompt badge reads. See
// #1715.
//
// The fake ACP agent returns session/prompt as a `rate_limit` JSON-RPC
// error (errorKind "rate_limit" + resets_at), which drives the cockpit
// worker to emit a real RateLimit event end-to-end; no client-side state
// is hand-seeded.

import { mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { test as base, expect } from "@playwright/test";
import {
  spawnAoeServe,
  listSessions,
  seedSessionViaAoeAdd,
} from "../../helpers/aoeServe";
import {
  enableCockpitAndWait,
  waitForCockpitView,
  attachServeDiagnostics,
} from "../../helpers/cockpit";

// Reset time an hour out so the cockpit notice and sidebar chip both
// have a concrete "resets at" to render.
const RESETS_AT = new Date(Date.now() + 60 * 60 * 1000).toISOString();

const SCRIPT = {
  turns: [
    {
      updates: [
        {
          sessionUpdate: "agent_message_chunk",
          content: { type: "text", text: "Working on it." },
        },
      ],
      // The prompt returns a rate_limit error instead of a stopReason,
      // parking the session.
      rateLimit: { resets_at: RESETS_AT, message: "usage limit reached" },
    },
  ],
};

base("sidebar row shows a rate-limited indicator after a park", async ({ page }, testInfo) => {
  let serveHandle: { home: string } | undefined;
  let serve: Awaited<ReturnType<typeof spawnAoeServe>> | undefined;
  const scriptDir = mkdtempSync(join(tmpdir(), "aoe-pw-sidebar-rl-"));
  const scriptPath = join(scriptDir, "script.json");
  writeFileSync(scriptPath, JSON.stringify(SCRIPT));

  try {
    serve = await spawnAoeServe({
      authMode: "none",
      cockpit: true,
      fakeAcpScript: scriptPath,
      workerIndex: testInfo.workerIndex,
      parallelIndex: testInfo.parallelIndex,
      seedFn: seedSessionViaAoeAdd({ title: "sidebar-rl-a" }),
    });
    serveHandle = serve;

    const sessions = await listSessions(serve.baseUrl);
    const sessionA = sessions.find((s) => s.title === "sidebar-rl-a");
    if (!sessionA) throw new Error("seeded session 'sidebar-rl-a' missing");

    await enableCockpitAndWait(serve.baseUrl, sessionA.id, 30_000, serve.home);

    await page.goto(`${serve.baseUrl}/session/${encodeURIComponent(sessionA.id)}`);
    await waitForCockpitView(page);

    const composer = page.getByRole("textbox", {
      name: /Send a message|Queue a follow-up/i,
    });
    await composer.fill("kick off A");
    await composer.press("Enter");

    // The cockpit surfaces the rate-limit park as a system notice; wait
    // for it so we know the RateLimit event landed and was persisted
    // before navigating away.
    await expect(page.getByText(/Rate-limited/i)).toBeVisible({
      timeout: 15_000,
    });

    // Navigate to the dashboard so the sidebar is the primary surface
    // and the cockpit view unmounts.
    await page.goto(serve.baseUrl);

    // The row for session A carries the rate-limited indicator, read from
    // the persisted client-only rate-limit state.
    await expect(page.getByTitle(/Rate-limited/i)).toBeVisible({
      timeout: 15_000,
    });
  } finally {
    try {
      if (serveHandle) await attachServeDiagnostics(testInfo, serveHandle);
    } catch {
      // best-effort diagnostics; do not block cleanup
    }
    try {
      if (serve) await serve.stop();
    } finally {
      rmSync(scriptDir, { recursive: true, force: true });
    }
  }
});
