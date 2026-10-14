// @vitest-environment jsdom

import { act, renderHook } from "@testing-library/react";
import { beforeEach, describe, expect, it, vi } from "vitest";
import type { PluginUiEntry } from "../../lib/api";
import { PluginUiProvider } from "../../lib/pluginUiContext";
import { nextAttentionSessionId } from "../../lib/sidebarSort";
import { useSidebarModel } from "../app/useSidebarModel";
import { session, workspace } from "./fixtures";

const { entriesRef } = vi.hoisted(() => ({ entriesRef: { current: [] as PluginUiEntry[] } }));
vi.mock("../usePluginUiState", () => ({
  usePluginUiState: () => ({ entries: entriesRef.current, revisions: {}, isRefreshing: false, poke: () => {} }),
}));

const workspaces = [
  workspace("w-running", "/repo", [session({ id: "running", status: "Running" })]),
  workspace("w-stopped", "/repo", [session({ id: "stopped", status: "Stopped" })]),
];
const toned = (sessionId: string): PluginUiEntry => ({
  plugin_id: "gh",
  slot: "row-column",
  id: "pr",
  session_id: sessionId,
  payload: { text: "PR", tone: "danger" },
});

function mountModel() {
  return renderHook(
    () =>
      useSidebarModel({
        workspaces,
        workspaceOrdering: workspaces.map((w) => w.id),
        setWorkspaceOrdering: () => {},
        markLocalOrderingUpdate: () => {},
        projects: [],
      }),
    { wrapper: PluginUiProvider },
  );
}

beforeEach(() => localStorage.clear());

describe("useSidebarModel plugin attention (#4446)", () => {
  it("sorts a toned workspace first under attention sort and offers it to jump-to-next", () => {
    entriesRef.current = [toned("stopped")];
    const { result } = mountModel();
    act(() => result.current.selectSortMode("attention"));
    expect(result.current.groups[0]!.workspaces.map((v) => v.workspace.id)).toEqual(["w-stopped", "w-running"]);
    const { orderedIds, attention } = result.current.attentionJump;
    expect(nextAttentionSessionId(orderedIds, attention, "running")).toBe("stopped");
  });

  it("leaves the order and jump set alone without a toned session", () => {
    entriesRef.current = [];
    const { result } = mountModel();
    act(() => result.current.selectSortMode("attention"));
    expect(result.current.groups[0]!.workspaces.map((v) => v.workspace.id)).toEqual(["w-running", "w-stopped"]);
    expect(result.current.attentionJump.attention.size).toBe(0);
  });
});
