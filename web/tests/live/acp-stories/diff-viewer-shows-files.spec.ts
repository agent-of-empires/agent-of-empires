// User story: a session with uncommitted changes shows the modified
// file in the diff panel.
//
// Seeds a session whose project directory has an extra file beyond
// the base commit; the dashboard's GET /api/sessions/:id/diff/files
// returns that file and DiffFileList renders a clickable row.

import { mkdirSync, writeFileSync } from "node:fs";
import { spawnSync } from "node:child_process";
import { join } from "node:path";
import { test as base, expect } from "@playwright/test";
import {
  spawnAoeServe,
  listSessions,
  resolveAoeBinary,
} from "../../helpers/aoeServe";

base("diff panel renders the changed file row", async ({ page }, testInfo) => {
  const serve = await spawnAoeServe({
    authMode: "none",
    workerIndex: testInfo.workerIndex,
    parallelIndex: testInfo.parallelIndex,
    seedFn: ({ home, env }) => {
      const projectDir = join(home, "project");
      mkdirSync(projectDir, { recursive: true });
      spawnSync("git", ["init", "-q"], { cwd: projectDir });
      spawnSync("git", ["commit", "--allow-empty", "-q", "-m", "init"], {
        cwd: projectDir,
        env: {
          ...env,
          GIT_AUTHOR_NAME: "t",
          GIT_AUTHOR_EMAIL: "t@t",
          GIT_COMMITTER_NAME: "t",
          GIT_COMMITTER_EMAIL: "t@t",
        },
      });
      // Untracked file: shows up in the diff against HEAD.
      writeFileSync(join(projectDir, "story.txt"), "hello story\n");
      const res = spawnSync(
        resolveAoeBinary(),
        ["add", projectDir, "-t", "story-diff-files", "-c", "claude"],
        { env },
      );
      if (res.status !== 0) {
        throw new Error(
          `aoe add failed: status=${res.status} stderr=${res.stderr?.toString() ?? "<none>"}`,
        );
      }
    },
  });

  try {
    const sessions = await listSessions(serve.baseUrl);
    const seeded = sessions.find((s) => s.title === "story-diff-files");
    if (!seeded) throw new Error("seeded session 'story-diff-files' missing");
    const sessionId = seeded.id;

    await page.goto(
      `${serve.baseUrl}/session/${encodeURIComponent(sessionId)}`,
    );
    await expect(page).toHaveURL(new RegExp(`/session/${sessionId}`), {
      timeout: 10_000,
    });

    // The diff-file row text appears twice in the DOM (file-list + the
    // viewer header once selected). `.first()` picks the clickable row.
    await expect(page.getByText("story.txt").first()).toBeVisible({
      timeout: 15_000,
    });
  } finally {
    await serve.stop();
  }
});
