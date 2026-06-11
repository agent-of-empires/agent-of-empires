// User story: Stop button cancels an active turn.
//
// Custom FAKE_ACP_SCRIPT keeps the turn alive with a wait_ms gap after
// the first chunk so the UI surfaces the Stop affordance long enough to
// click. Clicking Stop dispatches `runtime.cancelRun()` which POSTs
// /acp/cancel; the fake responds with stopped { cancelled } and the
// composer flips back to the idle "Send a message…" placeholder.

import { mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { test as base, expect } from "@playwright/test";
import { spawnAoeServe, listSessions, seedSessionViaAoeAdd } from "../../helpers/aoeServe";
import { waitForStructuredView, enableStructuredViewAndWait } from "../../helpers/acp";

const STOP_SCRIPT = {
  turns: [
    {
      updates: [
        {
          sessionUpdate: "agent_message_chunk",
          content: { type: "text", text: "Thinking..." },
        },
        // 30s holds the turn open longer than the test will ever wait
        // for either assertion, so the Stop affordance stays mounted
        // even on heavily loaded CI runners where the "Thinking..."
        // chunk and the click can be tens of seconds apart.
        { sessionUpdate: "wait_ms", ms: 30_000 },
        {
          sessionUpdate: "agent_message_chunk",
          content: { type: "text", text: "Should never appear." },
        },
      ],
      stopReason: "end_turn",
    },
  ],
};

base("Stop button cancels a running turn", async ({ page }, testInfo) => {
  const scriptDir = mkdtempSync(join(tmpdir(), "aoe-pw-story-stop-"));
  const scriptPath = join(scriptDir, "script.json");
  writeFileSync(scriptPath, JSON.stringify(STOP_SCRIPT));

  let serve: Awaited<ReturnType<typeof spawnAoeServe>> | undefined;

  try {
    serve = await spawnAoeServe({
      authMode: "none",
      acp: true,
      fakeAcpScript: scriptPath,
      workerIndex: testInfo.workerIndex,
      parallelIndex: testInfo.parallelIndex,
      seedFn: seedSessionViaAoeAdd({ title: "story-stop" }),
    });

    const sessions = await listSessions(serve.baseUrl);
    const seeded = sessions.find((s) => s.title === "story-stop");
    if (!seeded) throw new Error("seeded session 'story-stop' missing");
    const sessionId = seeded.id;

    await enableStructuredViewAndWait(serve.baseUrl, sessionId);

    await page.goto(`${serve.baseUrl}/session/${encodeURIComponent(sessionId)}`);
    await waitForStructuredView(page);

    const composer = page.getByRole("textbox", { name: /Send a message/i });
    await composer.fill("start a long turn");
    await composer.press("Enter");

    // First chunk arrives, then the fake waits. The Stop affordance is
    // mounted while the turn is active.
    await expect(page.getByText("Thinking...")).toBeVisible({
      timeout: 10_000,
    });
    const stopButton = page.getByRole("button", { name: "Stop" });
    await expect(stopButton).toBeVisible({ timeout: 5_000 });
    await stopButton.click();

    // Composer placeholder flips back to idle once the turn ends.
    await expect(page.getByRole("textbox", { name: /Send a message/i })).toBeVisible({ timeout: 10_000 });
    await expect(stopButton).toBeHidden({ timeout: 10_000 });
    // We do NOT assert that the post-wait chunk never renders. The fake
    // ACP harness is single-threaded JS and does not abort its
    // in-flight session/prompt loop when session/cancel arrives, so
    // the chunk after the wait_ms may still land after Stop. The
    // production server's cancel semantics are exercised by the
    // REST-level acp-cancel spec.
  } finally {
    try {
      if (serve) await serve.stop();
    } finally {
      rmSync(scriptDir, { recursive: true, force: true });
    }
  }
});
