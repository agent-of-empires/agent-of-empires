// Mocked sidebar API surface for sidebar specs.

import type { Page, Route } from "@playwright/test";
import { sessionResponse } from "./sessions";

export interface MockSessionInput {
  id: string;
  title: string;
  project_path: string;
  branch: string | null;
  created_at?: string;
  group?: string;
  /** Null means "No organization" on the org axis (#3283). */
  remote_owner?: string | null;
  /** Defaults to `${remote_owner}@example.com`. */
  remote_owner_key?: string | null;
  /** Extra SessionResponse fields merged over the defaults. */
  fields?: Record<string, unknown>;
}

type MockSession = MockSessionInput & { created_at: string };

function fillCreatedAt(s: MockSessionInput, fallbackIndex: number): MockSession {
  return {
    ...s,
    created_at: s.created_at ?? new Date(Date.UTC(2025, 0, 1 + fallbackIndex)).toISOString(),
  };
}

function toResponse(s: MockSession) {
  return sessionResponse({
    id: s.id,
    title: s.title,
    project_path: s.project_path,
    group_path: s.group ?? "",
    created_at: s.created_at,
    branch: s.branch,
    remote_owner: s.remote_owner ?? null,
    remote_owner_key:
      s.remote_owner_key !== undefined ? s.remote_owner_key : s.remote_owner ? `${s.remote_owner}@example.com` : null,
    ...s.fields,
  });
}

/** The server's workspace id (see useWorkspaces.ts). */
export function workspaceId(s: { project_path: string; branch: string | null; id: string }): string {
  return s.branch ? `${s.project_path}::${s.branch}` : `${s.project_path}::__session__::${s.id}`;
}

export interface SidebarView {
  group_by: "project" | "group";
  sort: "custom" | "name";
}

export interface SidebarMockHandle {
  puts: Array<{ order?: string[]; repositories?: string[] }>;
  /** `POST /api/reorder` bodies, in arrival order. */
  reorders: Array<{
    target: string;
    group: { profile: string; path: string };
    direction: "up" | "down";
    siblings?: string[];
  }>;
  /** `PATCH /api/sidebar-view` bodies, in arrival order. */
  viewPatches: Array<Partial<SidebarView>>;
  /** One-shot override for the next PUT's response. */
  nextPutResponse: { status?: number; body?: string } | null;
  readOnly: boolean;
  /** What the next read serves, as another Client's change would leave it. */
  view: SidebarView;
}

export interface SidebarMockOptions {
  sessions: MockSessionInput[];
  /** Defaults to the input order. */
  ordering?: string[];
  /** The shared Repository header order; defaults to none moved. */
  repositoryOrder?: string[];
  /** Each profile's Group order; defaults to none. */
  groupOrder?: Record<string, string[]>;
  /** Defaults to Repository and Custom. */
  view?: SidebarView;
  readOnly?: boolean;
  cityhall?: boolean;
  /** A successful PUT replaces the served ordering, like the real server. */
  persistPutOrdering?: boolean;
}

export async function installSidebarMocks(page: Page, opts: SidebarMockOptions): Promise<SidebarMockHandle> {
  const filled = opts.sessions.map((s, i) => fillCreatedAt(s, i));
  const handle: SidebarMockHandle = {
    puts: [],
    reorders: [],
    viewPatches: [],
    nextPutResponse: null,
    readOnly: !!opts.readOnly,
    view: opts.view ?? { group_by: "project", sort: "custom" },
  };

  let ordering = opts.ordering ?? filled.map((s) => workspaceId(s));
  let repositoryOrder = opts.repositoryOrder ?? [];
  const groupOrder = structuredClone(opts.groupOrder ?? {});
  // Each write publishes a newer snapshot, so its receipt and later reads line up like the Daemon's.
  let revision = 1;
  const runtime = (write = false) => {
    if (write) revision += 1;
    return { "aoe-runtime-epoch": "mock-runtime", "aoe-runtime-revision": String(revision) };
  };

  await page.route("**/api/login/status*", (r) => r.fulfill({ json: { required: false, authenticated: true } }));
  await page.route(/\/api\/sessions(\?.*)?$/, (r) => {
    if (r.request().method() !== "GET") return r.fulfill({ status: 400 });
    return r.fulfill({
      headers: runtime(),
      json: {
        sessions: filled.map(toResponse),
        workspace_ordering: ordering,
        repository_order: repositoryOrder,
        sidebar_view: handle.view,
        group_order: groupOrder,
      },
    });
  });
  await page.route("**/api/workspace-ordering", (r: Route) => {
    if (r.request().method() === "PUT") {
      const body = r.request().postDataJSON() as { order?: string[]; repositories?: string[] };
      handle.puts.push(body ?? {});
      const override = handle.nextPutResponse;
      handle.nextPutResponse = null;
      if (override) return r.fulfill(override);
      if (opts.persistPutOrdering && body?.order) ordering = body.order;
      if (opts.persistPutOrdering && body?.repositories) repositoryOrder = body.repositories;
    }
    return r.fulfill({ headers: runtime(true), json: { order: ordering, repositories: repositoryOrder } });
  });
  // Arranges the listed siblings slot for slot, then swaps the Group with its neighbouring sibling, like the Daemon.
  await page.route("**/api/reorder", (r: Route) => {
    const body = r.request().postDataJSON() as SidebarMockHandle["reorders"][number];
    handle.reorders.push(body);
    const order = (groupOrder[body.group.profile] ??= []);
    order.push(...(body.siblings ?? []).filter((p) => !order.includes(p)));
    const parent = (p: string) => p.slice(0, Math.max(0, p.lastIndexOf("/")));
    const siblings = order.flatMap((p, i) => (parent(p) === parent(body.group.path) ? [i] : []));
    const wanted = (body.siblings ?? []).filter((p) => siblings.some((i) => order[i] === p));
    siblings.filter((i) => wanted.includes(order[i]!)).forEach((slot, n) => (order[slot] = wanted[n]!));
    const at = siblings.findIndex((i) => order[i] === body.group.path);
    const to = siblings[at + (body.direction === "up" ? -1 : 1)];
    if (at < 0 || to === undefined) return r.fulfill({ json: { status: "at_edge" } });
    const from = siblings[at]!;
    [order[from], order[to]] = [order[to]!, order[from]!];
    return r.fulfill({ headers: runtime(true), json: { status: "moved", destination: null } });
  });
  await page.route("**/api/sidebar-view", (r: Route) => {
    const body = (r.request().postDataJSON() ?? {}) as Partial<SidebarView>;
    handle.viewPatches.push(body);
    handle.view = { ...handle.view, ...body };
    return r.fulfill({ headers: runtime(true), json: handle.view });
  });
  await page.route("**/api/about", (r) =>
    r.fulfill({
      json: {
        read_only: handle.readOnly,
        cityhall_mode: !!opts.cityhall,
        auth_mode: "none",
        behind_tunnel: false,
        profile: "default",
      },
    }),
  );
  for (const path of ["settings", "themes", "agents", "profiles", "groups", "devices"]) {
    await page.route(`**/api/${path}`, (r) => r.fulfill({ json: [] }));
  }
  await page.route("**/api/docker/status", (r) => r.fulfill({ json: {} }));

  return handle;
}

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
