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
const HELD_TURN_SCRIPT = {
  turns: [
    {
      updates: [
        { sessionUpdate: "agent_message_chunk", content: { type: "text", text: "working" } },
        { sessionUpdate: "wait_ms", ms: 4000 },
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

test("a mid-turn prompt is still rejected when the agent cannot be steered", async ({}, testInfo) => {
  const scriptDir = mkdtempSync(join(tmpdir(), "aoe-pw-nosteer-"));
  const scriptPath = join(scriptDir, "script.json");
  writeFileSync(scriptPath, JSON.stringify(HELD_TURN_SCRIPT));

  // Same fixture with steering off, so the only difference between the
  // two specs is the capability. Guards the fallback: agents without
  // steering must keep today's behavior, since their users rely on the
  // composer's client-side queue-after.
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

    await postPrompt(serve.baseUrl, sessionId, "also check the tests");
    await waitForReplayContains(serve.baseUrl, sessionId, "agent_busy");
  } finally {
    await serve.stop();
  }
});
