// @vitest-environment jsdom

import { renderHook } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";

import type { DockLayout, PaneGroup } from "../../lib/paneLayout";
import type { DockLocation } from "../../lib/panes";
import { useOpenPairedTerminal, usePairedTerminalReveal } from "../usePairedTerminalReveal";

const focus = vi.hoisted(() => ({ dispatchFocusTerminal: vi.fn(), setPendingTerminalFocus: vi.fn() }));
vi.mock("../../lib/terminalFocus", () => focus);

const group = (tabs: string[], active: string | null = tabs[0] ?? null): PaneGroup => ({ tabs, active });
const layout = (right: PaneGroup[], bottom: PaneGroup[] = [], collapsed: DockLocation[] = []): DockLayout => ({
  right,
  bottom,
  nextTerminalIndex: 3,
  closedPlugins: [],
  collapsed: { right: collapsed.includes("right"), bottom: collapsed.includes("bottom") },
});

afterEach(() => vi.clearAllMocks());

describe("usePairedTerminalReveal", () => {
  it.each<{
    label: string;
    singlePane: boolean;
    layout: DockLayout;
    /** What `prepare` reports; omitted, none is passed. */
    remounts?: boolean;
    shows: number;
    focusedNow: boolean;
    activated?: [string, string];
    opened?: [string, string];
  }>([
    {
      label: "one pane: promotes the paired view and focuses it next frame",
      singlePane: true,
      layout: layout([group(["terminal:2"])]),
      shows: 0,
      focusedNow: false,
    },
    {
      label: "an active terminal wins over an earlier inactive one, focused at once",
      singlePane: false,
      layout: layout([group(["diff", "terminal:1"])], [group(["terminal:2"])]),
      shows: 2,
      focusedNow: true,
    },
    {
      label: "an active terminal about to remount for a new mode latches focus for the new one",
      singlePane: false,
      layout: layout([group(["terminal:1"])]),
      remounts: true,
      shows: 1,
      focusedNow: false,
      activated: ["right", "terminal:1"],
    },
    {
      label: "an active terminal staying mounted is focused at once",
      singlePane: false,
      layout: layout([group(["terminal:1"])]),
      remounts: false,
      shows: 1,
      focusedNow: true,
    },
    {
      label: "an active terminal in a collapsed dock is revealed, and focus waits for it",
      singlePane: false,
      layout: layout([group(["terminal:1"])], [], ["right"]),
      shows: 1,
      focusedNow: false,
      activated: ["right", "terminal:1"],
    },
    {
      label: "a visible terminal wins over an active one in a collapsed dock",
      singlePane: false,
      layout: layout([group(["terminal:1"])], [group(["terminal:2"])], ["right"]),
      shows: 2,
      focusedNow: true,
    },
    {
      label: "an inactive terminal tab is activated and focus waits for its PTY",
      singlePane: false,
      layout: layout([group(["diff", "terminal:1"])]),
      shows: 1,
      focusedNow: false,
      activated: ["right", "terminal:1"],
    },
    {
      label: "no terminal tab opens the first on the right",
      singlePane: false,
      layout: layout([group(["diff"])]),
      shows: 0,
      focusedNow: false,
      opened: ["terminal:0", "right"],
    },
  ])("$label", ({ singlePane, layout, remounts, shows, focusedNow, activated, opened }) => {
    const raf = vi.spyOn(window, "requestAnimationFrame").mockImplementation((cb) => {
      cb(0);
      return 0;
    });
    const openTab = vi.fn();
    const activateTab = vi.fn();
    const setRightPanelView = vi.fn();
    const prepare = remounts === undefined ? undefined : vi.fn(() => remounts);
    const { result } = renderHook(() =>
      usePairedTerminalReveal({ singlePane, layout, openTab, activateTab, setRightPanelView }),
    );

    expect(result.current(prepare)).toBe(shows);
    if (prepare) expect(prepare.mock.calls).toEqual([[shows]]);
    expect(focus.dispatchFocusTerminal.mock.calls).toEqual(singlePane || focusedNow ? [["paired"]] : []);
    // Focus that can't happen now is latched for the shell to take once ready.
    expect(focus.setPendingTerminalFocus.mock.calls).toEqual(focusedNow ? [] : [["paired"]]);
    expect(raf).toHaveBeenCalledTimes(singlePane ? 1 : 0);
    expect(setRightPanelView.mock.calls).toEqual(singlePane ? [["paired"]] : []);
    expect(activateTab.mock.calls).toEqual(activated ? [activated] : []);
    expect(openTab.mock.calls).toEqual(opened ? [opened] : []);
    raf.mockRestore();
  });
});

describe("useOpenPairedTerminal", () => {
  it("sets the chosen mode on the shown terminal, only for the active Session, and says if it remounts", () => {
    const remounts: boolean[] = [];
    const reveal = vi.fn((prepare?: (terminalIndex: number) => boolean) => {
      remounts.push(prepare!(2));
      return 2;
    });
    const shownMode = vi.fn(() => "host" as const);
    const setMode = vi.fn();
    const { result } = renderHook(() => useOpenPairedTerminal("active", reveal, shownMode, setMode));

    result.current("other", "container");
    expect(reveal).not.toHaveBeenCalled();
    expect(setMode).not.toHaveBeenCalled();

    result.current("active", "container");
    result.current("active", "host");
    expect(setMode.mock.calls).toEqual([
      ["active", 2, "container"],
      ["active", 2, "host"],
    ]);
    expect(shownMode).toHaveBeenCalledWith("active", 2);
    expect(remounts).toEqual([true, false]);
  });
});
