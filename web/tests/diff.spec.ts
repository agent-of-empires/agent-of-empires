import { test, expect } from "./helpers/mockedTest";
import type { Page } from "@playwright/test";
import { clickSidebarSession } from "./helpers/sidebar";
import { makeAllDifferentPatch } from "./helpers/patch";
import {
  diffFileResponse,
  diffFilesResponse,
  mockDiffFiles,
  openDiffFile,
  openDiffSession,
  setupDiffSession,
} from "./helpers/diffMocks";

test.use({ viewport: { width: 1280, height: 720 } });

const EXAMPLE_FILES = diffFilesResponse([{ path: "src/example.ts" }]);

// Diff rendering goes through @pierre/diffs. Syntax highlighting is the
// library's concern and runs off-thread; ours is feeding it the right contents
// and surfacing the text for any extension, worker or no worker.
test.describe("Diff rendering (@pierre/diffs)", () => {
  const EXAMPLE_CONTENTS = diffFileResponse(
    { path: "src/example.ts" },
    'import { useState } from "react";\nconst x = 42;\nexport default x;\n',
    'import { useState } from "react";\n' +
      "const x: number = 42;\n" +
      "function greet(name: string): string {\n" +
      "  return `Hello, ${name}`;\n" +
      "export default x;\n",
  );

  test("renders both sides of a TypeScript diff", async ({ page }) => {
    await setupDiffSession(page, { files: EXAMPLE_FILES, contents: EXAMPLE_CONTENTS });
    await openDiffSession(page);
    await openDiffFile(page, "example.ts");

    // getByText pierces the renderer's shadow DOM.
    await expect(page.getByText("function greet").first()).toBeVisible({ timeout: 15000 });
    await expect(page.getByText("const x = 42;").first()).toBeVisible();
  });

  test("renders content for an unrecognised extension", async ({ page }) => {
    await setupDiffSession(page, {
      files: diffFilesResponse([{ path: "data.xyz", status: "added", additions: 1, deletions: 0 }]),
      contents: diffFileResponse(
        { path: "data.xyz", status: "added", additions: 1, deletions: 0 },
        "",
        "some unknown format content\n",
      ),
    });
    await openDiffSession(page);
    await openDiffFile(page, "data.xyz");

    await expect(page.getByText("some unknown format content").first()).toBeVisible({ timeout: 10000 });
  });

  // #3362: a worker that failed to load hung the pool's initialize() forever
  // while still reporting healthy, leaving the pane blank until a full reload.
  test("still renders the diff when the highlighter worker fails to load", async ({ page }) => {
    await setupDiffSession(page, { files: EXAMPLE_FILES, contents: EXAMPLE_CONTENTS });
    await page.route("**/assets/worker-*.js", (r) => r.abort());
    await openDiffSession(page);
    await openDiffFile(page, "example.ts");

    await expect(page.getByText("function greet").first()).toBeVisible({ timeout: 15000 });
    await expect(page.getByRole("status").filter({ hasText: "main thread" })).toBeVisible();
  });
});

// The renderer virtualizes large diffs: off-screen rows are not in the DOM
// until scrolled near.
test.describe("Diff virtualization", () => {
  const LINES = 1000;
  // Mostly-shared context with every 10th line changed, so changed lines (the
  // ones the assertions key on) are spread through the file.
  const bigContent = (prefix: string) =>
    Array.from({ length: LINES }, (_, i) =>
      i % 10 === 0 ? `${prefix} ${i + 1}: changed` : `shared line ${i + 1}`,
    ).join("\n") + "\n";

  const BIG = { path: "big.txt", additions: LINES, deletions: LINES };

  async function openBigFile(page: Page) {
    await setupDiffSession(page, {
      files: diffFilesResponse([BIG]),
      contents: diffFileResponse(BIG, bigContent("base"), bigContent("edit")),
    });
    await openDiffSession(page);
    await openDiffFile(page, "big.txt");
    await expect(page.getByText("edit 1:", { exact: false }).first()).toBeVisible({ timeout: 15000 });
  }

  test("late rows mount only after scrolling (virtualized)", async ({ page }) => {
    await openBigFile(page);

    await expect(page.getByText("edit 981:", { exact: false })).toHaveCount(0);

    await page.evaluate(() => {
      const host = document.querySelector("diffs-container");
      let el = host?.parentElement as HTMLElement | null;
      while (el && el.scrollHeight <= el.clientHeight) el = el.parentElement;
      if (el) el.scrollTop = el.scrollHeight;
    });

    await expect(page.getByText("edit 991:", { exact: false }).first()).toBeVisible({ timeout: 15000 });
  });

  test("find searches only changed lines, jumps off-screen, and highlights", async ({ page }) => {
    await openBigFile(page);

    await page.getByRole("button", { name: "Find in diff" }).click();
    const input = page.getByRole("textbox", { name: "Find in diff" });

    // Context lines are not searchable in the MVP, so a shared line matches nothing.
    await input.fill("shared line 50");
    await expect(page.getByText("0/0")).toBeVisible();

    // A changed line deep in the file is found via the model, not the DOM.
    await input.fill("edit 971:");
    await expect(page.getByText(/^1\/\d+$/).first()).toBeVisible();
    await expect(page.getByText("edit 971:", { exact: false }).first()).toBeVisible({ timeout: 15000 });
    await expect(page.locator("[data-selected-line]").first()).toBeVisible({ timeout: 5000 });
  });
});

// The diff is computed server-side and shipped as a unified patch; the client
// parses it as text and offloads highlighting, so big lockfile churn renders
// without hanging the tab.
test.describe("Large diff handling", () => {
  // Realistic lockfile-ish lines: long, distinct, entirely different old vs new.
  const lockLines = (n: number, salt: string) =>
    Array.from(
      { length: n },
      (_, i) =>
        `  /@scope/pkg-${salt}-${i}@${(i % 9) + 1}.${i % 20}.${i % 7}: ` +
        `resolution: {integrity: sha512-${salt}${"abc123".repeat(8)}${i}}`,
    ).join("\n") + "\n";

  async function mount(page: Page, adds: number, dels: number) {
    // Precompute fixture data so none of this lands inside timed sections.
    const oldContent = lockLines(dels, "old");
    const newContent = lockLines(adds, "new");
    const file = { path: "pnpm-lock.yaml", additions: adds, deletions: dels };
    await setupDiffSession(page, {
      files: diffFilesResponse([file]),
      contents: diffFileResponse(
        file,
        oldContent,
        newContent,
        makeAllDifferentPatch("pnpm-lock.yaml", oldContent, newContent),
      ),
    });
    await page.goto("/");
    await clickSidebarSession(page, "pinch-test");
  }

  test("a +10k/-13k lockfile churn renders without crashing", async ({ page }) => {
    await mount(page, 10000, 13000);
    const t0 = Date.now();
    await page.getByText("pnpm-lock.yaml").first().click();
    await expect(page.locator("diffs-container").first()).toBeVisible({ timeout: 30000 });
    await expect(page.getByText("pkg-old-0@", { exact: false }).first()).toBeVisible({ timeout: 15000 });
    const elapsed = Date.now() - t0;
    // Text-parse plus virtualized render; the old contents-diffing path took
    // ~8s and could OOM the tab. Generous bound to avoid CI flake.
    expect(elapsed).toBeLessThan(10_000);
  });

  test("mid-size diff renders", async ({ page }) => {
    await mount(page, 4000, 4000);
    await page.getByText("pnpm-lock.yaml").first().click();
    await expect(page.locator("diffs-container").first()).toBeVisible({ timeout: 30000 });
  });
});

// #2152: an empty changes panel names the base instead of a context-free
// "No changes yet".
test.describe("Diff empty state (#2152)", () => {
  test("single-repo empty state names the base", async ({ page }) => {
    await setupDiffSession(page, { files: diffFilesResponse([], [{ base_branch: "origin/develop" }]) });
    await openDiffSession(page);
    // "origin/develop" also appears in the header chip, so assert against the
    // empty-state paragraph specifically.
    const emptyLine = page.getByText(/No changes vs/);
    await expect(emptyLine).toBeVisible({ timeout: 10000 });
    await expect(emptyLine).toContainText("origin/develop");
  });

  test("multi-repo empty state lists every repo with its base", async ({ page }) => {
    await setupDiffSession(page, {
      files: diffFilesResponse(
        [],
        [
          { repo_name: "taskrunner", base_branch: "origin/develop" },
          { repo_name: "MessageManager", base_branch: "origin/develop" },
          { repo_name: "SmartCaller", base_branch: "origin/main" },
        ],
      ),
    });
    await openDiffSession(page);
    // Multi-repo empty routes through MultiRepoGroups: each member shows a
    // header (name + "vs <base>") and a per-repo "no changes" note.
    await expect(page.getByText("taskrunner")).toBeVisible({ timeout: 10000 });
    await expect(page.getByText("MessageManager")).toBeVisible();
    await expect(page.getByText("SmartCaller")).toBeVisible();
    await expect(page.getByText("vs origin/main")).toBeVisible();
    await expect(page.getByText("No changes in this repo.").first()).toBeVisible();
  });
});

// Multi-repo workspaces fold subfolders inside each per-repo group, with the
// collapsed-dirs key namespaced so repos stay independent.
test.describe("Diff multi-repo subfolder folding", () => {
  const MULTI_REPO_FIELDS = {
    workspace_repos: [
      { name: "repo-a", source_path: "/tmp/multi/repo-a" },
      { name: "repo-b", source_path: "/tmp/multi/repo-b" },
    ],
  };
  const MULTI_REPO_FILES = diffFilesResponse(
    [
      { path: "src/api/server.ts", additions: 5, deletions: 1, repo_name: "repo-a" },
      { path: "src/api/routes.ts", status: "added", additions: 12, deletions: 0, repo_name: "repo-a" },
      { path: "src/web/index.tsx", additions: 3, deletions: 3, repo_name: "repo-b" },
      { path: "src/web/utils/format.ts", status: "added", additions: 8, deletions: 0, repo_name: "repo-b" },
    ],
    [
      { repo_name: "repo-a", base_branch: "main" },
      { repo_name: "repo-b", base_branch: "main" },
    ],
  );

  async function openMultiRepo(page: Page) {
    await setupDiffSession(page, { files: MULTI_REPO_FILES, sessionFields: MULTI_REPO_FIELDS });
    await openDiffSession(page);
    await expect(page.getByText("2 repos", { exact: true }).first()).toBeVisible({ timeout: 10000 });
  }

  test("the view-mode toggle is shown in multi-repo mode", async ({ page }) => {
    await openMultiRepo(page);
    // The toggle's title flips with the current mode; match either so the
    // assertion stays robust against the desktop default.
    await expect(
      page.locator('button[title="Switch to tree view"], button[title="Switch to flat list"]').first(),
    ).toBeVisible();
  });

  test("tree mode renders foldable dir rows inside each repo group", async ({ page }) => {
    await openMultiRepo(page);
    const toFlat = page.locator('button[title="Switch to flat list"]').first();
    const toTree = page.locator('button[title="Switch to tree view"]').first();
    if (await toTree.isVisible().catch(() => false)) await toTree.click();
    await expect(toFlat).toBeVisible();
    // Tree mode collapses repo-b's `src → web → utils` chain, so `web` shows once.
    const webDir = page.getByRole("button", { name: /^web/ });
    await expect(webDir.first()).toBeVisible();
    await expect(page.getByText("format.ts").first()).toBeVisible();
    await webDir.first().click();
    await expect(page.getByText("format.ts").first()).toBeHidden();
    // Folding repo-b's `web/` must not collapse repo-a's `api/`.
    await expect(page.getByText("routes.ts").first()).toBeVisible();
  });
});

// #970: the `vs <ref>` chip in the file-list header opens a branch typeahead;
// selecting one PATCHes /diff-base and a reset clears the override.
test.describe("Diff base override (#970)", () => {
  const chip = (page: Page, base?: string) =>
    page.getByRole("button", {
      name: base ? new RegExp(`Change diff base \\(current: ${base}\\)`) : /Change diff base/,
    });

  async function setupBaseOverride(page: Page, opts: { sessionFields?: Record<string, unknown> } = {}) {
    await setupDiffSession(page, { files: EXAMPLE_FILES, sessionFields: opts.sessionFields });
    await page.route("**/api/git/branches**", (r) =>
      r.fulfill({
        json: [
          { name: "main", is_current: true },
          { name: "develop", is_current: false },
          { name: "upstream/main", is_current: false, remote_only: true },
        ],
      }),
    );
  }

  async function capturePatch(page: Page) {
    const seen: { body: { base_branch?: string | null } | null } = { body: null };
    await page.route("**/api/sessions/*/diff-base", (r) => {
      seen.body = JSON.parse(r.request().postData() || "{}");
      return r.fulfill({ json: { id: "pinch-test" } });
    });
    return seen;
  }

  test("clicking the chip opens a typeahead populated from /api/git/branches", async ({ page }) => {
    await setupBaseOverride(page);
    await openDiffSession(page);
    await expect(chip(page, "main")).toBeVisible({ timeout: 10000 });
    await chip(page, "main").click();
    await expect(page.getByPlaceholder("Search branches...")).toBeVisible();
    await expect(page.getByRole("option", { name: /^main/ })).toBeVisible();
    await expect(page.getByRole("option", { name: /upstream\/main/ })).toBeVisible();
  });

  test("selecting a branch PATCHes diff-base and the chip reflects the new value", async ({ page }) => {
    await setupBaseOverride(page);
    const patched = await capturePatch(page);
    // After the PATCH the client refetches diff/files. `useDiffFiles` skips
    // state updates when the files fingerprint is unchanged, so the second
    // response mutates the files too to trigger the per-repo-bases swap.
    await mockDiffFiles(page, (call) =>
      call === 1
        ? EXAMPLE_FILES
        : diffFilesResponse([{ path: "src/example.ts", additions: 4 }], [{ base_branch: "develop" }]),
    );

    await openDiffSession(page);
    await expect(chip(page, "main")).toBeVisible({ timeout: 10000 });
    await chip(page, "main").click();
    await page.getByRole("option", { name: /develop/ }).click();
    await expect.poll(() => patched.body?.base_branch).toBe("develop");
    await expect(chip(page, "develop")).toBeVisible({ timeout: 5000 });
  });

  test("reset clears the override (PATCH with null)", async ({ page }) => {
    await setupBaseOverride(page, { sessionFields: { base_branch_override: "upstream/main" } });
    const patched = await capturePatch(page);

    await openDiffSession(page);
    await expect(chip(page)).toBeVisible({ timeout: 10000 });
    await chip(page).click();
    const reset = page.getByRole("button", { name: /Reset to auto-detected/ });
    await expect(reset).toBeVisible();
    await reset.click();
    await expect.poll(() => patched.body?.base_branch).toBeNull();
  });
});
