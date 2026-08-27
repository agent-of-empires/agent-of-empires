// Mid-turn steering (#2805).
//
// A prompt that reaches the daemon while a turn is running used to be
// refused with `PromptRejected { reason: "agent_busy" }`. Against an
// agent that advertises `_session/steering` (and clears the separate
// steering version floor) the daemon now hands it to the running turn
// instead, which is what the claude CLI does with typed-ahead input.
//
// Both specs drive the REST prompt endpoint rather than the composer:
// the daemon-side gate is what changed, and going through REST is also
// the `aoe acp prompt` path, which had no client-side queue to fall back
// on and so just failed before this.

import { mkdtempSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { test, expect } from "@playwright/test";
import { spawnAoeServe, listSessions, seedSessionViaAoeAdd } from "../helpers/aoeServe";
import { enableStructuredViewAndWait, waitForReplayContains } from "../helpers/acp";

// One turn that stays open long enough for a second prompt to land
// inside it. `wait_ms` is the fake agent's hold primitive; it sleeps in
// slices so a cancel is still observed promptly.
//
// The hold also has to outlast the assertions made inside the turn, not
// just the prompt that lands in it: the queued spec reads `/queue` back
// while the turn is still running, and a turn that ends first drains the
// queue to empty. Neither spec waits for the turn to end, so a generous
// hold costs no wall time; 4s lost that race on a loaded runner.
const HELD_TURN_SCRIPT = {
  turns: [
    {
      updates: [
        { sessionUpdate: "agent_message_chunk", content: { type: "text", text: "working" } },
        { sessionUpdate: "wait_ms", ms: 30_000 },
      ],
      stopReason: "end_turn",
    },
  ],
};

async function postPrompt(baseUrl: string, sessionId: string, text: string) {
  return fetch(`${baseUrl}/api/sessions/${sessionId}/acp/prompt`, {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify({ text }),
  });
}

async function replayJson(baseUrl: string, sessionId: string): Promise<string> {
  const replay = await fetch(`${baseUrl}/api/sessions/${sessionId}/acp/replay?since=0`).then((r) => r.json());
  const frames: unknown[] = Array.isArray(replay) ? replay : (replay?.frames ?? []);
  return JSON.stringify(frames);
}

test("a mid-turn prompt is steered into the running turn instead of rejected", async ({}, testInfo) => {
  const scriptDir = mkdtempSync(join(tmpdir(), "aoe-pw-steer-"));
  const scriptPath = join(scriptDir, "script.json");
  writeFileSync(scriptPath, JSON.stringify(HELD_TURN_SCRIPT));

  const serve = await spawnAoeServe({
    authMode: "none",
    acp: true,
    fakeAcpScript: scriptPath,
    extraEnv: { FAKE_ACP_STEERING: "1" },
    workerIndex: testInfo.workerIndex,
    parallelIndex: testInfo.parallelIndex,
    seedFn: seedSessionViaAoeAdd({ title: "acp-steering" }),
  });

  try {
    const sessionId = (await listSessions(serve.baseUrl))[0]!.id;
    await enableStructuredViewAndWait(serve.baseUrl, sessionId);

    // The capability has to reach the event stream, otherwise the daemon
    // would take the reject path and this spec would pass for the wrong
    // reason.
    await waitForReplayContains(serve.baseUrl, sessionId, '"steering":true');

    await postPrompt(serve.baseUrl, sessionId, "start the turn");
    await waitForReplayContains(serve.baseUrl, sessionId, "working");

    // Lands while the turn is held open by `wait_ms`.
    const second = await postPrompt(serve.baseUrl, sessionId, "also check the tests");
    expect(second.ok).toBe(true);

    // The fake echoes an injected steer back into the running turn, so
    // this text only appears if the daemon really sent
    // `_session/steering` and the agent accepted it.
    await waitForReplayContains(serve.baseUrl, sessionId, "steered: also check the tests");

    const json = await replayJson(serve.baseUrl, sessionId);
    expect(json).not.toContain("agent_busy");
  } finally {
    await serve.stop();
  }
});

test("a mid-turn prompt is queued, not rejected, when the agent cannot be steered", async ({}, testInfo) => {
  const scriptDir = mkdtempSync(join(tmpdir(), "aoe-pw-nosteer-"));
  const scriptPath = join(scriptDir, "script.json");
  writeFileSync(scriptPath, JSON.stringify(HELD_TURN_SCRIPT));

  // Same fixture with steering off, so the only difference between the two
  // specs is the capability. Guards the fallback: an agent without steering
  // must still get queue-after semantics. Since Tier 3 that is the daemon's
  // job, not the composer's, so the prompt reaches the endpoint and comes back
  // `queued` rather than being refused with `agent_busy`.
  const serve = await spawnAoeServe({
    authMode: "none",
    acp: true,
    fakeAcpScript: scriptPath,
    workerIndex: testInfo.workerIndex,
    parallelIndex: testInfo.parallelIndex,
    seedFn: seedSessionViaAoeAdd({ title: "acp-no-steering" }),
  });

  try {
    const sessionId = (await listSessions(serve.baseUrl))[0]!.id;
    await enableStructuredViewAndWait(serve.baseUrl, sessionId);

    await postPrompt(serve.baseUrl, sessionId, "start the turn");
    await waitForReplayContains(serve.baseUrl, sessionId, "working");

    const res = await postPrompt(serve.baseUrl, sessionId, "also check the tests");
    expect(res.status).toBe(202);
    const dispatch = (await res.json()) as { disposition?: string; reason?: string; queued_id?: string };
    expect(dispatch.disposition).toBe("queued");
    expect(dispatch.reason).toBe("turn_active");

    // Parked on the server queue, so the turn-end drain delivers it. The old
    // `agent_busy` rejection is gone: the daemon no longer refuses a prompt it
    // can hold onto.
    const queue = (await fetch(`${serve.baseUrl}/api/sessions/${sessionId}/queue`).then((r) => r.json())) as Array<{
      id: string;
      text: string;
    }>;
    expect(queue.map((q) => q.text)).toEqual(["also check the tests"]);
    expect(queue[0]!.id).toBe(dispatch.queued_id);
    expect(await replayJson(serve.baseUrl, sessionId)).not.toContain("agent_busy");
  } finally {
    await serve.stop();
  }
});
