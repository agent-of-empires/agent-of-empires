import { spawnSync } from "node:child_process";
import { unlinkSync, writeFileSync } from "node:fs";
import { join } from "node:path";
import { test, expect, type TestInfo, type Page } from "@playwright/test";
import { listSessions, resolveAoeBinary, spawnAoeServe } from "../helpers/aoeServe";
import { commitAll, initWorkingRepo, writeFiles } from "../helpers/gitFixture";

function git(cwd: string, ...args: string[]) {
  const result = spawnSync("git", args, { cwd, encoding: "utf8" });
  if (result.status !== 0) throw new Error(`git ${args.join(" ")}: ${result.stderr}`);
}

async function evidence(page: Page, testInfo: TestInfo, step: string, result: string) {
  await page.evaluate(async () => {
    await document.fonts.ready;
    await new Promise<void>((resolve) => requestAnimationFrame(() => requestAnimationFrame(() => resolve())));
  });
  const screenshotPath = process.env.QA_SESSION_DIR
    ? join(process.env.QA_SESSION_DIR, `${step}.png`)
    : testInfo.outputPath(`${step}.png`);
  await page.screenshot({ path: screenshotPath });
  if (process.env.QA_SESSION_DIR) {
    writeFileSync(join(process.env.QA_SESSION_DIR, `${step}.json`), JSON.stringify({ url: page.url(), result }));
  }
  await testInfo.attach(step, {
    body: JSON.stringify({ url: page.url(), result }),
    contentType: "application/json",
  });
}

test("comparison modes use distinct Git baselines for the file list and contents", async ({ page }, testInfo) => {
  const consoleErrors: string[] = [];
  page.on("pageerror", (error) => consoleErrors.push(error.message));
  page.on("console", (message) => {
    if (message.type() === "error") consoleErrors.push(message.text());
  });
  const serve = await spawnAoeServe({
    authMode: "none",
    workerIndex: testInfo.workerIndex,
    parallelIndex: testInfo.parallelIndex,
    seedFn: ({ home, env }) => {
      const project = join(home, "project");
      initWorkingRepo(project);
      writeFiles(project, {
        "shared.txt": "main baseline\n",
        "staged.txt": "original staged file\n",
        "deleted.txt": "deleted content\n",
      });
      commitAll(project, "main baseline");
      git(project, "update-ref", "refs/remotes/origin/main", "HEAD");
      git(project, "switch", "-c", "feature/comparison");
      writeFiles(project, {
        "shared.txt": "upstream baseline\n",
        "pushed.txt": "already on tracking branch\n",
      });
      commitAll(project, "pushed changes");
      git(project, "remote", "add", "fork", ".");
      git(project, "update-ref", "refs/remotes/fork/review", "HEAD");
      git(project, "branch", "--set-upstream-to=fork/review");
      writeFiles(project, {
        "shared.txt": "local commit baseline\n",
        "committed.txt": "local committed change\n",
      });
      commitAll(project, "unpushed changes");
      writeFiles(project, {
        "shared.txt": "working tree content\n",
        "staged.txt": "staged change\n",
        "untracked.txt": "untracked content\n",
      });
      git(project, "add", "staged.txt");
      unlinkSync(join(project, "deleted.txt"));
      const added = spawnSync(resolveAoeBinary(), ["add", project, "-t", "diff-comparison", "-c", "claude"], { env });
      if (added.status !== 0) throw new Error(`aoe add failed: ${added.stderr?.toString()}`);
    },
  });

  try {
    const sessions = await listSessions(serve.baseUrl);
    const session = sessions.find((entry) => entry.title === "diff-comparison");
    expect(session).toBeDefined();
    const endpoint = `${serve.baseUrl}/api/sessions/${session!.id}/diff`;
    const dirtyPaths = ["deleted.txt", "shared.txt", "staged.txt", "untracked.txt"];
    const cases = [
      {
        mode: "origin_main",
        base: "origin/main",
        old: "main baseline",
        paths: [...dirtyPaths, "committed.txt", "pushed.txt"],
      },
      { mode: "upstream", base: "fork/review", old: "upstream baseline", paths: [...dirtyPaths, "committed.txt"] },
      { mode: "uncommitted", base: "HEAD", old: "local commit baseline", paths: dirtyPaths },
    ];

    await page.goto(`${serve.baseUrl}/`);
    await expect(page.getByRole("link").filter({ hasText: "diff-comparison" }).first()).toBeVisible();
    await evidence(page, testInfo, "001-dashboard", "Isolated test session is visible");
    await page.getByRole("link").filter({ hasText: "diff-comparison" }).first().click();
    const comparison = page.getByRole("combobox", { name: "Diff comparison" });
    await expect(comparison).toBeVisible();
    await evidence(page, testInfo, "002-session", "Comparison selector is visible");

    for (const [index, entry] of cases.entries()) {
      const filesResponse = await fetch(`${endpoint}/files?comparison=${entry.mode}`);
      expect(filesResponse.status).toBe(200);
      const files = await filesResponse.json();
      expect(files.warning ?? null).toBeNull();
      expect(files.per_repo_bases[0].base_branch).toBe(entry.base);
      expect(files.files.map((file: { path: string }) => file.path).sort()).toEqual([...entry.paths].sort());
      const contentsResponse = await fetch(`${endpoint}/file?path=shared.txt&comparison=${entry.mode}`);
      expect(contentsResponse.status).toBe(200);
      const contents = await contentsResponse.json();
      expect(contents.old_content).toBe(`${entry.old}\n`);
      expect(contents.new_content).toBe("working tree content\n");
      expect(contents.patch).toContain(`-${entry.old}`);
      expect(contents.patch).toContain("+working tree content");

      await comparison.selectOption(entry.mode);
      await expect(page.getByText(`${entry.paths.length} files`, { exact: true }).first()).toBeVisible();
      await evidence(
        page,
        testInfo,
        `00${index * 2 + 3}-${entry.mode}`,
        `${entry.base}: ${entry.paths.length} expected files`,
      );
      await page
        .getByRole("button", { name: /shared\.txt/ })
        .first()
        .click();
      await expect(page.getByText(entry.old, { exact: true }).first()).toBeVisible();
      await expect(page.getByText("working tree content", { exact: true }).first()).toBeVisible();
      await evidence(
        page,
        testInfo,
        `00${index * 2 + 4}-${entry.mode}-contents`,
        `Old content: ${entry.old}; new content: working tree content`,
      );
      await page.getByRole("button", { name: "Back to terminal" }).first().click();
      await expect(comparison).toBeVisible();
      await evidence(page, testInfo, `01${index + 1}-back-to-list`, "File viewer closed; comparison list is available");
    }

    git(join(serve.home, "project"), "branch", "--unset-upstream");
    await comparison.selectOption("upstream");
    await expect(page.getByText(/no .*upstream|upstream .*not|no .*tracking|tracking .*not/i).first()).toBeVisible();
    const missingUpstream = await fetch(`${endpoint}/file?path=shared.txt&comparison=upstream`);
    expect(missingUpstream.status).toBe(400);
    await evidence(
      page,
      testInfo,
      "009-missing-upstream",
      "Missing tracking branch is shown instead of a fallback diff",
    );

    git(join(serve.home, "project"), "update-ref", "-d", "refs/remotes/origin/main");
    await comparison.selectOption("origin_main");
    await expect(page.getByText(/origin\/main.*(not|missing|unavailable|found)/i).first()).toBeVisible();
    const missingMain = await fetch(`${endpoint}/files?comparison=origin_main`);
    const missingMainBody = await missingMain.json();
    expect(missingMainBody.files).toEqual([]);
    expect(missingMainBody.warning).toMatch(/origin\/main/);
    await expect(page.getByText("Comparison unavailable", { exact: true }).first()).toBeVisible();
    await expect(page.getByText("No changes vs origin/main", { exact: true })).toHaveCount(0);
    await evidence(
      page,
      testInfo,
      "010-missing-origin-main",
      "Missing origin/main is shown instead of a fallback diff",
    );
    expect(consoleErrors).toEqual([]);
    await testInfo.attach("console-errors", { body: JSON.stringify(consoleErrors), contentType: "application/json" });
  } finally {
    await serve.stop();
  }
});
