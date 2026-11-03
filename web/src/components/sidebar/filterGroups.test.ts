import { describe, expect, it } from "vitest";
import type { SidebarGroup, SidebarWorkspaceView } from "../../lib/sidebarGroups";
import type { Workspace } from "../../lib/types";
import { sunkViews } from "./filterGroups";

const workspace = (id: string, sessions: { id: string; archived_at?: string; trashed_at?: string }[]): Workspace =>
  ({ id, sessions }) as unknown as Workspace;

const group = (id: string, views: SidebarWorkspaceView[]): SidebarGroup => ({ id, workspaces: views }) as SidebarGroup;

const archived = (status: string, workspaceId: string, sessionId: string): SidebarWorkspaceView => ({
  key: `${status}::${workspaceId}`,
  workspace: workspace(workspaceId, [{ id: sessionId, archived_at: "2026-01-01T00:00:00Z" }]),
});

describe("sunkViews", () => {
  it("lists a workspace sliced across status buckets once", () => {
    const groups = [
      group("status:Idle", [archived("Idle", "w1", "s-idle")]),
      group("status:Running", [archived("Running", "w1", "s-running")]),
      group("status:Stopped", [archived("Stopped", "w2", "s-stopped")]),
    ];
    expect(sunkViews(groups).map((v) => v.workspace.id)).toEqual(["w1", "w2"]);
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
});
