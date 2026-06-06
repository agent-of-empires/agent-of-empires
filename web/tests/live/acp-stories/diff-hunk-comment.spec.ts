// User story: leave a comment on a diff hunk in a structured view session.
//
// Structured view-enabled session with an uncommitted file: click the file
// row → DiffFileViewer mounts → hover a line to surface the "+"
// gutter button → click it → CommentForm appears → write body, save.
// The persistent CommentsBanner then shows the comment count.

import { mkdirSync, writeFileSync } from "node:fs";
import { spawnSync } from "node:child_process";
import { join } from "node:path";
import { test as base, expect } from "@playwright/test";
import {
  spawnAoeServe,
  listSessions,
  resolveAoeBinary,
} from "../../helpers/aoeServe";
import {
  waitForStructuredView,
  enableStructuredViewAndWait,
} from "../../helpers/acp";

base(
  "comment on a diff hunk persists in the comments banner",
  async ({ page }, testInfo) => {
    const serve = await spawnAoeServe({
      authMode: "none",
      acp: true,
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
        writeFileSync(
          join(projectDir, "story.txt"),
          "line one\nline two\nline three\n",
        );
        const res = spawnSync(
          resolveAoeBinary(),
          ["add", projectDir, "-t", "story-hunk-comment", "-c", "claude"],
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
      const seeded = sessions.find((s) => s.title === "story-hunk-comment");
      if (!seeded)
        throw new Error("seeded session 'story-hunk-comment' missing");
      const sessionId = seeded.id;

      await enableStructuredViewAndWait(serve.baseUrl, sessionId);

      await page.goto(
        `${serve.baseUrl}/session/${encodeURIComponent(sessionId)}`,
      );
      await waitForStructuredView(page);

      // Click the diff file row to open the file viewer.
      const fileRow = page.getByText("story.txt").first();
      await expect(fileRow).toBeVisible({ timeout: 15_000 });
      await fileRow.click();

      // "+" gutter buttons live behind `opacity-0 group-hover:opacity-100`
      // on each diff line, so the click target only paints on hover.
      // Hover the gutter cell first to reveal the button, then click.
      // CommentForm uses a two-click range-selection model: first click
      // sets rangeStart, second click on the same line resolves a single-
      // line range and mounts the draft form.
      const addBtn = page
        .getByRole("button", {
          name: /Add comment on .* line/i,
        })
        .first();
      await expect(addBtn).toBeAttached({ timeout: 15_000 });
      // Hover the surrounding diff line to trigger group-hover.
      const lineRow = addBtn.locator(
        "xpath=ancestor::div[contains(@class,'group')][1]",
      );
      await lineRow.hover();
      await expect(addBtn).toBeVisible({ timeout: 5_000 });
      await addBtn.click(); // sets rangeStart
      await lineRow.hover(); // re-trigger group-hover after click
      await addBtn.click(); // resolves single-line range -> draft form mounts

      const composer = page.getByPlaceholder(/Leave a comment/i);
      await expect(composer).toBeVisible({ timeout: 5_000 });
      await composer.fill("looks good but rename this");
      await page.getByRole("button", { name: "Save", exact: true }).click();

      // The CommentsBanner / commentsCount surface should reflect one
      // saved comment. ContentSplit renders the right pane twice (desktop
      // `hidden md:flex` + mobile slide-in `md:hidden fixed`), so two
      // CommentsBanner copies live in the DOM at all viewports. The
      // desktop copy is the visible one at default Chromium width;
      // `.first()` resolves to it deterministically and avoids the strict-
      // mode multi-match.
      await expect(page.getByText(/1 comment/i).first()).toBeVisible({
        timeout: 10_000,
      });
    } finally {
      await serve.stop();
    }
  },
);
