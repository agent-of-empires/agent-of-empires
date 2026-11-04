// Sidebar grouping axes (#1220, #1234, #1720, #3283): repo, group, nested repo+group, and org buckets with
// per-axis collapse persisted in localStorage. Bucketing is unit-tested in src/lib/__tests__/sidebarGroups.test.ts.

import { test, expect } from "./helpers/mockedTest";
import type { Locator, Page } from "@playwright/test";
import { installSidebarMocks, type MockSessionInput } from "./helpers/sidebarMocks";
import { sessionResponse } from "./helpers/sessions";

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

function statusSessions(): MockSessionInput[] {
  return [
    {
      id: "s-idle",
      title: "idle-session",
      project_path: "/tmp/project",
      branch: "feat/idle",
      fields: { status: "Idle" },
    },
    {
      id: "s-waiting",
      title: "waiting-session",
      project_path: "/tmp/project",
      branch: "feat/waiting",
      fields: { status: "Waiting" },
    },
    {
      id: "s-running",
      title: "running-session",
      project_path: "/tmp/project",
      branch: "feat/running",
      fields: { status: "Running" },
    },
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

// The axis picker sets any axis directly, so status is one pick from the default.
async function pickAxis(page: Page, toggle: Locator, target: string) {
  await toggle.click();
  await page.getByTestId(`sidebar-axis-option-${target}`).click();
  await expect(toggle).toHaveAttribute("data-axis", target);
}

test.describe("sidebar repo groups (#1220)", () => {
  test("two repos render as two groups; the filter narrows groups and rows by repo name", async ({ page }) => {
    await installSidebarMocks(page, { sessions: twoRepoSessions() });
    await gotoDesktop(page);

    await expect(page.locator(HEADER)).toHaveCount(2);
    await expect(page.getByText("repo-alpha")).toBeVisible();
    await expect(page.getByText("repo-beta")).toBeVisible();

    await expect(page.locator(ROW)).toHaveCount(2);
    await expect(page.getByText("alpha-session")).toBeVisible();
    await expect(page.getByText("beta-session")).toBeVisible();

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
  test("axis toggle renders user groups by group_path; their collapse persists across reload per axis", async ({
    page,
  }) => {
    await installSidebarMocks(page, { sessions: groupedSessions() });
    await gotoDesktop(page);

    const headers = page.locator(HEADER);
    await expect(headers).toHaveCount(1);
    await expect(page.locator(ROW)).toHaveCount(3);

    const axisHeading = page.getByTestId("sidebar-axis-heading");
    await expect(axisHeading).toHaveText("Sessions");

    const axisToggle = page.locator(AXIS_TOGGLE);
    await expect(axisToggle).toHaveAttribute("data-axis", "repo");
    await pickAxis(page, axisToggle, "group");
    await expect(axisToggle).toHaveAttribute("data-axis", "group");
    await expect(axisHeading).toHaveText("Groups");

    await expect(headers).toHaveCount(2);
    await expect(page.locator(`${HEADER}[data-group-id='feature']`)).toBeVisible();
    await expect(page.locator(`${HEADER}[data-group-id='refactor']`)).toBeVisible();
    await expect(page.locator(ROW)).toHaveCount(3);

    const featureHeader = page.locator(`${HEADER}[data-group-id='feature']`);
    const featureExpand = featureHeader.locator("button[aria-expanded]");
    await expect(featureExpand).toHaveAttribute("aria-expanded", "true");

    await featureExpand.click();
    await expect(featureExpand).toHaveAttribute("aria-expanded", "false");
    await expect(page.getByText("feat-one")).toBeHidden();

    await page.reload();
    await expect(axisToggle).toHaveAttribute("data-axis", "group");
    await expect(featureHeader.locator("button[aria-expanded]")).toHaveAttribute("aria-expanded", "false");

    // Collapse maps are per axis; the picker switches straight back to repo.
    await pickAxis(page, axisToggle, "repo");
    await expect(page.locator(`${HEADER} button[aria-expanded]`)).toHaveAttribute("aria-expanded", "true");
  });
});

test.describe("sidebar nested repo+group axis (#1720)", () => {
  test("nests user groups inside the repo block; subgroup collapse is independent of repo collapse and persists", async ({
    page,
  }) => {
    await installSidebarMocks(page, { sessions: nestedSessions() });
    await gotoDesktop(page);

    const axisToggle = page.locator(AXIS_TOGGLE);
    await expect(axisToggle).toHaveAttribute("data-axis", "repo");
    await pickAxis(page, axisToggle, "repo+group");

    const repoBlocks = page.locator("[data-testid='sidebar-nested-repo']");
    await expect(repoBlocks).toHaveCount(1);
    const repo = repoBlocks.first();

    await expect(repo.locator("[data-testid='sidebar-nested-subgroup']")).toHaveCount(3);
    await expect(repo.locator("[data-testid='sidebar-nested-subgroup'] [data-group-id='feature']")).toBeVisible();
    await expect(repo.locator("[data-testid='sidebar-nested-subgroup'] [data-group-id='fix']")).toBeVisible();
    await expect(repo.locator("[data-testid='sidebar-nested-subgroup'] [data-group-id='__ungrouped__']")).toBeVisible();

    await expect(page.locator(ROW)).toHaveCount(4);

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

  test("a Shift+click range skips the rows hidden under a collapsed repo", async ({ page }) => {
    const sessions: MockSessionInput[] = ["a", "b", "c"].map((name) => ({
      id: `s-${name}`,
      title: `${name}-session`,
      project_path: `/tmp/repo-${name}`,
      branch: `feat/${name}`,
      group: "feature",
    }));
    const archived: string[] = [];
    await installSidebarMocks(page, { sessions });
    await page.route("**/api/sessions/*/archive", async (route) => {
      const id = decodeURIComponent(new URL(route.request().url()).pathname.split("/").at(-2)!);
      const input = sessions.find((s) => s.id === id);
      archived.push(id);
      await route.fulfill({
        json: sessionResponse({
          id,
          title: input?.title,
          project_path: input?.project_path,
          branch: input?.branch,
          group_path: input?.group ?? "",
          ...input?.fields,
        }),
      });
    });
    await gotoDesktop(page);
    await pickAxis(page, page.locator(AXIS_TOGGLE), "repo+group");

    // Hide repo B's row, then range from A to C across it.
    const repoB = page.locator("[data-testid='sidebar-nested-repo'][data-repo-id='/tmp/repo-b']");
    await repoB.locator(HEADER).first().locator("button[aria-expanded]").click();
    await expect(page.getByText("b-session")).toBeHidden();

    const row = (title: string) => page.locator(ROW).filter({ hasText: title });
    await row("a-session").click({ modifiers: ["Meta"] });
    await row("c-session").click({ modifiers: ["Shift"] });
    await row("c-session").click({ button: "right" });
    await page.getByTestId("sidebar-context-menu-bulk-archive").click();

    await expect.poll(() => archived.length).toBe(2);
    expect([...archived].sort()).toEqual(["s-a", "s-c"]);
  });
});

test.describe("sidebar status axis (#4867)", () => {
  test("groups sessions by execution status in attention order; collapse persists and stays per axis", async ({
    page,
  }) => {
    await installSidebarMocks(page, { sessions: statusSessions() });
    await gotoDesktop(page);

    const axisToggle = page.locator(AXIS_TOGGLE);
    const axisHeading = page.getByTestId("sidebar-axis-heading");
    await expect(axisToggle).toHaveAttribute("data-axis", "repo");
    await expect(page.locator(ROW)).toHaveCount(3);

    await pickAxis(page, axisToggle, "status");
    await expect(axisToggle).toHaveAttribute("data-axis", "status");
    await expect(axisHeading).toHaveText("Status");

    const headers = page.locator(HEADER);
    await expect(headers).toHaveCount(3);
    // The Attention sort's order: Waiting, then Idle, then Running.
    await expect(headers.nth(0)).toHaveAttribute("data-group-id", "status:Waiting");
    await expect(headers.nth(1)).toHaveAttribute("data-group-id", "status:Idle");
    await expect(headers.nth(2)).toHaveAttribute("data-group-id", "status:Running");
    await expect(page.locator(ROW)).toHaveCount(3);

    const waitingExpand = page.locator(`${HEADER}[data-group-id='status:Waiting']`).locator("button[aria-expanded]");
    await waitingExpand.click();
    await expect(waitingExpand).toHaveAttribute("aria-expanded", "false");
    await expect(page.getByText("waiting-session")).toBeHidden();
    await expect(page.getByText("running-session")).toBeVisible();

    await page.reload();
    await expect(axisToggle).toHaveAttribute("data-axis", "status");
    await expect(
      page.locator(`${HEADER}[data-group-id='status:Waiting']`).locator("button[aria-expanded]"),
    ).toHaveAttribute("aria-expanded", "false");

    // Collapse keys are per axis; the repo header never collapsed.
    await pickAxis(page, axisToggle, "repo");
    await expect(page.locator(`${HEADER} button[aria-expanded]`)).toHaveAttribute("aria-expanded", "true");
  });

  test("the picker follows the menu-button pattern: focus on open, arrows, Escape and Tab-out", async ({ page }) => {
    await installSidebarMocks(page, { sessions: statusSessions() });
    await gotoDesktop(page);

    const axisToggle = page.locator(AXIS_TOGGLE);
    const menu = page.getByTestId("sidebar-axis-menu");
    const option = (target: string) => page.getByTestId(`sidebar-axis-option-${target}`);
    await axisToggle.focus();
    await page.keyboard.press("Enter");

    await expect(menu).toBeVisible();
    // Same single-select menu semantics as the sort picker beside it.
    await expect(menu).toHaveAttribute("role", "menu");
    await expect(option("repo")).toHaveAttribute("role", "menuitemradio");
    // Focus enters on the checked option, and the menu owns the arrows, so options are not Tab stops.
    await expect(option("repo")).toBeFocused();
    await expect(option("repo")).toHaveAttribute("tabindex", "-1");

    await page.keyboard.press("ArrowDown");
    await expect(option("org")).toBeFocused();
    await page.keyboard.press("End");
    await expect(option("status")).toBeFocused();
    // Both ends wrap.
    await page.keyboard.press("ArrowDown");
    await expect(option("repo")).toBeFocused();
    await page.keyboard.press("ArrowUp");
    await expect(option("status")).toBeFocused();
    await page.keyboard.press("Home");
    await expect(option("repo")).toBeFocused();

    // Escape closes without picking and hands the focus back to the trigger.
    await page.keyboard.press("Escape");
    await expect(menu).toBeHidden();
    await expect(axisToggle).toBeFocused();
    await expect(axisToggle).toHaveAttribute("data-axis", "repo");

    // Enter on an arrowed-to option commits it, and the trigger keeps the focus.
    await page.keyboard.press("Enter");
    await page.keyboard.press("ArrowDown");
    await page.keyboard.press("Enter");
    await expect(menu).toBeHidden();
    await expect(axisToggle).toHaveAttribute("data-axis", "org");
    await expect(axisToggle).toBeFocused();

    // Tabbing out closes the menu instead of walking its options.
    await page.keyboard.press("Enter");
    await expect(menu).toBeVisible();
    await page.keyboard.press("Tab");
    await expect(menu).toBeHidden();
  });

  test("bulk archive targets each selected row's own session when one workspace straddles two buckets", async ({
    page,
  }) => {
    // One workspace with a Waiting and a Running session: the status axis renders it twice, and the
    // Waiting bucket sorts first, so the workspace id alone resolves to the Waiting session.
    const sessions: MockSessionInput[] = [
      {
        id: "s-wait",
        title: "wait-session",
        project_path: "/tmp/shared",
        branch: "feat/shared",
        fields: { status: "Waiting" },
      },
      {
        id: "s-run",
        title: "run-session",
        project_path: "/tmp/shared",
        branch: "feat/shared",
        fields: { status: "Running" },
      },
      {
        id: "s-other",
        title: "other-session",
        project_path: "/tmp/other",
        branch: "feat/other",
        fields: { status: "Running" },
      },
    ];
    const archived: string[] = [];
    await installSidebarMocks(page, { sessions });
    await page.route("**/api/sessions/*/archive", async (route) => {
      const id = decodeURIComponent(new URL(route.request().url()).pathname.split("/").at(-2)!);
      const input = sessions.find((s) => s.id === id);
      archived.push(id);
      await route.fulfill({
        json: sessionResponse({
          id,
          title: input?.title,
          project_path: input?.project_path,
          branch: input?.branch,
          group_path: input?.group ?? "",
          ...input?.fields,
        }),
      });
    });
    await gotoDesktop(page);
    await pickAxis(page, page.locator(AXIS_TOGGLE), "status");

    await expect(page.getByText("wait-session")).toBeVisible();
    // Cmd rather than Ctrl: Chromium on macOS reads Ctrl+click as a secondary click.
    const [runRow, otherRow] = ["run-session", "other-session"].map((title) =>
      page.locator(ROW).filter({ hasText: title }),
    );
    await runRow!.click({ modifiers: ["Meta"] });
    await otherRow!.click({ modifiers: ["Meta"] });
    await otherRow!.click({ button: "right" });
    await page.getByTestId("sidebar-context-menu-bulk-archive").click();

    await expect.poll(() => archived.length).toBe(2);
    expect([...archived].sort()).toEqual(["s-other", "s-run"]);
  });

  test("a workspace split across buckets marks only the slice showing the active session", async ({ page }) => {
    const sessions: MockSessionInput[] = [
      {
        id: "s-wait",
        title: "wait-session",
        project_path: "/tmp/shared",
        branch: "feat/shared",
        fields: { status: "Waiting" },
      },
      {
        id: "s-run",
        title: "run-session",
        project_path: "/tmp/shared",
        branch: "feat/shared",
        fields: { status: "Running" },
      },
    ];
    await installSidebarMocks(page, { sessions });
    await gotoDesktop(page);
    await pickAxis(page, page.locator(AXIS_TOGGLE), "status");

    await page.locator(ROW).filter({ hasText: "run-session" }).click();
    await expect(page).toHaveURL(/\/session\/s-run/);
    // Both slices share the workspace id, but only the Running one shows the open session.
    const active = page.locator("[data-active-session-row]");
    await expect(active).toHaveCount(1);
    await expect(active).toContainText("run-session");
  });

  test("a range across the footer selects each workspace once, not its hidden status slices", async ({ page }) => {
    const archivedUntil = { archived_at: "2026-01-01T00:00:00Z" };
    const sessions: MockSessionInput[] = [
      {
        id: "s-w1-waiting",
        title: "w1-waiting",
        project_path: "/tmp/w1",
        branch: "feat/w1",
        fields: { status: "Waiting", ...archivedUntil },
      },
      {
        id: "s-w1-running",
        title: "w1-running",
        project_path: "/tmp/w1",
        branch: "feat/w1",
        fields: { status: "Running", ...archivedUntil },
      },
      {
        id: "s-w2-idle",
        title: "w2-idle",
        project_path: "/tmp/w2",
        branch: "feat/w2",
        fields: { status: "Idle", ...archivedUntil },
      },
    ];
    const unarchived: string[] = [];
    await installSidebarMocks(page, { sessions });
    await page.route("**/api/sessions/*/archive", async (route) => {
      const id = decodeURIComponent(new URL(route.request().url()).pathname.split("/").at(-2)!);
      unarchived.push(id);
      await route.fulfill({ json: sessionResponse({ id }) });
    });
    await gotoDesktop(page);
    await pickAxis(page, page.locator(AXIS_TOGGLE), "status");
    await page.getByTestId("sidebar-sunk-toggle").click();

    // One footer row per workspace: W1 is sliced into Waiting and Running buckets internally.
    const footerRows = page.locator(`[data-testid='sidebar-sunk-section'] ${ROW}`);
    await expect(footerRows).toHaveCount(2);

    await footerRows.nth(0).click();
    await footerRows.nth(1).click({ modifiers: ["Shift"] });
    await footerRows.nth(1).click({ button: "right" });
    await page.getByTestId("sidebar-context-menu-bulk-unarchive").click();

    await expect.poll(() => unarchived.length).toBe(2);
    expect([...unarchived].sort()).toEqual(["s-w1-waiting", "s-w2-idle"]);
  });

  test("snoozing one status slice leaves its sibling row and bulk eligibility alone", async ({ page }) => {
    const sessions: MockSessionInput[] = [
      {
        id: "s-wait",
        title: "wait-session",
        project_path: "/tmp/shared",
        branch: "feat/shared",
        fields: { status: "Waiting" },
      },
      {
        id: "s-run",
        title: "run-session",
        project_path: "/tmp/shared",
        branch: "feat/shared",
        fields: { status: "Running" },
      },
    ];
    const snoozed: string[] = [];
    await installSidebarMocks(page, { sessions });
    await page.route("**/api/sessions/*/snooze", async (route) => {
      const id = decodeURIComponent(new URL(route.request().url()).pathname.split("/").at(-2)!);
      snoozed.push(id);
      const { minutes } = route.request().postDataJSON() as { minutes: number | null };
      await route.fulfill({
        json: sessionResponse({ id, snoozed_until: minutes == null ? null : "2099-01-01T00:00:00Z" }),
      });
    });
    await gotoDesktop(page);
    await pickAxis(page, page.locator(AXIS_TOGGLE), "status");

    const waitingRow = page.locator(ROW).filter({ hasText: "wait-session" });
    const runningRow = page.locator(ROW).filter({ hasText: "run-session" });
    await runningRow.click({ button: "right" });
    await page.getByTestId("sidebar-context-menu-snooze").click();
    await page.getByTestId("snooze-modal-preset-60").click();
    await expect.poll(() => snoozed).toEqual(["s-run"]);

    // Only the Running row carries the pending snooze.
    await expect(runningRow.getByLabel("Snoozed")).toBeVisible();
    await expect(waitingRow.getByLabel("Snoozed")).toHaveCount(0);

    // Bulk buckets follow the same scope: one snoozable row, one unsnoozable. A plain click first
    // clears the single selection the snooze row's right-click left behind.
    await waitingRow.click();
    await waitingRow.click({ modifiers: ["Meta"] });
    await runningRow.click({ modifiers: ["Meta"] });
    await runningRow.click({ button: "right" });
    await expect(page.getByTestId("sidebar-context-menu-bulk-unsnooze")).toHaveText("Unsnooze 1");
    await expect(page.getByText("Snooze 1", { exact: true })).toBeVisible();
  });
});

test.describe("sidebar org axis (#3283)", () => {
  test("buckets repos by remote owner, No organization last; repo collapse within an org persists independently", async ({
    page,
  }) => {
    await stubOwnerAvatars(page);
    await installSidebarMocks(page, { sessions: orgSessions() });
    await gotoDesktop(page);

    const axisToggle = page.locator(AXIS_TOGGLE);
    await expect(axisToggle).toHaveAttribute("data-axis", "repo");
    await pickAxis(page, axisToggle, "org");

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
