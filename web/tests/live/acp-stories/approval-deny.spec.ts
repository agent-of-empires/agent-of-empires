// User story: clicking Deny on an ApprovalCard resolves the request
// and the turn ends.
//
// Script emits a permission_request and then end_turn. After Deny, the
// fake's session/request_permission promise resolves with the reject
// decision and the turn finishes with no further chunks. The composer
// flips back to the idle "Send a message…" placeholder.

import { mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { test as base, expect } from "@playwright/test";
import {
  spawnAoeServe,
  listSessions,
  seedSessionViaAoeAdd,
} from "../../helpers/aoeServe";
import { waitForStructuredView, enableStructuredViewAndWait } from "../../helpers/acp";

const DENY_SCRIPT = {
  turns: [
    {
      updates: [
        {
          sessionUpdate: "agent_message_chunk",
          content: { type: "text", text: "Asking permission..." },
        },
        {
          sessionUpdate: "permission_request",
          toolCall: {
            toolCallId: "fake-tool-call-deny",
            title: "Delete file",
            kind: "edit",
          },
        },
      ],
      stopReason: "end_turn",
    },
  ],
};

base("ApprovalCard Deny resolves and the turn ends", async ({ page }, testInfo) => {
  const scriptDir = mkdtempSync(join(tmpdir(), "aoe-pw-story-deny-"));
  const scriptPath = join(scriptDir, "script.json");
  writeFileSync(scriptPath, JSON.stringify(DENY_SCRIPT));

  const serve = await spawnAoeServe({
    authMode: "none",
    acp: true,
    fakeAcpScript: scriptPath,
    workerIndex: testInfo.workerIndex,
    parallelIndex: testInfo.parallelIndex,
    seedFn: seedSessionViaAoeAdd({ title: "story-deny" }),
  });

  try {
    const sessions = await listSessions(serve.baseUrl);
    const seeded = sessions.find((s) => s.title === "story-deny");
    if (!seeded) throw new Error("seeded session 'story-deny' missing");
    const sessionId = seeded.id;

    await enableStructuredViewAndWait(serve.baseUrl, sessionId);

    await page.goto(`${serve.baseUrl}/session/${encodeURIComponent(sessionId)}`);
    await waitForStructuredView(page);

    const composer = page.getByRole("textbox", { name: /Send a message/i });
    await composer.fill("please delete something");
    await composer.press("Enter");

    const approvalDialog = page.getByRole("alertdialog", {
      name: /Approval needed/i,
    });
    await expect(approvalDialog).toBeVisible({ timeout: 10_000 });

    await approvalDialog.getByRole("button", { name: "Deny" }).click();

    await expect(approvalDialog).toBeHidden({ timeout: 10_000 });
    // Turn ends; idle composer reappears, Stop button gone. Asserting
    // visibility alone is insufficient — the textbox is visible
    // mid-turn too. Assert enabled (not pending) and cleared (the
    // post-submit clear ran) so this reliably proves the turn returned
    // to idle.
    await expect(composer).toBeVisible({ timeout: 10_000 });
    await expect(composer).toBeEnabled({ timeout: 10_000 });
    await expect(composer).toHaveValue("");
    await expect(page.getByRole("button", { name: "Stop" })).toBeHidden({
      timeout: 10_000,
    });
  } finally {
    await serve.stop();
    rmSync(scriptDir, { recursive: true, force: true });
  }
});
