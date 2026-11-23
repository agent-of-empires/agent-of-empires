// @vitest-environment jsdom
import { act, renderHook } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import { fileDraftKey, registerFileEditor, setFileDraft } from "../fileEditGuard";

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
  sweepOrphanPaneLayouts,
  syncPluginTabs,
  usePaneLayout,
  type AutoPane,
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

const CLOSED = { diff: false, terminal: false };
const OPEN = { diff: true, terminal: true };
const V3_KEY = "aoe-pane-layout-v3";
const persistV3 = (sessions: Record<string, unknown>) =>
  localStorage.setItem(V3_KEY, JSON.stringify({ version: 3, sessions }));
const savedSessions = () => Object.keys(JSON.parse(localStorage.getItem(V3_KEY) ?? "{}").sessions ?? {});
const NO_PLUGINS: AutoPane[] = [];
const PLUGIN: AutoPane[] = [{ id: "plugin:p:a", defaultDock: "right" }];

describe("usePaneLayout persistence", () => {
  const hook = (session = "s1", prefs = CLOSED) =>
    renderHook(() => usePaneLayout(session, false, prefs, NO_PLUGINS)).result;
  const persistGroups = (
    right: { tabs: string[]; active: string }[],
    bottom: { tabs: string[]; active: string }[] = [],
  ) => persistV3({ s1: { right, bottom, nextTerminalIndex: 1, closedPlugins: [] } });

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

  it("shows an auto-opened plugin without saving it, and saves it with the user's next change", () => {
    const { result, rerender } = renderHook(({ prefs }) => usePaneLayout("s1", false, prefs, PLUGIN), {
      initialProps: { prefs: CLOSED },
    });
    expect(dockTabs(result.current.layout, "right")).toEqual(["plugin:p:a"]);
    rerender({ prefs: OPEN });
    expect(dockTabs(result.current.layout, "right")).toEqual(["diff", "terminal:0", "plugin:p:a"]);
    expect(savedSessions()).toEqual([]);

    act(() => result.current.setDockCollapsed("right", true));
    expect(JSON.parse(localStorage.getItem(V3_KEY)!).sessions.s1.right[0].tabs).toEqual([
      "diff",
      "terminal:0",
      "plugin:p:a",
    ]);
    expect(isDockCollapsed(result.current.layout, "right")).toBe(true);
  });

  it("finishes a deferred close with the derivation of the Session that started it", async () => {
    const { result, rerender } = renderHook(
      ({ session, outside }) => usePaneLayout(session, outside, OPEN, NO_PLUGINS),
      { initialProps: { session: "s1", outside: false } },
    );
    let finish!: (ok: boolean) => void;
    act(() => result.current.closeTab("terminal:0", () => new Promise((r) => (finish = r))));
    rerender({ session: "s2", outside: true });
    await act(async () => finish(true));
    expect(JSON.parse(localStorage.getItem(V3_KEY)!).sessions.s1.right[0].tabs).toEqual(["diff"]);
  });

  it("adds a newly loaded plugin to a saved layout unless the user closed it", () => {
    const { result, rerender } = renderHook(({ plugins }) => usePaneLayout("s1", false, CLOSED, plugins), {
      initialProps: { plugins: NO_PLUGINS },
    });
    act(() => result.current.toggleKind("terminal", "right"));
    rerender({ plugins: PLUGIN });
    expect(dockTabs(result.current.layout, "right")).toEqual(["terminal:0", "plugin:p:a"]);
    act(() => result.current.closeTab("plugin:p:a"));
    expect(dockTabs(result.current.layout, "right")).toEqual(["terminal:0"]);
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
    persistGroups([{ tabs: ["diff", "diff", "terminal:0"], active: "diff" }], [{ tabs: ["diff"], active: "diff" }]);
    const { layout: l } = hook().current;
    expect([dockTabs(l, "right"), dockTabs(l, "bottom")]).toEqual([["diff", "terminal:0"], []]);
  });

  it("loads multiple persisted groups per dock without merging them", () => {
    persistGroups([
      { tabs: ["diff"], active: "diff" },
      { tabs: ["terminal:0"], active: "terminal:0" },
    ]);
    expect(groups(hook().current.layout)).toEqual([
      [["diff"], "diff"],
      [["terminal:0"], "terminal:0"],
    ]);
  });

  it("toggleKind adds then removes the terminal tabs", () => {
    const result = hook();
    act(() => result.current.toggleKind("terminal", "right"));
    expect(dockTabs(result.current.layout, "right")).toEqual(["terminal:0"]);
    act(() => result.current.toggleKind("terminal", "right"));
    expect(dockTabs(result.current.layout, "right")).toEqual([]);
  });

  it("saves nothing until the user changes a layout, so the defaults keep applying", () => {
    const { result, rerender } = renderHook(({ prefs }) => usePaneLayout("s1", false, prefs, NO_PLUGINS), {
      initialProps: { prefs: OPEN },
    });
    expect(savedSessions()).toEqual([]);
    rerender({ prefs: CLOSED });
    expect(dockTabs(result.current.layout, "right")).toEqual([]);

    act(() => result.current.toggleKind("terminal", "right"));
    rerender({ prefs: OPEN });
    expect(dockTabs(result.current.layout, "right")).toEqual(["terminal:0"]);
    expect(savedSessions()).toEqual(["s1"]);
  });

  it("sweeps saved layouts of sessions that are gone, keeping the live ones in view", () => {
    persistGroups([{ tabs: ["terminal:0"], active: "terminal:0" }]);
    const result = hook("s1", OPEN);
    const other = hook("s2");
    act(() => other.current.toggleKind("diff", "right"));
    act(() => sweepOrphanPaneLayouts(new Set(["s1"])));
    expect(savedSessions()).toEqual(["s1"]);
    act(() => sweepOrphanPaneLayouts(new Set()));
    expect(dockTabs(result.current.layout, "right")).toEqual(["diff", "terminal:0"]);
  });
});

describe("usePaneLayout upgrade from v2", () => {
  const V2_KEY = "aoe-pane-layout-v2";
  const shells = (l: DockLayout) => [dockTabs(l, "right"), dockTabs(l, "bottom"), l.nextTerminalIndex];

  it("keeps the terminals of a v2 Session so a new tab never reuses a live shell, and writes v3 before deleting v2", () => {
    let terminals = layout(["diff", "terminal:0"]);
    terminals = addTerminal(terminals, "right").layout;
    terminals = addTerminal(terminals, "bottom").layout;
    localStorage.setItem(
      V2_KEY,
      JSON.stringify({ version: 2, template: emptyLayout(), sessions: { s1: terminals, s2: layout(["diff"]) } }),
    );
    const { result } = renderHook(() => usePaneLayout("s1", false, CLOSED, NO_PLUGINS));
    expect(shells(result.current.layout)).toEqual([["diff", "terminal:0", "terminal:1"], ["terminal:2"], 3]);

    act(() => sweepOrphanPaneLayouts(new Set(["s1", "s2"])));
    expect(savedSessions()).toEqual(["s1"]);
    expect(localStorage.getItem(V2_KEY)).toBeNull();
    act(() => result.current.addTerminal("right"));
    expect(dockTabs(result.current.layout, "right")).toContain("terminal:3");
  });
});

describe("usePaneLayout outside a repository (#4574)", () => {
  it.each([
    // [outside, terminal default, seeded tabs]
    [false, true, ["diff", "terminal:0"]],
    // Not yet known: Diff waits for the Session row.
    [null, true, ["terminal:0"]],
    [true, true, ["terminal:0"]],
    [true, false, []],
  ])("outside=%s terminal=%s seeds %o with the Diff default on", (outside, terminal, expected) => {
    const { result } = renderHook(() => usePaneLayout("s1", outside, { diff: true, terminal }, NO_PLUGINS));
    expect(dockTabs(result.current.layout, "right")).toEqual(expected);
  });

  it("saves what was shown when the user acts while status is unknown", () => {
    const { result, rerender } = renderHook(({ outside }) => usePaneLayout("s1", outside, OPEN, NO_PLUGINS), {
      initialProps: { outside: null as boolean | null },
    });
    act(() => result.current.openTab("plugin:p:a", "right"));
    act(() => result.current.setDockCollapsed("right", true));
    act(() => result.current.setDockCollapsed("right", false));
    rerender({ outside: false });
    expect(dockTabs(result.current.layout, "right")).toEqual(["terminal:0", "plugin:p:a"]);
    expect(isDockCollapsed(result.current.layout, "right")).toBe(false);
  });

  it("applies a flag that arrives after seeding, but never closes a Diff the user opened", () => {
    const { result, rerender } = renderHook(({ outside }) => usePaneLayout("s1", outside, OPEN, NO_PLUGINS), {
      initialProps: { outside: false },
    });
    expect(dockTabs(result.current.layout, "right")).toContain("diff");
    rerender({ outside: true });
    expect(dockTabs(result.current.layout, "right")).not.toContain("diff");

    act(() => result.current.openTab("diff", "right"));
    rerender({ outside: true });
    expect(dockOf(result.current.layout, "diff")).toBe("right");
    expect(isDockCollapsed(result.current.layout, "right")).toBe(false);
  });
});

describe("seedLayout", () => {
  it.each([
    [true, true, ["diff", "terminal:0"]],
    [false, true, ["terminal:0"]],
    [true, false, ["diff"]],
    [false, false, []],
  ])("diff=%s terminal=%s opens %o with the first active", (diff, terminal, expected) => {
    const seeded = seedLayout({ diff, terminal });
    expect([dockTabs(seeded, "right"), seeded.right[0]?.active ?? null]).toEqual([expected, expected[0] ?? null]);
    expect(dockTabs(seeded, "bottom")).toEqual([]);
  });
});

describe("usePaneLayout file edit guard", () => {
  const key = fileDraftKey("s1", "notes.txt");
  const original = { content: "", version: "v1", is_binary: false, truncated: false };
  afterEach(() => {
    setFileDraft(key, null);
    vi.restoreAllMocks();
  });

  /** A dirty Files editor in the right dock beside a terminal in the bottom dock. */
  function editing(start = layout(["files"], ["terminal:0"]), tabAvailable?: (id: string) => boolean) {
    persistV3({ s1: start });
    const discard = vi.fn();
    setFileDraft(key, { original, text: "draft", pendingSave: null });
    const release = registerFileEditor(key, "files", discard);
    const confirm = vi.spyOn(window, "confirm").mockReturnValue(false);
    return {
      result: renderHook(() => usePaneLayout("s1", false, CLOSED, NO_PLUGINS, tabAvailable)).result,
      discard,
      release,
      confirm,
    };
  }

  it("changes the other dock without asking or discarding the visible draft", () => {
    const { result, discard, release, confirm } = editing();
    act(() => result.current.addTerminal("bottom"));
    act(() => result.current.closeTab("terminal:0"));
    act(() => result.current.setDockCollapsed("bottom", true));
    expect(dockTabs(result.current.layout, "bottom")).toEqual(["terminal:1"]);
    expect(isDockCollapsed(result.current.layout, "bottom")).toBe(true);
    expect([confirm.mock.calls.length, discard.mock.calls.length]).toEqual([0, 0]);
    release();
  });

  it("asks before hiding or moving the edited tab and keeps the layout when declined", () => {
    const { result, discard, release, confirm } = editing();
    act(() => result.current.openTab("terminal:1", "right"));
    act(() => result.current.moveTab("files", "bottom"));
    act(() => result.current.setDockCollapsed("right", true));
    expect([dockTabs(result.current.layout, "right"), isDockCollapsed(result.current.layout, "right")]).toEqual([
      ["files"],
      false,
    ]);
    expect([confirm.mock.calls.length, discard.mock.calls.length]).toEqual([3, 0]);
    confirm.mockReturnValue(true);
    act(() => result.current.closeTab("files"));
    expect(dockTabs(result.current.layout, "right")).toEqual([]);
    expect(discard).toHaveBeenCalledOnce();
    release();
  });

  it("asks when closing an earlier split group renumbers the edited one", () => {
    const start = placeTab(layout(["terminal:1", "files"]), "files", { dock: "right", group: 1, newGroup: true });
    const { result, discard, release, confirm } = editing(start);
    act(() => result.current.closeTab("terminal:1"));
    expect(dockTabs(result.current.layout, "right")).toEqual(["terminal:1", "files"]);
    expect([confirm.mock.calls.length, discard.mock.calls.length]).toEqual([1, 0]);
    release();
  });

  it("asks when the edited tab is shown in place of an unloaded plugin tab", () => {
    const start = setActive(layout(["plugin:x:pane", "files"]), "right", "plugin:x:pane");
    const { result, release, confirm } = editing(start, (id) => !id.startsWith("plugin:"));
    act(() => result.current.addTerminal("right"));
    expect(dockTabs(result.current.layout, "right")).toEqual(["plugin:x:pane", "files"]);
    expect(confirm).toHaveBeenCalledOnce();
    release();
  });

  it("asks before a close's side effect and skips it when declined or a save is pending", async () => {
    const start = placeTab(layout(["terminal:1", "files"]), "files", { dock: "right", group: 1, newGroup: true });
    const { result, release, confirm } = editing(start);
    const kill = vi.fn(async () => true);
    act(() => result.current.closeTab("terminal:1", kill));
    expect([confirm.mock.calls.length, kill.mock.calls.length]).toEqual([1, 0]);
    setFileDraft(key, { original, text: "draft", pendingSave: { promise: new Promise(() => {}) } });
    act(() => result.current.closeTab("terminal:1", kill));
    expect([confirm.mock.calls.length, kill.mock.calls.length]).toEqual([1, 0]);
    setFileDraft(key, { original, text: "draft", pendingSave: null });
    confirm.mockReturnValue(true);
    await act(async () => result.current.closeTab("terminal:1", kill));
    expect(kill).toHaveBeenCalledOnce();
    expect(dockTabs(result.current.layout, "right")).toEqual(["files"]);
    release();
  });

  it("keeps the tab when its close side effect fails", async () => {
    const { result, release, confirm } = editing(layout(["files"], ["terminal:1"]));
    await act(async () => result.current.closeTab("terminal:1", async () => false));
    expect(dockTabs(result.current.layout, "bottom")).toEqual(["terminal:1"]);
    expect(confirm).not.toHaveBeenCalled();
    release();
  });
});
