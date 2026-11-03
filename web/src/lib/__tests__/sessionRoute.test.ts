// @vitest-environment jsdom

import { afterEach, describe, expect, it, vi } from "vitest";

import { focusSessionPath, loadedFocusSessionId, openFocusWindow } from "../sessionRoute";

describe("focus window routes", () => {
  afterEach(() => vi.restoreAllMocks());

  it("opens a session's focus window beside this one, with its id escaped", () => {
    const open = vi.spyOn(window, "open").mockReturnValue(null);
    openFocusWindow("a b/c");
    expect(open).toHaveBeenCalledWith("/session/a%20b%2Fc?focus=1", "_blank");
    expect(focusSessionPath("a b/c")).toBe("/session/a%20b%2Fc?focus=1");
  });

  it("reads the focus session from the URL the document loaded with, not the current route", () => {
    const entries = vi.spyOn(performance, "getEntriesByType");
    entries.mockReturnValue([{ name: "https://aoe.test/session/s1?focus=1" } as PerformanceEntry]);
    expect(loadedFocusSessionId()).toBe("s1");

    entries.mockReturnValue([{ name: "https://aoe.test/session/s1" } as PerformanceEntry]);
    expect(loadedFocusSessionId()).toBeNull();

    // Without a navigation entry, the current URL is the best guess.
    entries.mockReturnValue([]);
    window.history.replaceState(null, "", "/session/s2?focus=1");
    expect(loadedFocusSessionId()).toBe("s2");
    window.history.replaceState(null, "", "/");
  });
});
