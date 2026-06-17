// User story: the agent's AskUserQuestion surfaces an elicitation card in
// the structured view; submitting an answer resolves it and the turn
// continues.
//
// The script emits an elicitation_request mid-turn (the fake translates
// this into a real form-mode `elicitation/create` JSON-RPC outbound) then
// a post-answer chunk. The fake awaits the client's response before
// emitting the second chunk, so seeing the post-answer text proves the
// submit round-tripped through the structured view and back to the agent.

import { mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { test as base, expect } from "@playwright/test";
import { spawnAoeServe, listSessions, seedSessionViaAoeAdd } from "../../helpers/aoeServe";
import { waitForStructuredView, enableStructuredViewAndWait } from "../../helpers/acp";

const ASK_SCRIPT = {
  turns: [
    {
      updates: [
        {
          sessionUpdate: "agent_message_chunk",
          content: { type: "text", text: "I need to know your preference..." },
        },
        {
          sessionUpdate: "elicitation_request",
          message: "Which color?",
          requestedSchema: {
            type: "object",
            properties: {
              question_0: {
                type: "string",
                title: "Which color?",
                oneOf: [
                  { const: "Red", title: "Red" },
                  { const: "Blue", title: "Blue" },
                ],
              },
            },
          },
        },
        {
          sessionUpdate: "agent_message_chunk",
          content: { type: "text", text: "Got your answer." },
        },
      ],
      stopReason: "end_turn",
    },
  ],
};

// A richer MCP-style form: an integer field with a range plus a boolean.
// Proves the number/integer/boolean kinds round-trip through the real
// backend (parse -> server-side validate -> build_response) end to end.
const MIXED_SCRIPT = {
  turns: [
    {
      updates: [
        {
          sessionUpdate: "elicitation_request",
          message: "Configure the run.",
          requestedSchema: {
            type: "object",
            title: "Run options",
            properties: {
              question_0: { type: "integer", title: "Workers", minimum: 1, maximum: 8 },
              question_1: { type: "boolean", title: "Verbose" },
            },
            required: ["question_0"],
          },
        },
        {
          sessionUpdate: "agent_message_chunk",
          content: { type: "text", text: "Configured." },
        },
      ],
      stopReason: "end_turn",
    },
  ],
};

base("AskUserQuestion card submit resolves and the turn continues", async ({ page }, testInfo) => {
  const scriptDir = mkdtempSync(join(tmpdir(), "aoe-pw-story-ask-"));
  const scriptPath = join(scriptDir, "script.json");
  writeFileSync(scriptPath, JSON.stringify(ASK_SCRIPT));

  const serve = await spawnAoeServe({
    authMode: "none",
    acp: true,
    fakeAcpScript: scriptPath,
    workerIndex: testInfo.workerIndex,
    parallelIndex: testInfo.parallelIndex,
    seedFn: seedSessionViaAoeAdd({ title: "story-ask" }),
  });

  try {
    const sessions = await listSessions(serve.baseUrl);
    const seeded = sessions.find((s) => s.title === "story-ask");
    if (!seeded) throw new Error("seeded session 'story-ask' missing");
    const sessionId = seeded.id;

    await enableStructuredViewAndWait(serve.baseUrl, sessionId);

    await page.goto(`${serve.baseUrl}/session/${encodeURIComponent(sessionId)}`);
    await waitForStructuredView(page);

    const composer = page.getByRole("textbox", { name: /Send a message/i });
    await composer.fill("help me pick");
    await composer.press("Enter");

    const questionDialog = page.getByRole("alertdialog", {
      name: /Question from the agent/i,
    });
    await expect(questionDialog).toBeVisible({ timeout: 10_000 });

    // #2145: the turn is still "running" while the question is pending, but
    // the agent is parked on the answer, not stalled. The working spinner
    // (rattle verbs, "Waiting on model…", the Force end turn watchdog) must
    // not render beneath the card. The spinner mounts immediately when shown,
    // so a count of 0 is decisive without waiting for the stall threshold.
    await expect(page.getByTestId("acp-working-spinner")).toHaveCount(0);

    // The fake gates the post-answer chunk on the user's submit. Assert it
    // is absent first so a no-op card couldn't pass this spec.
    const postAnswerChunk = page.getByText("Got your answer.");
    await expect(postAnswerChunk).toHaveCount(0);

    await questionDialog.getByLabel("Blue").check();
    await questionDialog.getByRole("button", { name: "Submit" }).click();

    await expect(postAnswerChunk).toBeVisible({ timeout: 10_000 });
    await expect(questionDialog).toBeHidden({ timeout: 10_000 });

    // #2209: the picked answer is recorded in the transcript as the user's
    // turn, so the history shows what was chosen rather than jumping to the
    // next agent output.
    const answerCard = page.getByTestId("elicitation-answer-card");
    await expect(answerCard).toBeVisible({ timeout: 10_000 });
    await expect(answerCard).toContainText("Which color?");
    await expect(answerCard).toContainText("Blue");
  } finally {
    await serve.stop();
    rmSync(scriptDir, { recursive: true, force: true });
  }
});

base("elicitation form with number + boolean fields round-trips", async ({ page }, testInfo) => {
  const scriptDir = mkdtempSync(join(tmpdir(), "aoe-pw-story-mixed-"));
  const scriptPath = join(scriptDir, "script.json");
  writeFileSync(scriptPath, JSON.stringify(MIXED_SCRIPT));

  const serve = await spawnAoeServe({
    authMode: "none",
    acp: true,
    fakeAcpScript: scriptPath,
    workerIndex: testInfo.workerIndex,
    parallelIndex: testInfo.parallelIndex,
    seedFn: seedSessionViaAoeAdd({ title: "story-mixed" }),
  });

  try {
    const sessions = await listSessions(serve.baseUrl);
    const seeded = sessions.find((s) => s.title === "story-mixed");
    if (!seeded) throw new Error("seeded session 'story-mixed' missing");
    const sessionId = seeded.id;

    await enableStructuredViewAndWait(serve.baseUrl, sessionId);

    await page.goto(`${serve.baseUrl}/session/${encodeURIComponent(sessionId)}`);
    await waitForStructuredView(page);

    const composer = page.getByRole("textbox", { name: /Send a message/i });
    await composer.fill("configure");
    await composer.press("Enter");

    const questionDialog = page.getByRole("alertdialog", { name: /Question from the agent/i });
    await expect(questionDialog).toBeVisible({ timeout: 10_000 });

    const postChunk = page.getByText("Configured.");
    await expect(postChunk).toHaveCount(0);

    await questionDialog.getByPlaceholder("Enter a number").fill("3");
    await questionDialog.getByRole("checkbox").check();
    await questionDialog.getByRole("button", { name: "Submit" }).click();

    await expect(postChunk).toBeVisible({ timeout: 10_000 });
    await expect(questionDialog).toBeHidden({ timeout: 10_000 });
  } finally {
    await serve.stop();
    rmSync(scriptDir, { recursive: true, force: true });
  }
});
