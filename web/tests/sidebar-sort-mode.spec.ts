// Sidebar sort picker (#1418, #1640): manual honors server ordering and allows drag; last activity and attention
// sort client-side, pin multi-repo last, persist in localStorage, and disable drag.

import { test, expect } from "./helpers/mockedTest";
import { sessionResponse as baseSession } from "./helpers/sessions";
import { mockStaticApis } from "./helpers/apiMocks";
import { Page } from "@playwright/test";

interface MockSession {
  id: string;
  title: string;
  project_path: string;
  branch: string | null;
  created_at: string;
  status?: string;
  urgent?: boolean;
  favorited?: boolean;
  last_accessed_at?: string | null;
  idle_entered_at?: string | null;
  workspace_repos?: { name: string; source_path: string; branch: string }[];
}

const sessionResponse = (s: MockSession) => baseSession({ favorited: false, urgent: false, ...s });

async function mockApis(
  page: Page,
  getSessions: () => MockSession[],
  getOrdering: () => string[],
  onPut?: (order: string[]) => void,
) {
  await mockStaticApis(page);
  await page.route("**/api/sessions", (r) => {
    if (r.request().method() !== "GET") return r.fulfill({ status: 400 });
    return r.fulfill({
      json: {
        sessions: getSessions().map(sessionResponse),
        workspace_ordering: getOrdering(),
      },
    });
  });
  await page.route("**/api/workspace-ordering", (r) => {
    const body = JSON.parse(r.request().postData() || "{}") as {
      order?: string[];
    };
    if (body.order) onPut?.(body.order);
    return r.fulfill({ json: { order: body.order ?? [] } });
  });
}

async function readWorkspaceTitles(page: Page): Promise<string[]> {
  return page.evaluate(() => {
    const rows = Array.from(document.querySelectorAll<HTMLAnchorElement>("[data-testid='sidebar-session-row']"));
    return rows.map((a) => a.querySelector("[title]")?.getAttribute("title") ?? "").filter(Boolean);
  });
}

const TOGGLE = "[data-testid='sidebar-sort-toggle']";

async function selectSortMode(page: Page, mode: string): Promise<void> {
  await page.locator(TOGGLE).click();
  await page.locator(`[data-testid='sidebar-sort-option-${mode}']`).click();
  await expect(page.locator(TOGGLE)).toHaveAttribute("data-sort-mode", mode);
}

test.describe("Sidebar sort picker (#1418, #1640)", () => {
  test("default is manual; rows follow server workspace_ordering", async ({ page }) => {
    const sessions: MockSession[] = [
      {
        id: "s-old",
        title: "old-ws",
        project_path: "/tmp/repo",
        branch: "feature/old",
        created_at: "2025-01-01T00:00:00Z",
      },
      {
        id: "s-new",
        title: "new-ws",
        project_path: "/tmp/repo",
        branch: "feature/new",
        created_at: "2025-04-01T00:00:00Z",
        last_accessed_at: "2025-04-15T00:00:00Z",
      },
    ];
    await mockApis(
      page,
      () => sessions,
      () => ["/tmp/repo::feature/old", "/tmp/repo::feature/new"],
    );
    await page.setViewportSize({ width: 1280, height: 720 });
    await page.goto("/");

    await expect(page.locator(TOGGLE)).toHaveAttribute("data-sort-mode", "manual");
    await expect.poll(() => readWorkspaceTitles(page), { timeout: 8000 }).toEqual(["old-ws", "new-ws"]);
  });

  test("selecting last-activity reorders desc and persists", async ({ page, context }) => {
    const sessions: MockSession[] = [
      {
        id: "s-old",
        title: "old-ws",
        project_path: "/tmp/repo",
        branch: "feature/old",
        created_at: "2025-01-01T00:00:00Z",
      },
      {
        id: "s-new",
        title: "new-ws",
        project_path: "/tmp/repo",
        branch: "feature/new",
        created_at: "2025-04-01T00:00:00Z",
        last_accessed_at: "2025-04-15T00:00:00Z",
      },
    ];
    await mockApis(
      page,
      () => sessions,
      () => ["/tmp/repo::feature/old", "/tmp/repo::feature/new"],
    );
    await page.setViewportSize({ width: 1280, height: 720 });
    await page.goto("/");

    await expect.poll(() => readWorkspaceTitles(page), { timeout: 8000 }).toEqual(["old-ws", "new-ws"]);

    await selectSortMode(page, "lastActivity");
    await expect.poll(() => readWorkspaceTitles(page), { timeout: 4000 }).toEqual(["new-ws", "old-ws"]);

    const stored = await page.evaluate(() => window.localStorage.getItem("aoe-sidebar-sort-mode"));
    expect(stored).toBe("lastActivity");

    await page.reload();
    await expect(page.locator(TOGGLE)).toHaveAttribute("data-sort-mode", "lastActivity");
    await expect.poll(() => readWorkspaceTitles(page), { timeout: 8000 }).toEqual(["new-ws", "old-ws"]);

    await selectSortMode(page, "manual");
    await expect.poll(() => readWorkspaceTitles(page), { timeout: 4000 }).toEqual(["old-ws", "new-ws"]);
    // suppress unused-binding lint without changing the signature
    void context;
  });

  test("drag affordances are absent in last-activity mode", async ({ page }) => {
    const sessions: MockSession[] = [
      {
        id: "s1",
        title: "alpha",
        project_path: "/tmp/repo",
        branch: "feature/a",
        created_at: "2025-01-01T00:00:00Z",
      },
      {
        id: "s2",
        title: "beta",
        project_path: "/tmp/repo",
        branch: "feature/b",
        created_at: "2025-02-01T00:00:00Z",
      },
    ];
    const puts: string[][] = [];
    await mockApis(
      page,
      () => sessions,
      () => ["/tmp/repo::feature/a", "/tmp/repo::feature/b"],
      (order) => puts.push(order),
    );
    await page.setViewportSize({ width: 1280, height: 720 });
    await page.goto("/");

    await expect(page.locator("[aria-roledescription='Press and hold to reorder']")).toHaveCount(2, { timeout: 8000 });

    await selectSortMode(page, "lastActivity");

    await expect(page.locator("[aria-roledescription='Press and hold to reorder']")).toHaveCount(0);

    const rows = page.locator("[data-testid='sidebar-session-row']");
    const sourceBox = await rows.nth(1).boundingBox();
    if (!sourceBox) throw new Error("row missing");
    await page.mouse.move(sourceBox.x + sourceBox.width - 4, sourceBox.y + sourceBox.height / 2);
    await page.mouse.down();
    await page.waitForTimeout(300);
    await page.mouse.move(sourceBox.x + 4, sourceBox.y + 4, { steps: 6 });
    await page.mouse.up();

    await selectSortMode(page, "manual");
    await expect(page.locator("[aria-roledescription='Press and hold to reorder']")).toHaveCount(2);

    expect(puts.length).toBe(0);
  });

  test("multi-repo group stays pinned at the bottom in last-activity mode", async ({ page }) => {
    const sessions: MockSession[] = [
      {
        id: "s-multi",
        title: "multi-recent",
        project_path: "/tmp/repo",
        branch: "feature/multi",
        created_at: "2025-01-01T00:00:00Z",
        last_accessed_at: "2025-12-01T00:00:00Z",
        workspace_repos: [
          { name: "repo-a", source_path: "/tmp/repo", branch: "feature/multi" },
          {
            name: "repo-b",
            source_path: "/tmp/other",
            branch: "feature/multi",
          },
        ],
      },
      {
        id: "s-single",
        title: "single-old",
        project_path: "/tmp/repo",
        branch: "feature/single",
        created_at: "2025-02-01T00:00:00Z",
        last_accessed_at: "2025-03-01T00:00:00Z",
      },
    ];
    await mockApis(
      page,
      () => sessions,
      () => ["/tmp/repo::feature/multi", "/tmp/repo::feature/single"],
    );
    await page.setViewportSize({ width: 1280, height: 720 });
    await page.goto("/");

    await expect(page.locator("[data-testid='sidebar-session-row']")).toHaveCount(2, { timeout: 8000 });

    await selectSortMode(page, "lastActivity");

    await expect.poll(() => readWorkspaceTitles(page), { timeout: 4000 }).toEqual(["single-old", "multi-recent"]);
  });

  test("attention sort floats waiting and urgent rows to the top (#1640)", async ({ page }) => {
    // One group with equal last_accessed_at, so only status ordering applies.
    const sessions: MockSession[] = [
      {
        id: "s-running",
        title: "running-ws",
        project_path: "/tmp/repo",
        branch: "feature/running",
        status: "Running",
        created_at: "2025-01-04T00:00:00Z",
        last_accessed_at: "2025-06-01T00:00:00Z",
      },
      {
        id: "s-waiting",
        title: "waiting-ws",
        project_path: "/tmp/repo",
        branch: "feature/waiting",
        status: "Waiting",
        created_at: "2025-01-03T00:00:00Z",
        last_accessed_at: "2025-06-01T00:00:00Z",
      },
      {
        id: "s-error",
        title: "error-ws",
        project_path: "/tmp/repo",
        branch: "feature/error",
        status: "Error",
        created_at: "2025-01-02T00:00:00Z",
        last_accessed_at: "2025-06-01T00:00:00Z",
      },
      {
        id: "s-urgent",
        title: "urgent-ws",
        project_path: "/tmp/repo",
        branch: "feature/urgent",
        // Urgent promotes across status ranks.
        status: "Running",
        urgent: true,
        created_at: "2025-01-01T00:00:00Z",
        last_accessed_at: "2025-06-01T00:00:00Z",
      },
    ];
    await mockApis(
      page,
      () => sessions,
      () => [
        "/tmp/repo::feature/running",
        "/tmp/repo::feature/waiting",
        "/tmp/repo::feature/error",
        "/tmp/repo::feature/urgent",
      ],
    );
    await page.setViewportSize({ width: 1280, height: 720 });
    await page.goto("/");

    await expect(page.locator("[data-testid='sidebar-session-row']")).toHaveCount(4, { timeout: 8000 });

    await selectSortMode(page, "attention");

    await expect
      .poll(() => readWorkspaceTitles(page), { timeout: 4000 })
      .toEqual(["urgent-ws", "waiting-ws", "error-ws", "running-ws"]);

    const stored = await page.evaluate(() => window.localStorage.getItem("aoe-sidebar-sort-mode"));
    expect(stored).toBe("attention");
  });

  // #1836, #2214: the trigger uses the shared portaled Tooltip, not a native title, and the list gains a separator.
  test("sort tooltip matches the styled group/filter tooltip; sidebar has separators (#1836)", async ({ page }) => {
    const sessions: MockSession[] = [
      {
        id: "s1",
        title: "alpha",
        project_path: "/tmp/repo",
        branch: "feature/a",
        created_at: "2025-01-01T00:00:00Z",
      },
    ];
    await mockApis(
      page,
      () => sessions,
      () => ["/tmp/repo::feature/a"],
    );
    await page.setViewportSize({ width: 1280, height: 720 });
    await page.goto("/");

    const toggle = page.locator(TOGGLE);
    await expect(toggle).toBeVisible({ timeout: 8000 });

    await expect(toggle).not.toHaveAttribute("title", /.+/);

    await toggle.hover();
    const tip = page.getByRole("tooltip").filter({ hasText: "Sort: manual, drag enabled" });
    await expect(tip).toHaveClass(/bg-surface-950/);
    await expect(tip).toHaveClass(/fixed/);

    await expect(page.locator("div.overflow-y-auto.border-t")).toHaveCount(1);
  });
});
