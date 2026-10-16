import { spawnSync } from "node:child_process";
import { unlinkSync } from "node:fs";
import { join } from "node:path";
import { expect, test } from "../helpers/liveTest";
import { listSessions, resolveAoeBinary } from "../helpers/aoeServe";
import { commitAll, initWorkingRepo, writeFiles } from "../helpers/gitFixture";

function git(cwd: string, ...args: string[]) {
  const result = spawnSync("git", args, { cwd, encoding: "utf8" });
  if (result.status !== 0) throw new Error(`git ${args.join(" ")}: ${result.stderr}`);
}

test("comparison modes use distinct Git baselines without leaking across sessions", async ({ page, spawnServe }) => {
  const consoleErrors: string[] = [];
  page.on("pageerror", (error) => consoleErrors.push(error.message));
  const serve = await spawnServe({
    authMode: "none",
    seedFn: ({ home, env }) => {
      const project = join(home, "project");
      initWorkingRepo(project, env);
      writeFiles(project, {
        "shared.txt": "main baseline\n",
        "staged.txt": "original staged file\n",
        "deleted.txt": "deleted content\n",
      });
      commitAll(project, "main baseline", env);
      git(project, "switch", "-c", "feature/comparison");
      writeFiles(project, {
        "shared.txt": "upstream baseline\n",
        "pushed.txt": "already on tracking branch\n",
      });
      commitAll(project, "pushed changes", env);
      git(project, "remote", "add", "fork", ".");
      git(project, "update-ref", "refs/remotes/fork/review", "HEAD");
      git(project, "branch", "--set-upstream-to=fork/review");
      writeFiles(project, {
        "shared.txt": "local commit baseline\n",
        "committed.txt": "local committed change\n",
      });
      commitAll(project, "unpushed changes", env);
      writeFiles(project, {
        "shared.txt": "working tree content\n",
        "staged.txt": "staged change\n",
        "untracked.txt": "untracked content\n",
      });
      git(project, "add", "staged.txt");
      unlinkSync(join(project, "deleted.txt"));
      const other = join(home, "without-upstream");
      initWorkingRepo(other, env);
      writeFiles(other, { "local.txt": "base\n" });
      commitAll(other, "base", env);
      writeFiles(other, { "local.txt": "working\n" });
      for (const [path, title] of [
        [project, "diff-comparison"],
        [other, "without-upstream"],
      ]) {
        const added = spawnSync(resolveAoeBinary(), ["add", path, "-t", title, "-c", "claude"], { env });
        if (added.status !== 0) throw new Error(`aoe add failed: ${added.stderr?.toString()}`);
      }
    },
  });

  const sessions = await listSessions(serve.baseUrl);
  const session = sessions.find((entry) => entry.title === "diff-comparison");
  expect(session).toBeDefined();
  const endpoint = `${serve.baseUrl}/api/sessions/${session!.id}/diff`;
  const dirtyPaths = ["deleted.txt", "shared.txt", "staged.txt", "untracked.txt"];
  const cases = [
    { mode: "base", base: "main", old: "main baseline", paths: [...dirtyPaths, "committed.txt", "pushed.txt"] },
    { mode: "upstream", base: "fork/review", old: "upstream baseline", paths: [...dirtyPaths, "committed.txt"] },
    { mode: "uncommitted", base: "HEAD", old: "local commit baseline", paths: dirtyPaths },
  ];

  await page.goto(`${serve.baseUrl}/`);
  await page.getByRole("link").filter({ hasText: "diff-comparison" }).first().click();
  const comparison = page.getByRole("button", { name: /^Diff comparison \(current:/ });
  await expect(comparison).toBeVisible();
  const chooseComparison = async (mode: string) => {
    const labels: Record<string, string> = { base: "Base", uncommitted: "Uncommitted", upstream: "Tracking branch" };
    await comparison.click();
    await page
      .getByRole("group", { name: "Diff comparison" })
      .getByRole("button", { name: labels[mode], exact: true })
      .click();
  };

  for (const entry of cases) {
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

    await chooseComparison(entry.mode);
    await expect(page.getByText(`${entry.paths.length} files`, { exact: true }).first()).toBeVisible();
    await page
      .getByRole("button", { name: /shared\.txt/ })
      .first()
      .click();
    await expect(page.getByText(entry.old, { exact: true }).first()).toBeVisible();
    await expect(page.getByText("working tree content", { exact: true }).first()).toBeVisible();
    await page.getByRole("button", { name: "Back to terminal" }).first().click();
    await expect(comparison).toBeVisible();
  }

  await chooseComparison("upstream");
  await expect(page.getByText("5 files", { exact: true }).first()).toBeVisible();
  await page.getByRole("link").filter({ hasText: "without-upstream" }).first().click();
  await expect(comparison).toHaveAccessibleName("Diff comparison (current: Base)");
  await expect(page.getByRole("button", { name: /local\.txt/ }).first()).toBeVisible();
  await expect(page.getByText("Comparison unavailable", { exact: true })).toHaveCount(0);
  await page.getByRole("link").filter({ hasText: "diff-comparison" }).first().click();
  await expect(comparison).toHaveAccessibleName("Diff comparison (current: Tracking branch)");

  git(join(serve.home, "project"), "branch", "--unset-upstream");
  await chooseComparison("uncommitted");
  await expect(page.getByText("4 files", { exact: true }).first()).toBeVisible();
  await chooseComparison("upstream");
  await expect(page.getByText(/no .*upstream|upstream .*not|no .*tracking|tracking .*not/i).first()).toBeVisible();
  const missingUpstream = await fetch(`${endpoint}/file?path=shared.txt&comparison=upstream`);
  expect(missingUpstream.status).toBe(400);
  const unavailable = await fetch(`${endpoint}/files?comparison=upstream`);
  const unavailableBody = await unavailable.json();
  expect(unavailableBody.files).toEqual([]);
  expect(unavailableBody.warning).toMatch(/upstream/);
  await expect(page.getByText("Comparison unavailable", { exact: true }).first()).toBeVisible();
  await expect(page.getByText("No changes vs upstream", { exact: true })).toHaveCount(0);
  expect(consoleErrors).toEqual([]);
});
