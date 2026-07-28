// Live: a codex `/new` drives a REAL conversation reset (#2979).
//
// codex-acp has no native `/new` (upstream codex-acp#317), so forwarding
// the raw text used to be swallowed as an unknown command while the UI
// rendered a "Conversation cleared" boundary the model never honored.
// The server now opens a fresh `session/new` on the live worker and swaps
// the stored acp_session_id. This spec drives the production path against
// a real `aoe serve` + the fake codex ACP agent and asserts the observable
// contract on the event stream:
//
//   - the clear boundary (`SessionCleared`) still renders,
//   - the reset boundary (`SessionContextReset`) fires,
//   - a SECOND `AcpSessionAssigned` lands with a NEW acp session id
//     (proof the worker ran a fresh session/new rather than replaying
//     the old conversation),
//   - the clear turn terminates with `Stopped(session_reset)`.
//
// API-only (no page): the reducer-side tracker behavior is unit-tested in
// `src/lib/acpTypes.test.ts`; the Rust reset mechanics in `src/acp/`.

import { test, expect } from "@playwright/test";
import { spawnAoeServe, listSessions, seedSessionViaAoeAdd } from "../helpers/aoeServe";

test("codex /new opens a fresh session/new and swaps the acp session id", async ({}, testInfo) => {
  const serve = await spawnAoeServe({
    authMode: "none",
    acp: true,
    workerIndex: testInfo.workerIndex,
    parallelIndex: testInfo.parallelIndex,
    seedFn: seedSessionViaAoeAdd({ title: "codex-new-reset", tool: "codex" }),
  });

  try {
    const sessions = await listSessions(serve.baseUrl);
    const sessionId = sessions[0]!.id;

    const replayFrames = async (): Promise<unknown[]> => {
      const replay = await fetch(`${serve.baseUrl}/api/sessions/${sessionId}/acp/replay?since=0`).then((r) => r.json());
      return replay.frames ?? [];
    };
    const assignedIds = async (): Promise<string[]> => {
      const ids: string[] = [];
      for (const f of await replayFrames()) {
        const ev = (f as { event?: Record<string, { acp_session_id?: string }> }).event;
        if (ev && typeof ev === "object" && "AcpSessionAssigned" in ev) {
          const id = ev.AcpSessionAssigned?.acp_session_id;
          if (id) ids.push(id);
        }
      }
      return ids;
    };

    await fetch(`${serve.baseUrl}/api/sessions/${sessionId}/acp/enable`, { method: "POST" });

    // Handshake oracle: the first AcpSessionAssigned frame proves the fake
    // codex worker is live and its initial session/new completed.
    await expect
      .poll(async () => (await assignedIds()).length, { timeout: 45_000, intervals: [500] })
      .toBeGreaterThan(0);
    const firstId = (await assignedIds())[0]!;

    // `/new` goes through the ordinary prompt endpoint; the server-side
    // clear detection reroutes it onto the driven-reset path. The assigned
    // session id above proves the worker is ready, so submit this stateful
    // request exactly once.
    const resetResponse = await fetch(`${serve.baseUrl}/api/sessions/${sessionId}/acp/prompt`, {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ text: "/new" }),
    });
    expect(resetResponse.status).toBe(202);

    // The reset's full event contract, in one poll: clear boundary, reset
    // boundary, a second assigned id, and the terminal Stopped.
    await expect
      .poll(
        async () => {
          const json = JSON.stringify(await replayFrames());
          return (
            json.includes('"SessionCleared"') &&
            json.includes("SessionContextReset") &&
            json.includes("session_reset") &&
            (await assignedIds()).length >= 2
          );
        },
        { timeout: 45_000, intervals: [500] },
      )
      .toBe(true);

    const ids = await assignedIds();
    const freshId = ids[ids.length - 1]!;
    expect(freshId, "the reset must mint a NEW acp session id via session/new").not.toBe(firstId);

    // The raw alias must not have been forwarded as a prompt: the fake
    // codex agent echoes scripted turns for real prompts, so a forwarded
    // "/new" would have produced an assistant turn instead of the reset
    // triple asserted above. Belt-and-suspenders: no agent message chunk
    // may reference the swallowed-unknown-command path.
    const json = JSON.stringify(await replayFrames());
    expect(json).not.toContain("unknown command");
  } finally {
    await serve.stop();
  }
});
