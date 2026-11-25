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
  deriveLayout,
  placeTab,
  removeSessionTerminals,
  removeTab,
  seedLayout,
  setActive,
  setDockCollapsed,
  sweepOrphanPaneLayouts,
  syncPluginTabs,
  terminalPlacements,
  usePaneLayout,
  withTerminals,
  type AutoPane,
  type DockLayout,
  type PlaceTarget,
  type TerminalPlacement,
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
const docks = (l: DockLayout) => [dockTabs(l, "right"), dockTabs(l, "bottom")];
const allTabs = (l: DockLayout) => docks(l).flat();

describe("pane layout pure ops", () => {
  it("addTab keeps the review order Diff, Files, Background, plugins, then terminals", () => {
    let l = emptyLayout();
    for (const id of ["terminal:s1:0", "plugin:p:a", "agents", "files", "diff", "plugin:p:b"])
      l = addTab(l, "right", id);
    expect(dockTabs(l, "right")).toEqual(["diff", "files", "agents", "plugin:p:a", "plugin:p:b", "terminal:s1:0"]);
  });

  it("addTab appends and is idempotent per tab id", () => {
    const l = layout(["diff", "terminal:s1:0"]);
    expect(dockTabs(l, "right")).toEqual(["diff", "terminal:s1:0"]);
    expect(addTab(l, "right", "diff")).toBe(l);
  });

  it("addTerminal allocates a monotonic index", () => {
    const a = addTerminal(emptyLayout(), "right", "s1");
    const b = addTerminal(a.layout, "right", "s1");
    // A shell the Daemon already sees alive is skipped.
    const c = addTerminal(b.layout, "right", "s1", 6);
    expect([a.tabId, b.tabId, c.tabId, c.layout.nextTerminalIndex]).toEqual([
      "terminal:s1:1",
      "terminal:s1:2",
      "terminal:s1:6",
      7,
    ]);
  });

  it("removeTab fixes the active tab and prunes the empty dock", () => {
    let l = removeTab(setActive(layout(["diff", "terminal:s1:0"]), "right", "terminal:s1:0"), "terminal:s1:0");
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

  it("removeSessionTerminals keeps other tabs and other Sessions' terminals", () => {
    let l = addTab(emptyLayout(), "right", "diff");
    l = addTerminal(addTerminal(l, "right", "s1").layout, "bottom", "s1").layout;
    l = addTab(l, "bottom", "terminal:s2:0");
    l = removeSessionTerminals(l, "s1");
    expect([dockTabs(l, "right"), dockTabs(l, "bottom")]).toEqual([["diff"], ["terminal:s2:0"]]);
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
const V4_KEY = "aoe-pane-layout-v4";
const persistV3 = (sessions: Record<string, unknown>) =>
  localStorage.setItem(V3_KEY, JSON.stringify({ version: 3, sessions }));
const saved = () => JSON.parse(localStorage.getItem(V4_KEY) ?? "{}");
const savedSessions = () => Object.keys(saved().sessions ?? {});
const NO_PLUGINS: AutoPane[] = [];
const PLUGIN: AutoPane[] = [{ id: "plugin:p:a", defaultDock: "right" }];

describe("usePaneLayout persistence", () => {
  const hook = (session = "s1", prefs = CLOSED) =>
    renderHook(() => usePaneLayout(session, false, prefs, NO_PLUGINS)).result;
  const persistGroups = (
    right: { tabs: string[]; active: string }[],
    bottom: { tabs: string[]; active: string }[] = [],
  ) => persistV3({ s1: { right, bottom, nextTerminalIndex: 1, closedPlugins: [] } });

  it("shares terminal tabs across Sessions, keeps collapse state per Session, and persists both", () => {
    const result = hook();
    act(() => result.current.addTerminal("right"));
    act(() => result.current.setDockCollapsed("right", true));
    const other = hook("s2").current.layout;
    expect([dockTabs(other, "right").includes("terminal:s1:1"), isDockCollapsed(other, "right")]).toEqual([
      true,
      false,
    ]);
    const reloaded = hook().current.layout;
    expect([dockTabs(reloaded, "right").includes("terminal:s1:1"), isDockCollapsed(reloaded, "right")]).toEqual([
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
    expect(docks(result.current.layout)).toEqual([["diff", "plugin:p:a"], ["terminal:s1:0"]]);
    expect(savedSessions()).toEqual([]);

    act(() => result.current.setDockCollapsed("right", true));
    expect(saved().sessions.s1.right[0].tabs).toEqual(["diff", "plugin:p:a"]);
    expect(isDockCollapsed(result.current.layout, "right")).toBe(true);
  });

  it("finishes a deferred close with the derivation of the Session that started it", async () => {
    const { result, rerender } = renderHook(
      ({ session, outside }) => usePaneLayout(session, outside, OPEN, NO_PLUGINS),
      { initialProps: { session: "s1", outside: false } },
    );
    let finish!: (ok: boolean) => void;
    act(() => result.current.closeTab("terminal:s1:0", () => new Promise((r) => (finish = r))));
    rerender({ session: "s2", outside: true });
    await act(async () => finish(true));
    expect(saved().sessions.s1.right[0].tabs).toEqual(["diff"]);
  });

  it("adds a newly loaded plugin to a saved layout unless the user closed it", () => {
    const { result, rerender } = renderHook(({ plugins }) => usePaneLayout("s1", false, CLOSED, plugins), {
      initialProps: { plugins: NO_PLUGINS },
    });
    act(() => result.current.toggleKind("terminal", "right"));
    rerender({ plugins: PLUGIN });
    expect(dockTabs(result.current.layout, "right")).toEqual(["plugin:p:a", "terminal:s1:0"]);
    act(() => result.current.closeTab("plugin:p:a"));
    expect(dockTabs(result.current.layout, "right")).toEqual(["terminal:s1:0"]);
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
    persistGroups([{ tabs: ["diff", "diff", "terminal:s1:0"], active: "diff" }], [{ tabs: ["diff"], active: "diff" }]);
    const { layout: l } = hook().current;
    expect([dockTabs(l, "right"), dockTabs(l, "bottom")]).toEqual([["diff", "terminal:s1:0"], []]);
  });

  it("loads multiple persisted groups per dock without merging them", () => {
    persistGroups([
      { tabs: ["diff"], active: "diff" },
      { tabs: ["terminal:s1:0"], active: "terminal:s1:0" },
    ]);
    expect(groups(hook().current.layout)).toEqual([
      [["diff"], "diff"],
      [["terminal:s1:0"], "terminal:s1:0"],
    ]);
  });

  it("toggleKind adds then removes the terminal tabs", () => {
    const result = hook();
    act(() => result.current.toggleKind("terminal", "right"));
    expect(dockTabs(result.current.layout, "right")).toEqual(["terminal:s1:0"]);
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
    expect(dockTabs(result.current.layout, "right")).toEqual(["terminal:s1:0"]);
    expect(savedSessions()).toEqual(["s1"]);
  });

  it("sweeps saved layouts of sessions that are gone, keeping the live ones in view", () => {
    persistGroups([{ tabs: ["terminal:s1:0"], active: "terminal:s1:0" }]);
    const result = hook("s1", OPEN);
    const other = hook("s2");
    act(() => other.current.toggleKind("diff", "right"));
    act(() => sweepOrphanPaneLayouts(new Set(["s1"])));
    expect(savedSessions()).toEqual(["s1"]);
    act(() => sweepOrphanPaneLayouts(new Set()));
    expect(docks(result.current.layout)).toEqual([["diff"], ["terminal:s1:0"]]);
  });
});

describe("usePaneLayout upgrade from v2", () => {
  const V2_KEY = "aoe-pane-layout-v2";
  const shells = (l: DockLayout) => [dockTabs(l, "right"), dockTabs(l, "bottom"), l.nextTerminalIndex];

  it("keeps and binds the terminals of a v2 Session so a new tab never reuses a live shell, then writes v4", () => {
    // v2 stored unbound `terminal:<n>` ids.
    const v2 = (right: string[], bottom: string[], next: number) => ({
      ...layout(right, bottom),
      nextTerminalIndex: next,
    });
    localStorage.setItem(
      V2_KEY,
      JSON.stringify({
        version: 2,
        template: emptyLayout(),
        sessions: { s1: v2(["diff", "terminal:0", "terminal:1"], ["terminal:2"], 3), s2: layout(["diff"]) },
      }),
    );
    const { result } = renderHook(() => usePaneLayout("s1", false, CLOSED, NO_PLUGINS));
    expect(shells(result.current.layout)).toEqual([["diff", "terminal:s1:0", "terminal:s1:1"], ["terminal:s1:2"], 3]);

    act(() => sweepOrphanPaneLayouts(new Set(["s1", "s2"])));
    expect(savedSessions()).toEqual(["s1"]);
    expect(localStorage.getItem(V2_KEY)).toBeNull();
    act(() => result.current.addTerminal("right"));
    expect(dockTabs(result.current.layout, "right")).toContain("terminal:s1:3");
  });
});

describe("usePaneLayout upgrade from v3", () => {
  it("binds each Session's terminals to its own shells and keeps them per Session until a change", () => {
    persistV3({
      s1: { right: [{ tabs: ["terminal:0", "terminal:1"], active: "terminal:1" }], bottom: [], nextTerminalIndex: 2 },
      s2: { right: [{ tabs: ["diff", "terminal:0"], active: "terminal:0" }], bottom: [], nextTerminalIndex: 1 },
    });
    const s1 = renderHook(() => usePaneLayout("s1", false, CLOSED, NO_PLUGINS)).result;
    const s2 = renderHook(() => usePaneLayout("s2", false, CLOSED, NO_PLUGINS)).result;
    expect(groups(s1.current.layout)).toEqual([[["terminal:s1:0", "terminal:s1:1"], "terminal:s1:1"]]);
    expect(groups(s2.current.layout)).toEqual([[["diff", "terminal:s2:0"], "terminal:s2:0"]]);

    // The first change shares every saved Session's terminals, so it closes none of another Session's.
    act(() => s1.current.setDockCollapsed("bottom", true));
    expect(saved().terminals).toEqual([
      { id: "terminal:s1:0", dock: "right" },
      { id: "terminal:s1:1", dock: "right" },
      { id: "terminal:s2:0", dock: "right" },
    ]);
    expect(groups(s2.current.layout)).toEqual([
      [["diff", "terminal:s1:0", "terminal:s1:1", "terminal:s2:0"], "terminal:s2:0"],
    ]);
    act(() => sweepOrphanPaneLayouts(new Set(["s1", "s2"])));
    expect(localStorage.getItem(V3_KEY)).toBeNull();
  });
});

describe("terminal tabs across Sessions", () => {
  const hook = (session: string, prefs = CLOSED) =>
    renderHook(() => usePaneLayout(session, false, prefs, NO_PLUGINS)).result;

  it("keeps another Session's tab when its own terminals toggle, and closes tabs of a gone Session", () => {
    const s1 = hook("s1");
    act(() => s1.current.toggleKind("terminal", "right"));
    const s2 = hook("s2");
    act(() => s2.current.toggleKind("terminal", "bottom"));
    expect([dockTabs(s1.current.layout, "right"), dockTabs(s1.current.layout, "bottom")]).toEqual([
      ["terminal:s1:0"],
      ["terminal:s2:0"],
    ]);
    act(() => s2.current.toggleKind("terminal", "bottom"));
    expect(terminalPlacements(s1.current.layout)).toEqual([{ id: "terminal:s1:0", dock: "right" }]);

    act(() => s2.current.openTab("terminal:s2:3", "right"));
    act(() => sweepOrphanPaneLayouts(new Set(["s1"])));
    expect(dockTabs(s1.current.layout, "right")).toEqual(["terminal:s1:0"]);
    expect(saved().terminals).toEqual([{ id: "terminal:s1:0", dock: "right" }]);
  });

  it.each<[string, TerminalPlacement[] | null, string[]]>([
    ["seeds its own terminal before any is shared", null, ["terminal:s2:0"]],
    ["seeds its own terminal once every terminal is closed", [], ["terminal:s2:0"]],
    [
      "shows the shared terminals instead of seeding another",
      [{ id: "terminal:s1:4", dock: "right" }],
      ["terminal:s1:4"],
    ],
  ])("an unchanged Session %s", (_name, terminals, expected) => {
    expect(allTabs(deriveLayout("s2", undefined, terminals, { diff: false, terminal: true }))).toEqual(expected);
  });

  it("keeps a terminal the user saved on the right there, while a fresh Session opens its own at the bottom", () => {
    // Saved before the default moved: still on the right.
    persistV3({ s1: { right: [{ tabs: ["terminal:0"], active: "terminal:0" }], bottom: [], nextTerminalIndex: 1 } });
    const s1 = hook("s1");
    expect(docks(s1.current.layout)).toEqual([["terminal:s1:0"], []]);
    act(() => s1.current.setDockCollapsed("bottom", true));
    const fresh = hook("s2");
    expect(docks(fresh.current.layout)).toEqual([["terminal:s1:0"], []]);
    act(() => fresh.current.toggleKind("terminal", "bottom"));
    expect(docks(fresh.current.layout)).toEqual([["terminal:s1:0"], ["terminal:s2:0"]]);
    expect(docks(s1.current.layout)).toEqual([["terminal:s1:0"], ["terminal:s2:0"]]);
  });

  it("withTerminals moves and reorders a shared terminal to its shared placement, leaving other tabs and focus", () => {
    const l = withTerminals(layout(["terminal:s1:0", "diff", "terminal:s1:1"], ["terminal:s1:2"]), [
      { id: "terminal:s1:1", dock: "right" },
      { id: "terminal:s1:0", dock: "right" },
      { id: "terminal:s1:2", dock: "right" },
    ]);
    expect([groups(l), dockTabs(l, "bottom")]).toEqual([
      [[["diff", "terminal:s1:1", "terminal:s1:0", "terminal:s1:2"], "terminal:s1:0"]],
      [],
    ]);
    const same = layout(["diff", "terminal:s1:0"]);
    expect(withTerminals(same, [{ id: "terminal:s1:0", dock: "right" }])).toBe(same);
  });

  it("a terminal moved in one Session moves in another, and a change there does not move it back", () => {
    const s1 = hook("s1");
    act(() => s1.current.toggleKind("terminal", "bottom"));
    const s2 = hook("s2");
    act(() => s2.current.setDockCollapsed("right", true));
    act(() => s1.current.moveTab("terminal:s1:0", "right"));
    expect(docks(s2.current.layout)).toEqual([["terminal:s1:0"], []]);
    act(() => s2.current.setDockCollapsed("right", false));
    expect(saved().terminals).toEqual([{ id: "terminal:s1:0", dock: "right" }]);
  });

  it("withTerminals closes unshared terminal tabs and adds shared ones without taking focus", () => {
    const l = withTerminals(layout(["diff", "terminal:s1:0"]), [{ id: "terminal:s2:1", dock: "bottom" }]);
    expect([groups(l), dockTabs(l, "bottom")]).toEqual([[[["diff"], "diff"]], ["terminal:s2:1"]]);
  });
});

describe("usePaneLayout outside a repository (#4574)", () => {
  it.each([
    // [outside, terminal default, seeded right and bottom tabs]
    [false, true, [["diff"], ["terminal:s1:0"]]],
    // Not yet known: Diff waits for the Session row.
    [null, true, [[], ["terminal:s1:0"]]],
    [true, true, [[], ["terminal:s1:0"]]],
    [true, false, [[], []]],
  ])("outside=%s terminal=%s seeds %o with the Diff default on", (outside, terminal, expected) => {
    const { result } = renderHook(() => usePaneLayout("s1", outside, { diff: true, terminal }, NO_PLUGINS));
    expect(docks(result.current.layout)).toEqual(expected);
  });

  it("saves what was shown when the user acts while status is unknown", () => {
    const { result, rerender } = renderHook(({ outside }) => usePaneLayout("s1", outside, OPEN, NO_PLUGINS), {
      initialProps: { outside: null as boolean | null },
    });
    act(() => result.current.openTab("plugin:p:a", "right"));
    act(() => result.current.setDockCollapsed("right", true));
    act(() => result.current.setDockCollapsed("right", false));
    rerender({ outside: false });
    expect(docks(result.current.layout)).toEqual([["plugin:p:a"], ["terminal:s1:0"]]);
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
    [true, true, ["diff"], ["terminal:s1:0"]],
    [false, true, [], ["terminal:s1:0"]],
    [true, false, ["diff"], []],
    [false, false, [], []],
  ])(
    "diff=%s terminal=%s opens %o on the right and %o at the bottom, each first active",
    (diff, terminal, right, bottom) => {
      const seeded = seedLayout({ diff, terminal }, "s1");
      expect(docks(seeded)).toEqual([right, bottom]);
      expect([seeded.right[0]?.active ?? null, seeded.bottom[0]?.active ?? null]).toEqual([
        right[0] ?? null,
        bottom[0] ?? null,
      ]);
    },
  );
});

describe("usePaneLayout file edit guard", () => {
  const key = fileDraftKey("s1", "notes.txt");
  const original = { content: "", version: "v1", is_binary: false, truncated: false };
  afterEach(() => {
    setFileDraft(key, null);
    vi.restoreAllMocks();
  });

  /** A dirty Files editor in the right dock beside a terminal in the bottom dock. */
  function editing(start = layout(["files"], ["terminal:s1:0"]), tabAvailable?: (id: string) => boolean) {
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
    act(() => result.current.closeTab("terminal:s1:0"));
    act(() => result.current.setDockCollapsed("bottom", true));
    expect(dockTabs(result.current.layout, "bottom")).toEqual(["terminal:s1:1"]);
    expect(isDockCollapsed(result.current.layout, "bottom")).toBe(true);
    expect([confirm.mock.calls.length, discard.mock.calls.length]).toEqual([0, 0]);
    release();
  });

  it("asks before hiding or moving the edited tab and keeps the layout when declined", () => {
    const { result, discard, release, confirm } = editing();
    act(() => result.current.openTab("terminal:s1:1", "right"));
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
    const start = placeTab(layout(["terminal:s1:1", "files"]), "files", { dock: "right", group: 1, newGroup: true });
    const { result, discard, release, confirm } = editing(start);
    act(() => result.current.closeTab("terminal:s1:1"));
    expect(dockTabs(result.current.layout, "right")).toEqual(["terminal:s1:1", "files"]);
    expect([confirm.mock.calls.length, discard.mock.calls.length]).toEqual([1, 0]);
    release();
  });

  it("asks when the edited tab is shown in place of an unloaded plugin tab", () => {
    const start = setActive(layout(["plugin:x:pane", "files"]), "right", "plugin:x:pane");
    const { result, release, confirm } = editing(start, (id) => !id.startsWith("plugin:"));
    act(() => result.current.addTerminal("right"));
    expect(dockTabs(result.current.layout, "right")).toEqual(["files", "plugin:x:pane"]);
    expect(confirm).toHaveBeenCalledOnce();
    release();
  });

  it("asks before a close's side effect and skips it when declined or a save is pending", async () => {
    const start = placeTab(layout(["terminal:s1:1", "files"]), "files", { dock: "right", group: 1, newGroup: true });
    const { result, release, confirm } = editing(start);
    const kill = vi.fn(async () => true);
    act(() => result.current.closeTab("terminal:s1:1", kill));
    expect([confirm.mock.calls.length, kill.mock.calls.length]).toEqual([1, 0]);
    setFileDraft(key, { original, text: "draft", pendingSave: { promise: new Promise(() => {}) } });
    act(() => result.current.closeTab("terminal:s1:1", kill));
    expect([confirm.mock.calls.length, kill.mock.calls.length]).toEqual([1, 0]);
    setFileDraft(key, { original, text: "draft", pendingSave: null });
    confirm.mockReturnValue(true);
    await act(async () => result.current.closeTab("terminal:s1:1", kill));
    expect(kill).toHaveBeenCalledOnce();
    expect(dockTabs(result.current.layout, "right")).toEqual(["files"]);
    release();
  });

  it("keeps the tab when its close side effect fails", async () => {
    const { result, release, confirm } = editing(layout(["files"], ["terminal:s1:1"]));
    await act(async () => result.current.closeTab("terminal:s1:1", async () => false));
    expect(dockTabs(result.current.layout, "bottom")).toEqual(["terminal:s1:1"]);
    expect(confirm).not.toHaveBeenCalled();
    release();
  });
});
