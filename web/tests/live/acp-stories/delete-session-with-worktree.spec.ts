// User story: delete a session that has a managed worktree, opting
// into "Delete worktree" + "Delete branch".
//
// Seeds the session with `aoe add -w new-branch -b` so the
// DeleteSessionDialog renders its checkbox section. Toggling
// delete_worktree on and confirming results in a DELETE body with
// `delete_worktree: true`.

import { mkdirSync } from "node:fs";
import { spawnSync } from "node:child_process";
import { join } from "node:path";
import { test as base, expect } from "@playwright/test";
import {
  spawnAoeServe,
  listSessions,
  resolveAoeBinary,
} from "../../helpers/aoeServe";

base("DeleteSessionDialog can opt into deleting the worktree", async ({ page }, testInfo) => {
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
      const res = spawnSync(
        resolveAoeBinary(),
        [
          "add",
          projectDir,
          "-t",
          "story-delete-wt",
          "-c",
          "claude",
          "-w",
          "feature/x",
          "-b",
        ],
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
    const seeded = sessions.find((s) => s.title === "story-delete-wt");
    if (!seeded) throw new Error("seeded session 'story-delete-wt' missing");
    const sessionId = seeded.id;

    await page.goto(serve.baseUrl);
    const row = page
      .locator('[data-testid="sidebar-session-row"]')
      .filter({ hasText: "story-delete-wt" })
      .first();
    await expect(row).toBeVisible({ timeout: 10_000 });

    await row.click({ button: "right" });
    await page.locator('[data-testid="sidebar-context-menu-delete"]').click();

    const dialog = page.locator('[data-testid="delete-session-dialog"]');
    await expect(dialog).toBeVisible({ timeout: 5_000 });

    const worktreeCheckbox = page.locator(
      '[data-testid="delete-session-checkbox-worktree"]',
    );
    const branchCheckbox = page.locator(
      '[data-testid="delete-session-checkbox-branch"]',
    );
    await expect(worktreeCheckbox).toBeVisible({ timeout: 5_000 });
    await expect(branchCheckbox).toBeVisible({ timeout: 5_000 });
    // Custom Checkbox component renders as a `<label data-checked="...">`,
    // not a native input, so toBeChecked() does not apply. The label
    // attribute is the source of truth.
    // Worktree defaults to true via cleanupDefaults; branch defaults
    // to false. The user story is "opt into deleting BOTH", so click
    // the branch checkbox to flip it on before submit.
    await expect(worktreeCheckbox).toHaveAttribute("data-checked", "true");
    await expect(branchCheckbox).toHaveAttribute("data-checked", "false");
    // The Checkbox renders as <label> with an inner <span> that owns
    // the onClick (DeleteSessionDialog.tsx ~line 210). Clicking the
    // label itself does NOT fire onChange — there is no <input>
    // associated. Click the first inner span (the colored box) to
    // flip state.
    await branchCheckbox.locator("span").first().click();
    await expect(branchCheckbox).toHaveAttribute("data-checked", "true");

    const deletePromise = page.waitForResponse(
      (res) =>
        res.url().endsWith(`/api/sessions/${sessionId}`) &&
        res.request().method() === "DELETE",
    );
    await dialog.getByRole("button", { name: /^Delete$/ }).click();
    const response = await deletePromise;
    expect(response.ok()).toBeTruthy();
    const payload = response.request().postDataJSON();
    expect(payload.delete_worktree).toBe(true);
    expect(payload.delete_branch).toBe(true);

    await expect(row).toHaveCount(0, { timeout: 10_000 });
  } finally {
    await serve.stop();
  }
});
