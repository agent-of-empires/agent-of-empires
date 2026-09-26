// Right panel against a real server (#1221): diff viewer, Files pane, comments. The diff list's toggle and
// keyboard select are DiffFileList.test.tsx; the paired shell's mode picker is PairedShellPane.test.tsx.

import { rmSync } from "node:fs";
import { join } from "node:path";
import type { Page } from "@playwright/test";
import { test, expect, type ServeHandle } from "../helpers/liveTest";
import { listSessions, seedSessionViaAoeAdd } from "../helpers/aoeServe";
import { generateLargeFileContent, pngStubBytes, writeBinaryFile } from "../helpers/gitFixture";
import { enableStructuredViewAndWait } from "../helpers/acp";

async function openSession(page: Page, serve: ServeHandle, title: string) {
  await page.goto(`${serve.baseUrl}/`);
  const row = page.getByRole("link").filter({ hasText: title }).first();
  await expect(row).toBeVisible({ timeout: 10_000 });
  await row.click();
}

// The dashboard mounts a desktop and a hidden mobile right panel, so visible-anywhere checks use first().
const first = (page: Page, text: string) => page.getByText(text, { exact: true }).first();

test("files pane renders a Markdown file in a scratch session", async ({ page, spawnServe }) => {
  // #3088: a non-git directory has no diff, so its files come from the Files pane.
  const serve = await spawnServe({
    seedFn: seedSessionViaAoeAdd({
      title: "rp-files-md",
      subdir: "scratch-project",
      git: false,
      files: { "plan.md": "# The Plan\n\n- step one\n- step two\n", "readme.txt": "not markdown\n" },
    }),
  });
  await openSession(page, serve, "rp-files-md");
  await page.getByRole("button", { name: "Toggle Files pane" }).first().click();
  const planRow = page.getByRole("button", { name: "plan.md" }).first();
  await expect(planRow).toBeVisible({ timeout: 10_000 });
  await planRow.click();
  await expect(page.getByRole("heading", { name: "The Plan" }).first()).toBeVisible({ timeout: 10_000 });
  await expect(page.getByRole("listitem").filter({ hasText: "step one" }).first()).toBeVisible();
});

test("files pane numbers the lines of a source file", async ({ page, spawnServe }) => {
  // #4003: the file pane renders through the shared diff renderer, so it numbers lines like the diff pane.
  const serve = await spawnServe({
    seedFn: seedSessionViaAoeAdd({
      title: "rp-files-gutter",
      subdir: "gutter-project",
      git: false,
      files: { "notes.ts": "const a = 1;\nconst b = 2;\nconst c = 3;\n" },
    }),
  });
  await openSession(page, serve, "rp-files-gutter");
  await page.getByRole("button", { name: "Toggle Files pane" }).first().click();
  const notesRow = page.getByRole("button", { name: "notes.ts" }).first();
  await expect(notesRow).toBeVisible({ timeout: 10_000 });
  await notesRow.click();

  // Four cells for three lines of code: a file ending in a newline carries a
  // final empty line and the renderer numbers it, the way an editor shows it.
  const gutter = page.locator("[data-line-number-content]");
  await expect(gutter).toHaveCount(4, { timeout: 10_000 });
  await expect(gutter).toHaveText(["1", "2", "3", "4"]);
});

test("right panel diff viewer: 1000-line file scrolls, binary file shows placeholder", async ({ page, spawnServe }) => {
  const serve = await spawnServe({
    seedFn: seedSessionViaAoeAdd({
      title: "rp-large",
      // Committed first so every line shows as modified rather than one added hunk.
      committed: { "big.txt": generateLargeFileContent(1000, "base") },
      files: { "big.txt": generateLargeFileContent(1000, "edit") },
      prepare: (dir) => writeBinaryFile(dir, "image.png", pngStubBytes()),
    }),
  });
  await openSession(page, serve, "rp-large");
  await expect(first(page, "2 files")).toBeVisible({ timeout: 15_000 });

  await page
    .getByRole("button", { name: /big\.txt/ })
    .first()
    .click();
  await expect(page.getByText("big.txt").first()).toBeVisible({ timeout: 10_000 });
  await expect(page.locator("diffs-container").first()).toBeVisible({ timeout: 10_000 });
  // Virtualized: the last row mounts only after scrolling near it.
  await expect(page.getByText("edit 999:", { exact: false })).toHaveCount(0);
  await page.evaluate(() => {
    let el = document.querySelector("diffs-container")?.parentElement ?? null;
    while (el && el.scrollHeight <= el.clientHeight) el = el.parentElement;
    if (el) el.scrollTop = el.scrollHeight;
  });
  await expect(page.getByText("edit 999:", { exact: false }).first()).toBeVisible({ timeout: 15_000 });

  await page
    .getByRole("button", { name: /image\.png/ })
    .first()
    .click();
  await expect(first(page, "Binary file changed")).toBeVisible({ timeout: 10_000 });
});

test("right panel diff list: Open file shows the worktree copy, saves HTML, and is disabled for a deleted file", async ({
  page,
  spawnServe,
}) => {
  const serve = await spawnServe({
    seedFn: seedSessionViaAoeAdd({
      title: "rp-open-file",
      committed: { "notes.txt": "committed copy\n", "gone.txt": "bye\n" },
      files: { "notes.txt": "worktree copy\n", "page.html": "<script>document.title = 'ran';</script>\n" },
      prepare: (dir) => {
        writeBinaryFile(dir, "image.png", pngStubBytes());
        rmSync(join(dir, "gone.txt"));
      },
    }),
  });
  await openSession(page, serve, "rp-open-file");
  await expect(first(page, "4 files")).toBeVisible({ timeout: 15_000 });

  const openFileFor = async (name: RegExp) => {
    await page.getByRole("button", { name }).first().click({ button: "right" });
    return page.getByRole("menuitem", { name: "Open file" });
  };

  const [textTab] = await Promise.all([page.waitForEvent("popup"), (await openFileFor(/notes\.txt/)).click()]);
  await expect(textTab.locator("body")).toContainText("worktree copy", { timeout: 10_000 });
  await textTab.close();

  const [imageTab] = await Promise.all([page.waitForEvent("popup"), (await openFileFor(/image\.png/)).click()]);
  await expect(imageTab.locator("img")).toHaveCount(1, { timeout: 10_000 });
  await imageTab.close();

  // Active content is saved under its own name rather than rendered in the dashboard's origin.
  const [download] = await Promise.all([page.waitForEvent("download"), (await openFileFor(/page\.html/)).click()]);
  expect(download.suggestedFilename()).toBe("page.html");

  await expect(await openFileFor(/gone\.txt/)).toBeDisabled();
});

test("right panel notifications: structured view comments banner appears on stage, clears on discard", async ({
  page,
  spawnServe,
}) => {
  const serve = await spawnServe({
    acp: true,
    seedFn: seedSessionViaAoeAdd({
      title: "rp-notif",
      // A modified hunk gives the comment gutter line numbers.
      committed: { "notes.md": "line a\nline b\nline c\n" },
      files: { "notes.md": "line A\nline B\nline C\n" },
    }),
  });
  const sessionId = (await listSessions(serve.baseUrl)).find((s) => s.title === "rp-notif")?.id;
  if (!sessionId) throw new Error("seeded structured view session not visible in /api/sessions");
  await enableStructuredViewAndWait(serve.baseUrl, sessionId, 30_000, serve.home);
  // Accept Discard-all's confirm.
  page.on("dialog", (dialog) => void dialog.accept());

  await openSession(page, serve, "rp-notif");
  await expect(first(page, "1 file")).toBeVisible({ timeout: 15_000 });
  await page
    .getByRole("button", { name: /notes\.md/ })
    .first()
    .click();
  // The comment gutter exists only in the Raw diff view.
  await page.getByRole("button", { name: "Raw", exact: true }).first().click();
  const gutterLine1 = page.locator("[data-line-number-content]").filter({ hasText: /^1$/ }).first();
  await expect(gutterLine1).toBeVisible({ timeout: 10_000 });
  await gutterLine1.click();
  const textarea = page.getByPlaceholder(/Leave a comment/);
  await expect(textarea).toBeFocused({ timeout: 5_000 });
  await textarea.fill("nit");
  await page.getByRole("button", { name: "Save", exact: true }).click();

  await expect(first(page, "1 comment")).toBeVisible({ timeout: 10_000 });
  await expect(page.getByRole("button", { name: "Send", exact: true }).first()).toBeVisible();
  const discard = page.getByRole("button", { name: "Discard all", exact: true }).first();
  await expect(discard).toBeVisible();
  await discard.click();
  await expect(page.getByText("1 comment", { exact: true })).toHaveCount(0, { timeout: 10_000 });
});
