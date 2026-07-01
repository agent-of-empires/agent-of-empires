// Live-backend spec: structured view transcript local file links (#1718, #1809, #1810).
//
// Seeds a git repo with committed-then-modified files so the diff
// endpoint returns real content, registers it as a structured view session, and
// scripts the fake ACP agent to emit an assistant message containing four
// markdown links: an in-worktree (changed) file reference, an unchanged
// committed file, a `path:line` reference deep into a fully-rewritten (tall)
// file, and an absolute path outside any repo root. Drives the real UI:
//   - clicking the in-repo link opens the file in the in-app diff viewer
//     and keeps the /session/<id> route (no navigation away),
//   - clicking the unchanged file shows its full contents via the full-file
//     fallback rather than a dead end (#1810),
//   - clicking the `path:line` link scrolls the cited line into view (#1809):
//     the row is virtualized and only mounts once scrolled near, so asserting
//     its content is visible proves the scroll happened (it stays off-screen
//     at the top without the fix),
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

// A unique token that only appears on the cited line (line 80) of the tall
// file's modified content, so a viewport visibility check is unambiguous.
const SCROLL_SENTINEL = "TARGET_SENTINEL_424242";

// Baseline / modified contents for a 100-line file where every line is
// rewritten, so the diff is tall enough that line 80 sits well below the fold
// when the file first opens.
function tallFile(): { baseline: string; modified: string } {
  const lines: string[] = [];
  for (let i = 0; i < 100; i++) lines.push(`export const v${i} = ${i};`);
  const baseline = lines.join("\n") + "\n";
  const modified =
    lines.map((l, i) => (i === 79 ? `export const ${SCROLL_SENTINEL} = ${i};` : `${l} // edited`)).join("\n") + "\n";
  return { baseline, modified };
}

base(
  "structured view transcript file links open in-app, scroll to cited line, and render out-of-repo paths as inert text",
  async ({ page }, testInfo) => {
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
        const tall = tallFile();
        initWorkingRepo(projectDir);
        // src/b.ts is committed and left untouched, so it has no diff against
        // the base and exercises the full-file fallback. See #1810.
        writeFiles(projectDir, {
          "src/a.ts": "export const a = 1;\n",
          "src/b.ts": "export const unchangedConst = 42;\n",
          "src/long.ts": tall.baseline,
        });
        commitAll(projectDir, "baseline");
        writeFiles(projectDir, { "src/a.ts": "export const a = 11;\n", "src/long.ts": tall.modified });

        // `aoe add <dir>` makes project_path the modified working tree,
        // so an absolute path under projectDir resolves to a repo file.
        // Bake that path into the agent message now that home is known.
        const inRepoLink = `${projectDir}/src/a.ts:1`;
        const unchangedLink = `${projectDir}/src/b.ts:1`;
        const deepLink = `${projectDir}/src/long.ts:80`;
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
                      text: `See [a.ts](${inRepoLink}), [b.ts](${unchangedLink}), [deep](${deepLink}) and [missing](${outsidePath}).`,
                    },
                  },
                ],
                stopReason: "end_turn",
              },
            ],
          }),
        );

        const addRes = spawnSync(resolveAoeBinary(), ["add", projectDir, "-t", "acp-filelink", "-c", "claude"], {
          env,
        });
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

      // Trigger the scripted agent turn that emits the links.
      const promptRes = await fetch(`${serve.baseUrl}/api/sessions/${sessionId}/acp/prompt`, {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({ text: "show me the file" }),
      });
      expect(promptRes.status).toBeGreaterThanOrEqual(200);
      expect(promptRes.status).toBeLessThan(300);

      const sessionUrl = new RegExp(`/session/${sessionId}`);

      // Out-of-repo path: rendered as inert, non-clickable text rather than a
      // link that would dead-end in a toast or route to the SPA. See #2587.
      const inertMissing = page.locator("span.acp-inert-path", { hasText: "missing" });
      await expect(inertMissing).toBeVisible({ timeout: 15_000 });
      await expect(page.getByRole("link", { name: "missing" })).toHaveCount(0);
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

      // `path:line` link: close the open file to reveal the transcript again,
      // then click the deep link. Line 80 is virtualized far below the fold, so
      // its content is only in the DOM and visible once the viewer scrolls to
      // it. Without the scroll-to-line wiring the file opens at the top and the
      // sentinel never mounts. See #1809.
      await page.getByRole("button", { name: "Back to terminal" }).click();
      const deepFileLink = page.getByRole("link", { name: "deep" });
      await expect(deepFileLink).toBeVisible();
      await deepFileLink.click();
      await expect(page.getByText(new RegExp(SCROLL_SENTINEL)).first()).toBeVisible({
        timeout: 10_000,
      });
      await expect(page).toHaveURL(sessionUrl);

      // Unchanged in-repo link: b.ts has no diff against the base, so the viewer
      // falls back to showing its full contents instead of a dead end. See #1810.
      await page.getByRole("button", { name: "Back to terminal" }).click();
      const unchangedFileLink = page.getByRole("link", { name: "b.ts" });
      await expect(unchangedFileLink).toBeVisible();
      await unchangedFileLink.click();
      await expect(page.getByText(/export const unchangedConst = 42/).first()).toBeVisible({
        timeout: 10_000,
      });
      await expect(page).toHaveURL(sessionUrl);
    } finally {
      await serve.stop();
    }
  },
);
