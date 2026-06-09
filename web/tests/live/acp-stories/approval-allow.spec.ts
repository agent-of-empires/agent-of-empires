// User story: clicking Allow on an ApprovalCard resolves the request
// and the turn continues.
//
// Script emits a permission_request mid-turn (the fake translates this
// into a real session/request_permission JSON-RPC outbound) then a
// post-approval chunk. The fake awaits the client decision before
// emitting the second chunk, so seeing the post-approval text in the
// transcript proves the click round-tripped through the structured view.

import { mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { test as base, expect } from "@playwright/test";
import { spawnAoeServe, listSessions, seedSessionViaAoeAdd } from "../../helpers/aoeServe";
import { waitForStructuredView, enableStructuredViewAndWait } from "../../helpers/acp";

const ALLOW_SCRIPT = {
  turns: [
    {
      updates: [
        {
          sessionUpdate: "agent_message_chunk",
          content: { type: "text", text: "About to write a file..." },
        },
        {
          sessionUpdate: "permission_request",
          toolCall: {
            toolCallId: "fake-tool-call-allow",
            title: "Write file",
            kind: "edit",
          },
        },
        {
          sessionUpdate: "agent_message_chunk",
          content: { type: "text", text: "Write complete." },
        },
      ],
      stopReason: "end_turn",
    },
  ],
};

base("ApprovalCard Allow resolves and the turn continues", async ({ page }, testInfo) => {
  const scriptDir = mkdtempSync(join(tmpdir(), "aoe-pw-story-allow-"));
  const scriptPath = join(scriptDir, "script.json");
  writeFileSync(scriptPath, JSON.stringify(ALLOW_SCRIPT));

  const serve = await spawnAoeServe({
    authMode: "none",
    acp: true,
    fakeAcpScript: scriptPath,
    workerIndex: testInfo.workerIndex,
    parallelIndex: testInfo.parallelIndex,
    seedFn: seedSessionViaAoeAdd({ title: "story-allow" }),
  });

  try {
    const sessions = await listSessions(serve.baseUrl);
    const seeded = sessions.find((s) => s.title === "story-allow");
    if (!seeded) throw new Error("seeded session 'story-allow' missing");
    const sessionId = seeded.id;

    await enableStructuredViewAndWait(serve.baseUrl, sessionId);

    await page.goto(`${serve.baseUrl}/session/${encodeURIComponent(sessionId)}`);
    await waitForStructuredView(page);

    const composer = page.getByRole("textbox", { name: /Send a message/i });
    await composer.fill("please write something");
    await composer.press("Enter");

    const approvalDialog = page.getByRole("alertdialog", {
      name: /Approval needed/i,
    });
    await expect(approvalDialog).toBeVisible({ timeout: 10_000 });

    // The fake script gates the post-approval chunk on the user's
    // Allow click. Prove the gate works by asserting the
    // post-approval text is absent BEFORE clicking; otherwise this
    // spec would pass even if the dialog were a no-op and the chunk
    // emitted unconditionally.
    const postApprovalChunk = page.getByText("Write complete.");
    await expect(postApprovalChunk).toHaveCount(0);

    await approvalDialog.getByRole("button", { name: "Allow" }).click();

    // Fake awaits the decision; once Allow lands, the next chunk emits.
    await expect(postApprovalChunk).toBeVisible({ timeout: 10_000 });
    // Approval card is dismissed after resolution.
    await expect(approvalDialog).toBeHidden({ timeout: 10_000 });
  } finally {
    await serve.stop();
    rmSync(scriptDir, { recursive: true, force: true });
  }
});
