// Structured view context-primer.
//
// `GET /api/sessions/:id/acp/context-primer?before_seq=N` builds a
// markdown recap from `UserPromptSent` + `Stopped` turn boundaries
// already in the SQLite event store. The handler reads straight from
// `acp_event_store.replay_before(...)` without going through the
// ACP supervisor, so this spec runs cleanly while #1237 keeps the
// prompt path parked.
//
// Seeding strategy: `POST /acp/prompt` calls `publish_user_prompt`
// BEFORE forwarding to the supervisor, so the `UserPromptSent` event
// lands in the event store even when `send_prompt` later 404s. Pairing
// it with `POST /acp/force_end_turn` (synthetic `Stopped`) gives a
// complete turn the primer can render.

import { test, expect } from "@playwright/test";
import {
  spawnAoeServe,
  listSessions,
  seedSessionViaAoeAdd,
} from "../helpers/aoeServe";

const PRIMER_TEXT = "primer-fixture-prompt-1224";

test("structured view/context-primer renders the seeded turn", async ({}, testInfo) => {
  const serve = await spawnAoeServe({
    authMode: "none",
    acp: true,
    workerIndex: testInfo.workerIndex,
    parallelIndex: testInfo.parallelIndex,
    seedFn: seedSessionViaAoeAdd({ title: "acp-primer" }),
  });

  try {
    const sessions = await listSessions(serve.baseUrl);
    const sessionId = sessions[0]!.id;

    await fetch(
      `${serve.baseUrl}/api/sessions/${sessionId}/acp/enable`,
      { method: "POST" },
    );

    // publish_user_prompt is called BEFORE send_prompt forwarding, so
    // UserPromptSent lands in the event store regardless of whether
    // send_prompt succeeds or 404s due to no live worker.
    await fetch(
      `${serve.baseUrl}/api/sessions/${sessionId}/acp/prompt`,
      {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({ text: PRIMER_TEXT }),
      },
    );
    // Synthetic Stopped closes the turn boundary so the primer renders
    // it as a complete turn rather than a half-finished one.
    await fetch(
      `${serve.baseUrl}/api/sessions/${sessionId}/acp/force_end_turn`,
      { method: "POST" },
    );

    let highestSeq = 0;
    await expect
      .poll(
        async () => {
          const replay = await fetch(
            `${serve.baseUrl}/api/sessions/${sessionId}/acp/replay?since=0`,
          ).then((r) => r.json());
          const json = JSON.stringify(replay.frames);
          if (
            json.includes(PRIMER_TEXT) &&
            json.includes("user_forced") &&
            replay.highest_seq !== null
          ) {
            highestSeq = replay.highest_seq;
            return true;
          }
          return false;
        },
        { timeout: 15_000, intervals: [100, 200, 500, 1000] },
      )
      .toBe(true);
    expect(highestSeq).toBeGreaterThan(0);

    const primerRes = await fetch(
      `${serve.baseUrl}/api/sessions/${sessionId}/acp/context-primer?before_seq=${highestSeq + 1}`,
    );
    expect(primerRes.ok).toBeTruthy();
    const primer = (await primerRes.json()) as {
      primer: string;
      included_event_count: number;
      included_turn_count: number;
      truncated: boolean;
      max_chars: number;
    };
    expect(primer.included_event_count).toBeGreaterThan(0);
    expect(primer.included_turn_count).toBeGreaterThanOrEqual(1);
    expect(primer.primer).toContain(PRIMER_TEXT);
    expect(primer.max_chars).toBeGreaterThan(0);
  } finally {
    await serve.stop();
  }
});
