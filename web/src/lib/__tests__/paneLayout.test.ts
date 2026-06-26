// @vitest-environment jsdom
import { act, renderHook } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it } from "vitest";

import {
  addTab,
  addTerminal,
  dockOf,
  dockTabs,
  moveTab,
  removeAllTerminals,
  removeTab,
  setActive,
  syncPluginTabs,
  usePaneLayout,
  type DockLayout,
} from "../paneLayout";

beforeEach(() => localStorage.clear());
afterEach(() => localStorage.clear());

function emptyLayout(): DockLayout {
  return { right: [], bottom: [], nextTerminalIndex: 1, closedPlugins: [] };
}

describe("pane layout pure ops", () => {
  it("addTab appends, sets active, and is idempotent per tab id", () => {
    let l = addTab(emptyLayout(), "right", "diff");
    l = addTab(l, "right", "terminal:0");
    expect(dockTabs(l, "right")).toEqual(["diff", "terminal:0"]);
    // adding an already-open tab is a no-op (returns the same reference)
    expect(addTab(l, "right", "diff")).toBe(l);
  });

  it("addTerminal allocates a monotonic index and bumps the counter", () => {
    const a = addTerminal(emptyLayout(), "right");
    expect(a.tabId).toBe("terminal:1");
    const b = addTerminal(a.layout, "right");
    expect(b.tabId).toBe("terminal:2");
    expect(b.layout.nextTerminalIndex).toBe(3);
  });

  it("removeTab fixes the active tab and prunes the empty dock", () => {
    let l = addTab(emptyLayout(), "right", "diff");
    l = addTab(l, "right", "terminal:0");
    l = setActive(l, "right", "terminal:0");
    l = removeTab(l, "terminal:0");
    expect(dockTabs(l, "right")).toEqual(["diff"]);
    expect(l.right[0]!.active).toBe("diff");
    l = removeTab(l, "diff");
    expect(l.right).toEqual([]); // dock hidden once empty
  });

  it("moveTab relocates a tab between docks", () => {
    let l = addTab(emptyLayout(), "right", "diff");
    l = moveTab(l, "diff", "bottom");
    expect(dockOf(l, "diff")).toBe("bottom");
    expect(dockTabs(l, "right")).toEqual([]);
  });

  it("removeAllTerminals clears every terminal tab but keeps others", () => {
    let l = addTab(emptyLayout(), "right", "diff");
    l = addTerminal(l, "right").layout;
    l = addTerminal(l, "bottom").layout;
    l = removeAllTerminals(l);
    expect(dockTabs(l, "right")).toEqual(["diff"]);
    expect(dockTabs(l, "bottom")).toEqual([]);
  });

  it("closing a plugin tab suppresses its auto re-add; syncPluginTabs respects it", () => {
    let l = addTab(emptyLayout(), "right", "plugin:p:a");
    l = removeTab(l, "plugin:p:a");
    expect(l.closedPlugins).toContain("plugin:p:a");
    // sync must not re-add a tab the user explicitly closed
    l = syncPluginTabs(l, [{ id: "plugin:p:a", defaultDock: "right" }]);
    expect(dockOf(l, "plugin:p:a")).toBeNull();
    // a brand-new plugin pane is added to its default dock
    l = syncPluginTabs(l, [{ id: "plugin:p:b", defaultDock: "bottom" }]);
    expect(dockOf(l, "plugin:p:b")).toBe("bottom");
  });
});

describe("usePaneLayout migration + persistence", () => {
  it("migrates the v1 expanded layout to terminal:0 + diff tabs", () => {
    localStorage.setItem(
      "aoe-pane-layout",
      JSON.stringify({ diff: { open: true, dock: "right" }, terminal: { open: true, dock: "bottom" } }),
    );
    const { result } = renderHook(() => usePaneLayout("s1"));
    expect(dockTabs(result.current.layout, "right")).toEqual(["diff"]);
    expect(dockTabs(result.current.layout, "bottom")).toEqual(["terminal:0"]);
  });

  it("migrates the legacy collapsed flag (1 = both docks empty)", () => {
    localStorage.setItem("aoe-right-collapsed", "1");
    const { result } = renderHook(() => usePaneLayout("s1"));
    expect(result.current.layout.right).toEqual([]);
    expect(result.current.layout.bottom).toEqual([]);
  });

  it("keeps terminal tab sets independent per session and persists", () => {
    const { result } = renderHook(() => usePaneLayout("s1"));
    act(() => result.current.addTerminal("right"));
    expect(dockTabs(result.current.layout, "right")).toContain("terminal:1");

    // A different session starts from the template, unaffected by s1's tab.
    const other = renderHook(() => usePaneLayout("s2"));
    expect(dockTabs(other.result.current.layout, "right")).not.toContain("terminal:1");

    // s1's addition round-trips through localStorage.
    const reloaded = renderHook(() => usePaneLayout("s1"));
    expect(dockTabs(reloaded.result.current.layout, "right")).toContain("terminal:1");
  });

  it("toggleKind adds then removes the terminal tabs", () => {
    localStorage.setItem("aoe-right-collapsed", "1"); // start empty
    const { result } = renderHook(() => usePaneLayout("s1"));
    act(() => result.current.toggleKind("terminal", "right"));
    expect(dockTabs(result.current.layout, "right")).toEqual(["terminal:0"]);
    act(() => result.current.toggleKind("terminal", "right"));
    expect(dockTabs(result.current.layout, "right")).toEqual([]);
  });
});
