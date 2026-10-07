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
  seedLayout,
  setActive,
  setDockCollapsed,
  syncPluginTabs,
  usePaneLayout,
  type DockLayout,
  type PlaceTarget,
} from "../paneLayout";

beforeEach(() => localStorage.clear());
afterEach(() => localStorage.clear());

function emptyLayout(): DockLayout {
  return { right: [], bottom: [], nextTerminalIndex: 1, closedPlugins: [], collapsed: { right: false, bottom: false } };
}

/** Right-dock tabs `right`, bottom-dock tabs `bottom`, each dock's first tab active. */
function layout(right: string[], bottom: string[] = []): DockLayout {
  let l = emptyLayout();
  for (const id of right) l = addTab(l, "right", id);
  for (const id of bottom) l = addTab(l, "bottom", id);
  if (right[0]) l = setActive(l, "right", right[0]);
  if (bottom[0]) l = setActive(l, "bottom", bottom[0]);
  return l;
}

/** Right dock split into groups [a] and [b]. */
const split = () => placeTab(layout(["a", "b"]), "b", { dock: "right", group: 1, newGroup: true });
const groups = (l: DockLayout) => l.right.map((g) => [g.tabs, g.active]);

describe("pane layout pure ops", () => {
  it("addTab appends and is idempotent per tab id", () => {
    const l = layout(["diff", "terminal:0"]);
    expect(dockTabs(l, "right")).toEqual(["diff", "terminal:0"]);
    expect(addTab(l, "right", "diff")).toBe(l);
  });

  it("addTerminal allocates a monotonic index", () => {
    const a = addTerminal(emptyLayout(), "right");
    const b = addTerminal(a.layout, "right");
    expect([a.tabId, b.tabId, b.layout.nextTerminalIndex]).toEqual(["terminal:1", "terminal:2", 3]);
  });

  it("removeTab fixes the active tab and prunes the empty dock", () => {
    let l = removeTab(setActive(layout(["diff", "terminal:0"]), "right", "terminal:0"), "terminal:0");
    expect(groups(l)).toEqual([[["diff"], "diff"]]);
    l = removeTab(l, "diff");
    expect(l.right).toEqual([]);
  });

  it("moveTab appends to the destination and is a no-op within the same dock", () => {
    const l = moveTab(layout(["a"], ["x"]), "a", "bottom");
    expect([dockOf(l, "a"), dockTabs(l, "right"), dockTabs(l, "bottom")]).toEqual(["bottom", [], ["x", "a"]]);
    expect(moveTab(l, "a", "bottom")).toBe(l);
  });

  it.each<[string, string, PlaceTarget, string[], string]>([
    ["keeps a moved active tab active", "a", { dock: "right", group: 0, index: 2 }, ["b", "c", "a"], "a"],
    [
      "keeps the active tab when a background tab moves",
      "c",
      { dock: "right", group: 0, index: 0 },
      ["c", "a", "b"],
      "a",
    ],
    ["clamps an out-of-range index", "a", { dock: "right", group: 0, index: 99 }, ["b", "c", "a"], "a"],
  ])("placeTab within a group %s", (_name, tab, target, tabs, active) => {
    const l = placeTab(layout(["a", "b", "c"]), tab, target);
    expect(groups(l)).toEqual([[tabs, active]]);
  });

  it("placeTab across docks inserts at the index and activates the tab there", () => {
    const l = placeTab(layout(["a", "b"], ["x", "y"]), "a", { dock: "bottom", group: 0, index: 1 });
    expect(groups(l)).toEqual([[["b"], "b"]]);
    expect([l.bottom[0]!.tabs, l.bottom[0]!.active]).toEqual([["x", "a", "y"], "a"]);
  });

  it("placeTab never marks a moved plugin tab as closed", () => {
    const l = placeTab(layout(["plugin:p:a"]), "plugin:p:a", { dock: "bottom", group: 0, newGroup: true });
    expect(dockOf(l, "plugin:p:a")).toBe("bottom");
    expect(l.closedPlugins).toEqual([]);
  });

  it("split groups keep independent active tabs and prune only when emptied", () => {
    const l = split();
    expect(groups(l)).toEqual([
      [["a"], "a"],
      [["b"], "b"],
    ]);
    expect([isActiveTab(l, "a"), isActiveTab(l, "b")]).toEqual([true, true]);
    expect(groups(removeTab(l, "a"))).toEqual([[["b"], "b"]]);
    expect(groups(placeTab(l, "a", { dock: "right", group: 1, index: 1 }))).toEqual([[["b", "a"], "a"]]);
  });

  it("removeAllTerminals keeps other tabs", () => {
    let l = addTab(emptyLayout(), "right", "diff");
    l = addTerminal(addTerminal(l, "right").layout, "bottom").layout;
    l = removeAllTerminals(l);
    expect([dockTabs(l, "right"), dockTabs(l, "bottom")]).toEqual([["diff"], []]);
  });

  it("a closed plugin is not re-added by syncPluginTabs; a new one is", () => {
    let l = removeTab(layout(["plugin:p:a"]), "plugin:p:a");
    expect(l.closedPlugins).toContain("plugin:p:a");
    l = syncPluginTabs(l, [
      { id: "plugin:p:a", defaultDock: "right" },
      { id: "plugin:p:b", defaultDock: "bottom" },
    ]);
    expect([dockOf(l, "plugin:p:a"), dockOf(l, "plugin:p:b")]).toEqual([null, "bottom"]);
  });

  it("setDockCollapsed preserves tabs, the active tab, and close intent", () => {
    const open = setActive(layout(["diff", "plugin:p:a"]), "right", "plugin:p:a");
    const collapsed = setDockCollapsed(open, "right", true);
    expect(isDockCollapsed(collapsed, "right")).toBe(true);
    expect(collapsed.closedPlugins).toEqual([]);
    const reopened = setDockCollapsed(collapsed, "right", false);
    expect(isDockCollapsed(reopened, "right")).toBe(false);
    for (const l of [collapsed, reopened]) expect(groups(l)).toEqual([[["diff", "plugin:p:a"], "plugin:p:a"]]);
  });
});

describe("usePaneLayout migration + persistence", () => {
  const hook = (session = "s1") => renderHook(() => usePaneLayout(session)).result;
  const persistV2 = (right: { tabs: string[]; active: string }[], bottom: { tabs: string[]; active: string }[] = []) =>
    localStorage.setItem(
      "aoe-pane-layout-v2",
      JSON.stringify({
        version: 2,
        template: { right: [], bottom: [], nextTerminalIndex: 1, closedPlugins: [] },
        sessions: { s1: { right, bottom, nextTerminalIndex: 1, closedPlugins: [] } },
      }),
    );

  it("migrates the v1 layout to per-dock tabs", () => {
    localStorage.setItem(
      "aoe-pane-layout",
      JSON.stringify({ diff: { open: true, dock: "right" }, terminal: { open: true, dock: "bottom" } }),
    );
    const { layout: l } = hook().current;
    expect([dockTabs(l, "right"), dockTabs(l, "bottom")]).toEqual([["diff"], ["terminal:0"]]);
  });

  it("migrates the legacy collapsed flag to empty docks", () => {
    localStorage.setItem("aoe-right-collapsed", "1");
    const { layout: l } = hook().current;
    expect([l.right, l.bottom]).toEqual([[], []]);
  });

  it("keeps terminals and collapse state per session and persists them", () => {
    const result = hook();
    act(() => result.current.addTerminal("right"));
    act(() => result.current.setDockCollapsed("right", true));
    const other = hook("s2").current.layout;
    expect([dockTabs(other, "right").includes("terminal:1"), isDockCollapsed(other, "right")]).toEqual([false, false]);
    const reloaded = hook().current.layout;
    expect([dockTabs(reloaded, "right").includes("terminal:1"), isDockCollapsed(reloaded, "right")]).toEqual([
      true,
      true,
    ]);
  });

  it("syncPlugins adds a tab without revealing a collapsed dock", () => {
    const result = hook();
    act(() => result.current.setDockCollapsed("right", true));
    act(() => result.current.syncPlugins([{ id: "plugin:p:a", defaultDock: "right" }]));
    expect(dockOf(result.current.layout, "plugin:p:a")).toBe("right");
    expect(isDockCollapsed(result.current.layout, "right")).toBe(true);
  });

  it.each([
    ["openTab reveals a collapsed dock", "diff", false],
    ["togglePlugin reveals an open plugin instead of closing it", "plugin:p:a", true],
  ])("%s", (_name, tab, preopen) => {
    const result = hook();
    if (preopen) act(() => result.current.openTab(tab, "right"));
    act(() => result.current.setDockCollapsed("right", true));
    act(() => (preopen ? result.current.togglePlugin(tab, "right") : result.current.openTab(tab, "right")));
    expect(dockOf(result.current.layout, tab)).toBe("right");
    expect(isDockCollapsed(result.current.layout, "right")).toBe(false);
    expect(result.current.layout.closedPlugins).toEqual([]);
  });

  it("drops tab ids duplicated across docks or within a group on load", () => {
    persistV2([{ tabs: ["diff", "diff", "terminal:0"], active: "diff" }], [{ tabs: ["diff"], active: "diff" }]);
    const { layout: l } = hook().current;
    expect([dockTabs(l, "right"), dockTabs(l, "bottom")]).toEqual([["diff", "terminal:0"], []]);
  });

  it("loads multiple persisted groups per dock without merging them", () => {
    persistV2([
      { tabs: ["diff"], active: "diff" },
      { tabs: ["terminal:0"], active: "terminal:0" },
    ]);
    expect(groups(hook().current.layout)).toEqual([
      [["diff"], "diff"],
      [["terminal:0"], "terminal:0"],
    ]);
  });

  it("toggleKind adds then removes the terminal tabs", () => {
    localStorage.setItem("aoe-right-collapsed", "1");
    const result = hook();
    act(() => result.current.toggleKind("terminal", "right"));
    expect(dockTabs(result.current.layout, "right")).toEqual(["terminal:0"]);
    act(() => result.current.toggleKind("terminal", "right"));
    expect(dockTabs(result.current.layout, "right")).toEqual([]);
  });
});

describe("seedLayout (auto-open pane prefs, #3035)", () => {
  it.each([
    [true, true, ["diff", "terminal:0", "terminal:1"]],
    [false, true, ["terminal:0", "terminal:1"]],
    [true, false, ["diff"]],
    [false, false, []],
  ])("diff=%s terminal=%s keeps %o", (diff, terminal, expected) => {
    const seeded = seedLayout(layout(["diff", "terminal:0", "terminal:1"]), { diff, terminal });
    expect(dockTabs(seeded, "right")).toEqual(expected);
    expect(seeded.closedPlugins).toEqual([]);
  });

  it("is a no-op on an empty template", () => {
    const seeded = seedLayout(emptyLayout(), { diff: false, terminal: false });
    expect([dockTabs(seeded, "right"), dockTabs(seeded, "bottom")]).toEqual([[], []]);
  });
});
