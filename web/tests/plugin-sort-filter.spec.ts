// Mocked-Playwright coverage for the plugin sort-key and filter-facet slots in
// the sidebar (#2401).
//
// Drives the WorkspaceSidebar against fully-stubbed /api responses, including
// /api/plugins/ui-state, so the only thing under test is the React wiring:
//   1. A plugin sort-key appears in the sort picker and reorders rows by the
//      referenced row-column's sort_value in the declared direction.
//   2. A plugin filter-facet renders a facet control that filters rows by the
//      referenced row-column's filter_values, combined with the text filter.
//
// The fallback-when-entries-vanish and the comparator math live in the unit
// tests (src/lib/__tests__/pluginUi.test.ts, sidebarSort.test.ts).

import { test, expect } from "./helpers/mockedTest";
import { Page } from "@playwright/test";

interface MockSession {
  id: string;
  title: string;
  branch: string;
  created_at: string;
}

function sessionResponse(s: MockSession) {
  return {
    id: s.id,
    title: s.title,
    project_path: "/tmp/repo",
    group_path: "/tmp/repo",
    tool: "claude",
    status: "Idle",
    yolo_mode: false,
    created_at: s.created_at,
    last_accessed_at: null,
    idle_entered_at: null,
    last_error: null,
    branch: s.branch,
    main_repo_path: null,
    is_sandboxed: false,
    favorited: false,
    urgent: false,
    has_terminal: true,
    profile: "default",
    workspace_repos: [],
  };
}

// A row-column with a sort scalar, a status facet token, plus the global
// sort-key and filter-facet that reference them, for one session.
function entriesForSession(sessionId: string, sortValue: number, status: string) {
  return [
    {
      plugin_id: "acme.kit",
      slot: "row-column",
      id: "metric",
      session_id: sessionId,
      payload: { text: String(sortValue), sort_value: sortValue },
    },
    {
      plugin_id: "acme.kit",
      slot: "row-column",
      id: "status-col",
      session_id: sessionId,
      payload: { text: status, filter_values: [status] },
    },
  ];
}

const GLOBAL_ENTRIES = [
  {
    plugin_id: "acme.kit",
    slot: "sort-key",
    id: "by-metric",
    payload: { label: "Metric", column: "metric", direction: "desc" },
  },
  {
    plugin_id: "acme.kit",
    slot: "filter-facet",
    id: "by-status",
    payload: {
      label: "Status",
      column: "status-col",
      options: [
        { value: "running", label: "Running", tone: "success" },
        { value: "idle", label: "Idle" },
      ],
    },
  },
];

async function mockApis(page: Page, sessions: MockSession[], ordering: string[], uiEntries: unknown[]) {
  await page.route("**/api/login/status", (r) => r.fulfill({ json: { required: false, authenticated: true } }));
  await page.route("**/api/sessions", (r) => {
    if (r.request().method() !== "GET") return r.fulfill({ status: 400 });
    return r.fulfill({ json: { sessions: sessions.map(sessionResponse), workspace_ordering: ordering } });
  });
  await page.route("**/api/plugins/ui-state", (r) => r.fulfill({ json: { entries: uiEntries, notifications: [] } }));
  for (const path of ["settings", "themes", "agents", "profiles", "groups", "devices", "docker/status", "about"]) {
    await page.route(`**/api/${path}`, (r) => r.fulfill({ json: path === "docker/status" ? {} : [] }));
  }
}

async function readWorkspaceTitles(page: Page): Promise<string[]> {
  return page.evaluate(() => {
    const rows = Array.from(document.querySelectorAll<HTMLAnchorElement>("[data-testid='sidebar-session-row']"));
    return rows.map((a) => a.querySelector("[title]")?.getAttribute("title") ?? "").filter(Boolean);
  });
}

const TOGGLE = "[data-testid='sidebar-sort-toggle']";

const SESSIONS: MockSession[] = [
  { id: "s-low", title: "low-ws", branch: "feature/low", created_at: "2025-01-01T00:00:00Z" },
  { id: "s-high", title: "high-ws", branch: "feature/high", created_at: "2025-01-02T00:00:00Z" },
  { id: "s-mid", title: "mid-ws", branch: "feature/mid", created_at: "2025-01-03T00:00:00Z" },
];
// Manual order pins low, then high, then mid; absent the plugin sort this is
// the render order.
const ORDERING = ["/tmp/repo::feature/low", "/tmp/repo::feature/high", "/tmp/repo::feature/mid"];

const UI_ENTRIES = [
  ...GLOBAL_ENTRIES,
  ...entriesForSession("s-low", 1, "idle"),
  ...entriesForSession("s-high", 100, "running"),
  ...entriesForSession("s-mid", 50, "running"),
];

test.describe("Plugin sort-key and filter-facet slots (#2401)", () => {
  test("a plugin sort-key reorders rows by sort_value desc", async ({ page }) => {
    await mockApis(page, SESSIONS, ORDERING, UI_ENTRIES);
    await page.setViewportSize({ width: 1280, height: 720 });
    await page.goto("/");

    // Manual order first.
    await expect.poll(() => readWorkspaceTitles(page), { timeout: 8000 }).toEqual(["low-ws", "high-ws", "mid-ws"]);

    // Open the picker; the plugin option appears below the built-ins.
    await page.locator(TOGGLE).click();
    await page.locator("[data-testid='sidebar-sort-option-plugin-by-metric']").click();
    await expect(page.locator(TOGGLE)).toHaveAttribute("data-sort-mode", "plugin");

    // Rows now order by sort_value descending: 100, 50, 1.
    await expect.poll(() => readWorkspaceTitles(page), { timeout: 4000 }).toEqual(["high-ws", "mid-ws", "low-ws"]);

    // The plugin sort is ephemeral: not persisted to localStorage.
    const stored = await page.evaluate(() => window.localStorage.getItem("aoe-sidebar-sort-mode"));
    expect(stored).not.toBe("plugin");
  });

  test("a plugin filter-facet filters rows by filter_values", async ({ page }) => {
    await mockApis(page, SESSIONS, ORDERING, UI_ENTRIES);
    await page.setViewportSize({ width: 1280, height: 720 });
    await page.goto("/");

    await expect(page.locator("[data-testid='sidebar-session-row']")).toHaveCount(3, { timeout: 8000 });

    // Open the facet panel and select "running".
    await page.locator("[data-testid='sidebar-facet-toggle']").click();
    await page.locator("[data-testid='sidebar-facet-option-by-status-running']").click();

    // Only the two running sessions remain (high, mid); idle low-ws drops.
    await expect
      .poll(() => readWorkspaceTitles(page), { timeout: 4000 })
      .toEqual(expect.arrayContaining(["high-ws", "mid-ws"]));
    await expect.poll(() => readWorkspaceTitles(page), { timeout: 4000 }).not.toContain("low-ws");

    // Deselecting restores all three rows.
    await page.locator("[data-testid='sidebar-facet-option-by-status-running']").click();
    await expect.poll(() => readWorkspaceTitles(page), { timeout: 4000 }).toHaveLength(3);
  });
});
