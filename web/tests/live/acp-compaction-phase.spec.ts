// Compaction phase (#3219).
//
// `/compact` runs 90 to 170 seconds and the adapter emits nothing at all
// between its "Compacting..." and "Compacting completed." markers. Three
// things must hold for that window:
//
//   1. the spinner names the phase instead of relabelling to
//      "Waiting on model", which is the wedged-agent string;
//   2. the Force-end-turn hatch stays hidden, since publishing a
//      synthetic Stopped plus a session/cancel there aborts the
//      compaction (the daemon-side twin of that abort was #2898);
//   3. a follow-up parks in the queue instead of being steered into a
//      turn that only summarizes context and never answers it.
//
// The fake agent reproduces the real shape: the start marker, a held
// window, then the completion marker. `wait_ms` is its hold primitive.

import { mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { test, expect } from "@playwright/test";
import { spawnAoeServe, listSessions, seedSessionViaAoeAdd } from "../helpers/aoeServe";
import { enableStructuredViewAndWait, waitForReplayContains, waitForStructuredView } from "../helpers/acp";

// Only has to outlast posting the prompt plus typing the follow-up,
// which is sub-second once the composer is located by form name rather
// than accessible name. Kept small so the spec stays near the live tier's
// budget; the fake clamps `wait_ms` at 60s regardless.
const COMPACTION_HOLD_MS = 6_000;

const COMPACTING_SCRIPT = {
  turns: [
    {
      updates: [
        { sessionUpdate: "agent_message_chunk", content: { type: "text", text: "Compacting..." } },
        { sessionUpdate: "wait_ms", ms: COMPACTION_HOLD_MS },
        { sessionUpdate: "agent_message_chunk", content: { type: "text", text: "\n\nCompacting completed." } },
      ],
      stopReason: "end_turn",
    },
    {
      updates: [{ sessionUpdate: "agent_message_chunk", content: { type: "text", text: "answered after compaction" } }],
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

test("the spinner names the compaction phase and hides the force-end hatch", async ({ page }, testInfo) => {
  const scriptDir = mkdtempSync(join(tmpdir(), "aoe-pw-compact-ui-"));
  const scriptPath = join(scriptDir, "script.json");
  writeFileSync(scriptPath, JSON.stringify(COMPACTING_SCRIPT));

  const serve = await spawnAoeServe({
    authMode: "none",
    acp: true,
    fakeAcpScript: scriptPath,
    extraEnv: { FAKE_ACP_STEERING: "1" },
    workerIndex: testInfo.workerIndex,
    parallelIndex: testInfo.parallelIndex,
    seedFn: seedSessionViaAoeAdd({ title: "acp-compaction-ui" }),
  });

  try {
    const sessionId = (await listSessions(serve.baseUrl))[0]!.id;
    await enableStructuredViewAndWait(serve.baseUrl, sessionId);
    await page.goto(`${serve.baseUrl}/session/${encodeURIComponent(sessionId)}`);
    await waitForStructuredView(page);

    // Located by its stable form name, not by accessible name: the
    // composer's placeholder changes while a turn is running, so a
    // name-based locator silently blocks until the turn ends, which is
    // exactly the window this spec needs to type inside.
    const composer = page.locator('textarea[name="input"]');
    // Start the compaction over REST rather than through the composer.
    // Typing `/compact` opens the slash-command picker, whose
    // keyboard-selection semantics are incidental to this spec and are
    // covered elsewhere; what matters here is how the view behaves once
    // the phase is live. The follow-up below still goes through the real
    // composer, because the park decision is the thing under test.
    await postPrompt(serve.baseUrl, sessionId, "/compact");

    const spinner = page.getByTestId("acp-working-spinner");
    await expect(spinner).toContainText(/Compaction in progress/i, { timeout: 10_000 });

    // Send the follow-up FIRST, while the phase is provably live. Any
    // assertion that outlasts the hold would leave the follow-up landing
    // on an idle session, which passes for the wrong reason.
    await composer.fill("also check the tests");
    await composer.press("Enter");
    await expect(page.getByRole("button", { name: /^also check the tests$/ })).toBeVisible({ timeout: 5_000 });
    // Still parked, not sent: the phase is live and the answer to the
    // follow-up has not arrived. Asserting both pins the ordering, so a
    // compaction that ended early cannot let this pass by draining
    // before the assertion runs.
    await expect(spinner).toContainText(/Compaction in progress/i);
    await expect(page.getByText("answered after compaction")).toHaveCount(0);
    // The two symptoms this fixes, asserted while the phase is still
    // live: the wedged-agent label, and the hatch that would abort the
    // compaction.
    await expect(spinner).not.toContainText(/Waiting on model/i);
    await expect(page.getByRole("button", { name: /force end turn/i })).toHaveCount(0);

    // Completion clears the phase, and the parked prompt drains as the
    // next turn against the compacted context.
    await expect(page.getByText("Compacting completed.")).toBeVisible({ timeout: 15_000 });
    await expect(page.getByText("answered after compaction")).toBeVisible({ timeout: 15_000 });

    const json = await replayJson(serve.baseUrl, sessionId);
    expect(json).toContain("ConversationCompactionStarted");
    expect(json).not.toContain("steered: also check the tests");
  } finally {
    await serve.stop();
    rmSync(scriptDir, { recursive: true, force: true });
  }
});

test("a prompt reaching the daemon mid-compaction is rejected, not steered", async ({}, testInfo) => {
  const scriptDir = mkdtempSync(join(tmpdir(), "aoe-pw-compact-rest-"));
  const scriptPath = join(scriptDir, "script.json");
  writeFileSync(scriptPath, JSON.stringify(COMPACTING_SCRIPT));

  // The composer gates cover the UI; this covers the POST that was
  // already in flight when the marker landed, and any direct API caller
  // that does not consume the event stream. Steering is enabled, so the
  // only reason to refuse is the compaction phase.
  const serve = await spawnAoeServe({
    authMode: "none",
    acp: true,
    fakeAcpScript: scriptPath,
    extraEnv: { FAKE_ACP_STEERING: "1" },
    workerIndex: testInfo.workerIndex,
    parallelIndex: testInfo.parallelIndex,
    seedFn: seedSessionViaAoeAdd({ title: "acp-compaction-rest" }),
  });

  try {
    const sessionId = (await listSessions(serve.baseUrl))[0]!.id;
    await enableStructuredViewAndWait(serve.baseUrl, sessionId);
    // Without the capability on the stream the daemon would take the
    // plain busy-reject path and this would pass for the wrong reason.
    await waitForReplayContains(serve.baseUrl, sessionId, '"steering":true');

    await postPrompt(serve.baseUrl, sessionId, "/compact");
    await waitForReplayContains(serve.baseUrl, sessionId, "ConversationCompactionStarted");

    await postPrompt(serve.baseUrl, sessionId, "also check the tests");
    await waitForReplayContains(serve.baseUrl, sessionId, "agent_busy");

    const json = await replayJson(serve.baseUrl, sessionId);
    expect(json).not.toContain("steered: also check the tests");
  } finally {
    await serve.stop();
    rmSync(scriptDir, { recursive: true, force: true });
  }
});
