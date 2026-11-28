import { describe, expect, it } from "vitest";
import {
  UNGROUPED_GROUP_ID,
  type NestedSidebarGroup,
  type SidebarGroup,
  type SidebarWorkspaceView,
} from "../../lib/sidebarGroups";
import type { Workspace } from "../../lib/types";
import { renderedOrder, sunkViews, visibleSubgroups } from "./filterGroups";

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
  it("lists a workspace sliced across Groups once, with every archived session", () => {
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

  it("lists archived rows newest first, then snoozed rows soonest wake first, whatever the group order", () => {
    const row = (groupId: string, id: string, sessions: SessionStub[]) => ({
      key: `${groupId}::${id}`,
      workspace: workspace(id, sessions),
    });
    const groups = [
      group("a", [
        row("a", "late-wake", [{ id: "s1", snoozed_until: "2027-06-01T00:00:00Z" }]),
        row("a", "old", [{ id: "s2", archived_at: "2026-01-01T00:00:00Z", last_accessed_at: "2026-06-01T00:00:00Z" }]),
        row("a", "stale", [{ id: "s3", archived_at: "2026-01-01T00:00:00Z" }]),
      ]),
      group("b", [
        row("b", "soon-wake", [{ id: "s4", snoozed_until: "2027-01-01T00:00:00Z" }]),
        row("b", "mixed", [
          { id: "s5", archived_at: "2026-01-01T00:00:00Z" },
          { id: "s6", archived_at: "2026-04-01T00:00:00Z" },
        ]),
        row("b", "trashed-and-snoozed", [
          { id: "s7", trashed_at: "2026-02-01T00:00:00Z" },
          { id: "s8", snoozed_until: "2027-03-01T00:00:00Z" },
        ]),
      ]),
    ];
    // A future wake time never outranks an archive; archived_at wins over later activity; ties keep group order.
    expect(sunkViews(groups).map((v) => v.workspace.id)).toEqual([
      "mixed",
      "old",
      "stale",
      "soon-wake",
      "trashed-and-snoozed",
      "late-wake",
    ]);
    expect(renderedOrder([], false, true, groups)).toEqual([
      "b::mixed",
      "a::old",
      "a::stale",
      "b::soon-wake",
      "b::trashed-and-snoozed",
      "a::late-wake",
    ]);
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
  it("drops the children a collapsed repo hides", () => {
    const nested = [
      { repo: group("/a", []), subgroups: [group("feature", [live("a")])] },
      { repo: { ...group("/b", []), collapsed: true }, subgroups: [group("fix", [live("b")])] },
    ] as NestedSidebarGroup[];
    expect(visibleSubgroups(nested, false).map((g) => g.id)).toEqual(["feature"]);
    expect(visibleSubgroups(nested, true).map((g) => g.id)).toEqual(["feature", "fix"]);
  });

  it("ranges over a repo's ungrouped rows even when a browser saved them collapsed", () => {
    const shut = (id: string) => ({ ...group(id, [live(id)]), collapsed: true });
    const nested = [
      { repo: group("/a", []), subgroups: [shut(UNGROUPED_GROUP_ID), shut("fix")] },
    ] as NestedSidebarGroup[];
    expect(renderedOrder(visibleSubgroups(nested, false), false, false)).toEqual([`${UNGROUPED_GROUP_ID}-row`]);
  });
});
