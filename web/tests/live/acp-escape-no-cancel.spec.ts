// Escape must leave the active structured-view turn running.
import { mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { test, expect } from "@playwright/test";
import { spawnAoeServe, listSessions, seedSessionViaAoeAdd } from "../helpers/aoeServe";
import { enableStructuredViewAndWait, waitForStructuredView } from "../helpers/acp";

test("Escape inside the structured view composer does not POST /acp/cancel", async ({ page }, testInfo) => {
  const scriptDir = mkdtempSync(join(tmpdir(), "aoe-pw-escape-"));
  const scriptPath = join(scriptDir, "script.json");
  writeFileSync(
    scriptPath,
    JSON.stringify({
      turns: [
        {
          updates: [
            { sessionUpdate: "agent_message_chunk", content: { type: "text", text: "ESCAPE_TURN_ACTIVE" } },
            { sessionUpdate: "wait_for_release" },
            { sessionUpdate: "agent_message_chunk", content: { type: "text", text: "ESCAPE_TURN_COMPLETED" } },
          ],
          stopReason: "end_turn",
        },
      ],
    }),
  );
  const serve = await spawnAoeServe({
    authMode: "none",
    acp: true,
    fakeAcpScript: scriptPath,
    workerIndex: testInfo.workerIndex,
    parallelIndex: testInfo.parallelIndex,
    seedFn: seedSessionViaAoeAdd({ title: "escape-no-cancel" }),
  });
  try {
    const sessionId = (await listSessions(serve.baseUrl))[0]!.id;
    await enableStructuredViewAndWait(serve.baseUrl, sessionId);
    let cancelCount = 0;
    page.on("request", (req) => {
      if (req.method() === "POST" && req.url().endsWith(`/api/sessions/${sessionId}/acp/cancel`)) cancelCount++;
    });
    await page.goto(`${serve.baseUrl}/session/${sessionId}`);
    await waitForStructuredView(page);
    const composer = page.locator('textarea[name="input"]');
    await composer.fill("stay in the turn");
    await composer.press("Enter");
    await expect(page.getByText("ESCAPE_TURN_ACTIVE", { exact: true })).toBeVisible();
    await expect(page.getByTestId("composer-actions").getByRole("button", { name: "Stop" })).toBeVisible();
    await composer.press("Escape");
    writeFileSync(`${scriptPath}.release`, "release");
    await expect(page.getByText(/ESCAPE_TURN_COMPLETED/)).toBeVisible();
    await expect(page.getByRole("textbox", { name: /Send a message/i })).toBeVisible();
    expect(cancelCount).toBe(0);
  } finally {
    await serve.stop();
    rmSync(scriptDir, { recursive: true, force: true });
  }
});
