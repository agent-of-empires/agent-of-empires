// The Diff pane's Branch / Uncommitted / Last turn picker: the list, the viewer and the base picker follow it, and
// Last turn waits for a turn snapshot. Request logic: useDiffFiles.test.ts, DiffScopePicker.test.tsx.

import { test, expect } from "./helpers/mockedTest";
import {
  diffFileResponse,
  diffFilesResponse,
  openDiffFile,
  openDiffSession,
  setupDiffSession,
} from "./helpers/diffMocks";

test.use({ viewport: { width: 1280, height: 720 } });

const BRANCH = diffFilesResponse([{ path: "branch.ts" }, { path: "edited.ts" }]);
const UNCOMMITTED = diffFilesResponse([{ path: "edited.ts" }]);
const LAST_TURN = diffFilesResponse([{ path: "turn.ts" }]);

test("the scope picker switches the file list and the viewer between Branch, Uncommitted and Last turn", async ({
  page,
}) => {
  let snapshot: string | null = null;
  await setupDiffSession(page, { files: () => ({ ...BRANCH, last_turn_at: snapshot }) });
  const scopes: string[] = [];
  await page.route(/\/api\/sessions\/[^/]+\/diff\/files\?scope=/, (r) => {
    const scope = new URL(r.request().url()).searchParams.get("scope")!;
    scopes.push(scope);
    if (scope === "last_turn" && !snapshot) return r.fulfill({ status: 409, json: { error: "no_turn_snapshot" } });
    return r.fulfill({ json: { ...(scope === "uncommitted" ? UNCOMMITTED : LAST_TURN), last_turn_at: snapshot } });
  });
  const viewerScopes: (string | null)[] = [];
  await page.route(/\/api\/sessions\/[^/]+\/diff\/file\?/, (r) => {
    viewerScopes.push(new URL(r.request().url()).searchParams.get("scope"));
    return r.fulfill({ json: diffFileResponse({ path: "edited.ts" }, "a\n", "b\n") });
  });
  await openDiffSession(page);

  const list = page.locator('[data-pane-dock="right"]');
  await expect(list.getByText("branch.ts")).toBeVisible();
  await expect(page.getByRole("radio", { name: "Branch" })).toHaveAttribute("aria-checked", "true");
  await expect(page.getByRole("button", { name: /^Change diff base/ })).toBeVisible();
  const lastTurn = page.getByRole("radio", { name: "Last turn" });
  await expect(lastTurn).toBeDisabled();
  await expect(lastTurn).toHaveAttribute("title", "No turn has started since the daemon started");

  await page.getByRole("radio", { name: "Uncommitted" }).click();
  await expect(list.getByText("branch.ts")).toHaveCount(0);
  await expect(list.getByText("edited.ts")).toBeVisible();
  // The base branch only matters for Branch.
  await expect(page.getByRole("button", { name: /^Change diff base/ })).toHaveCount(0);
  await openDiffFile(page, "edited.ts");
  await expect.poll(() => viewerScopes).toContain("uncommitted");

  snapshot = "2026-11-25T12:00:00Z";
  await page.getByRole("radio", { name: "Branch" }).click();
  await expect(lastTurn).toBeEnabled();
  await lastTurn.click();
  await expect(list.getByText("turn.ts")).toBeVisible();
  await expect(list.getByText("edited.ts")).toHaveCount(0);
  expect(scopes).toContain("last_turn");

  // Remembered for the Session in this browser.
  await page.reload();
  await page.getByTestId("pane-toggle-diff").waitFor();
  await expect(page.getByRole("radio", { name: "Last turn" })).toHaveAttribute("aria-checked", "true");
});
