// Structured view spawn + prompt happy path.
//
// Seeds a session via `aoe add` BEFORE serve boots (`seedFn`), with the
// fake ACP agent on PATH as both `claude` and `aoe-agent`. After boot,
// the spec enables structured view per-session, spawns the structured view worker,
// sends a prompt, and asserts the replay endpoint surfaces the scripted
// `agent_message_chunk`.

import { test as base, expect } from "@playwright/test";
import {
  spawnAoeServe,
  listSessions,
  seedSessionViaAoeAdd,
} from "../helpers/aoeServe";
import { enableStructuredViewAndWait, waitForReplayContains } from "../helpers/acp";

base("structured view spawn + prompt round-trip emits an agent_message_chunk", async ({}, testInfo) => {
  const serve = await spawnAoeServe({
    authMode: "none",
    acp: true,
    workerIndex: testInfo.workerIndex,
    parallelIndex: testInfo.parallelIndex,
    seedFn: seedSessionViaAoeAdd({ title: "acp-trace" }),
  });

  try {
    const sessions = await listSessions(serve.baseUrl);
    expect(sessions.length).toBeGreaterThan(0);
    const sessionId: string = sessions[0]!.id;

    // `structured view/enable` flips the per-session structured_view flag AND
    // implicitly spawns the structured view supervisor via tokio::spawn. A
    // follow-up explicit POST to /acp/spawn would 409 with
    // "already running", so we only call enable and let it own the
    // spawn lifecycle. `enableStructuredViewAndWait` POSTs enable, asserts a
    // 2xx, then waits for the ACP handshake (initialize + session/new)
    // to finish before returning.
    await enableStructuredViewAndWait(serve.baseUrl, sessionId);

    const promptRes = await fetch(
      `${serve.baseUrl}/api/sessions/${sessionId}/acp/prompt`,
      {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({ text: "hello structured view" }),
      },
    );
    expect(promptRes.status).toBeGreaterThanOrEqual(200);
    expect(promptRes.status).toBeLessThan(300);

    // Match either casing in case the wire format moves to snake_case
    // (frames currently serialize `event` as an externally-tagged enum,
    // keyed `AgentMessageChunk`; src/server/api/acp.rs::acp_replay).
    await waitForReplayContains(serve.baseUrl, sessionId, [
      "agent_message_chunk",
      "AgentMessageChunk",
    ]);
  } finally {
    await serve.stop();
  }
});
