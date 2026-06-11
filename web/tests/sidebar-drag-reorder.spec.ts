import { test, expect } from "./helpers/mockedTest";
import { Page } from "@playwright/test";

// End-to-end coverage for drag-to-reorder workspaces in the sidebar.
// Verifies:
//   1. Server-supplied ordering is applied on first paint.
//   2. Dragging via the grip handle reorders the row visually.
//   3. The new order is PUT to /api/workspace-ordering.
//   4. Default order (no server ordering) is birth-key newest-first.

interface MockSession {
  id: string;
  title: string;
  project_path: string;
  branch: string | null;
  created_at: string;
}

function sessionResponse(s: MockSession) {
  return {
    id: s.id,
    title: s.title,
    project_path: s.project_path,
    group_path: s.project_path,
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
    has_terminal: true,
    profile: "default",
    workspace_repos: [],
  };
}

async function mockApis(page: Page, getSessions: () => MockSession[], getOrdering: () => string[]) {
  await page.route("**/api/login/status", (r) => r.fulfill({ json: { required: false, authenticated: true } }));
  await page.route("**/api/sessions", (r) => {
    if (r.request().method() !== "GET") return r.fulfill({ status: 400 });
    return r.fulfill({
      json: {
        sessions: getSessions().map(sessionResponse),
        workspace_ordering: getOrdering(),
      },
    });
  });
  // The client only PUTs `/api/workspace-ordering` from an explicit
  // drag now — the server-side merge in `list_sessions` covers the
  // "new workspace appears" case. The drag test overrides this route
  // with `page.route(..., ...)` to capture the PUT body.
  await page.route("**/api/workspace-ordering", (r) => r.fulfill({ json: { order: [] } }));
  for (const path of ["settings", "themes", "agents", "profiles", "groups", "devices", "docker/status", "about"]) {
    await page.route(`**/api/${path}`, (r) => r.fulfill({ json: path === "docker/status" ? {} : [] }));
  }
}

async function readWorkspaceOrder(page: Page): Promise<string[]> {
  return page.evaluate(() => {
    const links = Array.from(document.querySelectorAll<HTMLAnchorElement>("a[href^='/session/']"));
    return links.map((a) => a.querySelector("[title]")?.getAttribute("title") ?? "").filter(Boolean);
  });
}

test.describe("Sidebar drag-to-reorder (#1169)", () => {
  test("applies the server-supplied ordering on first paint", async ({ page }) => {
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
      },
    ];

    // Server pins old-ws above new-ws even though new-ws has a later
    // created_at. Without the ordering plumbing the default would put
    // new-ws first.
    await mockApis(
      page,
      () => sessions,
      () => ["/tmp/repo::feature/old", "/tmp/repo::feature/new"],
    );
    await page.setViewportSize({ width: 1280, height: 720 });
    await page.goto("/");

    // Drag tests rely on the sidebar starting in manual sort mode (the
    // default). If a future change flips the default to last-activity,
    // the drag affordance disappears and these tests break in confusing
    // ways. Assert the toggle's state up front so the failure mode is
    // obvious. See #1418.
    await expect(page.locator("[data-testid='sidebar-sort-toggle']")).toHaveAttribute("data-sort-mode", "manual");
    await expect.poll(() => readWorkspaceOrder(page), { timeout: 8000 }).toEqual(["old-ws", "new-ws"]);
  });

  test("renders the server-returned order verbatim (no client-side resort)", async ({ page }) => {
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
      },
    ];

    // The real server prepends unseen workspace ids newest-first; the
    // mock simulates that by returning the merged list. The client
    // honors it directly without further sorting.
    await mockApis(
      page,
      () => sessions,
      () => ["/tmp/repo::feature/new", "/tmp/repo::feature/old"],
    );
    await page.setViewportSize({ width: 1280, height: 720 });
    await page.goto("/");

    await expect.poll(() => readWorkspaceOrder(page), { timeout: 8000 }).toEqual(["new-ws", "old-ws"]);
  });

  test("press-and-hold drag reorders the row and PUTs the new order", async ({ page }) => {
    const sessions: MockSession[] = [
      {
        id: "s-c",
        title: "gamma",
        project_path: "/tmp/repo",
        branch: "feature/c",
        created_at: "2025-01-01T00:00:00Z",
      },
      {
        id: "s-b",
        title: "beta",
        project_path: "/tmp/repo",
        branch: "feature/b",
        created_at: "2025-02-01T00:00:00Z",
      },
      {
        id: "s-a",
        title: "alpha",
        project_path: "/tmp/repo",
        branch: "feature/a",
        created_at: "2025-03-01T00:00:00Z",
      },
    ];

    // The real server returns the merged ordering; the mock mirrors
    // that, so the client renders the rows in the exact order the
    // server gave it.
    const initialOrdering = ["/tmp/repo::feature/a", "/tmp/repo::feature/b", "/tmp/repo::feature/c"];

    const puts: string[][] = [];
    await mockApis(
      page,
      () => sessions,
      () => initialOrdering,
    );
    await page.route("**/api/workspace-ordering", (r) => {
      const body = JSON.parse(r.request().postData() || "{}") as {
        order?: string[];
      };
      if (body.order) puts.push(body.order);
      return r.fulfill({ json: { order: body.order ?? [] } });
    });
    await page.setViewportSize({ width: 1280, height: 720 });
    await page.goto("/");

    // Initial newest-first per the server-supplied ordering: alpha,
    // beta, gamma.
    await expect.poll(() => readWorkspaceOrder(page), { timeout: 8000 }).toEqual(["alpha", "beta", "gamma"]);

    // Press-and-hold on gamma's row, wait past the 250ms sensor delay,
    // then drag up onto alpha's row. The press has to target the
    // sortable wrapper (not the inner Link or its children), because
    // dnd-kit's MouseSensor only listens on `setNodeRef`'d nodes; clicks
    // that land on the Link bypass the sensor and just navigate.
    const wrappers = page.locator("[aria-roledescription='Press and hold to reorder']");
    await expect(wrappers).toHaveCount(3);
    const sourceBox = await wrappers.nth(2).boundingBox();
    const targetBox = await wrappers.nth(0).boundingBox();
    expect(sourceBox).not.toBeNull();
    expect(targetBox).not.toBeNull();
    if (!sourceBox || !targetBox) throw new Error("rows missing");

    // Press near the right edge to avoid landing on the inner Link's
    // status glyph. The drag listeners are on the wrapper div, so the
    // exact pixel doesn't matter as long as it's inside.
    await page.mouse.move(sourceBox.x + sourceBox.width - 4, sourceBox.y + sourceBox.height / 2);
    await page.mouse.down();
    await page.waitForTimeout(250);
    await page.mouse.move(targetBox.x + targetBox.width / 2, targetBox.y + targetBox.height / 2, { steps: 12 });

    // Mid-drag: the source row must visibly lift (amber ring + shadow).
    // We assert this before releasing so the test fails if a future
    // refactor strips the visual feedback. The dragged row's
    // `className` is on the dnd-kit-injected wrapper, which we already
    // selected via the aria-roledescription locator.
    const sourceClass = await wrappers.nth(2).getAttribute("class");
    expect(sourceClass ?? "").toContain("ring-2");
    expect(sourceClass ?? "").toContain("shadow-lg");

    await page.mouse.up();

    // Order now starts with gamma (visually moved to the top).
    await expect.poll(() => readWorkspaceOrder(page), { timeout: 4000 }).toEqual(["gamma", "alpha", "beta"]);

    // The drag is the only PUT now (server-side merge handles "new
    // workspace appears" without a client write).
    await expect
      .poll(() => puts.at(-1), { timeout: 4000 })
      .toEqual(["/tmp/repo::feature/c", "/tmp/repo::feature/a", "/tmp/repo::feature/b"]);
    expect(puts.length).toBe(1);
  });
});
