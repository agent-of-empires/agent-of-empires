// User story: queue follow-up prompts on a structured view session mid-turn,
// navigate away to the sidebar (dashboard) view, and see a count badge
// on that session's row.
//
// Single acp-enabled session: kick off a long turn, queue two
// follow-ups via the composer's Queue button, then navigate to `/` so
// the sidebar is the primary surface and StructuredView unmounts. The
// queued prompts are client-only state mirrored into localStorage by
// persistState, so the sidebar row reads the count (2) without the
// structured view mounted. The queue does not drain while no structured view hook
// is running, so the badge is stable on the dashboard.

import { mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { test as base, expect } from "@playwright/test";
import { spawnAoeServe, listSessions, seedSessionViaAoeAdd } from "../../helpers/aoeServe";
import { enableStructuredViewAndWait, waitForStructuredView, attachServeDiagnostics } from "../../helpers/acp";

const SCRIPT = {
  turns: [
    {
      updates: [
        {
          sessionUpdate: "agent_message_chunk",
          content: { type: "text", text: "First turn." },
        },
        // Keep turn 1 in flight long enough to queue two follow-ups and
        // navigate; well under the structured view supervisor idle watchdog.
        { sessionUpdate: "wait_ms", ms: 6_000 },
      ],
      stopReason: "end_turn",
    },
  ],
};

base("sidebar row shows the queued-prompt count badge", async ({ page }, testInfo) => {
  let serveHandle: { home: string } | undefined;
  let serve: Awaited<ReturnType<typeof spawnAoeServe>> | undefined;
  const scriptDir = mkdtempSync(join(tmpdir(), "aoe-pw-sidebar-queue-"));
  const scriptPath = join(scriptDir, "script.json");
  writeFileSync(scriptPath, JSON.stringify(SCRIPT));

  try {
    serve = await spawnAoeServe({
      authMode: "none",
      acp: true,
      fakeAcpScript: scriptPath,
      workerIndex: testInfo.workerIndex,
      parallelIndex: testInfo.parallelIndex,
      seedFn: seedSessionViaAoeAdd({ title: "sidebar-queue-a" }),
    });
    serveHandle = serve;

    const sessions = await listSessions(serve.baseUrl);
    const sessionA = sessions.find((s) => s.title === "sidebar-queue-a");
    if (!sessionA) throw new Error("seeded session 'sidebar-queue-a' missing");

    await enableStructuredViewAndWait(serve.baseUrl, sessionA.id, 30_000, serve.home);

    await page.goto(`${serve.baseUrl}/session/${encodeURIComponent(sessionA.id)}`);
    await waitForStructuredView(page);

    const composer = page.getByRole("textbox", {
      name: /Send a message|Queue a follow-up/i,
    });
    await composer.fill("kick off A");
    await composer.press("Enter");
    await expect(page.getByText("First turn.")).toBeVisible({
      timeout: 10_000,
    });

    // The Queue button only renders while the turn is active.
    const queueBtn = page.getByRole("button", {
      name: /Queue follow-up message/i,
    });
    await expect(queueBtn).toBeVisible({ timeout: 5_000 });
    await composer.fill("follow-up one");
    await queueBtn.click();
    await composer.fill("follow-up two");
    await queueBtn.click();

    // Navigate to the dashboard so the sidebar is the primary surface.
    await page.goto(serve.baseUrl);

    // The row for session A should carry a "2 queued" badge, read from
    // the persisted client-only queue state.
    await expect(page.getByTitle("2 queued prompts")).toBeVisible({
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
