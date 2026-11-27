// @vitest-environment jsdom

import { renderHook } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";

import { fileDraftKey, registerFileEditor, setFileDraft } from "../../lib/fileEditGuard";
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
    /** Focus dispatched next frame, after a collapsed dock opens around a mounted shell. */
    focusedNextFrame?: boolean;
    activated?: [string, string];
    opened?: [string, string];
  }>([
    {
      label: "one pane: promotes the paired view and focuses it next frame",
      singlePane: true,
      layout: layout([group(["terminal:active:2"])]),
      shows: 0,
      focusedNow: false,
    },
    {
      label: "an active terminal wins over an earlier inactive one, focused at once",
      singlePane: false,
      layout: layout([group(["diff", "terminal:active:1"])], [group(["terminal:active:2"])]),
      shows: 2,
      focusedNow: true,
    },
    {
      label: "an active terminal about to remount for a new mode latches focus for the new one",
      singlePane: false,
      layout: layout([group(["terminal:active:1"])]),
      remounts: true,
      shows: 1,
      focusedNow: false,
      activated: ["right", "terminal:active:1"],
    },
    {
      label: "an active terminal staying mounted is focused at once",
      singlePane: false,
      layout: layout([group(["terminal:active:1"])]),
      remounts: false,
      shows: 1,
      focusedNow: true,
    },
    {
      label: "an active terminal in a collapsed dock is revealed and focused once the dock opens",
      singlePane: false,
      layout: layout([], [group(["terminal:active:1"])], ["bottom"]),
      shows: 1,
      focusedNow: false,
      focusedNextFrame: true,
      activated: ["bottom", "terminal:active:1"],
    },
    {
      label: "Cmd+` with a terminal shown in each dock focuses the right one by its index",
      singlePane: false,
      layout: layout([group(["terminal:active:1"])], [group(["terminal:active:2"])]),
      shows: 1,
      focusedNow: true,
    },
    {
      label: "a visible terminal wins over an active one in a collapsed dock",
      singlePane: false,
      layout: layout([group(["terminal:active:1"])], [group(["terminal:active:2"])], ["right"]),
      shows: 2,
      focusedNow: true,
    },
    {
      label: "an inactive terminal tab is activated and focus waits for its PTY",
      singlePane: false,
      layout: layout([group(["diff", "terminal:active:1"])]),
      shows: 1,
      focusedNow: false,
      activated: ["right", "terminal:active:1"],
    },
    {
      label: "no terminal tab opens the first in the right dock, under the review panes",
      singlePane: false,
      layout: layout([group(["diff"])]),
      shows: 0,
      focusedNow: false,
      opened: ["terminal:active:0", "right"],
    },
  ])("$label", ({ singlePane, layout, remounts, shows, focusedNow, focusedNextFrame, activated, opened }) => {
    const raf = vi.spyOn(window, "requestAnimationFrame").mockImplementation((cb) => {
      cb(0);
      return 0;
    });
    const openTab = vi.fn();
    const activateTab = vi.fn();
    const showMobileShell = vi.fn();
    const prepare = remounts === undefined ? undefined : vi.fn(() => remounts);
    const { result } = renderHook(() =>
      usePairedTerminalReveal({ singlePane, activeSessionId: "active", layout, openTab, activateTab, showMobileShell }),
    );

    expect(result.current.revealActive(prepare)).toBe(shows);
    if (prepare) expect(prepare.mock.calls).toEqual([[shows]]);
    // Both name the shown terminal, so a second docked one doesn't take the focus.
    expect(focus.dispatchFocusTerminal.mock.calls).toEqual(
      singlePane || focusedNow || focusedNextFrame ? [["paired", shows, "active"]] : [],
    );
    // Focus that can't happen now is latched for the shell to take once ready.
    expect(focus.setPendingTerminalFocus.mock.calls).toEqual(focusedNow ? [] : [["paired", shows, "active"]]);
    expect(raf).toHaveBeenCalledTimes(singlePane || focusedNextFrame ? 1 : 0);
    expect(showMobileShell.mock.calls).toEqual(singlePane ? [[{ sessionId: "active", index: 0 }]] : []);
    expect(activateTab.mock.calls).toEqual(activated ? [activated] : []);
    expect(openTab.mock.calls).toEqual(opened ? [opened] : []);
    raf.mockRestore();
  });

  it("one pane: keeping an unsaved main-view edit leaves the view and mode untouched", () => {
    const key = fileDraftKey("s1", "notes.txt");
    const original = { content: "", version: "v1", is_binary: false, truncated: false };
    setFileDraft(key, { original, text: "draft", pendingSave: null });
    const release = registerFileEditor(key, null, vi.fn());
    const confirm = vi.spyOn(window, "confirm").mockReturnValue(false);
    const showMobileShell = vi.fn();
    const prepare = vi.fn(() => false);
    const { result } = renderHook(() =>
      usePairedTerminalReveal({
        singlePane: true,
        activeSessionId: "active",
        layout: layout([group(["terminal:active:0"])]),
        openTab: vi.fn(),
        activateTab: vi.fn(),
        showMobileShell,
      }),
    );

    expect(result.current.revealActive(prepare)).toBeNull();
    expect(confirm).toHaveBeenCalledOnce();
    expect(prepare).not.toHaveBeenCalled();
    expect(showMobileShell).not.toHaveBeenCalled();
    expect(focus.setPendingTerminalFocus).not.toHaveBeenCalled();
    release();
    setFileDraft(key, null);
    confirm.mockRestore();
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

  it("Open Terminal with a terminal shown in each dock focuses the one it set the mode on", () => {
    const setMode = vi.fn();
    const { result } = renderHook(() => {
      const { revealActive } = usePairedTerminalReveal({
        singlePane: false,
        activeSessionId: "active",
        layout: layout([group(["terminal:active:1"])], [group(["terminal:active:2"])]),
        openTab: vi.fn(),
        activateTab: vi.fn(),
        showMobileShell: vi.fn(),
      });
      return useOpenPairedTerminal("active", revealActive, () => "host", setMode);
    });

    result.current("active", "host");
    expect(setMode.mock.calls).toEqual([["active", 1, "host"]]);
    expect(focus.dispatchFocusTerminal.mock.calls).toEqual([["paired", 1, "active"]]);
  });
});

describe("revealShell", () => {
  it.each<{ label: string; layout: DockLayout; activated?: [string, string]; opened?: [string, string] }>([
    {
      label: "activates another Session's open tab",
      layout: layout([group(["terminal:active:0", "terminal:other:2"])]),
      activated: ["right", "terminal:other:2"],
    },
    {
      label: "opens a tab beside the other terminals",
      layout: layout([group(["diff"])], [group(["terminal:active:0"])]),
      opened: ["terminal:other:2", "bottom"],
    },
  ])("$label", ({ layout, activated, opened }) => {
    const openTab = vi.fn();
    const activateTab = vi.fn();
    const { result } = renderHook(() =>
      usePairedTerminalReveal({
        singlePane: false,
        activeSessionId: "active",
        layout,
        openTab,
        activateTab,
        showMobileShell: vi.fn(),
      }),
    );
    expect(result.current.revealShell({ sessionId: "other", index: 2 })).toBe(true);
    expect(focus.setPendingTerminalFocus.mock.calls).toEqual([["paired", 2, "other"]]);
    expect(activateTab.mock.calls).toEqual(activated ? [activated] : []);
    expect(openTab.mock.calls).toEqual(opened ? [opened] : []);
  });

  it("shows another Session's shell in the one-pane view", () => {
    vi.spyOn(window, "requestAnimationFrame").mockImplementation(() => 0);
    const showMobileShell = vi.fn();
    const { result } = renderHook(() =>
      usePairedTerminalReveal({
        singlePane: true,
        activeSessionId: "active",
        layout: layout([]),
        openTab: vi.fn(),
        activateTab: vi.fn(),
        showMobileShell,
      }),
    );
    result.current.revealShell({ sessionId: "other", index: 1 });
    expect(showMobileShell.mock.calls).toEqual([[{ sessionId: "other", index: 1 }]]);
  });
});
