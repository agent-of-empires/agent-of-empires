// @vitest-environment jsdom
import { act, renderHook } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it } from "vitest";

import {
  addTab,
  addTerminal,
  dockOf,
  dockTabs,
  isDockCollapsed,
  isActiveTab,
  moveTab,
  placeTab,
  removeAllTerminals,
  removeTab,
  setActive,
  setDockCollapsed,
  syncPluginTabs,
  usePaneLayout,
  type DockLayout,
} from "../paneLayout";

beforeEach(() => localStorage.clear());
afterEach(() => localStorage.clear());

function emptyLayout(): DockLayout {
  return { right: [], bottom: [], nextTerminalIndex: 1, closedPlugins: [], collapsed: { right: false, bottom: false } };
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

  it("placeTab reorders within a dock without changing the active tab", () => {
    let l = addTab(emptyLayout(), "right", "a");
    l = addTab(l, "right", "b");
    l = addTab(l, "right", "c");
    l = setActive(l, "right", "a");
    // Drag "a" to the end. index is the post-removal index, so end == 2.
    l = placeTab(l, "a", { dock: "right", group: 0, index: 2 });
    expect(dockTabs(l, "right")).toEqual(["b", "c", "a"]);
    // Reordering the active tab keeps it active.
    expect(l.right[0]!.active).toBe("a");
  });

  it("placeTab reordering a background tab keeps the existing active tab", () => {
    let l = addTab(emptyLayout(), "right", "a");
    l = addTab(l, "right", "b");
    l = addTab(l, "right", "c");
    l = setActive(l, "right", "a");
    l = placeTab(l, "c", { dock: "right", group: 0, index: 0 });
    expect(dockTabs(l, "right")).toEqual(["c", "a", "b"]);
    expect(l.right[0]!.active).toBe("a");
  });

  it("placeTab moves a tab across docks at an index and activates it there", () => {
    let l = addTab(emptyLayout(), "right", "a");
    l = addTab(l, "right", "b");
    l = addTab(l, "bottom", "x");
    l = addTab(l, "bottom", "y");
    l = setActive(l, "right", "a");
    l = setActive(l, "bottom", "x");
    // Move right's active "a" between x and y.
    l = placeTab(l, "a", { dock: "bottom", group: 0, index: 1 });
    expect(dockTabs(l, "right")).toEqual(["b"]);
    expect(dockTabs(l, "bottom")).toEqual(["x", "a", "y"]);
    // Moved tab activates in its destination.
    expect(l.bottom[0]!.active).toBe("a");
    // Source falls back to a neighbor.
    expect(l.right[0]!.active).toBe("b");
  });

  it("placeTab clamps an out-of-range index within a group", () => {
    let l = addTab(emptyLayout(), "right", "a");
    l = addTab(l, "right", "b");
    l = placeTab(l, "a", { dock: "right", group: 0, index: 99 });
    expect(dockTabs(l, "right")).toEqual(["b", "a"]);
  });

  it("placeTab does not mark a moved plugin tab as closed", () => {
    let l = addTab(emptyLayout(), "right", "plugin:p:a");
    l = placeTab(l, "plugin:p:a", { dock: "bottom", group: 0, newGroup: true });
    expect(dockOf(l, "plugin:p:a")).toBe("bottom");
    expect(l.closedPlugins).not.toContain("plugin:p:a");
  });

  it("placeTab split lifts a tab into a new sibling group in the same dock", () => {
    let l = addTab(emptyLayout(), "right", "a");
    l = addTab(l, "right", "b");
    // Split "b" into a fresh group after group 0.
    l = placeTab(l, "b", { dock: "right", group: 1, newGroup: true });
    expect(l.right.length).toBe(2);
    expect(l.right[0]!.tabs).toEqual(["a"]);
    expect(l.right[0]!.active).toBe("a");
    expect(l.right[1]!.tabs).toEqual(["b"]);
    expect(l.right[1]!.active).toBe("b");
  });

  it("removeTab prunes only the emptied group, leaving siblings intact", () => {
    let l = addTab(emptyLayout(), "right", "a");
    l = addTab(l, "right", "b");
    l = placeTab(l, "b", { dock: "right", group: 1, newGroup: true });
    l = removeTab(l, "a"); // empties group 0 only
    expect(l.right.length).toBe(1);
    expect(l.right[0]!.tabs).toEqual(["b"]);
  });

  it("placeTab into another group activates it there and prunes the emptied source group", () => {
    let l = addTab(emptyLayout(), "right", "a");
    l = addTab(l, "right", "b");
    l = placeTab(l, "b", { dock: "right", group: 1, newGroup: true }); // groups [a], [b]
    l = setActive(l, "right", "a");
    // Move "a" (its group's only tab) into group 1; group 0 prunes and the
    // target-group index shifts down by one.
    l = placeTab(l, "a", { dock: "right", group: 1, index: 1 });
    expect(l.right.length).toBe(1);
    expect(l.right[0]!.tabs).toEqual(["b", "a"]);
    expect(l.right[0]!.active).toBe("a");
  });

  it("isActiveTab reports each group's active tab independently", () => {
    let l = addTab(emptyLayout(), "right", "a");
    l = addTab(l, "right", "b");
    l = placeTab(l, "b", { dock: "right", group: 1, newGroup: true });
    expect(isActiveTab(l, "a")).toBe(true);
    expect(isActiveTab(l, "b")).toBe(true);
  });

  it("moveTab appends to the destination and is a no-op within the same dock", () => {
    let l = addTab(emptyLayout(), "bottom", "x");
    l = addTab(l, "right", "a");
    l = moveTab(l, "a", "bottom");
    expect(dockTabs(l, "bottom")).toEqual(["x", "a"]);
    expect(moveTab(l, "a", "bottom")).toBe(l);
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

  it("setDockCollapsed preserves tabs, active tab, and plugin close intent", () => {
    let l = addTab(emptyLayout(), "right", "diff");
    l = addTab(l, "right", "plugin:p:a");
    l = setActive(l, "right", "plugin:p:a");

    l = setDockCollapsed(l, "right", true);

    expect(isDockCollapsed(l, "right")).toBe(true);
    expect(dockTabs(l, "right")).toEqual(["diff", "plugin:p:a"]);
    expect(l.right[0]!.active).toBe("plugin:p:a");
    expect(l.closedPlugins).not.toContain("plugin:p:a");

    l = setDockCollapsed(l, "right", false);

    expect(isDockCollapsed(l, "right")).toBe(false);
    expect(dockTabs(l, "right")).toEqual(["diff", "plugin:p:a"]);
    expect(l.right[0]!.active).toBe("plugin:p:a");
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

  it("persists dock collapse state per session", () => {
    const { result } = renderHook(() => usePaneLayout("s1"));
    act(() => result.current.setDockCollapsed("right", true));
    expect(isDockCollapsed(result.current.layout, "right")).toBe(true);

    const other = renderHook(() => usePaneLayout("s2"));
    expect(isDockCollapsed(other.result.current.layout, "right")).toBe(false);

    const reloaded = renderHook(() => usePaneLayout("s1"));
    expect(isDockCollapsed(reloaded.result.current.layout, "right")).toBe(true);
  });

  it("syncPlugins adds plugin tabs without revealing a collapsed dock", () => {
    const { result } = renderHook(() => usePaneLayout("s1"));
    act(() => result.current.setDockCollapsed("right", true));
    act(() => result.current.syncPlugins([{ id: "plugin:p:a", defaultDock: "right" }]));

    expect(dockOf(result.current.layout, "plugin:p:a")).toBe("right");
    expect(isDockCollapsed(result.current.layout, "right")).toBe(true);
  });

  it("openTab reveals a collapsed target dock", () => {
    const { result } = renderHook(() => usePaneLayout("s1"));
    act(() => result.current.setDockCollapsed("right", true));
    act(() => result.current.openTab("diff", "right"));

    expect(dockOf(result.current.layout, "diff")).toBe("right");
    expect(isDockCollapsed(result.current.layout, "right")).toBe(false);
  });

  it("togglePlugin reveals an open plugin in a collapsed dock instead of closing it", () => {
    const { result } = renderHook(() => usePaneLayout("s1"));
    act(() => result.current.openTab("plugin:p:a", "right"));
    act(() => result.current.setDockCollapsed("right", true));
    act(() => result.current.togglePlugin("plugin:p:a", "right"));

    expect(dockOf(result.current.layout, "plugin:p:a")).toBe("right");
    expect(isDockCollapsed(result.current.layout, "right")).toBe(false);
    expect(result.current.layout.closedPlugins).not.toContain("plugin:p:a");
  });

  it("drops a tab id duplicated across docks on load (keeps the first dock)", () => {
    localStorage.setItem(
      "aoe-pane-layout-v2",
      JSON.stringify({
        version: 2,
        template: { right: [], bottom: [], nextTerminalIndex: 1, closedPlugins: [] },
        sessions: {
          s1: {
            right: [{ tabs: ["diff", "terminal:0"], active: "diff" }],
            bottom: [{ tabs: ["diff"], active: "diff" }],
            nextTerminalIndex: 1,
            closedPlugins: [],
          },
        },
      }),
    );
    const { result } = renderHook(() => usePaneLayout("s1"));
    expect(dockTabs(result.current.layout, "right")).toEqual(["diff", "terminal:0"]);
    expect(dockTabs(result.current.layout, "bottom")).toEqual([]);
  });

  it("drops a tab id duplicated within one group on load", () => {
    localStorage.setItem(
      "aoe-pane-layout-v2",
      JSON.stringify({
        version: 2,
        template: { right: [], bottom: [], nextTerminalIndex: 1, closedPlugins: [] },
        sessions: {
          s1: {
            right: [{ tabs: ["diff", "diff", "terminal:0"], active: "diff" }],
            bottom: [],
            nextTerminalIndex: 1,
            closedPlugins: [],
          },
        },
      }),
    );
    const { result } = renderHook(() => usePaneLayout("s1"));
    expect(dockTabs(result.current.layout, "right")).toEqual(["diff", "terminal:0"]);
  });

  it("loads multiple persisted groups per dock without merging them", () => {
    localStorage.setItem(
      "aoe-pane-layout-v2",
      JSON.stringify({
        version: 2,
        template: { right: [], bottom: [], nextTerminalIndex: 1, closedPlugins: [] },
        sessions: {
          s1: {
            right: [
              { tabs: ["diff"], active: "diff" },
              { tabs: ["terminal:0"], active: "terminal:0" },
            ],
            bottom: [],
            nextTerminalIndex: 1,
            closedPlugins: [],
          },
        },
      }),
    );
    const { result } = renderHook(() => usePaneLayout("s1"));
    expect(result.current.layout.right.length).toBe(2);
    expect(result.current.layout.right[0]!.tabs).toEqual(["diff"]);
    expect(result.current.layout.right[1]!.tabs).toEqual(["terminal:0"]);
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
