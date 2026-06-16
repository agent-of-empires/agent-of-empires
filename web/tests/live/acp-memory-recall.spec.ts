// Live-backend spec: synthesize-mode memory recall card rendering (#2142).
//
// Scripts the fake ACP agent to emit a `memory_recall` tool call in
// synthesize mode, carrying the SDK's raw recall payload: a
// <system-reminder> envelope wrapping cat -n line-numbered markdown.
// Drives the real UI and asserts the structured view MemoryRecallCard
// strips the envelope tags and line numbers and renders the body as
// markdown (heading + list elements), not raw source.

import { spawnSync } from "node:child_process";
import { mkdirSync, mkdtempSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { test as base, expect } from "@playwright/test";
import { spawnAoeServe, listSessions, resolveAoeBinary } from "../helpers/aoeServe";
import { enableStructuredViewAndWait, waitForStructuredView } from "../helpers/acp";

base("structured view renders synthesize memory recall as cleaned markdown", async ({ page }, testInfo) => {
  const scriptDir = mkdtempSync(join(tmpdir(), "aoe-acp-memrecall-"));
  const scriptPath = join(scriptDir, "script.json");

  // The fake agent forwards a scripted `tool_call` update verbatim, so we
  // reproduce the exact shape claude-agent-acp emits for synthesize-mode
  // recall: the recalled file content inside a <system-reminder> envelope
  // with cat -n line-number prefixes, plus the claudeCode tool metadata.
  const dirtyText =
    "<system-reminder>\n" +
    "     1\t# User profile\n" +
    "     2\t\n" +
    "     3\tUser is a senior engineer working on agent-of-empires.\n" +
    "     4\t\n" +
    "     5\t- prefers terse output\n" +
    "     6\t- no em dashes\n" +
    "</system-reminder>";

  writeFileSync(
    scriptPath,
    JSON.stringify({
      turns: [
        {
          updates: [
            {
              sessionUpdate: "tool_call",
              toolCallId: "mem-synth-1",
              title: "Recalled synthesized memory",
              kind: "read",
              status: "completed",
              content: [{ type: "content", content: { type: "text", text: dirtyText } }],
              _meta: { claudeCode: { toolName: "memory_recall", toolResponse: { mode: "synthesize" } } },
            },
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
    seedFn: ({ home, env }) => {
      const projectDir = join(home, "project");
      mkdirSync(projectDir, { recursive: true });
      // `-c claude` resolves the CLAUDE agent profile, the one that
      // gates `supports_memory_recall_tool` (claudeCode meta namespace).
      const addRes = spawnSync(resolveAoeBinary(), ["add", projectDir, "-t", "acp-memrecall", "-c", "claude"], { env });
      if (addRes.status !== 0) {
        throw new Error(`aoe add failed: status=${addRes.status} stderr=${addRes.stderr?.toString() ?? "<none>"}`);
      }
    },
  });

  try {
    const sessions = await listSessions(serve.baseUrl);
    const sessionId: string = sessions[0]!.id;
    await enableStructuredViewAndWait(serve.baseUrl, sessionId);

    await page.goto(`${serve.baseUrl}/session/${sessionId}`);
    await waitForStructuredView(page);

    const promptRes = await fetch(`${serve.baseUrl}/api/sessions/${sessionId}/acp/prompt`, {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ text: "what do you remember" }),
    });
    expect(promptRes.status).toBeGreaterThanOrEqual(200);
    expect(promptRes.status).toBeLessThan(300);

    // The card lands collapsed; click its header to reveal the body.
    const cardToggle = page.getByRole("button").filter({ hasText: "Synthesised memory" });
    await expect(cardToggle).toBeVisible({ timeout: 15_000 });
    await cardToggle.click();

    const body = page.getByTestId("memory-recall-synthesized");
    await expect(body).toBeVisible();
    // Body content survives.
    await expect(body).toContainText("User is a senior engineer working on agent-of-empires.");
    await expect(body).toContainText("prefers terse output");
    // Transport noise is stripped.
    await expect(body).not.toContainText("system-reminder");
    // No cat -n numeric-tab line-number prefixes survive.
    expect(await body.innerText()).not.toMatch(/\d+\t/);
    // Markdown rendered to elements, not raw source.
    await expect(body.locator("h1")).toHaveText("User profile");
    await expect(body.locator("li")).toHaveCount(2);
  } finally {
    await serve.stop();
  }
});
