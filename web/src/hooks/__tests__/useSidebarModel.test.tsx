// @vitest-environment jsdom

import { act, renderHook, waitFor } from "@testing-library/react";
import { http, HttpResponse } from "msw";
import { setupServer } from "msw/node";
import { afterAll, afterEach, beforeAll, beforeEach, describe, expect, it, vi } from "vitest";
import type { SidebarView } from "../../lib/api";
import { reportFeatureUse } from "../../lib/featureUse";
import { toastBus, type ToastApi } from "../../lib/toastBus";
import type { SessionResponse } from "../../lib/types";
import { useSidebarModel } from "../app/useSidebarModel";
import { MULTI_REPO_GROUP_ID, SCRATCH_GROUP_ID } from "../useRepoGroups";
import { useSessions } from "../useSessions";
import { useWorkspaces } from "../useWorkspaces";
import { session } from "./fixtures";

/** What the Daemon serves; each write bumps the revision like a published snapshot. */
interface Daemon {
  revision: number;
  sessions: SessionResponse[];
  workspace_ordering: string[];
  repository_order: string[];
  sidebar_view: SidebarView;
  group_order: Record<string, string[]>;
}

const row = (id: string, repo: string, title: string, created_at = "2025-01-01T00:00:00Z") =>
  session({ id, title, project_path: repo, branch: id, group_path: "", created_at });
const wsId = (s: SessionResponse) => `${s.project_path}::${s.branch}`;

let daemon: Daemon;
const writes: { method: string; body: unknown }[] = [];
const runtime = (revision: number) => ({ "aoe-runtime-epoch": "boot", "aoe-runtime-revision": String(revision) });
const server = setupServer(
  http.get("*/api/sessions", () =>
    HttpResponse.json(
      {
        sessions: daemon.sessions,
        workspace_ordering: daemon.workspace_ordering,
        repository_order: daemon.repository_order,
        sidebar_view: daemon.sidebar_view,
        group_order: daemon.group_order,
      },
      { headers: runtime(daemon.revision) },
    ),
  ),
  http.patch("*/api/sidebar-view", async ({ request }) => {
    const body = (await request.json()) as Partial<SidebarView>;
    writes.push({ method: "PATCH", body });
    daemon.sidebar_view = { ...daemon.sidebar_view, ...body };
    daemon.revision += 1;
    return HttpResponse.json(daemon.sidebar_view, { headers: runtime(daemon.revision) });
  }),
  http.put("*/api/workspace-ordering", async ({ request }) => {
    const body = (await request.json()) as { order?: string[]; repositories?: string[] };
    writes.push({ method: "PUT", body });
    if (body.order) daemon.workspace_ordering = body.order;
    if (body.repositories) daemon.repository_order = body.repositories;
    daemon.revision += 1;
    return HttpResponse.json({ order: daemon.workspace_ordering }, { headers: runtime(daemon.revision) });
  }),
  http.post("*/api/reorder", async ({ request }) => {
    const body = (await request.json()) as ReorderBody;
    writes.push({ method: "POST", body });
    if (!reorder(body)) return HttpResponse.json({ status: "at_edge" });
    daemon.revision += 1;
    return HttpResponse.json({ status: "moved", destination: null }, { headers: runtime(daemon.revision) });
  }),
);

interface ReorderBody {
  group: { profile: string; path: string };
  direction: "up" | "down";
  siblings?: string[];
}

/** Arranges the listed siblings slot for slot, then swaps the Group with its neighbour, as the Daemon does. */
function reorder(body: ReorderBody): boolean {
  const order = (daemon.group_order[body.group.profile] ??= []);
  // A Group that only sessions name joins the tree at the end.
  order.push(...(body.siblings ?? []).filter((p) => !order.includes(p)));
  const parent = (p: string) => p.slice(0, Math.max(0, p.lastIndexOf("/")));
  const siblings = order.flatMap((p, i) => (parent(p) === parent(body.group.path) ? [i] : []));
  const wanted = (body.siblings ?? []).filter((p) => siblings.some((i) => order[i] === p));
  const slots = siblings.filter((i) => wanted.includes(order[i]!));
  slots.forEach((slot, n) => (order[slot] = wanted[n]!));
  const at = siblings.findIndex((i) => order[i] === body.group.path);
  const to = siblings[at + (body.direction === "up" ? -1 : 1)];
  if (at < 0 || to === undefined) return false;
  [order[siblings[at]!], order[to]] = [order[to]!, order[siblings[at]!]!];
  return true;
}
beforeAll(() => server.listen({ onUnhandledRequest: "error" }));
afterEach(() => {
  server.resetHandlers();
  toastBus.handler = null;
});
afterAll(() => server.close());

/** The toasts a hook raised; `undo` runs the newest Undo offer as its button would. */
const toasts = {
  info: vi.fn(),
  undo: [] as { message: string; run: () => boolean | void; dismissed: boolean }[],
};
const toastApi = {
  info: toasts.info,
  infoAction: (message: string, _label: string, run: () => boolean | void) => {
    const offer = { message, run, dismissed: false };
    toasts.undo.push(offer);
    return () => void (offer.dismissed = true);
  },
} as unknown as ToastApi;

/** A handler that answers its requests only once released, in the order they arrived. */
function heldWrites(method: "patch" | "put", path: string) {
  const releases: (() => void)[] = [];
  server.use(
    http[method](path, async ({ request }) => {
      const body = (await request.json()) as Record<string, unknown>;
      writes.push({ method: method.toUpperCase(), body });
      await new Promise<void>((r) => releases.push(r));
      if (method === "patch") daemon.sidebar_view = { ...daemon.sidebar_view, ...body };
      else {
        if (body.order) daemon.workspace_ordering = body.order as string[];
        if (body.repositories) daemon.repository_order = body.repositories as string[];
      }
      daemon.revision += 1;
      return HttpResponse.json({}, { headers: runtime(daemon.revision) });
    }),
  );
  return releases;
}

beforeEach(() => {
  localStorage.clear();
  toasts.info.mockClear();
  toasts.undo.length = 0;
  toastBus.handler = toastApi;
  writes.length = 0;
  vi.mocked(reportFeatureUse).mockClear();
  const sessions = [
    row("b2", "/r/beta", "zulu", "2025-01-02T00:00:00Z"),
    row("b1", "/r/beta", "alpha"),
    row("a1", "/r/Alpha", "mike"),
  ];
  daemon = {
    revision: 1,
    sessions,
    workspace_ordering: sessions.map(wsId),
    repository_order: ["/r/beta", "/r/Alpha"],
    sidebar_view: { group_by: "project", sort: "custom" },
    group_order: {},
  };
});

function mount() {
  return renderHook(() => {
    const sessions = useSessions();
    const workspaces = useWorkspaces(sessions.sessions);
    const model = useSidebarModel({
      workspaces,
      projects: [],
      view: sessions.sidebarView,
      workspaceOrdering: sessions.workspaceOrdering,
      repositoryOrder: sessions.repositoryOrder,
      groupOrder: sessions.groupOrder,
      moveGroup: sessions.moveGroup,
      changeOrdering: sessions.changeOrdering,
      orderingSaving: sessions.orderingSaving,
      changeSidebarView: sessions.changeSidebarView,
      onSharedRead: sessions.onSharedRead,
    });
    return { sessions, model };
  });
}
type Mounted = ReturnType<typeof mount>["result"];

const headers = (r: Mounted) => r.current.model.nestedGroups.map((n) => n.repo.id);
const rows = (r: Mounted) =>
  r.current.model.nestedGroups.flatMap((n) => n.subgroups.flatMap((g) => g.workspaces.map((v) => v.workspace.id)));

/** Reads the Daemon at `revision`, as the next poll would, and waits for that read to land. */
async function pollAt(r: Mounted, revision: number) {
  daemon.revision = revision;
  const seen = vi.fn();
  server.events.on("response:mocked", seen);
  await act(async () => r.current.sessions.refresh());
  await waitFor(() => expect(seen).toHaveBeenCalled());
  server.events.removeListener("response:mocked", seen);
  // One more turn so the response body is applied.
  await act(async () => {});
}

describe("the shared sidebar view", () => {
  it("adopts a grouping and sort another Client picks on the next poll", async () => {
    const { result } = mount();
    await waitFor(() => expect(headers(result)).toEqual(["/r/beta", "/r/Alpha"]));
    daemon.sidebar_view = { group_by: "group", sort: "name" };
    await pollAt(result, 2);
    await waitFor(() => expect(result.current.model.groupBy).toBe("group"));
    expect(result.current.model.sort).toBe("name");
  });

  it("shows a pick at once, sends it, and lets no poll older than its receipt undo it", async () => {
    const { result } = mount();
    await waitFor(() => expect(rows(result)).toHaveLength(3));
    let release!: () => void;
    const held = new Promise<void>((r) => (release = r));
    server.use(
      http.patch("*/api/sidebar-view", async ({ request }) => {
        writes.push({ method: "PATCH", body: await request.json() });
        await held;
        return HttpResponse.json({ group_by: "project", sort: "name" }, { headers: runtime(5) });
      }),
    );
    act(() => result.current.model.selectSort("name"));
    expect(result.current.model.sort).toBe("name");
    expect(reportFeatureUse).toHaveBeenCalledWith("sort_az");
    await waitFor(() => expect(writes).toEqual([{ method: "PATCH", body: { sort: "name" } }]));

    // A poll while the write is in flight still carries Custom.
    await pollAt(result, 2);
    expect(result.current.model.sort).toBe("name");

    await act(async () => release());
    // A read older than the receipt is not adopted either.
    await pollAt(result, 4);
    expect(result.current.model.sort).toBe("name");

    // Once the Daemon has moved past the receipt, its value wins again.
    daemon.sidebar_view = { group_by: "group", sort: "custom" };
    await pollAt(result, 6);
    await waitFor(() => expect(result.current.model.sort).toBe("custom"));
    expect(result.current.model.groupBy).toBe("group");
  });

  it("gives up on a write that never answers, so polls reconcile again", async () => {
    const { result } = mount();
    await waitFor(() => expect(rows(result)).toHaveLength(3));
    server.use(http.patch("*/api/sidebar-view", () => new Promise<never>(() => {})));
    const deadline = new AbortController();
    const timeout = vi.spyOn(AbortSignal, "timeout").mockReturnValue(deadline.signal);
    let settled = false;
    act(() => void result.current.sessions.changeSidebarView({ sort: "name" }).then(() => (settled = true)));
    await waitFor(() => expect(timeout).toHaveBeenCalledWith(15_000));
    timeout.mockRestore();
    await pollAt(result, 2);
    expect(result.current.model.sort).toBe("name");

    await act(async () => deadline.abort());
    await waitFor(() => expect(settled).toBe(true));
    await pollAt(result, 3);
    expect(result.current.model.sort).toBe("custom");
  });

  it("ignores a read from before a restart that a write's receipt already reported", async () => {
    const { result } = mount();
    await waitFor(() => expect(rows(result)).toHaveLength(3));
    let release!: () => void;
    const held = new Promise<void>((r) => (release = r));
    const stale = { ...daemon, sidebar_view: { ...daemon.sidebar_view } };
    server.use(
      http.get("*/api/sessions", async () => {
        await held;
        return HttpResponse.json({ ...stale, sessions: stale.sessions }, { headers: runtime(9) });
      }),
      http.patch("*/api/sidebar-view", () =>
        HttpResponse.json({}, { headers: { "aoe-runtime-epoch": "reboot", "aoe-runtime-revision": "1" } }),
      ),
    );
    // The stale read would put back a grouping and sort this Client never picked.
    stale.sidebar_view = { group_by: "group", sort: "custom" };
    // A failed read first, so the held read's commit shows as the error clearing even when it is rejected.
    server.use(http.get("*/api/sessions", () => new HttpResponse(null, { status: 500 }), { once: true }));
    act(() => void result.current.sessions.refresh());
    await waitFor(() => expect(result.current.sessions.error).toBe(true));
    const reads: string[] = [];
    const listen = ({ request }: { request: Request }) => {
      if (request.method === "GET" && new URL(request.url).pathname === "/api/sessions") reads.push(request.url);
    };
    server.events.on("request:start", listen);
    act(() => void result.current.sessions.refresh());
    await waitFor(() => expect(reads).toHaveLength(1));
    server.events.removeListener("request:start", listen);
    await act(async () => result.current.sessions.changeSidebarView({ sort: "name" }));
    await act(async () => release());
    await waitFor(() => expect(result.current.sessions.error).toBe(false));
    expect(result.current.model.sort).toBe("name");
    expect(result.current.model.groupBy).toBe("project");
  });

  it("puts a failed pick back on the next poll", async () => {
    server.use(http.patch("*/api/sidebar-view", () => new HttpResponse(null, { status: 500 })));
    const { result } = mount();
    await waitFor(() => expect(rows(result)).toHaveLength(3));
    await act(async () => result.current.model.selectGroupBy("group"));
    expect(reportFeatureUse).toHaveBeenCalledWith("group_by_manual");
    await pollAt(result, 2);
    await waitFor(() => expect(result.current.model.groupBy).toBe("project"));
  });
});

describe("sorts", () => {
  it("orders rows by title and headers by name under Name, ignoring case", async () => {
    daemon.sidebar_view = { group_by: "project", sort: "name" };
    const { result } = mount();
    await waitFor(() => expect(headers(result)).toEqual(["/r/Alpha", "/r/beta"]));
    expect(rows(result)).toEqual(["/r/Alpha::a1", "/r/beta::b1", "/r/beta::b2"]);
  });

  it("follows the shared Custom order and puts a never-moved header on top, newest first", async () => {
    daemon.repository_order = ["/r/Alpha"];
    daemon.workspace_ordering = ["/r/beta::b1", "/r/beta::b2", "/r/Alpha::a1"];
    const { result } = mount();
    await waitFor(() => expect(headers(result)).toEqual(["/r/beta", "/r/Alpha"]));
    expect(rows(result)).toEqual(["/r/beta::b1", "/r/beta::b2", "/r/Alpha::a1"]);
  });
});

describe("the shared Custom order", () => {
  it("moves a row next to its target in the shared order under Custom", async () => {
    const { result } = mount();
    await waitFor(() => expect(rows(result)).toEqual(["/r/beta::b2", "/r/beta::b1", "/r/Alpha::a1"]));
    await act(async () => result.current.model.moveWorkspace("/r/beta::b2", "/r/beta::b1", true));
    expect(writes).toEqual([{ method: "PUT", body: { order: ["/r/beta::b1", "/r/beta::b2", "/r/Alpha::a1"] } }]);
    expect(rows(result)).toEqual(["/r/beta::b1", "/r/beta::b2", "/r/Alpha::a1"]);
    expect(reportFeatureUse).toHaveBeenCalledWith("row_reorder");
  });

  it("under Name, a row drag freezes the order on screen and switches to Custom without moving anything else", async () => {
    daemon.sidebar_view = { group_by: "project", sort: "name" };
    const { result } = mount();
    await waitFor(() => expect(rows(result)).toEqual(["/r/Alpha::a1", "/r/beta::b1", "/r/beta::b2"]));
    await act(async () => result.current.model.moveWorkspace("/r/beta::b2", "/r/beta::b1", false));
    await waitFor(() => expect(writes).toHaveLength(2));
    expect(writes).toEqual([
      {
        method: "PUT",
        body: { order: ["/r/Alpha::a1", "/r/beta::b2", "/r/beta::b1"], repositories: ["/r/Alpha", "/r/beta"] },
      },
      { method: "PATCH", body: { sort: "custom" } },
    ]);
    expect(result.current.model.sort).toBe("custom");
    expect(headers(result)).toEqual(["/r/Alpha", "/r/beta"]);
    expect(rows(result)).toEqual(["/r/Alpha::a1", "/r/beta::b2", "/r/beta::b1"]);
  });

  it("under Name, a failed ordering write leaves the sort on Name", async () => {
    daemon.sidebar_view = { group_by: "project", sort: "name" };
    server.use(http.put("*/api/workspace-ordering", () => new HttpResponse(null, { status: 500 })));
    const { result } = mount();
    await waitFor(() => expect(rows(result)).toEqual(["/r/Alpha::a1", "/r/beta::b1", "/r/beta::b2"]));
    await act(async () => result.current.model.moveWorkspace("/r/beta::b2", "/r/beta::b1", false));
    await pollAt(result, 2);
    await waitFor(() => expect(result.current.model.sort).toBe("name"));
    expect(writes).toEqual([]);
    expect(rows(result)).toEqual(["/r/Alpha::a1", "/r/beta::b1", "/r/beta::b2"]);
  });

  it("under Name, puts the order back and says so when the switch to Custom fails", async () => {
    daemon.sidebar_view = { group_by: "project", sort: "name" };
    const before = [...daemon.workspace_ordering];
    server.use(
      http.patch("*/api/sidebar-view", async ({ request }) => {
        writes.push({ method: "PATCH", body: await request.json() });
        return new HttpResponse(null, { status: 500 });
      }),
    );
    const { result } = mount();
    await waitFor(() => expect(rows(result)).toEqual(["/r/Alpha::a1", "/r/beta::b1", "/r/beta::b2"]));
    await act(async () => result.current.model.moveWorkspace("/r/beta::b2", "/r/beta::b1", false));
    await waitFor(() => expect(toasts.info).toHaveBeenCalledWith("Couldn't switch to Custom, so nothing moved"));
    expect(writes.map((w) => w.method)).toEqual(["PUT", "PATCH", "PUT"]);
    expect(daemon.workspace_ordering).toEqual(before);
    expect(toasts.undo).toEqual([]);
  });

  it("under Name, a drag's switch to Custom waits for its order, refuses another move, and yields to a newer pick", async () => {
    daemon.sidebar_view = { group_by: "project", sort: "name" };
    const { result } = mount();
    await waitFor(() => expect(rows(result)).toEqual(["/r/Alpha::a1", "/r/beta::b1", "/r/beta::b2"]));
    let release!: () => void;
    const held = new Promise<void>((r) => (release = r));
    server.use(
      http.put("*/api/workspace-ordering", async ({ request }) => {
        writes.push({ method: "PUT", body: await request.json() });
        await held;
        return HttpResponse.json({}, { headers: runtime(5) });
      }),
    );
    act(() => result.current.model.moveWorkspace("/r/beta::b2", "/r/beta::b1", false));
    await waitFor(() => expect(writes).toHaveLength(1));
    expect(result.current.model.sort).toBe("custom");

    act(() => result.current.model.moveRepository("/r/beta", "/r/Alpha", false));
    expect(headers(result)).toEqual(["/r/Alpha", "/r/beta"]);

    // The newer pick waits behind the switch it supersedes, so it reaches the Daemon last.
    act(() => result.current.model.selectSort("name"));
    await pollAt(result, 2);
    expect(writes).toHaveLength(1);
    await act(async () => release());
    await waitFor(() => expect(writes).toHaveLength(2));
    await pollAt(result, 6);
    expect(writes.map((w) => w.method)).toEqual(["PUT", "PATCH"]);
    expect(writes[1]!.body).toEqual({ sort: "name" });
    expect(result.current.model.sort).toBe("name");
  });

  it("under Name, a header drag freezes the rows too and moves only that header", async () => {
    daemon.sidebar_view = { group_by: "project", sort: "name" };
    const { result } = mount();
    await waitFor(() => expect(headers(result)).toEqual(["/r/Alpha", "/r/beta"]));
    await act(async () => result.current.model.moveRepository("/r/beta", "/r/Alpha", false));
    await waitFor(() => expect(writes).toHaveLength(2));
    expect(writes[0]!.body).toEqual({
      order: ["/r/Alpha::a1", "/r/beta::b1", "/r/beta::b2"],
      repositories: ["/r/beta", "/r/Alpha"],
    });
    expect(headers(result)).toEqual(["/r/beta", "/r/Alpha"]);
    expect(rows(result)).toEqual(["/r/beta::b1", "/r/beta::b2", "/r/Alpha::a1"]);
  });

  it("a header drag keeps the place of a saved repository with no header", async () => {
    daemon.repository_order = ["/r/beta", "/r/gone", "/r/Alpha"];
    const { result } = mount();
    await waitFor(() => expect(headers(result)).toEqual(["/r/beta", "/r/Alpha"]));
    await act(async () => result.current.model.moveRepository("/r/Alpha", "/r/beta", false));
    expect(writes[0]!.body).toEqual({ repositories: ["/r/Alpha", "/r/beta", "/r/gone"] });
  });

  it("a drag of the Scratch or Multi-repo header writes nothing", async () => {
    daemon.sidebar_view = { group_by: "project", sort: "name" };
    const { result } = mount();
    await waitFor(() => expect(headers(result)).toEqual(["/r/Alpha", "/r/beta"]));
    for (const id of [SCRATCH_GROUP_ID, MULTI_REPO_GROUP_ID]) {
      act(() => result.current.model.moveRepository(id, "/r/beta", false));
      act(() => result.current.model.moveRepository("/r/beta", id, true));
    }
    expect(result.current.sessions.orderingSaving()).toBe(false);
    expect(writes).toEqual([]);
    expect(result.current.model.sort).toBe("name");
  });

  it("a header drag under Custom writes only the repository order", async () => {
    const { result } = mount();
    await waitFor(() => expect(headers(result)).toEqual(["/r/beta", "/r/Alpha"]));
    await act(async () => result.current.model.moveRepository("/r/Alpha", "/r/beta", false));
    expect(writes).toEqual([{ method: "PUT", body: { repositories: ["/r/Alpha", "/r/beta"] } }]);
    expect(headers(result)).toEqual(["/r/Alpha", "/r/beta"]);
  });

  it("resets the Custom order to newest first", async () => {
    const { result } = mount();
    await waitFor(() => expect(rows(result)).toHaveLength(3));
    await act(async () => result.current.model.resetCustomOrder());
    expect(writes).toEqual([{ method: "PUT", body: { order: [], repositories: [] } }]);
    // Nothing is listed, so headers and rows sit newest first until the Daemon's merged order arrives.
    expect(headers(result)).toEqual(["/r/beta", "/r/Alpha"]);
    expect(rows(result)).toEqual(["/r/beta::b2", "/r/beta::b1", "/r/Alpha::a1"]);

    // A drag before the next poll starts from the rows on screen, not the cleared local order.
    await act(async () => result.current.model.moveWorkspace("/r/beta::b1", "/r/beta::b2", false));
    expect(writes[1]).toEqual({ method: "PUT", body: { order: ["/r/beta::b1", "/r/beta::b2", "/r/Alpha::a1"] } });
    expect(rows(result)).toEqual(["/r/beta::b1", "/r/beta::b2", "/r/Alpha::a1"]);
  });
});

describe("write order", () => {
  it("sends each sort pick only after the previous one answered, so the latest pick lands last", async () => {
    const { result } = mount();
    await waitFor(() => expect(rows(result)).toHaveLength(3));
    const releases = heldWrites("patch", "*/api/sidebar-view");
    act(() => result.current.model.selectSort("name"));
    await waitFor(() => expect(writes).toHaveLength(1));
    act(() => result.current.model.selectGroupBy("group"));
    act(() => result.current.model.selectSort("custom"));
    await pollAt(result, 2);
    expect(writes).toHaveLength(1);

    await act(async () => releases[0]!());
    await waitFor(() => expect(writes).toHaveLength(2));
    await act(async () => releases[1]!());
    await waitFor(() => expect(writes).toHaveLength(3));
    await act(async () => releases[2]!());
    expect(writes.map((w) => w.body)).toEqual([{ sort: "name" }, { group_by: "group" }, { sort: "custom" }]);
    await pollAt(result, 10);
    expect(daemon.sidebar_view).toEqual({ group_by: "group", sort: "custom" });
    expect(result.current.model.sort).toBe("custom");
  });

  it("sends each ordering write after the previous one answered", async () => {
    const { result } = mount();
    await waitFor(() => expect(rows(result)).toHaveLength(3));
    const releases = heldWrites("put", "*/api/workspace-ordering");
    act(() => void result.current.sessions.changeOrdering({ repositories: ["/r/Alpha", "/r/beta"] }));
    act(() => void result.current.sessions.changeOrdering({ order: [], repositories: [] }));
    await waitFor(() => expect(writes).toHaveLength(1));
    await pollAt(result, 2);
    expect(writes).toHaveLength(1);
    await act(async () => releases[0]!());
    await waitFor(() => expect(writes).toHaveLength(2));
    await act(async () => releases[1]!());
    expect(writes.map((w) => w.body)).toEqual([
      { repositories: ["/r/Alpha", "/r/beta"] },
      { order: [], repositories: [] },
    ]);
    expect(daemon.repository_order).toEqual([]);
  });

  it("refuses a move or a Reset while the last move is saving, with a notice", async () => {
    const { result } = mount();
    await waitFor(() => expect(headers(result)).toEqual(["/r/beta", "/r/Alpha"]));
    const releases = heldWrites("put", "*/api/workspace-ordering");
    act(() => result.current.model.moveRepository("/r/Alpha", "/r/beta", false));
    await waitFor(() => expect(writes).toHaveLength(1));
    act(() => result.current.model.moveWorkspace("/r/beta::b2", "/r/beta::b1", true));
    expect(toasts.info).toHaveBeenCalledWith("The last move is still saving");
    toasts.info.mockClear();
    act(() => result.current.model.resetCustomOrder());
    expect(toasts.info).toHaveBeenCalledWith("The last move is still saving");
    expect(headers(result)).toEqual(["/r/Alpha", "/r/beta"]);
    await act(async () => releases[0]!());
    await waitFor(() => expect(result.current.sessions.orderingSaving()).toBe(false));
    expect(writes).toHaveLength(1);
    expect(toasts.undo).toEqual([]);
    act(() => result.current.model.moveWorkspace("/r/beta::b2", "/r/beta::b1", true));
    await waitFor(() => expect(writes).toHaveLength(2));
  });
});

describe("Undo", () => {
  it("after a drag under Name switched to Custom, puts back the order, the repositories and Name", async () => {
    daemon.sidebar_view = { group_by: "project", sort: "name" };
    const before = { order: [...daemon.workspace_ordering], repositories: [...daemon.repository_order] };
    const { result } = mount();
    await waitFor(() => expect(rows(result)).toEqual(["/r/Alpha::a1", "/r/beta::b1", "/r/beta::b2"]));
    await act(async () => result.current.model.moveWorkspace("/r/beta::b2", "/r/beta::b1", false));
    await waitFor(() => expect(toasts.undo.map((u) => u.message)).toEqual(["Sorted by Custom now"]));
    await waitFor(() => expect(writes).toHaveLength(2));

    await act(async () => void toasts.undo[0]!.run());
    await waitFor(() => expect(writes).toHaveLength(4));
    expect(writes.slice(2)).toEqual([
      { method: "PUT", body: before },
      { method: "PATCH", body: { sort: "name" } },
    ]);
    expect(result.current.model.sort).toBe("name");
    // Single level: the Undo offers nothing further.
    expect(toasts.undo).toHaveLength(1);
  });

  it("after Reset, puts back the previous lists, and is held while a write is saving", async () => {
    const before = { order: [...daemon.workspace_ordering], repositories: [...daemon.repository_order] };
    const { result } = mount();
    await waitFor(() => expect(rows(result)).toHaveLength(3));
    await act(async () => result.current.model.resetCustomOrder());
    await waitFor(() => expect(toasts.undo.map((u) => u.message)).toEqual(["Custom order reset"]));

    const releases = heldWrites("put", "*/api/workspace-ordering");
    act(() => void result.current.sessions.changeOrdering({ repositories: ["/r/beta"] }));
    await waitFor(() => expect(writes).toHaveLength(2));
    let kept: boolean | void = undefined;
    act(() => void (kept = toasts.undo[0]!.run()));
    expect(kept).toBe(false);
    expect(toasts.info).toHaveBeenCalledWith("The last move is still saving");
    await act(async () => releases[0]!());
    await waitFor(() => expect(result.current.sessions.orderingSaving()).toBe(false));

    server.resetHandlers();
    await act(async () => void toasts.undo[0]!.run());
    await waitFor(() => expect(writes).toHaveLength(3));
    expect(writes[2]).toEqual({ method: "PUT", body: before });
    expect(writes.map((w) => w.method)).not.toContain("PATCH");
  });

  it("is dropped by the next order change, and not offered when the write fails", async () => {
    const { result } = mount();
    await waitFor(() => expect(headers(result)).toEqual(["/r/beta", "/r/Alpha"]));
    await act(async () => result.current.model.resetCustomOrder());
    await waitFor(() => expect(toasts.undo).toHaveLength(1));
    await act(async () => result.current.model.moveRepository("/r/Alpha", "/r/beta", false));
    expect(toasts.undo[0]!.dismissed).toBe(true);

    server.use(http.put("*/api/workspace-ordering", () => new HttpResponse(null, { status: 500 })));
    await waitFor(() => expect(result.current.sessions.orderingSaving()).toBe(false));
    await act(async () => result.current.model.resetCustomOrder());
    await waitFor(() => expect(result.current.sessions.orderingSaving()).toBe(false));
    expect(toasts.undo).toHaveLength(1);
  });
});

describe("Undo after another Client", () => {
  it("survives the read of its own write, and is dropped once a read shows another Client's change", async () => {
    const { result } = mount();
    await waitFor(() => expect(rows(result)).toHaveLength(3));
    await act(async () => result.current.model.resetCustomOrder());
    await waitFor(() => expect(toasts.undo).toHaveLength(1));
    // The Daemon lists the rows the cleared order lacks, newest first.
    daemon.workspace_ordering = ["/r/beta::b2", "/r/beta::b1", "/r/Alpha::a1"];
    await pollAt(result, daemon.revision);
    expect(toasts.undo[0]!.dismissed).toBe(false);
    daemon.repository_order = ["/r/Alpha"];
    await pollAt(result, daemon.revision + 1);
    expect(toasts.undo[0]!.dismissed).toBe(true);
  });

  it("puts back only the lists its change wrote", async () => {
    daemon.sidebar_view = { group_by: "group", sort: "name" };
    const before = [...daemon.workspace_ordering];
    const { result } = mount();
    await waitFor(() => expect(rows(result)).toHaveLength(3));
    await act(async () => result.current.model.moveWorkspace("/r/beta::b2", "/r/beta::b1", false));
    await waitFor(() => expect(toasts.undo).toHaveLength(1));
    await waitFor(() => expect(writes).toHaveLength(2));
    await act(async () => void toasts.undo[0]!.run());
    await waitFor(() => expect(writes).toHaveLength(4));
    expect(writes[2]).toEqual({ method: "PUT", body: { order: before } });
  });
});

describe("Group headers", () => {
  const grouped = (id: string, group_path: string) =>
    session({ id, title: id, project_path: "/r/one", branch: id, group_path, created_at: "2025-01-01T00:00:00Z" });
  const groupHeaders = (r: Mounted) => r.current.model.groups.map((g) => g.id);

  beforeEach(() => {
    daemon.sessions = ["zeta", "alpha", "mid", "alpha/inner", "loose"].map((path) =>
      grouped(path.replace("/", "-"), path),
    );
    daemon.workspace_ordering = daemon.sessions.map(wsId);
    daemon.group_order = { default: ["zeta", "alpha", "alpha/inner", "mid"] };
    daemon.sidebar_view = { group_by: "group", sort: "custom" };
  });

  it("follow the Daemon's Group order under Custom, unlisted ones after by name, and A to Z under Name", async () => {
    const { result } = mount();
    await waitFor(() => expect(groupHeaders(result)).toEqual(["zeta", "alpha", "alpha/inner", "mid", "loose"]));
    daemon.sidebar_view = { group_by: "group", sort: "name" };
    await pollAt(result, 2);
    await waitFor(() => expect(groupHeaders(result)).toEqual(["alpha", "alpha/inner", "loose", "mid", "zeta"]));
  });

  it("a drag sends one move carrying the siblings, and refuses another meanwhile", async () => {
    const { result } = mount();
    await waitFor(() => expect(groupHeaders(result)).toHaveLength(5));
    const releases = heldReorders();
    act(() => result.current.model.moveGroupHeader("mid", "zeta", false));
    // Shown in its new place at once.
    expect(groupHeaders(result)).toEqual(["mid", "zeta", "alpha", "alpha/inner", "loose"]);
    await waitFor(() => expect(writes).toHaveLength(1));
    act(() => result.current.model.moveGroupHeader("zeta", "alpha", true));
    expect(toasts.info).toHaveBeenCalledWith("The last move is still saving");
    await act(async () => releases.shift()!());
    await waitFor(() => expect(result.current.sessions.orderingSaving()).toBe(false));
    expect(writes).toEqual([
      {
        method: "POST",
        body: {
          target: "group",
          group: { profile: "default", path: "mid" },
          direction: "up",
          siblings: ["zeta", "mid", "alpha", "loose"],
        },
      },
    ]);
    await pollAt(result, daemon.revision);
    expect(groupHeaders(result)).toEqual(["mid", "zeta", "alpha", "alpha/inner", "loose"]);
  });

  it("keep their place when a row drag switches Name to Custom, and Undo puts their order back", async () => {
    daemon.sidebar_view = { group_by: "group", sort: "name" };
    const { result } = mount();
    const named = ["alpha", "alpha/inner", "loose", "mid", "zeta"];
    await waitFor(() => expect(groupHeaders(result)).toEqual(named));
    await act(async () => result.current.model.moveWorkspace("/r/one::zeta", "/r/one::mid", false));
    await waitFor(() => expect(writes).toHaveLength(3));
    expect(writes.slice(1)).toEqual([
      {
        method: "POST",
        body: {
          target: "group",
          group: { profile: "default", path: "alpha" },
          direction: "up",
          siblings: ["loose", "alpha", "mid", "zeta"],
        },
      },
      { method: "PATCH", body: { sort: "custom" } },
    ]);
    expect(result.current.model.sort).toBe("custom");
    expect(groupHeaders(result)).toEqual(named);
    await pollAt(result, daemon.revision);
    expect(groupHeaders(result)).toEqual(named);

    await act(async () => void toasts.undo[0]!.run());
    await waitFor(() => expect(writes).toHaveLength(6));
    expect(writes[4]).toMatchObject({ method: "POST", body: { group: { path: "zeta" } } });
    expect(daemon.group_order.default).toEqual(["zeta", "alpha", "alpha/inner", "mid", "loose"]);
    expect(writes[5]).toEqual({ method: "PATCH", body: { sort: "name" } });
  });

  it("drop the Undo when the first read shows another Client rearranged them", async () => {
    daemon.sidebar_view = { group_by: "group", sort: "name" };
    const { result } = mount();
    await waitFor(() => expect(groupHeaders(result)).toHaveLength(5));
    await act(async () => result.current.model.moveWorkspace("/r/one::zeta", "/r/one::mid", false));
    await waitFor(() => expect(toasts.undo).toHaveLength(1));
    daemon.group_order.default = ["zeta", "mid", "alpha", "alpha/inner", "loose"];
    await pollAt(result, daemon.revision + 1);
    expect(toasts.undo[0]!.dismissed).toBe(true);
  });

  it("put the order back when a Group move fails during a switch from Name", async () => {
    daemon.sidebar_view = { group_by: "group", sort: "name" };
    const before = [...daemon.workspace_ordering];
    server.use(http.post("*/api/reorder", () => new HttpResponse(null, { status: 500 }), { once: true }));
    const { result } = mount();
    await waitFor(() => expect(groupHeaders(result)).toHaveLength(5));
    await act(async () => result.current.model.moveWorkspace("/r/one::zeta", "/r/one::mid", false));
    await waitFor(() => expect(toasts.info).toHaveBeenCalledWith("Couldn't switch to Custom, so nothing moved"));
    expect(writes.map((w) => w.method)).toEqual(["PUT", "PUT", "POST"]);
    expect(daemon.workspace_ordering).toEqual(before);
    expect(toasts.undo).toEqual([]);
    await pollAt(result, daemon.revision);
    expect(result.current.model.sort).toBe("name");
  });

  it("refuse a switch from Name that would reorder the Groups of several profiles", async () => {
    for (const [order, refused] of [
      [{ default: ["zeta"], work: ["alpha"] }, true],
      [{ default: ["alpha"], work: ["zeta"] }, false],
    ] as const) {
      writes.length = 0;
      toasts.info.mockClear();
      const [first, second] = Object.keys(order).map((profile) => order[profile as keyof typeof order][0]);
      daemon.sessions = [
        { ...grouped("one", first), profile: "default" },
        { ...grouped("two", first), profile: "default" },
        { ...grouped("three", second), profile: "work" },
      ];
      daemon.workspace_ordering = daemon.sessions.map(wsId);
      daemon.group_order = { ...order };
      daemon.sidebar_view = { group_by: "group", sort: "name" };
      const { result, unmount } = mount();
      await waitFor(() => expect(groupHeaders(result)).toEqual(["alpha", "zeta"]));
      await act(async () => result.current.model.moveWorkspace("/r/one::two", "/r/one::one", false));
      if (refused) {
        expect(toasts.info).toHaveBeenCalledWith("Pick Custom first: Groups of several profiles can't keep this order");
        expect(writes).toEqual([]);
        expect(result.current.model.sort).toBe("name");
      } else {
        await waitFor(() => expect(writes.map((w) => w.method)).toEqual(["PUT", "PATCH"]));
        expect(groupHeaders(result)).toEqual(["alpha", "zeta"]);
      }
      unmount();
    }
  });

  it("are not moved under Name, which points to Custom, nor across parents", async () => {
    daemon.sidebar_view = { group_by: "group", sort: "name" };
    const { result } = mount();
    await waitFor(() => expect(groupHeaders(result)).toHaveLength(5));
    act(() => result.current.model.moveGroupHeader("zeta", "alpha", false));
    expect(toasts.info).toHaveBeenCalledWith("Pick Custom to arrange groups");
    act(() => result.current.model.selectSort("custom"));
    await waitFor(() => expect(result.current.sessions.orderingSaving()).toBe(false));
    act(() => result.current.model.moveGroupHeader("alpha/inner", "zeta", false));
    expect(toasts.info).toHaveBeenLastCalledWith("Couldn't move the group there");
    expect(writes.filter((w) => w.method === "POST")).toEqual([]);
  });

  it("say why a drop the Daemon can't carry out snaps back", async () => {
    daemon.sessions.push(
      { ...grouped("other-mid", "mid"), profile: "other" },
      { ...grouped("other-only", "theirs"), profile: "other" },
    );
    daemon.group_order.other = ["theirs", "mid"];
    const { result } = mount();
    await waitFor(() => expect(groupHeaders(result)).toContain("theirs"));
    for (const [active, over, notice] of [
      ["mid", "zeta", "Groups move only within one profile"],
      ["zeta", "theirs", "Groups move only within one profile"],
    ] as const) {
      act(() => result.current.model.moveGroupHeader(active, over, false));
      expect(toasts.info).toHaveBeenLastCalledWith(notice);
    }
    expect(writes.filter((w) => w.method === "POST")).toEqual([]);

    server.use(http.post("*/api/reorder", () => HttpResponse.json({ status: "at_edge" })));
    act(() => result.current.model.moveGroupHeader("alpha", "zeta", false));
    await waitFor(() => expect(toasts.info).toHaveBeenLastCalledWith("Couldn't move the group there"));
  });

  it("count only the dragged profile's Groups as siblings", async () => {
    daemon.sessions.push({ ...grouped("other-only", "aaa"), profile: "other" });
    daemon.group_order.other = ["aaa"];
    server.use(
      http.post("*/api/reorder", async ({ request }) => {
        writes.push({ method: "POST", body: await request.json() });
        daemon.revision += 1;
        return HttpResponse.json({ status: "moved", destination: null }, { headers: runtime(daemon.revision) });
      }),
    );
    const { result } = mount();
    await waitFor(() => expect(groupHeaders(result)).toContain("aaa"));
    act(() => result.current.model.moveGroupHeader("zeta", "loose", true));
    await waitFor(() => expect(result.current.sessions.orderingSaving()).toBe(false));
    // The other profile's aaa is not a sibling.
    expect(writes.filter((w) => w.method === "POST").map((w) => (w.body as ReorderBody).siblings)).toEqual([
      ["alpha", "mid", "zeta", "loose"],
    ]);
  });
});

/** Holds each `POST /api/reorder` until released, then moves the Group as the default handler does. */
function heldReorders() {
  const releases: (() => void)[] = [];
  server.use(
    http.post("*/api/reorder", async ({ request }) => {
      const body = (await request.json()) as ReorderBody;
      writes.push({ method: "POST", body });
      await new Promise<void>((r) => releases.push(r));
      reorder(body);
      daemon.revision += 1;
      return HttpResponse.json({ status: "moved", destination: null }, { headers: runtime(daemon.revision) });
    }),
  );
  return releases;
}
