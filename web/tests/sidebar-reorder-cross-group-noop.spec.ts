// Drag is constrained to within one repo group: each repo's
// `SortableContext` lives behind a separate boundary, so an `over.id`
// that points into a different group's workspace list never matches
// the active group's reorder logic (WorkspaceSidebar.tsx:1639-1664).
//
// Seeds two repos with three workspaces each, drags a row from group
// A onto a row in group B, and asserts:
//   - both groups' visual orders are unchanged
//   - zero PUTs flew (handleDragEnd short-circuits at the `over.id`
//     check, no `onReorderWorkspaces` call)

import { test, expect } from "./helpers/mockedTest";
import {
  installSidebarMocks,
  workspaceId,
  type MockSessionInput,
} from "./helpers/sidebarMocks";

test("dragging a row onto a different repo group is a no-op", async ({ page }) => {
  const repoA = "/tmp/repo-a";
  const repoB = "/tmp/repo-b";
  const sessions: MockSessionInput[] = [
    { id: "a1", title: "alpha-a", project_path: repoA, branch: "feat/1" },
    { id: "a2", title: "beta-a", project_path: repoA, branch: "feat/2" },
    { id: "b1", title: "alpha-b", project_path: repoB, branch: "feat/1" },
    { id: "b2", title: "beta-b", project_path: repoB, branch: "feat/2" },
  ];
  const handle = await installSidebarMocks(page, {
    sessions,
    ordering: sessions.map((s) => workspaceId(s)),
  });

  await page.setViewportSize({ width: 1280, height: 720 });
  await page.goto("/");

  const wrappers = page.locator(
    "[aria-roledescription='Press and hold to reorder']",
  );
  await expect(wrappers).toHaveCount(4);

  // First two wrappers belong to repo A, next two to repo B (the
  // server-supplied ordering controls the render order). Drag the
  // last A row onto the first B row.
  const sourceBox = await wrappers.nth(1).boundingBox();
  const targetBox = await wrappers.nth(2).boundingBox();
  if (!sourceBox || !targetBox) throw new Error("row box missing");

  await page.mouse.move(
    sourceBox.x + sourceBox.width - 4,
    sourceBox.y + sourceBox.height / 2,
  );
  await page.mouse.down();
  await page.waitForTimeout(250);
  await page.mouse.move(
    targetBox.x + targetBox.width / 2,
    targetBox.y + targetBox.height / 2,
    { steps: 12 },
  );
  await page.mouse.up();

  // Wait a frame so handleDragEnd has had a chance to fire on a
  // regression that doesn't short-circuit cross-group.
  await page.waitForTimeout(300);

  const afterOrder = await wrappers.evaluateAll((els) =>
    els.map(
      (el) =>
        el.querySelector("span.truncate[title]")?.getAttribute("title") ?? "",
    ),
  );
  expect(afterOrder).toEqual(["alpha-a", "beta-a", "alpha-b", "beta-b"]);
  expect(handle.puts).toEqual([]);
});
