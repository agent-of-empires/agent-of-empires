// Structured view cancel.
//
// `POST /api/sessions/:id/acp/cancel` forwards a `session/cancel`
// notification to the live ACP agent, which is expected to emit
// `stopped { reason: "cancelled" }` mid-turn so the UI can clear its
// spinner.
//
// Skipped pending #1237. This spec needs an in-flight prompt to cancel,
// and the prompt-side path currently surfaces
// `AgentStartupError { message: "ACP connection failed: Authentication
// required" }` between UserPromptSent and the scripted update emission.
// Unskip once #1237 is resolved.

import { mkdtempSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { test as base, expect } from "@playwright/test";
import {
  spawnAoeServe,
  listSessions,
  seedSessionViaAoeAdd,
} from "../helpers/aoeServe";
import {
  enableStructuredViewAndWait,
  waitForReplayContains,
} from "../helpers/acp";

const SLOW_TURN_SCRIPT = {
  turns: [
    {
      updates: [
        {
          sessionUpdate: "agent_message_chunk",
          content: { type: "text", text: "Thinking..." },
        },
        // The cancel notification should land between this chunk and
        // the final stop. The fake agent's `session/cancel` handler
        // emits `stopped { stopReason: "cancelled" }`, but only when
        // the prompt path actually reaches the agent.
      ],
      stopReason: "end_turn",
    },
  ],
};

base.skip(
  "structured view/cancel publishes Stopped reason:cancelled mid-turn",
  async ({}, testInfo) => {
    const scriptDir = mkdtempSync(join(tmpdir(), "aoe-pw-cancel-"));
    const scriptPath = join(scriptDir, "script.json");
    writeFileSync(scriptPath, JSON.stringify(SLOW_TURN_SCRIPT));

    const serve = await spawnAoeServe({
      authMode: "none",
      acp: true,
      fakeAcpScript: scriptPath,
      workerIndex: testInfo.workerIndex,
      parallelIndex: testInfo.parallelIndex,
      seedFn: seedSessionViaAoeAdd({ title: "acp-cancel" }),
    });

    try {
      const sessions = await listSessions(serve.baseUrl);
      const sessionId = sessions[0]!.id;

      await enableStructuredViewAndWait(serve.baseUrl, sessionId);

      await fetch(`${serve.baseUrl}/api/sessions/${sessionId}/acp/prompt`, {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({ text: "long-running thought" }),
      });

      const cancelRes = await fetch(
        `${serve.baseUrl}/api/sessions/${sessionId}/acp/cancel`,
        { method: "POST" },
      );
      expect(cancelRes.status).toBe(202);

      await waitForReplayContains(serve.baseUrl, sessionId, "cancelled");
    } finally {
      await serve.stop();
    }
  },
);
