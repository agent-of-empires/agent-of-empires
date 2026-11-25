import { describe, expect, it } from "vitest";
import type { NestedSidebarGroup, OrgNestedGroup, SidebarGroup, SidebarWorkspaceView } from "../../lib/sidebarGroups";
import type { Workspace } from "../../lib/types";
import { renderedOrder, sunkViews, visibleOrgRepos, visibleSubgroups } from "./filterGroups";

type SessionStub = {
  id: string;
  archived_at?: string;
  snoozed_until?: string;
  last_accessed_at?: string;
  trashed_at?: string;
};

const workspace = (id: string, sessions: SessionStub[]): Workspace => ({ id, sessions }) as unknown as Workspace;

const group = (id: string, views: SidebarWorkspaceView[]): SidebarGroup => ({ id, workspaces: views }) as SidebarGroup;

const archived = (status: string, workspaceId: string, sessionId: string): SidebarWorkspaceView => ({
  key: `${status}::${workspaceId}`,
  workspace: workspace(workspaceId, [{ id: sessionId, archived_at: "2026-01-01T00:00:00Z" }]),
});

describe("sunkViews", () => {
  it("lists a workspace sliced across status buckets once, with every archived session", () => {
    const groups = [
      group("status:Idle", [archived("Idle", "w1", "s-idle")]),
      group("status:Running", [archived("Running", "w1", "s-running")]),
      group("status:Stopped", [archived("Stopped", "w2", "s-stopped")]),
    ];
    const views = sunkViews(groups);
    expect(views.map((v) => v.workspace.id)).toEqual(["w1", "w2"]);
    expect(views[0]!.workspace.sessions.map((s) => s.id)).toEqual(["s-idle", "s-running"]);
  });

  it("leaves live and trashed workspaces to the main list and the trash menu", () => {
    const groups = [
      group("status:Idle", [
        { key: "Idle::live", workspace: workspace("live", [{ id: "s-live" }]) },
        {
          key: "Idle::trashed",
          workspace: workspace("trashed", [{ id: "s-trashed", trashed_at: "2026-01-01T00:00:00Z" }]),
        },
      ]),
    ];
    expect(sunkViews(groups)).toEqual([]);
  });

  it("orders rows newest sunk first across groups, whatever the group order", () => {
    const row = (groupId: string, id: string, sessions: SessionStub[]) => ({
      key: `${groupId}::${id}`,
      workspace: workspace(id, sessions),
    });
    const groups = [
      group("a", [
        row("a", "old", [{ id: "s1", archived_at: "2026-01-01T00:00:00Z", last_accessed_at: "2026-06-01T00:00:00Z" }]),
        row("a", "stale", [{ id: "s2", archived_at: "2026-01-01T00:00:00Z" }]),
      ]),
      group("b", [
        row("b", "snoozed", [{ id: "s3", snoozed_until: "2026-03-01T00:00:00Z" }]),
        row("b", "mixed", [
          { id: "s4", archived_at: "2026-01-01T00:00:00Z" },
          { id: "s5", archived_at: "2026-04-01T00:00:00Z" },
        ]),
      ]),
    ];
    // archived_at wins over later activity; ties keep group order.
    expect(sunkViews(groups).map((v) => v.workspace.id)).toEqual(["mixed", "snoozed", "old", "stale"]);
  });
});

const live = (id: string): SidebarWorkspaceView => ({
  key: `${id}-row`,
  workspace: workspace(id, [{ id: `${id}-s` }]),
});

const sunkSlice = (status: string, id: string, sessionId: string): SidebarWorkspaceView => ({
  key: `${status}::${id}`,
  workspace: workspace(id, [{ id: sessionId, archived_at: "2026-01-01T00:00:00Z" }]),
});

describe("renderedOrder", () => {
  it("orders live row keys and skips rows under a collapsed group", () => {
    const open = group("open", [live("a")]);
    const shut = { ...group("shut", [live("b")]), collapsed: true };
    expect(renderedOrder([open, shut], false, false)).toEqual(["a-row"]);
    // A filter expands every group, matching what the list renders.
    expect(renderedOrder([open, shut], true, false)).toEqual(["a-row", "b-row"]);
  });

  it("keys the footer once per workspace, from the rows it renders", () => {
    const groups = [
      group("status:Waiting", [sunkSlice("Waiting", "w1", "s-wait")]),
      group("status:Running", [sunkSlice("Running", "w1", "s-run")]),
      group("status:Stopped", [sunkSlice("Stopped", "w2", "s-stop")]),
    ];
    // w1 has two archived slices but one footer row; a range must not reach the hidden slice.
    expect(renderedOrder([], false, true, groups)).toEqual(["Waiting::w1", "Stopped::w2"]);
    expect(renderedOrder([], false, false, groups)).toEqual([]);
  });
});

describe("visible parents", () => {
  it("drops the children a collapsed repo or org hides", () => {
    const nested = [
      { repo: group("/a", []), subgroups: [group("feature", [live("a")])] },
      { repo: { ...group("/b", []), collapsed: true }, subgroups: [group("fix", [live("b")])] },
    ] as NestedSidebarGroup[];
    expect(visibleSubgroups(nested, false).map((g) => g.id)).toEqual(["feature"]);
    expect(visibleSubgroups(nested, true).map((g) => g.id)).toEqual(["feature", "fix"]);

    const orgs = [
      { org: { ...group("acme", []), collapsed: true }, repos: [group("/a", [])] },
      { org: group("other", []), repos: [group("/b", [])] },
    ] as OrgNestedGroup[];
    expect(visibleOrgRepos(orgs, false).map((g) => g.id)).toEqual(["/b"]);
    expect(visibleOrgRepos(orgs, true).map((g) => g.id)).toEqual(["/a", "/b"]);
  });
});
