// @vitest-environment jsdom

import { act, renderHook, waitFor } from "@testing-library/react";
import { http, HttpResponse } from "msw";
import { setupServer } from "msw/node";
import { afterAll, afterEach, beforeAll, beforeEach, describe, expect, it, vi } from "vitest";
import type { SidebarView } from "../../lib/api";
import { reportFeatureUse } from "../../lib/featureUse";
import type { SessionResponse } from "../../lib/types";
import { useSidebarModel } from "../app/useSidebarModel";
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
);
beforeAll(() => server.listen({ onUnhandledRequest: "error" }));
afterEach(() => server.resetHandlers());
afterAll(() => server.close());

beforeEach(() => {
  localStorage.clear();
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
      changeOrdering: sessions.changeOrdering,
      changeSidebarView: sessions.changeSidebarView,
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

    act(() => result.current.model.selectSort("name"));
    await waitFor(() => expect(writes).toHaveLength(2));
    await act(async () => release());
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
