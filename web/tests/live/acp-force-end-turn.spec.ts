// Structured view force-end-turn escape hatch.
//
// `POST /api/sessions/:id/acp/force_end_turn` publishes a synthetic
// `Stopped { reason: "user_forced" }` directly to the event store and
// best-effort cancels any in-flight agent turn. The publish does not
// require a healthy ACP supervisor, so this spec runs cleanly even
// while #1237 keeps the prompt path parked.

import { test, expect } from "@playwright/test";
import {
  spawnAoeServe,
  listSessions,
  seedSessionViaAoeAdd,
} from "../helpers/aoeServe";
import { waitForReplayContains } from "../helpers/acp";

test("structured view/force_end_turn publishes a synthetic Stopped event", async ({}, testInfo) => {
  const serve = await spawnAoeServe({
    authMode: "none",
    acp: true,
    workerIndex: testInfo.workerIndex,
    parallelIndex: testInfo.parallelIndex,
    seedFn: seedSessionViaAoeAdd({ title: "acp-force-end" }),
  });

  try {
    const sessions = await listSessions(serve.baseUrl);
    const sessionId = sessions[0]!.id;

    // Flip to structured view so the supervisor is in scope; force_end_turn does
    // not require a healthy worker but does require the master switch
    // and a session that's been touched at least once.
    const enableRes = await fetch(
      `${serve.baseUrl}/api/sessions/${sessionId}/acp/enable`,
      { method: "POST" },
    );
    expect(enableRes.ok).toBeTruthy();

    const forceRes = await fetch(
      `${serve.baseUrl}/api/sessions/${sessionId}/acp/force_end_turn`,
      { method: "POST" },
    );
    expect(forceRes.status).toBe(202);

    await waitForReplayContains(serve.baseUrl, sessionId, "user_forced");
  } finally {
    await serve.stop();
  }
});
