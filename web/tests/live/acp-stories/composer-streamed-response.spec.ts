// User story: streamed agent response renders progressively in the chat.
//
// Custom FAKE_ACP_SCRIPT emits three agent_message_chunk updates in
// sequence within a single turn. The structured view reducer at
// web/src/lib/acpTypes.ts appends each chunk; the rendered DOM
// must show the concatenated message after the turn ends.

import { mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { test as base, expect } from "@playwright/test";
import { spawnAoeServe, listSessions, seedSessionViaAoeAdd } from "../../helpers/aoeServe";
import { waitForStructuredView, enableStructuredViewAndWait } from "../../helpers/acp";

const STREAM_SCRIPT = {
  turns: [
    {
      updates: [
        {
          sessionUpdate: "agent_message_chunk",
          content: { type: "text", text: "Once " },
        },
        {
          sessionUpdate: "agent_message_chunk",
          content: { type: "text", text: "upon " },
        },
        {
          sessionUpdate: "agent_message_chunk",
          content: { type: "text", text: "a time." },
        },
      ],
      stopReason: "end_turn",
    },
  ],
};

base("multi-chunk agent response assembles in the transcript", async ({ page }, testInfo) => {
  const scriptDir = mkdtempSync(join(tmpdir(), "aoe-pw-story-stream-"));
  const scriptPath = join(scriptDir, "script.json");
  writeFileSync(scriptPath, JSON.stringify(STREAM_SCRIPT));

  let serve: Awaited<ReturnType<typeof spawnAoeServe>> | undefined;

  try {
    serve = await spawnAoeServe({
      authMode: "none",
      acp: true,
      fakeAcpScript: scriptPath,
      workerIndex: testInfo.workerIndex,
      parallelIndex: testInfo.parallelIndex,
      seedFn: seedSessionViaAoeAdd({ title: "story-stream" }),
    });

    const sessions = await listSessions(serve.baseUrl);
    const seeded = sessions.find((s) => s.title === "story-stream");
    if (!seeded) throw new Error("seeded session 'story-stream' missing");
    const sessionId = seeded.id;

    await enableStructuredViewAndWait(serve.baseUrl, sessionId);

    await page.goto(`${serve.baseUrl}/session/${encodeURIComponent(sessionId)}`);
    await waitForStructuredView(page);

    const composer = page.getByRole("textbox", { name: /Send a message/i });
    await composer.fill("tell me a story");
    await composer.press("Enter");

    await expect(page.getByText("Once upon a time.")).toBeVisible({
      timeout: 10_000,
    });
  } finally {
    try {
      if (serve) await serve.stop();
    } finally {
      rmSync(scriptDir, { recursive: true, force: true });
    }
  }
});
