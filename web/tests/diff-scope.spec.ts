// The Diff pane's Branch / Uncommitted picker: the list, the viewer and the base picker follow it. Request logic:
// useDiffFiles.test.ts, DiffScopePicker.test.tsx.

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

test("the scope picker switches the file list and the viewer between Branch and Uncommitted", async ({ page }) => {
  await setupDiffSession(page, { files: () => BRANCH });
  const scopes: string[] = [];
  await page.route(/\/api\/sessions\/[^/]+\/diff\/files\?scope=/, (r) => {
    const scope = new URL(r.request().url()).searchParams.get("scope")!;
    scopes.push(scope);
    return r.fulfill({ json: UNCOMMITTED });
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

  await page.getByRole("radio", { name: "Uncommitted" }).click();
  await expect(list.getByText("branch.ts")).toHaveCount(0);
  await expect(list.getByText("edited.ts")).toBeVisible();
  // The base branch only matters for Branch.
  await expect(page.getByRole("button", { name: /^Change diff base/ })).toHaveCount(0);
  await openDiffFile(page, "edited.ts");
  await expect.poll(() => viewerScopes).toContain("uncommitted");

  expect(scopes).toContain("uncommitted");

  // Remembered for the Session in this browser.
  await page.reload();
  await page.getByTestId("right-dock-toggle").waitFor();
  await expect(page.getByRole("radio", { name: "Uncommitted" })).toHaveAttribute("aria-checked", "true");
});
