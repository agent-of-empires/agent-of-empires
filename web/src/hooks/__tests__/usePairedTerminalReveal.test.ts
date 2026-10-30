// @vitest-environment jsdom

import { renderHook } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";

import type { DockLayout, PaneGroup } from "../../lib/paneLayout";
import { useOpenPairedTerminal, usePairedTerminalReveal } from "../usePairedTerminalReveal";

const focus = vi.hoisted(() => ({ dispatchFocusTerminal: vi.fn(), setPendingTerminalFocus: vi.fn() }));
vi.mock("../../lib/terminalFocus", () => focus);

const group = (tabs: string[], active: string | null = tabs[0] ?? null): PaneGroup => ({ tabs, active });
const layout = (right: PaneGroup[], bottom: PaneGroup[] = []): DockLayout => ({
  right,
  bottom,
  nextTerminalIndex: 3,
  closedPlugins: [],
  collapsed: { right: false, bottom: false },
});

afterEach(() => vi.clearAllMocks());

describe("usePairedTerminalReveal", () => {
  it.each<{
    label: string;
    singlePane: boolean;
    layout: DockLayout;
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
  ])("$label", ({ singlePane, layout, shows, focusedNow, activated, opened }) => {
    const raf = vi.spyOn(window, "requestAnimationFrame").mockImplementation((cb) => {
      cb(0);
      return 0;
    });
    const openTab = vi.fn();
    const activateTab = vi.fn();
    const setRightPanelView = vi.fn();
    const { result } = renderHook(() =>
      usePairedTerminalReveal({ singlePane, layout, openTab, activateTab, setRightPanelView }),
    );

    expect(result.current()).toBe(shows);
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
  it("opens the shell in the chosen mode only for the active Session", () => {
    const reveal = vi.fn(() => 2);
    const setMode = vi.fn();
    const { result } = renderHook(() => useOpenPairedTerminal("active", reveal, setMode));

    result.current("other", "container");
    expect(reveal).not.toHaveBeenCalled();
    expect(setMode).not.toHaveBeenCalled();

    result.current("active", "container");
    expect(setMode.mock.calls).toEqual([["active", 2, "container"]]);
  });
});
