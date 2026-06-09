// Live-backend DirectoryBrowser spec (#1219).
//
// Drives the real `/api/filesystem/home` + `/api/filesystem/browse`
// endpoints behind an isolated HOME tree, opens the wizard, switches to
// the Browse tab, navigates the listing, picks a git repo, and confirms
// the `aoe-last-browse-dir` localStorage key persisted by
// `DirectoryBrowser` is set to the picked repo's parent (mirrors the
// TUI behavior: reopening the picker lands one level up so the repo's
// siblings are visible).

import { mkdirSync, writeFileSync } from "node:fs";
import { join } from "node:path";
import { test as base, expect } from "@playwright/test";
import { spawnAoeServe } from "../helpers/aoeServe";

base("DirectoryBrowser: home -> projects -> repo-a -> persisted last dir", async ({ page }, testInfo) => {
  const serve = await spawnAoeServe({
    authMode: "none",
    workerIndex: testInfo.workerIndex,
    parallelIndex: testInfo.parallelIndex,
    seedFn: ({ home }) => {
      // Build $HOME/projects/repo-a/.git so the browser shows `projects`
      // at home, `repo-a` (with the repo badge) once we descend, and so
      // selecting `repo-a` persists $HOME/projects as the last-used dir.
      const projects = join(home, "projects");
      const repoA = join(projects, "repo-a");
      const repoB = join(projects, "repo-b");
      mkdirSync(join(repoA, ".git"), { recursive: true });
      writeFileSync(join(repoA, ".git", "HEAD"), "ref: refs/heads/main\n");
      mkdirSync(repoB, { recursive: true });
    },
  });

  try {
    await page.goto(serve.baseUrl);

    // Resolve the actual HOME path via the same endpoint the SPA uses,
    // so assertions don't hard-code the tmpdir.
    const homeRes = await fetch(`${serve.baseUrl}/api/filesystem/home`);
    expect(homeRes.ok).toBeTruthy();
    const { path: homePath } = (await homeRes.json()) as { path: string };
    expect(homePath).toBeTruthy();

    // Wait for an interactive surface, then open the wizard.
    await page.locator("body").click();
    await page.keyboard.press("n");
    await expect(page.getByRole("heading", { name: "New session" })).toBeVisible({
      timeout: 10_000,
    });

    // Empty home (no recent sessions on this fresh serve) defaults to
    // the Browse tab. Wait for the projects entry to appear under home.
    const projectsEntry = page.getByRole("option").filter({ hasText: "projects" });
    await expect(projectsEntry).toBeVisible({ timeout: 10_000 });

    await projectsEntry.click();
    // Inside `projects`, repo-a should carry the repo badge.
    const repoARow = page.getByRole("option").filter({ hasText: "repo-a" });
    await expect(repoARow).toBeVisible();
    const repoBRow = page.getByRole("option").filter({ hasText: "repo-b" });
    await expect(repoBRow).toBeVisible();
    // Only repo-a has .git: the row's accessible name includes the
    // trailing "repo" badge (the entry text + badge span concatenate).
    await expect(repoARow).toHaveAccessibleName(/^repo-a\s+repo$/);
    await expect(repoBRow).toHaveAccessibleName(/^repo-b$/);

    // Picking repo-a flips the wizard back to the Recent tab and writes
    // the parent dir to localStorage.
    await repoARow.click();
    await expect(page.getByText("Selected project")).toBeVisible();
    await expect(page.getByText(`${homePath}/projects/repo-a`)).toBeVisible();

    const persisted = await page.evaluate(() => window.localStorage.getItem("aoe-last-browse-dir"));
    expect(persisted).toBe(`${homePath}/projects`);

    // Close + reopen the wizard. The Browse tab should land on the
    // persisted dir, showing repo-a / repo-b directly (no `projects`).
    await page.getByRole("button", { name: "Close" }).click();
    await page.locator("body").click();
    await page.keyboard.press("n");
    await expect(page.getByRole("heading", { name: "New session" })).toBeVisible();
    await expect(page.getByRole("option").filter({ hasText: "repo-a" })).toBeVisible({
      timeout: 10_000,
    });
    // `projects` no longer appears as an entry: we're inside it.
    await expect(page.getByRole("option").filter({ hasText: "projects" })).toHaveCount(0);
  } finally {
    await serve.stop();
  }
});

base("DirectoryBrowser: parent-dir row navigates up one level", async ({ page }, testInfo) => {
  const serve = await spawnAoeServe({
    authMode: "none",
    workerIndex: testInfo.workerIndex,
    parallelIndex: testInfo.parallelIndex,
    seedFn: ({ home }) => {
      mkdirSync(join(home, "projects", "nested"), { recursive: true });
    },
  });

  try {
    await page.goto(serve.baseUrl);
    await page.locator("body").click();
    await page.keyboard.press("n");
    await expect(page.getByRole("heading", { name: "New session" })).toBeVisible({
      timeout: 10_000,
    });

    // Descend twice: home -> projects -> nested.
    await page.getByRole("option").filter({ hasText: "projects" }).click({ timeout: 10_000 });
    await page.getByRole("option").filter({ hasText: "nested" }).click();
    await expect(page.getByText("No visible subfolders here")).toBeVisible();

    // Parent-dir option ("..") goes back up one level, where `nested`
    // is once again a child entry.
    await page.getByRole("option").filter({ hasText: "(parent directory)" }).click();
    await expect(page.getByRole("option").filter({ hasText: "nested" })).toBeVisible({
      timeout: 5_000,
    });
  } finally {
    await serve.stop();
  }
});
