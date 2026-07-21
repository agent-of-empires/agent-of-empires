// Live-backend spec: a plugin's tool-card-badge slot renders on a real MCP
// tool card (#2986).
//
// Stands up a real fake plugin whose worker declares the `tool-card-badge` slot
// and pushes a provenance badge targeting the `acmecorp` MCP server, then
// scripts the fake ACP agent to emit an `mcp__acmecorp__get_issue` tool call.
// Asserts the plugin's pill renders in the MCP card header. There is no
// ui-state seeding endpoint (workers push it over RPC), so a real worker is the
// only correct way to exercise this end to end.

import { spawnSync } from "node:child_process";
import { mkdirSync, mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { test as base, expect } from "@playwright/test";
import { spawnAoeServe, listSessions, resolveAoeBinary } from "../helpers/aoeServe";
import { enableStructuredViewAndWait, waitForStructuredView } from "../helpers/acp";

const pluginDir = join(dirname(fileURLToPath(import.meta.url)), "..", "fixtures", "tool-card-badge-plugin");

base("plugin tool-card-badge pill renders on the matching MCP card", async ({ page }, testInfo) => {
  const scriptDir = mkdtempSync(join(tmpdir(), "aoe-tool-card-badge-"));
  const scriptPath = join(scriptDir, "script.json");

  // claude-agent-acp ships MCP calls as kind "other" with the raw
  // `mcp__<server>__<verb>` string as the title; the frontend reclassifies it
  // into an MCP card. The worker badges the `acmecorp` server, so the target
  // name here (server slug) must match.
  writeFileSync(
    scriptPath,
    JSON.stringify({
      turns: [
        {
          updates: [
            {
              sessionUpdate: "tool_call",
              toolCallId: "mcp-1",
              title: "mcp__acmecorp__get_issue",
              kind: "other",
              status: "completed",
              content: [{ type: "content", content: { type: "text", text: "ok" } }],
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
      const addRes = spawnSync(resolveAoeBinary(), ["add", projectDir, "-t", "tool-card-badge", "-c", "claude"], {
        env,
      });
      if (addRes.status !== 0) {
        throw new Error(
          `aoe add failed: status=${addRes.status} error=${addRes.error ?? "<none>"} stderr=${addRes.stderr?.toString() ?? "<none>"}`,
        );
      }
      // Install + auto-grant the fake plugin so its worker launches at daemon
      // boot and pushes the badge over ui.state.set.
      const installRes = spawnSync(resolveAoeBinary(), ["plugin", "install", pluginDir, "--yes"], { env });
      if (installRes.status !== 0) {
        throw new Error(
          `aoe plugin install failed: status=${installRes.status} error=${installRes.error ?? "<none>"} stderr=${installRes.stderr?.toString() ?? "<none>"}`,
        );
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
      body: JSON.stringify({ text: "call the mcp tool" }),
    });
    expect(promptRes.status).toBeGreaterThanOrEqual(200);
    expect(promptRes.status).toBeLessThan(300);

    // The MCP card renders from the scripted tool call.
    await expect(page.getByText("MCP · Acmecorp")).toBeVisible({ timeout: 15_000 });

    // The plugin worker pushes ui-state and the dashboard polls it, so the pill
    // appears a poll later; wait for it on the tool-card-badge slot.
    const pill = page.locator('[data-plugin-slot="tool-card-badge"]');
    await expect(pill).toContainText("Company MCP", { timeout: 15_000 });
    await expect(pill).toHaveAttribute("data-plugin-id", "acme.provenance");
  } finally {
    await serve.stop();
    rmSync(scriptDir, { recursive: true, force: true });
  }
});
