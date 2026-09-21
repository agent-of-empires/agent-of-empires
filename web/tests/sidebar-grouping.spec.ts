// Sidebar grouping axes (#1220, #1234, #1720, #3283): repo, group, nested repo+group, and org buckets with
// per-axis collapse persisted in localStorage. Bucketing is unit-tested in src/lib/__tests__/sidebarGroups.test.ts.

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

function groupedSessions(): MockSessionInput[] {
  return [
    { id: "s-f1", title: "feat-one", project_path: "/tmp/project", branch: "feat/one", group: "feature" },
    { id: "s-f2", title: "feat-two", project_path: "/tmp/project", branch: "feat/two", group: "feature" },
    { id: "s-r1", title: "refac-one", project_path: "/tmp/project", branch: "refac/one", group: "refactor" },
  ];
}

function nestedSessions(): MockSessionInput[] {
  return [
    { id: "s-f1", title: "feat-one", project_path: "/tmp/project", branch: "feat/one", group: "feature" },
    { id: "s-f2", title: "feat-two", project_path: "/tmp/project", branch: "feat/two", group: "feature" },
    { id: "s-x1", title: "fix-one", project_path: "/tmp/project", branch: "fix/one", group: "fix" },
    { id: "s-l1", title: "loose-one", project_path: "/tmp/project", branch: "loose/one" },
  ];
}

function orgSessions(): MockSessionInput[] {
  return [
    { id: "s-a", title: "alpha-session", project_path: "/tmp/repo-alpha", branch: "feat/a", remote_owner: "acme" },
    { id: "s-b", title: "beta-session", project_path: "/tmp/repo-beta", branch: "feat/b", remote_owner: "acme" },
    { id: "s-c", title: "gamma-session", project_path: "/tmp/repo-gamma", branch: "feat/c", remote_owner: null },
  ];
}

// Stub owner avatars so the test makes no network request.
async function stubOwnerAvatars(page: Page) {
  await page.route("https://github.com/**", (r) => r.fulfill({ status: 404, body: "" }));
}

async function resolvedColor(page: Page, token: string): Promise<string> {
  return await page.evaluate((name) => {
    const probe = document.createElement("span");
    probe.style.color = `var(${name})`;
    document.body.append(probe);
    const rgb = getComputedStyle(probe).color;
    probe.remove();
    return rgb;
  }, token);
}

async function gotoDesktop(page: Page) {
  await page.setViewportSize({ width: 1280, height: 720 });
  await page.goto("/");
}

// The toggle cycles repo, org, group, repo+group.
async function cycleAxisTo(toggle: Locator, target: string) {
  for (let i = 0; i < 3; i++) {
    const current = await toggle.getAttribute("data-axis");
    if (current === target) return;
    await toggle.click();
    // Wait for each advance so a pending render cannot cause an extra click.
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

    // Clear the input rather than the toggle, which would also hide the input.
    await filter.fill("");
    await expect(page.locator(HEADER)).toHaveCount(2);

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

    await expect(page.getByText("alpha-session")).toBeHidden();
    await expect(page.getByText("beta-session")).toBeVisible();

    await expandBtn.click();
    await expect(expandBtn).toHaveAttribute("aria-expanded", "true");
    await expect(page.getByText("alpha-session")).toBeVisible();
  });

  test("a collapsed group marks that it holds the open session (#3912)", async ({ page }) => {
    await installSidebarMocks(page, { sessions: twoRepoSessions() });
    await page.setViewportSize({ width: 1280, height: 720 });
    await page.goto("/session/s-a");

    const alphaHeader = page.locator(HEADER, { has: page.getByText("repo-alpha") });
    const betaHeader = page.locator(HEADER, { has: page.getByText("repo-beta") });
    const expandBtn = alphaHeader.locator("button[aria-expanded]");

    // Expanded, the open session's row carries the frame instead of the header.
    await expect(expandBtn).toHaveAttribute("aria-expanded", "true");
    expect(await alphaHeader.getAttribute("class")).not.toContain("border-session-active");

    // Collapsed, the header uses the projected token; border-brand-600 fails contrast on catppuccin-latte.
    await expandBtn.click();
    await expect(page.getByText("alpha-session")).toBeHidden();
    expect(await alphaHeader.getAttribute("class")).toContain("border-session-active");
    expect(await betaHeader.getAttribute("class")).not.toContain("border-session-active");
    // toHaveCSS polls past the color transition.
    await expect(alphaHeader).toHaveCSS("border-left-color", await resolvedColor(page, "--color-session-active"));
  });
});

test.describe("sidebar user-group axis (#1234)", () => {
  test("axis toggle renders user groups by group_path", async ({ page }) => {
    await installSidebarMocks(page, { sessions: groupedSessions() });
    await gotoDesktop(page);

    const headers = page.locator(HEADER);
    await expect(headers).toHaveCount(1);
    await expect(page.locator(ROW)).toHaveCount(3);

    const axisHeading = page.getByTestId("sidebar-axis-heading");
    await expect(axisHeading).toHaveText("Sessions");

    const axisToggle = page.locator(AXIS_TOGGLE);
    await expect(axisToggle).toHaveAttribute("data-axis", "repo");
    await cycleAxisTo(axisToggle, "group");
    await expect(axisToggle).toHaveAttribute("data-axis", "group");
    await expect(axisHeading).toHaveText("Groups");

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
    await cycleAxisTo(axisToggle, "group");

    const featureHeader = page.locator(`${HEADER}[data-group-id='feature']`);
    const featureExpand = featureHeader.locator("button[aria-expanded]");
    await expect(featureExpand).toHaveAttribute("aria-expanded", "true");

    await featureExpand.click();
    await expect(featureExpand).toHaveAttribute("aria-expanded", "false");
    await expect(page.getByText("feat-one")).toBeHidden();

    await page.reload();
    await expect(axisToggle).toHaveAttribute("data-axis", "group");
    await expect(featureHeader.locator("button[aria-expanded]")).toHaveAttribute("aria-expanded", "false");

    // Collapse maps are per axis; repo is two clicks past group.
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

    const repoBlocks = page.locator("[data-testid='sidebar-nested-repo']");
    await expect(repoBlocks).toHaveCount(1);
    const repo = repoBlocks.first();

    await expect(repo.locator("[data-testid='sidebar-nested-subgroup']")).toHaveCount(3);
    await expect(repo.locator("[data-testid='sidebar-nested-subgroup'] [data-group-id='feature']")).toBeVisible();
    await expect(repo.locator("[data-testid='sidebar-nested-subgroup'] [data-group-id='fix']")).toBeVisible();
    await expect(repo.locator("[data-testid='sidebar-nested-subgroup'] [data-group-id='__ungrouped__']")).toBeVisible();

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

    await featureExpand.click();
    await expect(featureExpand).toHaveAttribute("aria-expanded", "false");
    await expect(page.getByText("feat-one")).toBeHidden();
    await expect(page.getByText("fix-one")).toBeVisible();

    await page.reload();
    await expect(axisToggle).toHaveAttribute("data-axis", "repo+group");
    await expect(
      page
        .locator("[data-testid='sidebar-nested-subgroup'] [data-group-id='feature']")
        .locator("button[aria-expanded]"),
    ).toHaveAttribute("aria-expanded", "false");

    const repoHeader = page.locator("[data-testid='sidebar-nested-repo']").first().locator(HEADER).first();
    await repoHeader.locator("button[aria-expanded]").click();
    await expect(page.locator("[data-testid='sidebar-nested-subgroup']")).toHaveCount(0);
  });
});

test.describe("sidebar org axis (#3283)", () => {
  test("buckets repos by remote owner, with No organization pinned last", async ({ page }) => {
    await stubOwnerAvatars(page);
    await installSidebarMocks(page, { sessions: orgSessions() });
    await gotoDesktop(page);

    const axisToggle = page.locator(AXIS_TOGGLE);
    await expect(axisToggle).toHaveAttribute("data-axis", "repo");
    await cycleAxisTo(axisToggle, "org");

    const orgBlocks = page.locator("[data-testid='sidebar-org-group']");
    await expect(orgBlocks).toHaveCount(2);

    const acmeOrg = page.locator("[data-testid='sidebar-org-group'][data-org-id='acme@example.com']");
    await expect(acmeOrg.locator(HEADER).first()).toContainText("acme");
    await expect(acmeOrg.locator("[data-testid='sidebar-org-repo']")).toHaveCount(2);

    const noOrg = page.locator("[data-testid='sidebar-org-group'][data-org-id='__no_org__']");
    await expect(noOrg.locator(HEADER).first()).toContainText("No organization");
    await expect(noOrg.locator("[data-testid='sidebar-org-repo']")).toHaveCount(1);

    await expect(orgBlocks.nth(0)).toHaveAttribute("data-org-id", "acme@example.com");
    await expect(orgBlocks.nth(1)).toHaveAttribute("data-org-id", "__no_org__");

    await expect(page.locator(ROW)).toHaveCount(3);
  });

  test("repo collapse within an org is independent of the org header and persists", async ({ page }) => {
    await stubOwnerAvatars(page);
    await installSidebarMocks(page, { sessions: orgSessions() });
    await gotoDesktop(page);

    const axisToggle = page.locator(AXIS_TOGGLE);
    await expect(axisToggle).toHaveAttribute("data-axis", "repo");
    await cycleAxisTo(axisToggle, "org");

    const acmeOrg = page.locator("[data-testid='sidebar-org-group'][data-org-id='acme@example.com']");
    const alphaRepo = acmeOrg.locator("[data-testid='sidebar-org-repo'][data-repo-id='/tmp/repo-alpha']");
    const alphaExpand = alphaRepo.locator("button[aria-expanded]");
    await expect(alphaExpand).toHaveAttribute("aria-expanded", "true");

    await alphaExpand.click();
    await expect(alphaExpand).toHaveAttribute("aria-expanded", "false");
    await expect(page.getByText("alpha-session")).toBeHidden();
    await expect(page.getByText("beta-session")).toBeVisible();

    await page.reload();
    await expect(axisToggle).toHaveAttribute("data-axis", "org");
    await expect(
      page.locator("[data-testid='sidebar-org-repo'][data-repo-id='/tmp/repo-alpha']").locator("button[aria-expanded]"),
    ).toHaveAttribute("aria-expanded", "false");

    const orgHeader = acmeOrg.locator(HEADER).first();
    await orgHeader.locator("button[aria-expanded]").click();
    await expect(acmeOrg.locator("[data-testid='sidebar-org-repo']")).toHaveCount(0);
  });
});
