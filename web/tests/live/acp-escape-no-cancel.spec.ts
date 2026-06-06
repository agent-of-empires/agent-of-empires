// Structured view Escape does not cancel the active turn.
//
// Regression guard for the structured view composer's `cancelOnEscape={false}`
// wiring on ComposerPrimitive.Input. assistant-ui's default Escape
// binding calls runtime.cancelRun, which in the structured view funnels through
// onCancel into POST /api/sessions/:id/acp/cancel. We disabled that
// binding so accidental Escape presses cannot abort an in-flight turn
// while the user is typing the next prompt.
//
// Skipped pending #1237: the supervisor's ACP handshake against the
// fake agent fails with "Authentication required" before a turn can
// start, so we cannot reach the turn-active branch of the composer
// from the browser. Unskip alongside the sibling structured view specs
// (acp-spawn-prompt, acp-mode-switch, acp-approval) once
// the harness installs a working `claude-agent-acp` shim.

import { test as base, expect } from "@playwright/test";
import {
  spawnAoeServe,
  listSessions,
  seedSessionViaAoeAdd,
} from "../helpers/aoeServe";
import { enableStructuredViewAndWait } from "../helpers/acp";

base.skip(
  "Escape inside the structured view composer does not POST /acp/cancel",
  async ({ page }, testInfo) => {
    const serve = await spawnAoeServe({
      authMode: "none",
      acp: true,
      workerIndex: testInfo.workerIndex,
      parallelIndex: testInfo.parallelIndex,
      seedFn: seedSessionViaAoeAdd({ title: "escape-no-cancel" }),
    });

    try {
      const sessions = await listSessions(serve.baseUrl);
      const sessionId: string = sessions[0]!.id;

      await enableStructuredViewAndWait(serve.baseUrl, sessionId);

      // Send a prompt so the agent enters turn-active.
      const promptRes = await fetch(
        `${serve.baseUrl}/api/sessions/${sessionId}/acp/prompt`,
        {
          method: "POST",
          headers: { "Content-Type": "application/json" },
          body: JSON.stringify({ text: "stay in the turn" }),
        },
      );
      expect(promptRes.ok).toBeTruthy();

      // Track any POST to /acp/cancel emitted by the page after this
      // point. If the regression returns, our keypress below produces
      // exactly one such request.
      let cancelCount = 0;
      page.on("request", (req) => {
        if (
          req.method() === "POST" &&
          req.url().includes(`/api/sessions/${sessionId}/acp/cancel`)
        ) {
          cancelCount += 1;
        }
      });

      await page.goto(`${serve.baseUrl}/sessions/${sessionId}`);

      // Focus the composer textarea and press Escape. The composer
      // mounts the assistant-ui ComposerPrimitive.Input with
      // cancelOnEscape={false}; the keystroke should be a no-op.
      const composer = page.getByRole("textbox", {
        name: /Send a message|Queue a follow-up/i,
      });
      await composer.focus();
      await page.keyboard.press("Escape");
      // Hold for a tick so any cancel fetch has time to fire.
      await page.waitForTimeout(500);

      expect(cancelCount).toBe(0);
    } finally {
      await serve.stop();
    }
  },
);
