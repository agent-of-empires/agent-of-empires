// Live-backend spec: structured view transcript local file links (#1718).
//
// Seeds a git repo with a committed-then-modified file so the diff
// endpoint returns real content, registers it as a structured view session, and
// scripts the fake ACP agent to emit an assistant message containing two
// markdown links: one local file reference inside the worktree and one
// absolute path outside any repo root. Drives the real UI:
//   - clicking the in-repo link opens the file in the in-app diff viewer
//     and keeps the /session/<id> route (no navigation away),
//   - clicking the out-of-repo link surfaces a non-destructive toast and
//     leaves the route unchanged.

import { spawnSync } from "node:child_process";
import { mkdtempSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { test as base, expect } from "@playwright/test";
import { spawnAoeServe, listSessions, resolveAoeBinary } from "../helpers/aoeServe";
import { commitAll, initWorkingRepo, writeFiles } from "../helpers/gitFixture";
import { enableStructuredViewAndWait, waitForStructuredView } from "../helpers/acp";

base("structured view transcript file links open in-app and toast on miss", async ({ page }, testInfo) => {
  const scriptDir = mkdtempSync(join(tmpdir(), "aoe-acp-filelink-"));
  const scriptPath = join(scriptDir, "script.json");
  const outsidePath = "/tmp/aoe-1718-not-a-repo/missing.ts:1";

  const serve = await spawnAoeServe({
    authMode: "none",
    acp: true,
    fakeAcpScript: scriptPath,
    workerIndex: testInfo.workerIndex,
    parallelIndex: testInfo.parallelIndex,
    seedFn: ({ home, env }) => {
      const projectDir = join(home, "project");
      initWorkingRepo(projectDir);
      writeFiles(projectDir, { "src/a.ts": "export const a = 1;\n" });
      commitAll(projectDir, "baseline");
      writeFiles(projectDir, { "src/a.ts": "export const a = 11;\n" });

      // `aoe add <dir>` makes project_path the modified working tree,
      // so an absolute path under projectDir resolves to a repo file.
      // Bake that path into the agent message now that home is known.
      const inRepoLink = `${projectDir}/src/a.ts:1`;
      writeFileSync(
        scriptPath,
        JSON.stringify({
          turns: [
            {
              updates: [
                {
                  sessionUpdate: "agent_message_chunk",
                  content: {
                    type: "text",
                    text: `See [a.ts](${inRepoLink}) and [missing](${outsidePath}).`,
                  },
                },
              ],
              stopReason: "end_turn",
            },
          ],
        }),
      );

      const addRes = spawnSync(resolveAoeBinary(), ["add", projectDir, "-t", "acp-filelink", "-c", "claude"], { env });
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

    // Trigger the scripted agent turn that emits the two links.
    const promptRes = await fetch(`${serve.baseUrl}/api/sessions/${sessionId}/acp/prompt`, {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ text: "show me the file" }),
    });
    expect(promptRes.status).toBeGreaterThanOrEqual(200);
    expect(promptRes.status).toBeLessThan(300);

    const sessionUrl = new RegExp(`/session/${sessionId}`);

    // Out-of-repo link: clicking surfaces a toast and does not navigate.
    const missingLink = page.getByRole("link", { name: "missing" });
    await expect(missingLink).toBeVisible({ timeout: 15_000 });
    await missingLink.click();
    await expect(page.locator('[role="alert"]')).toContainText(/Could not open/i, { timeout: 10_000 });
    await expect(page).toHaveURL(sessionUrl);

    // In-repo link: clicking opens the file in the in-app diff viewer,
    // showing the modified content, still on the same session route.
    const fileLink = page.getByRole("link", { name: "a.ts" });
    await expect(fileLink).toBeVisible();
    await fileLink.click();
    await expect(page.getByText(/export const a = 11/).first()).toBeVisible({
      timeout: 10_000,
    });
    await expect(page).toHaveURL(sessionUrl);
  } finally {
    await serve.stop();
  }
});
