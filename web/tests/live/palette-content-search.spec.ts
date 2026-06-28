// Conversation content search round-trip (#2515).
//
// Seeds a structured session, sends a prompt carrying a unique token, then
// asserts GET /api/sessions/search surfaces that session by its conversation
// content (the user prompt text), not by any title/metadata. The token is
// chosen so it cannot appear in the session title, branch, or path, so a hit
// proves the search reached the stored event content.

import { test as base, expect } from "@playwright/test";
import { spawnAoeServe, listSessions, seedSessionViaAoeAdd } from "../helpers/aoeServe";
import { enableStructuredViewAndWait, waitForReplayContains } from "../helpers/acp";

const NEEDLE = "xyzzycontentneedle";

base("search surfaces a session by its conversation content", async ({}, testInfo) => {
  const serve = await spawnAoeServe({
    authMode: "none",
    acp: true,
    workerIndex: testInfo.workerIndex,
    parallelIndex: testInfo.parallelIndex,
    seedFn: seedSessionViaAoeAdd({ title: "content-search" }),
  });

  try {
    const sessions = await listSessions(serve.baseUrl);
    expect(sessions.length).toBeGreaterThan(0);
    const sessionId: string = sessions[0]!.id;

    await enableStructuredViewAndWait(serve.baseUrl, sessionId);

    const promptRes = await fetch(`${serve.baseUrl}/api/sessions/${sessionId}/acp/prompt`, {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ text: `please remember ${NEEDLE} for later` }),
    });
    expect(promptRes.status).toBeGreaterThanOrEqual(200);
    expect(promptRes.status).toBeLessThan(300);

    // The prompt is recorded as a UserPromptSent event; wait until it lands.
    await waitForReplayContains(serve.baseUrl, sessionId, ["user_prompt_sent", "UserPromptSent"]);

    // Searching the unique token must return exactly this session.
    await expect
      .poll(
        async () => {
          const res = await fetch(`${serve.baseUrl}/api/sessions/search?q=${NEEDLE}`);
          if (!res.ok) return [];
          const body = await res.json();
          return (body.results ?? []).map((h: { session_id: string }) => h.session_id);
        },
        { timeout: 10_000 },
      )
      .toContain(sessionId);

    // A token that appears nowhere in the conversation returns no hit.
    const miss = await fetch(`${serve.baseUrl}/api/sessions/search?q=zzznevertypedthis`);
    const missBody = await miss.json();
    expect(missBody.results ?? []).toHaveLength(0);
  } finally {
    await serve.stop();
  }
});
