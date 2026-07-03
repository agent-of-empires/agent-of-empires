// @vitest-environment jsdom
import { renderHook } from "@testing-library/react";
import { GitBranch } from "lucide-react";
import { describe, expect, it, vi } from "vitest";

import type { PluginUiEntry } from "../api";
import { isPluginPaneId, resolvePaneIcon, usePluginPanes } from "../pluginPanes";

const { entriesRef } = vi.hoisted(() => ({ entriesRef: { current: [] as PluginUiEntry[] } }));
vi.mock("../pluginUiContext", () => ({ usePluginUiEntries: () => entriesRef.current }));

function set(entries: PluginUiEntry[]) {
  entriesRef.current = entries;
}

describe("usePluginPanes", () => {
  it("derives namespaced panes for the session with title + default dock", () => {
    set([
      {
        plugin_id: "gh",
        slot: "pane",
        id: "main",
        session_id: "s1",
        payload: { title: "GitHub", default_location: "bottom" },
      },
      { plugin_id: "gh", slot: "pane", id: "other", session_id: "s2", payload: { title: "Nope" } },
      { plugin_id: "gh", slot: "detail-badge", id: "b", session_id: "s1", payload: { text: "x" } },
    ]);
    const { result } = renderHook(() => usePluginPanes("s1"));
    expect(result.current).toHaveLength(1);
    expect(result.current[0]).toMatchObject({ id: "plugin:gh:main", title: "GitHub", defaultDock: "bottom" });
    expect(isPluginPaneId(result.current[0]!.id)).toBe(true);
  });

  it("defaults the dock to right and the title to the plugin id", () => {
    set([{ plugin_id: "gh", slot: "pane", id: "main", session_id: "s1", payload: {} }]);
    const { result } = renderHook(() => usePluginPanes("s1"));
    expect(result.current[0]).toMatchObject({ title: "gh", defaultDock: "right" });
  });

  it("returns nothing without a session", () => {
    set([{ plugin_id: "gh", slot: "pane", id: "main", session_id: "s1", payload: {} }]);
    expect(renderHook(() => usePluginPanes(null)).result.current).toEqual([]);
  });
});

describe("resolvePaneIcon", () => {
  it("prefers the pane's own runtime icon over the manifest icon", () => {
    expect(resolvePaneIcon(GitBranch, "puzzle")).toBe(GitBranch);
  });

  it("falls back to the manifest identity icon when the pane sets none", () => {
    expect(resolvePaneIcon(undefined, "git-branch")).toBeDefined();
  });

  it("falls through to undefined for an unknown manifest icon name", () => {
    expect(resolvePaneIcon(undefined, "not-a-real-lucide-icon-name")).toBeUndefined();
  });

  it("falls through to undefined when neither is set", () => {
    expect(resolvePaneIcon(undefined, undefined)).toBeUndefined();
  });
});
