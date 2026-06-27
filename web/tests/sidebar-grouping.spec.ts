// Mocked coverage for the WorkspaceSidebar grouping surface, ported from
// the live suite (tests/live/sidebar-groups.spec.ts, sidebar-groups-axis
// .spec.ts, sidebar-nested-axis.spec.ts and the acp-stories filter /
// fold-group user stories).
//
//   - Two sessions in two different repos surface as two repo groups,
//     each with its session row (#1220).
//   - The filter input narrows the visible groups/rows by matching
//     against title, project path, branch, or agent, and a no-match
//     query renders the empty-state placeholder (#1220).
//   - The group header chevron flips `aria-expanded` and hides the row
//     list; tapping again restores it (#1220).
//   - The axis toggle re-buckets sessions by `group_path` (#1234), the
//     per-axis collapse map persists across reload, and the nested
//     repo+group axis renders subgroup headers inside the repository
//     block with independent collapse (#1720).
//
// Bucketing/split correctness is unit-tested in
// `src/lib/__tests__/sidebarGroups.test.ts`; live persistence semantics
// were folded into this mocked port because every assertion here is
// driven by client-side state (localStorage axis + collapse maps) over
// a static session list, which the stubbed /api surface reproduces.

import { test, expect } from "./helpers/mockedTest";
import type { Locator, Page } from "@playwright/test";
import { installSidebarMocks, type MockSessionInput } from "./helpers/sidebarMocks";

const HEADER = "[data-testid='sidebar-group-header']";
const ROW = "[data-testid='sidebar-session-row']";
const AXIS_TOGGLE = "[data-testid='sidebar-axis-toggle']";

function twoRepoSessions(): MockSessionInput[] {
  return [
    { id: "s-a", title: "alpha-session", project_path: "/tmp/repo-alpha", branch: "feat/a" },
    { id: "s-b", title: "beta-session", project_path: "/tmp/repo-beta", branch: "feat/b" },
  ];
}

// Three sessions in ONE repo across two user groups, mirroring the live
// seed (`aoe add -g feature` / `-g refactor`).
function groupedSessions(): MockSessionInput[] {
  return [
    { id: "s-f1", title: "feat-one", project_path: "/tmp/project", branch: "feat/one", group: "feature" },
    { id: "s-f2", title: "feat-two", project_path: "/tmp/project", branch: "feat/two", group: "feature" },
    { id: "s-r1", title: "refac-one", project_path: "/tmp/project", branch: "refac/one", group: "refactor" },
  ];
}

// groupedSessions plus a "fix" group and one ungrouped session, for the
// nested repo+group axis.
function nestedSessions(): MockSessionInput[] {
  return [
    { id: "s-f1", title: "feat-one", project_path: "/tmp/project", branch: "feat/one", group: "feature" },
    { id: "s-f2", title: "feat-two", project_path: "/tmp/project", branch: "feat/two", group: "feature" },
    { id: "s-x1", title: "fix-one", project_path: "/tmp/project", branch: "fix/one", group: "fix" },
    { id: "s-l1", title: "loose-one", project_path: "/tmp/project", branch: "loose/one" },
  ];
}

async function gotoDesktop(page: Page) {
  await page.setViewportSize({ width: 1280, height: 720 });
  await page.goto("/");
}

// Click the layers toggle until it reaches the requested axis. The toggle
// cycles repo -> group -> repo+group -> repo, so a bounded loop lands on
// any target without hard-coding the click count.
async function cycleAxisTo(toggle: Locator, target: string) {
  for (let i = 0; i < 3; i++) {
    const current = await toggle.getAttribute("data-axis");
    if (current === target) return;
    await toggle.click();
    // Wait for the axis to actually advance before reading again, so a
    // not-yet-flushed re-render cannot trigger an extra overshooting click.
    await expect(toggle).not.toHaveAttribute("data-axis", current ?? "");
  }
  await expect(toggle).toHaveAttribute("data-axis", target);
}

test.describe("sidebar repo groups (#1220)", () => {
  test("two repos render as two groups, both rows visible", async ({ page }) => {
    await installSidebarMocks(page, { sessions: twoRepoSessions() });
    await gotoDesktop(page);

    await expect(page.locator(HEADER)).toHaveCount(2);
    await expect(page.getByText("repo-alpha")).toBeVisible();
    await expect(page.getByText("repo-beta")).toBeVisible();

    await expect(page.locator(ROW)).toHaveCount(2);
    await expect(page.getByText("alpha-session")).toBeVisible();
    await expect(page.getByText("beta-session")).toBeVisible();
  });

  test("filter input narrows visible groups + rows by repo name", async ({ page }) => {
    await installSidebarMocks(page, { sessions: twoRepoSessions() });
    await gotoDesktop(page);

    await expect(page.locator(HEADER)).toHaveCount(2);

    await page.getByLabel("Filter sessions").click();
    const filter = page.locator("[data-testid='sidebar-filter-input']");
    await expect(filter).toBeVisible();

    await filter.fill("alpha");
    await expect(page.locator(HEADER)).toHaveCount(1);
    await expect(page.getByText("repo-alpha")).toBeVisible();
    await expect(page.getByText("repo-beta")).toBeHidden();

    // Clearing the input restores both groups; we drive the same input
    // rather than toggling the filter off because the toggle button
    // ALSO clears the query, which would hide the input we'd want to
    // assert on.
    await filter.fill("");
    await expect(page.locator(HEADER)).toHaveCount(2);

    // No-match query renders the empty-state placeholder.
    await filter.fill("nonexistent-repo-xyz");
    await expect(page.getByText(/No matches for/)).toBeVisible();
    await expect(page.locator(ROW)).toHaveCount(0);
  });

  test("group header chevron toggles aria-expanded and hides rows", async ({ page }) => {
    await installSidebarMocks(page, { sessions: twoRepoSessions() });
    await gotoDesktop(page);

    const alphaHeader = page.locator(HEADER, { has: page.getByText("repo-alpha") });
    const expandBtn = alphaHeader.locator("button[aria-expanded]");
    await expect(expandBtn).toHaveAttribute("aria-expanded", "true");

    await expandBtn.click();
    await expect(expandBtn).toHaveAttribute("aria-expanded", "false");

    // Collapsing the alpha group hides its row but leaves beta's row
    // (in the other group) untouched.
    await expect(page.getByText("alpha-session")).toBeHidden();
    await expect(page.getByText("beta-session")).toBeVisible();

    await expandBtn.click();
    await expect(expandBtn).toHaveAttribute("aria-expanded", "true");
    await expect(page.getByText("alpha-session")).toBeVisible();
  });
});

test.describe("sidebar user-group axis (#1234)", () => {
  test("axis toggle renders user groups by group_path", async ({ page }) => {
    await installSidebarMocks(page, { sessions: groupedSessions() });
    await gotoDesktop(page);

    // Default axis is "By repo": all three sessions live in one repo dir,
    // so there is a single repo group and three rows.
    const headers = page.locator(HEADER);
    await expect(headers).toHaveCount(1);
    await expect(page.locator(ROW)).toHaveCount(3);

    // The control-row heading names the repo axis "Sessions", not "Projects".
    const axisHeading = page.getByTestId("sidebar-axis-heading");
    await expect(axisHeading).toHaveText("Sessions");

    const axisToggle = page.locator(AXIS_TOGGLE);
    await expect(axisToggle).toHaveAttribute("data-axis", "repo");
    await axisToggle.click();
    await expect(axisToggle).toHaveAttribute("data-axis", "group");
    await expect(axisHeading).toHaveText("Groups");

    // Group axis: two headers, keyed by group_path. All three rows stay
    // visible, now nested under their group.
    await expect(headers).toHaveCount(2);
    await expect(page.locator(`${HEADER}[data-group-id='feature']`)).toBeVisible();
    await expect(page.locator(`${HEADER}[data-group-id='refactor']`)).toBeVisible();
    await expect(page.locator(ROW)).toHaveCount(3);
  });

  test("group-axis collapse persists across reload and is per-axis", async ({ page }) => {
    await installSidebarMocks(page, { sessions: groupedSessions() });
    await gotoDesktop(page);

    const axisToggle = page.locator(AXIS_TOGGLE);
    await expect(axisToggle).toHaveAttribute("data-axis", "repo");
    await axisToggle.click();

    const featureHeader = page.locator(`${HEADER}[data-group-id='feature']`);
    const featureExpand = featureHeader.locator("button[aria-expanded]");
    await expect(featureExpand).toHaveAttribute("aria-expanded", "true");

    await featureExpand.click();
    await expect(featureExpand).toHaveAttribute("aria-expanded", "false");
    await expect(page.getByText("feat-one")).toBeHidden();

    // Reload: the axis choice and the group collapse both restore from
    // localStorage.
    await page.reload();
    await expect(axisToggle).toHaveAttribute("data-axis", "group");
    await expect(featureHeader.locator("button[aria-expanded]")).toHaveAttribute("aria-expanded", "false");

    // Cycling back to the repo axis shows an independent collapse map:
    // the repo group is not collapsed just because a user group was. The
    // toggle cycles repo -> group -> repo+group -> repo (#1720), so
    // returning to repo from group takes two clicks.
    await cycleAxisTo(axisToggle, "repo");
    await expect(page.locator(`${HEADER} button[aria-expanded]`)).toHaveAttribute("aria-expanded", "true");
  });
});

test.describe("sidebar nested repo+group axis (#1720)", () => {
  test("nests user groups inside the repo block", async ({ page }) => {
    await installSidebarMocks(page, { sessions: nestedSessions() });
    await gotoDesktop(page);

    const axisToggle = page.locator(AXIS_TOGGLE);
    await expect(axisToggle).toHaveAttribute("data-axis", "repo");
    await cycleAxisTo(axisToggle, "repo+group");

    // One repository block holds all four sessions, split into three
    // nested subgroups: feature, fix, and Ungrouped.
    const repoBlocks = page.locator("[data-testid='sidebar-nested-repo']");
    await expect(repoBlocks).toHaveCount(1);
    const repo = repoBlocks.first();

    await expect(repo.locator("[data-testid='sidebar-nested-subgroup']")).toHaveCount(3);
    await expect(repo.locator("[data-testid='sidebar-nested-subgroup'] [data-group-id='feature']")).toBeVisible();
    await expect(repo.locator("[data-testid='sidebar-nested-subgroup'] [data-group-id='fix']")).toBeVisible();
    await expect(repo.locator("[data-testid='sidebar-nested-subgroup'] [data-group-id='__ungrouped__']")).toBeVisible();

    // Every session stays visible, now nested under its subgroup.
    await expect(page.locator(ROW)).toHaveCount(4);
  });

  test("subgroup collapse is independent of repo collapse and persists", async ({ page }) => {
    await installSidebarMocks(page, { sessions: nestedSessions() });
    await gotoDesktop(page);

    const axisToggle = page.locator(AXIS_TOGGLE);
    await expect(axisToggle).toHaveAttribute("data-axis", "repo");
    await cycleAxisTo(axisToggle, "repo+group");

    const featureSub = page.locator("[data-testid='sidebar-nested-subgroup'] [data-group-id='feature']");
    const featureExpand = featureSub.locator("button[aria-expanded]");
    await expect(featureExpand).toHaveAttribute("aria-expanded", "true");

    // Collapse just the feature subgroup: its rows hide, the fix
    // subgroup's rows stay, and the repo header stays expanded.
    await featureExpand.click();
    await expect(featureExpand).toHaveAttribute("aria-expanded", "false");
    await expect(page.getByText("feat-one")).toBeHidden();
    await expect(page.getByText("fix-one")).toBeVisible();

    // The subgroup collapse survives a reload (per-repo localStorage key).
    await page.reload();
    await expect(axisToggle).toHaveAttribute("data-axis", "repo+group");
    await expect(
      page
        .locator("[data-testid='sidebar-nested-subgroup'] [data-group-id='feature']")
        .locator("button[aria-expanded]"),
    ).toHaveAttribute("aria-expanded", "false");

    // Collapsing the repo header hides every nested subgroup.
    const repoHeader = page.locator("[data-testid='sidebar-nested-repo']").first().locator(HEADER).first();
    await repoHeader.locator("button[aria-expanded]").click();
    await expect(page.locator("[data-testid='sidebar-nested-subgroup']")).toHaveCount(0);
  });
});
