// Shared mocked-Playwright setup for the sidebar-reorder story specs
// under `web/tests/sidebar-reorder-*.spec.ts` (#1419). The drag-to-
// reorder UI sits on top of dnd-kit and a handful of REST endpoints;
// the per-test specifics are just the input gesture + the assertion.
// Extracted from `sidebar-drag-reorder.spec.ts` and trimmed to what
// the story specs actually need.

import type { Page, Route } from "@playwright/test";

export interface MockSessionInput {
  id: string;
  title: string;
  project_path: string;
  branch: string | null;
  /** ISO 8601 timestamp; controls newest-first default ordering. */
  created_at?: string;
}

type MockSession = Required<MockSessionInput>;

function fillCreatedAt(
  s: MockSessionInput,
  fallbackIndex: number,
): MockSession {
  return {
    ...s,
    // Stagger fallback timestamps a day apart so a list seeded in
    // arrival order has a deterministic newest-first sort if anything
    // ever falls back to created_at.
    created_at:
      s.created_at ??
      new Date(Date.UTC(2025, 0, 1 + fallbackIndex)).toISOString(),
  };
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

/** Workspace id format used by the server: `<project_path>::<branch>`
 *  for branched sessions, `<project_path>::__session__::<id>` for
 *  ones without a branch. Mirrors `useWorkspaces.ts:31`. */
export function workspaceId(s: {
  project_path: string;
  branch: string | null;
  id: string;
}): string {
  return s.branch
    ? `${s.project_path}::${s.branch}`
    : `${s.project_path}::__session__::${s.id}`;
}

export interface SidebarMockHandle {
  /** Recorded `PUT /api/workspace-ordering` bodies in arrival order. */
  puts: Array<{ order?: string[] }>;
  /** Override the fulfill response for the next PUT (used by the
   *  failure-mode story). Reset after each call. */
  nextPutResponse: { status?: number; body?: string } | null;
  /** Override the read_only flag from `/api/about` (used by the
   *  read-only story). */
  readOnly: boolean;
}

export interface SidebarMockOptions {
  sessions: MockSessionInput[];
  /** Server-supplied workspace ordering (the full list, with each id
   *  composed via `workspaceId`). Defaults to the input order. */
  ordering?: string[];
  /** Add `read_only: true` to the `/api/about` response. */
  readOnly?: boolean;
}

/** Install routes for the surface the sidebar uses. Returns a handle
 *  the test can read for captured PUT bodies and tweak the next PUT's
 *  fulfill (for the failure-mode story). */
export async function installSidebarMocks(
  page: Page,
  opts: SidebarMockOptions,
): Promise<SidebarMockHandle> {
  const filled = opts.sessions.map((s, i) => fillCreatedAt(s, i));
  const handle: SidebarMockHandle = {
    puts: [],
    nextPutResponse: null,
    readOnly: !!opts.readOnly,
  };

  const ordering = opts.ordering ?? filled.map((s) => workspaceId(s));

  await page.route("**/api/login/status", (r) =>
    r.fulfill({ json: { required: false, authenticated: true } }),
  );
  await page.route("**/api/sessions", (r) => {
    if (r.request().method() !== "GET") return r.fulfill({ status: 400 });
    return r.fulfill({
      json: {
        sessions: filled.map(sessionResponse),
        workspace_ordering: ordering,
      },
    });
  });
  await page.route("**/api/workspace-ordering", (r: Route) => {
    if (r.request().method() === "PUT") {
      const body = r.request().postDataJSON() as { order?: string[] };
      handle.puts.push(body ?? {});
      const override = handle.nextPutResponse;
      handle.nextPutResponse = null;
      if (override) return r.fulfill(override);
    }
    return r.fulfill({ json: { order: [] } });
  });
  await page.route("**/api/about", (r) =>
    r.fulfill({
      json: {
        read_only: handle.readOnly,
        auth_mode: "none",
        behind_tunnel: false,
        profile: "default",
      },
    }),
  );
  for (const path of [
    "settings",
    "themes",
    "agents",
    "profiles",
    "groups",
    "devices",
  ]) {
    await page.route(`**/api/${path}`, (r) => r.fulfill({ json: [] }));
  }
  await page.route("**/api/docker/status", (r) => r.fulfill({ json: {} }));

  return handle;
}

/** Standard three sessions in one repo, used by the activation /
 *  suppression / mobile stories. Title -> branch -> id are aligned so
 *  the test reads naturally. */
export function threeSessionsInOneRepo(): MockSessionInput[] {
  return [
    {
      id: "s-a",
      title: "alpha",
      project_path: "/tmp/repo",
      branch: "feature/a",
      created_at: "2025-03-01T00:00:00Z",
    },
    {
      id: "s-b",
      title: "beta",
      project_path: "/tmp/repo",
      branch: "feature/b",
      created_at: "2025-02-01T00:00:00Z",
    },
    {
      id: "s-c",
      title: "gamma",
      project_path: "/tmp/repo",
      branch: "feature/c",
      created_at: "2025-01-01T00:00:00Z",
    },
  ];
}
