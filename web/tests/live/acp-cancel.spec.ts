// Structured view cancel.
//
// `POST /api/sessions/:id/acp/cancel` forwards a `session/cancel`
// notification to the live ACP agent, which is expected to emit
// `stopped { reason: "cancelled" }` mid-turn so the UI can clear its
// spinner.
//
import { mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { test as base, expect } from "@playwright/test";
import { spawnAoeServe, listSessions, seedSessionViaAoeAdd } from "../helpers/aoeServe";
import { enableStructuredViewAndWait, waitForReplayContains } from "../helpers/acp";

const SLOW_TURN_SCRIPT = {
  turns: [
    {
      updates: [
        {
          sessionUpdate: "agent_message_chunk",
          content: { type: "text", text: "Thinking..." },
        },
        { sessionUpdate: "wait_for_release" },
        { sessionUpdate: "agent_message_chunk", content: { type: "text", text: "MUST_NOT_COMPLETE" } },
      ],
      stopReason: "end_turn",
    },
  ],
};

base("structured view/cancel publishes Stopped reason:cancelled mid-turn", async ({}, testInfo) => {
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

    await waitForReplayContains(serve.baseUrl, sessionId, "Thinking...");
    const cancelRes = await fetch(`${serve.baseUrl}/api/sessions/${sessionId}/acp/cancel`, { method: "POST" });
    expect(cancelRes.status).toBe(202);

    await waitForReplayContains(serve.baseUrl, sessionId, '"reason":"cancelled"');
    const replay = await fetch(`${serve.baseUrl}/api/sessions/${sessionId}/acp/replay?since=0`).then((r) => r.json());
    expect(JSON.stringify(replay)).not.toContain("MUST_NOT_COMPLETE");
  } finally {
    await serve.stop();
    rmSync(scriptDir, { recursive: true, force: true });
  }
});
