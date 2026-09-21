// Right panel against a real server (#1221): diff list and viewer, Files pane, paired terminal, comments.

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

test("right panel diff list: counts, tree/flat toggle, keyboard select", async ({ page, spawnServe }) => {
  const serve = await spawnServe({
    seedFn: seedSessionViaAoeAdd({
      title: "rp-files",
      committed: {
        "src/a.ts": "export const a = 1;\n",
        "src/b.ts": "export const b = 2;\n",
        "src/nested/c.ts": "export const c = 3;\n",
        "lib/d.ts": "export const d = 4;\n",
        "README.md": "# Old\n",
      },
      files: {
        "src/a.ts": "export const a = 11;\n",
        "src/b.ts": "export const b = 22;\n",
        "src/nested/c.ts": "export const c = 33;\n",
        "lib/d.ts": "export const d = 44;\n",
        "README.md": "# New\n",
      },
    }),
  });
  await openSession(page, serve, "rp-files");
  await expect(first(page, "5 files")).toBeVisible({ timeout: 15_000 });

  // The toggle's title names the other mode; land in tree mode first.
  const toTree = page.locator('button[title="Switch to tree view"]').first();
  const toFlat = page.locator('button[title="Switch to flat list"]').first();
  if (await toTree.isVisible().catch(() => false)) await toTree.click();
  await expect(toFlat).toBeVisible();
  await expect(page.getByRole("button", { name: /^src/ }).first()).toBeVisible();
  await toFlat.click();
  await expect(toTree).toBeVisible();

  // Files sort by path, so row 0 is README.md and row 1 is lib/d.ts. Assert viewer-only content.
  const firstRow = page.locator('button[data-index="0"]').first();
  await firstRow.hover();
  await firstRow.click();
  // Markdown renders by default (#3088); toggling Raw would steal focus from the list.
  await expect(page.getByRole("heading", { name: "New" }).first()).toBeVisible({ timeout: 10_000 });
  await page.keyboard.press("ArrowDown");
  await page.keyboard.press("Enter");
  await expect(page.getByText(/export const d = 4/).first()).toBeVisible({ timeout: 10_000 });
});

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

test("right panel paired terminal: Host shown, Container hidden on non-sandboxed session", async ({
  page,
  spawnServe,
}) => {
  const serve = await spawnServe({ seedFn: seedSessionViaAoeAdd({ title: "rp-paired" }) });
  await openSession(page, serve, "rp-paired");
  // #2437: the paired terminal mounts only while its tab is active.
  await page.getByTestId("pane-tab-terminal:0").filter({ visible: true }).click({ timeout: 10_000 });
  await expect(first(page, "Shell")).toBeVisible({ timeout: 10_000 });
  await expect(page.getByRole("button", { name: "Host", exact: true }).first()).toBeVisible();
  // Container is offered only for sandboxed sessions.
  await expect(page.getByRole("button", { name: "Container", exact: true })).toHaveCount(0);
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
